//! Unified file watcher that routes events to pluggable handlers.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use notify::{Event, EventKind, RecursiveMode, Watcher};
use tokio::sync::{RwLock, mpsc};
use tokio::task::JoinHandle;
use tokio::time::{Duration, sleep};

use crate::documents::DocumentStore;
use crate::documents::config::ChunkingConfig;
use crate::error::IndexError;
use crate::indexing::ReindexOutcome;
use crate::indexing::facade::IndexFacade;
use crate::mcp::notifications::{FileChangeEvent, NotificationBroadcaster};

use super::debouncer::Debouncer;
use super::error::WatchError;
use super::handler::{WatchAction, WatchHandler};
use super::path_registry::PathRegistry;

/// Unified file watcher with pluggable handlers.
///
/// Provides a single `notify::RecommendedWatcher` that routes file events
/// to appropriate handlers based on path matching.
pub struct UnifiedWatcher {
    /// Registered handlers.
    handlers: Vec<Box<dyn WatchHandler>>,
    /// Path registry for tracking and directory computation.
    registry: PathRegistry,
    /// Shared debouncer for all file events.
    debouncer: Debouncer,
    /// Debouncer for created-directory discovery scopes, kept as a second,
    /// separate `Debouncer` instance rather than sharing `debouncer`: the
    /// file debouncer carries paths destined for re-index, while a
    /// directory path recorded here means "extend the watch set and walk
    /// for catch-up files" -- a different action on the other side of the
    /// queue, even though both share the same record/take_ready mechanics.
    dir_debouncer: Debouncer,
    /// Channel for receiving file events.
    event_rx: mpsc::Receiver<notify::Result<Event>>,
    /// The underlying file watcher.
    _watcher: notify::RecommendedWatcher,
    /// Notification broadcaster for MCP integration.
    broadcaster: Arc<NotificationBroadcaster>,
    /// Shared facade for executing code actions.
    facade: Arc<RwLock<IndexFacade>>,
    /// Document store for executing document actions (optional).
    document_store: Option<Arc<RwLock<DocumentStore>>>,
    /// Chunking config for document re-indexing.
    chunking_config: ChunkingConfig,
    /// Path for semantic search persistence.
    index_path: PathBuf,
    /// Workspace root for path resolution.
    workspace_root: PathBuf,
    /// Whether the index is potentially stale due to a backend overflow/rescan
    /// or watch error (i.e. we may have missed filesystem events).
    stale: bool,
    /// When the staleness window started (or was last extended by a new signal).
    stale_since: Option<Instant>,
    /// Whether to actively refresh the index when an overflow/rescan is detected.
    refresh_on_overflow: bool,
    /// Quiet window duration used both for debouncing individual file events
    /// and for deciding when a stale/overflow episode has settled enough to
    /// fire a catch-up reindex.
    debounce_window: Duration,
    /// In-flight catch-up reindex task, if one is currently running. Guards
    /// against a second overflow signal firing a duplicate full reindex
    /// while one is already in progress, and lets the run loop keep
    /// draining `event_rx`/`broadcast_rx` while the reindex runs.
    catch_up_task: Option<JoinHandle<Result<ReindexOutcome, WatchError>>>,
    /// When the in-flight catch-up task (if any) was spawned. Used on
    /// success to detect whether a newer overflow/rescan signal arrived
    /// *after* this task's walk began (in which case that signal's changes
    /// may not be reflected in the completed walk, so staleness must stay
    /// armed rather than being cleared out from under it).
    catch_up_started_at: Option<Instant>,
    /// When the most recent catch-up reindex (success or failure) completed,
    /// used to enforce `CATCH_UP_COOLDOWN` independent of `debounce_window`.
    last_catch_up_completed: Option<Instant>,
    /// Consecutive catch-up failures for the current stale episode, used to
    /// bound retries so a permanent failure does not hot-loop forever.
    catch_up_attempts: u32,
    /// Consecutive contention rejections (`IndexError::ReindexInProgress`),
    /// tracked separately from `catch_up_attempts` since contention is not a
    /// genuine failure and must never consume a bounded attempt. Reset on
    /// any successful catch-up or genuine failure, so it measures a
    /// continuous contention streak rather than a lifetime total; used to
    /// escalate logging if the gate holder appears wedged (see
    /// `CONSECUTIVE_CONTENTION_WARN_THRESHOLD`).
    consecutive_contention: u32,
    /// Registered watch roots from handlers; scopes created-directory
    /// handling and stays watched even when a root holds no indexed
    /// file directly.
    handler_roots: Vec<PathBuf>,
    /// Test-only counter of `discoverable_entries_for` invocations from the
    /// created-directory drain path, so tests can assert the number of
    /// walks a burst produces directly instead of only inferring "one walk"
    /// from its side effects (which a per-scope-walk regression could still
    /// satisfy).
    #[cfg(test)]
    walk_count: Arc<std::sync::atomic::AtomicUsize>,
}

impl UnifiedWatcher {
    /// Create a builder for configuring the watcher.
    pub fn builder() -> UnifiedWatcherBuilder {
        UnifiedWatcherBuilder::new()
    }

    /// Start watching for file changes.
    ///
    /// This is the main event loop that:
    /// 1. Receives file events from notify
    /// 2. Debounces modification events
    /// 3. Routes events to matching handlers
    /// 4. Executes returned actions
    /// 5. Broadcasts notifications
    pub async fn watch(mut self) -> Result<(), WatchError> {
        // Initialize all handlers
        for handler in &self.handlers {
            if let Err(e) = handler.refresh_paths().await {
                tracing::warn!(
                    "[watcher] failed to initialize {} handler: {e}",
                    handler.name()
                );
            }
        }

        // Collect all paths from handlers and register them
        let mut all_paths = Vec::new();
        for handler in &self.handlers {
            all_paths.extend(handler.tracked_paths().await);
        }

        let new_dirs = self.registry.add_paths(all_paths);
        let total_paths = self.registry.path_count();
        let total_dirs = self.registry.dir_count();

        if total_paths == 0 {
            tracing::warn!("[watcher] no files to watch - index some files first");
        } else {
            crate::log_event!(
                "watcher",
                "monitoring",
                "{total_paths} files in {total_dirs} directories"
            );
        }

        // Watch all directories
        for dir in new_dirs {
            self.watch_directory(&dir)?;
        }

        self.register_handler_roots().await;

        // Subscribe to broadcaster for IndexReloaded events
        let mut broadcast_rx = self.broadcaster.subscribe();

        crate::log_event!("watcher", "started");

        loop {
            // Periodic check for debounced events
            let timeout = sleep(Duration::from_millis(100));
            tokio::pin!(timeout);

            tokio::select! {
                // Handle incoming file events
                Some(res) = self.event_rx.recv() => {
                    match res {
                        Ok(event) => {
                            self.handle_event(event).await;
                        }
                        Err(e) => {
                            tracing::error!("[watcher] file watch error: {e}");
                            // A backend error means we may have missed events -
                            // the index may be stale until a rescan/reindex resolves it.
                            if self.refresh_on_overflow {
                                self.mark_stale();
                            }
                        }
                    }
                }

                // Process debounced changes
                _ = &mut timeout => {
                    // Coalesced created-directory walks run first: this only
                    // orders the two `take_ready()` calls within the same
                    // tick (files recorded here still wait a full debounce
                    // window regardless of order, since `record()` stamps
                    // `Instant::now()` after this call already started). The
                    // ordering matters for `maybe_start_catch_up()` below,
                    // which must see any scope still debouncing in either
                    // debouncer before deciding whether to fire.
                    self.process_pending_created_dirs().await;

                    let ready = self.debouncer.take_ready();
                    for path in ready {
                        self.process_modification(&path).await;
                    }

                    // Complete any in-flight catch-up reindex before
                    // considering whether to start a new one, so the loop
                    // never has more than one catch-up reindex running at a
                    // time.
                    self.poll_catch_up_task().await;
                    self.maybe_start_catch_up();
                }

                // Handle broadcast notifications
                Ok(event) = broadcast_rx.recv() => {
                    if matches!(event, FileChangeEvent::IndexReloaded) {
                        self.handle_index_reloaded().await;
                    }
                }
            }
        }
    }

    /// Watch a directory for changes.
    fn watch_directory(&mut self, dir: &PathBuf) -> Result<(), WatchError> {
        let watch_path = if dir.is_absolute() {
            dir.clone()
        } else {
            self.workspace_root.join(dir)
        };

        match self
            ._watcher
            .watch(&watch_path, RecursiveMode::NonRecursive)
        {
            Ok(_) => {
                crate::debug_event!("watcher", "watching", "{}", watch_path.display());
                Ok(())
            }
            Err(e) => {
                tracing::warn!("[watcher] failed to watch {}: {e}", watch_path.display());
                // Continue - don't fail completely
                Ok(())
            }
        }
    }

    /// Mark the index as potentially stale and (re)start the quiet-window clock.
    ///
    /// While stale, every subsequent observed watcher signal (Ok or Err) bumps
    /// `stale_since` so the quiet window measures quiet-since-last-signal.
    fn mark_stale(&mut self) {
        self.stale = true;
        self.stale_since = Some(Instant::now());
    }

    /// While already stale, restart the quiet-window clock without touching the
    /// `stale` flag. Called for every observed watcher signal so the window
    /// measures quiet-since-last-signal and catch-up only fires once activity
    /// truly settles. A no-op when not stale.
    fn bump_stale_clock(&mut self) {
        if self.stale {
            self.stale_since = Some(Instant::now());
        }
    }

    /// If a catch-up reindex task is in flight and has finished, take its
    /// result and update staleness state accordingly via
    /// [`Self::handle_catch_up_success`] / [`Self::handle_catch_up_failure`].
    async fn poll_catch_up_task(&mut self) {
        let is_finished = match &self.catch_up_task {
            Some(handle) => handle.is_finished(),
            None => return,
        };
        if !is_finished {
            return;
        }

        // Safe: `catch_up_task` was `Some` and finished above.
        let handle = self.catch_up_task.take().expect("checked Some above");
        let started_at = self.catch_up_started_at.take().unwrap_or_else(Instant::now);
        let join_result = handle.await;
        self.last_catch_up_completed = Some(Instant::now());

        match join_result {
            Ok(Ok(outcome)) => self.handle_catch_up_success(outcome, started_at),
            Ok(Err(e)) => self.handle_catch_up_failure(CatchUpFailure::Watch(e)),
            Err(join_err) => self.handle_catch_up_failure(CatchUpFailure::JoinFailed(format!(
                "catch-up reindex task did not complete cleanly: {join_err}"
            ))),
        }
    }

    /// Handle a successfully completed catch-up reindex.
    ///
    /// Logs the outcome and broadcasts `IndexReloaded` unconditionally, but
    /// only clears `stale`/`stale_since` if no newer overflow/rescan signal
    /// arrived *after* `started_at` (i.e. after this task's walk began). A
    /// signal that arrived mid-walk may not be reflected in `outcome`, so
    /// staleness must stay armed for `maybe_start_catch_up` to re-fire once
    /// the (already-elapsed, per `last_catch_up_completed`) cooldown allows.
    fn handle_catch_up_success(&mut self, outcome: ReindexOutcome, started_at: Instant) {
        crate::log_event!(
            "watcher",
            "catch-up reindex complete",
            "{} files reindexed, {} symbols",
            outcome.reindexed,
            outcome.symbol_count
        );
        self.broadcaster.send(FileChangeEvent::IndexReloaded);
        self.catch_up_attempts = 0;
        self.consecutive_contention = 0;

        if should_clear_stale_after_success(self.stale_since, started_at) {
            self.stale = false;
            self.stale_since = None;
        } else {
            crate::log_event!(
                "watcher",
                "catch-up reindex",
                "a newer overflow/rescan signal arrived during the walk; index remains stale for a re-fire"
            );
        }
    }

    /// Handle a failed (or non-cleanly-joined) catch-up reindex attempt.
    ///
    /// A contention rejection (another full reindex, e.g. an MCP
    /// `reindex(force: true)`, is already holding the facade's reindex gate)
    /// is not a genuine failure: the work is already being done elsewhere,
    /// so this does not consume an attempt or abandon the stale episode.
    /// `stale`/`stale_since` are left exactly as they are so
    /// `should_start_catch_up` re-fires after `CATCH_UP_COOLDOWN` once the
    /// other reindex releases the gate.
    ///
    /// A genuine failure re-arms the quiet window for a retry, unless the
    /// bounded attempt count has been exhausted, in which case staleness
    /// tracking is cleared to avoid an infinite hot-loop on a permanent
    /// failure.
    ///
    /// A long streak of *consecutive* contention rejections (tracked
    /// separately from `catch_up_attempts`, which contention never
    /// consumes) escalates to `tracing::warn!` once past
    /// `CONSECUTIVE_CONTENTION_WARN_THRESHOLD`: normal reindex handoffs are
    /// brief, so a sustained streak likely means the gate holder is wedged
    /// and no signal above debug level would otherwise surface that.
    fn handle_catch_up_failure(&mut self, failure: CatchUpFailure) {
        if failure.is_contention() {
            self.consecutive_contention += 1;

            if self.consecutive_contention > CONSECUTIVE_CONTENTION_WARN_THRESHOLD {
                tracing::warn!(
                    "[watcher] catch-up reindex has been rejected by reindex-gate contention {} times in a row; \
                     another reindex may be wedged. A restart may be needed if this persists.",
                    self.consecutive_contention
                );
            } else {
                crate::debug_event!(
                    "watcher",
                    "catch-up reindex deferred",
                    "another full reindex is already in progress; will retry after cooldown"
                );
            }
            return;
        }

        self.consecutive_contention = 0;
        self.catch_up_attempts += 1;

        if self.catch_up_attempts >= MAX_CATCH_UP_ATTEMPTS {
            tracing::error!(
                "[watcher] catch-up reindex failed after {} attempts, giving up for this episode: {failure}. A manual force-reindex may be needed.",
                self.catch_up_attempts
            );
            self.stale = false;
            self.stale_since = None;
            self.catch_up_attempts = 0;
        } else {
            tracing::error!(
                "[watcher] catch-up reindex failed (attempt {}/{MAX_CATCH_UP_ATTEMPTS}): {failure}. A manual force-reindex may be needed if this persists.",
                self.catch_up_attempts
            );
            self.stale = true;
            self.stale_since = Some(Instant::now());
        }
    }

    /// Start a catch-up reindex task if warranted by
    /// [`should_start_catch_up`] (no in-flight task, stale + quiet window
    /// elapsed + no pending debounce work + cooldown elapsed).
    ///
    /// Fires the reindex via `tokio::spawn` (rather than awaiting inline) so
    /// the run loop keeps draining `event_rx`/`broadcast_rx` while the
    /// (potentially multi-second, full-workspace) reindex runs.
    fn maybe_start_catch_up(&mut self) {
        let Some(since) = self.stale_since else {
            return;
        };

        if !should_start_catch_up(
            self.catch_up_task.is_some(),
            self.stale,
            self.debouncer.has_pending() || self.dir_debouncer.has_pending(),
            since.elapsed(),
            self.debounce_window,
            self.last_catch_up_completed.map(|t| t.elapsed()),
            CATCH_UP_COOLDOWN,
        ) {
            return;
        }

        crate::log_event!(
            "watcher",
            "catch-up reindex",
            "quiet window elapsed after overflow/rescan; reindexing"
        );

        let facade = Arc::clone(&self.facade);
        self.catch_up_started_at = Some(Instant::now());
        self.catch_up_task = Some(tokio::spawn(async move {
            crate::indexing::reindex_locked(&facade, None, true, None)
                .await
                .map_err(|source| WatchError::CatchUpReindexFailed { source })
        }));
    }

    /// Handle an incoming file event.
    async fn handle_event(&mut self, event: Event) {
        // notify 8.2.0 signals backend overflow/rescan (e.g. inotify IN_Q_OVERFLOW)
        // via a backend-agnostic flag rather than a path-bearing event kind. A
        // rescan/overflow event carries EMPTY paths, so the loop below would
        // silently drop it without this check - the index may be stale because
        // filesystem events were dropped by the OS or backend.
        if event.need_rescan() && self.refresh_on_overflow {
            crate::log_event!(
                "watcher",
                "overflow/rescan",
                "backend reported a rescan condition; index may be stale until refreshed"
            );
            self.mark_stale();
        }

        // Bind `kind` before the loop below moves `event.paths` out of
        // `event`; `EventKind` is `Copy`, so this avoids reading `event.kind`
        // across the partial move and makes the per-path composition below
        // explicit rather than relying on field-by-field partial-move rules.
        let kind = event.kind;

        for path in event.paths {
            // A created directory never matches a file handler (extension
            // gate); it is the watcher's own concern: extend the watch set
            // and catch up files that landed before the watch existed.
            if matches!(kind, EventKind::Create(_)) && path.is_dir() {
                // Apply the handler-root prefix gate at record time so a
                // path outside every registered root never enters the
                // coalescing map; the walk itself runs later, batched, in
                // `process_pending_created_dirs`.
                if self.handler_roots.iter().any(|r| path.starts_with(r)) {
                    self.dir_debouncer.record(path);
                }
                continue;
            }

            // Check if any handler cares about this path
            let matched = self.handlers.iter().any(|h| h.matches(&path));
            if !matched {
                crate::trace_event!("watcher", "unmatched", "{:?} {}", kind, path.display());
                continue;
            }

            match kind {
                EventKind::Create(_) | EventKind::Modify(_) => {
                    // Debounce creations and modifications alike; the
                    // exists() re-check in process_modification handles
                    // paths that vanish before the debounce fires.
                    self.debouncer.record(path);
                }
                EventKind::Remove(_) => {
                    // Handle deletions immediately
                    self.debouncer.remove(&path);
                    self.process_deletion(&path).await;
                }
                _ => {}
            }
        }

        // Any observed signal received while stale restarts the quiet window,
        // so a rescan followed by ongoing activity settles into a single
        // catch-up reindex rather than firing mid-burst. Note: when this
        // event itself triggered `mark_stale()` above, `stale_since` was
        // already just set to `now`; this call re-sets it to a
        // (negligibly later) `now` again. Harmless, and simpler than
        // threading a "did mark_stale fire this call" flag through.
        self.bump_stale_clock();
    }

    /// Register handler watch roots: watched directly so directory
    /// creation at the top of a root is visible even when the root
    /// holds no indexed file itself.
    async fn register_handler_roots(&mut self) {
        let mut roots = Vec::new();
        for handler in &self.handlers {
            roots.extend(handler.watch_roots().await);
        }
        for root in &roots {
            if self.registry.add_watch_dir(root.clone()) {
                if let Err(e) = self.watch_directory(root) {
                    tracing::warn!("[watcher] failed to watch root: {e}");
                }
            }
        }
        self.handler_roots = roots;
    }

    /// Drain every created-directory scope that has settled (been quiet for
    /// the debounce window), and for each owning indexed root run exactly
    /// one filesystem walk covering all of that root's settled scopes at
    /// once -- rather than one full-root walk per created directory. This
    /// is why the walk is deferred out of `handle_event` and into the
    /// select-loop timeout tick: the burst has to settle first so
    /// `dir_debouncer` has coalesced the whole batch before the walk runs.
    ///
    /// Watches every traversable directory of each new subtree (ignore
    /// chains anchored at the root prune ignored trees), then routes the
    /// files already inside through the normal debounce -> eligibility ->
    /// reindex path.
    async fn process_pending_created_dirs(&mut self) {
        let ready = self.dir_debouncer.take_ready();
        if ready.is_empty() {
            return;
        }

        // Resolve each ready path's owning root under one short read lock --
        // `discoverable_scope_root` is a settings lookup plus a single
        // `canonicalize()`, not a directory walk. The actual walk(s) run
        // below, off the lock and on blocking-pool threads, so a large or
        // bursty newly-materialized subtree cannot stall the tokio worker
        // driving the watch loop.
        let (scopes_by_root, settings) = {
            let facade = self.facade.read().await;
            let settings = Arc::clone(facade.settings());
            let mut scopes_by_root: std::collections::HashMap<PathBuf, Vec<PathBuf>> =
                std::collections::HashMap::new();
            for path in &ready {
                if let Some((scope, root)) = facade.discoverable_scope_root(path) {
                    scopes_by_root.entry(root).or_default().push(scope);
                }
            }
            (scopes_by_root, settings)
        };

        // Spawn every root's walk before awaiting any of them, so the total
        // stall on this select-loop tick is bounded by the slowest single
        // root's walk rather than the sum of all roots' walks. Pruning and
        // dedup of each root's scope list also move inside the blocking
        // closure here, so the (O(n^2) worst case) pruning pass runs off
        // the tokio worker driving the watch loop too, not just the walk.
        let mut tasks = Vec::with_capacity(scopes_by_root.len());
        for (root, scopes) in scopes_by_root {
            let settings = Arc::clone(&settings);
            let root_owned = root.clone();
            let scopes_for_walk = scopes.clone();
            #[cfg(test)]
            let walk_count = Arc::clone(&self.walk_count);
            let handle = tokio::task::spawn_blocking(move || {
                let mut scopes = prune_nested_scopes(&scopes_for_walk);
                // `prune_nested_scopes` deliberately keeps ancestor/descendant
                // duplicates (see its doc comment), but exact duplicates can
                // still arrive here after canonicalization; dedup them so
                // `discoverable_entries_for`'s per-path `any(starts_with)`
                // filter doesn't redundantly re-check the same scope.
                scopes.sort();
                scopes.dedup();
                #[cfg(test)]
                walk_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                IndexFacade::discoverable_entries_for(&settings, &root_owned, &scopes)
            });
            tasks.push((root, scopes, handle));
        }

        for (root, scopes, handle) in tasks {
            let walk_result = handle.await;

            let (dirs, files) = match walk_result {
                Ok(Ok(entries)) => entries,
                Ok(Err(e)) => {
                    tracing::warn!(
                        "[watcher] failed to discover entries under {}: {e}",
                        root.display()
                    );
                    // `take_ready()` above already removed these scopes from
                    // `dir_debouncer`, so without this they would be dropped
                    // permanently on a transient failure (e.g. a permission
                    // race during a large extract). Re-record them so the
                    // next settled tick retries the whole batch. This is not
                    // attempt-bounded: a permanently-failing scope retries
                    // indefinitely, but only once per debounce window (the
                    // record/take_ready cadence), not in a tight loop.
                    for scope in scopes {
                        self.dir_debouncer.record(scope);
                    }
                    continue;
                }
                Err(e) => {
                    tracing::warn!(
                        "[watcher] discovery task for {} panicked: {e}",
                        root.display()
                    );
                    for scope in scopes {
                        self.dir_debouncer.record(scope);
                    }
                    continue;
                }
            };

            for dir in dirs {
                if self.registry.add_watch_dir(dir.clone()) {
                    if let Err(e) = self.watch_directory(&dir) {
                        tracing::warn!("[watcher] failed to watch created dir: {e}");
                    }
                }
            }
            if !files.is_empty() {
                crate::log_event!(
                    "watcher",
                    "created dirs",
                    "{} ({} files to catch up)",
                    root.display(),
                    files.len()
                );
            }
            for file in files {
                self.debouncer.record(file);
            }
        }
    }

    /// Process a debounced file modification.
    async fn process_modification(&self, path: &Path) {
        // Check if file still exists (handles rename-as-modify on macOS)
        if !path.exists() {
            self.process_deletion(path).await;
            return;
        }

        for handler in &self.handlers {
            if !handler.matches(path) {
                continue;
            }

            crate::log_event!(handler.name(), "modified", "{}", path.display());

            match handler.on_modify(path).await {
                Ok(action) => {
                    if let Err(e) = self.execute_action(action, handler.name()).await {
                        tracing::error!("[{}] action error: {e}", handler.name());
                    }
                }
                Err(e) => {
                    tracing::error!("[{}] handler error: {e}", handler.name());
                }
            }
        }
    }

    /// Process a file deletion.
    async fn process_deletion(&self, path: &Path) {
        for handler in &self.handlers {
            if !handler.matches(path) {
                continue;
            }

            crate::log_event!(handler.name(), "deleted", "{}", path.display());

            match handler.on_delete(path).await {
                Ok(action) => {
                    if let Err(e) = self.execute_action(action, handler.name()).await {
                        tracing::error!("[{}] action error: {e}", handler.name());
                    }
                }
                Err(e) => {
                    tracing::error!("[{}] handler error: {e}", handler.name());
                }
            }
        }
    }

    /// Execute an action returned by a handler.
    async fn execute_action(
        &self,
        action: WatchAction,
        handler_name: &str,
    ) -> Result<(), WatchError> {
        match action {
            WatchAction::ReindexCode { path } => {
                let mut indexer = self.facade.write().await;
                match indexer.index_file(&path) {
                    Ok(result) => {
                        use crate::IndexingResult;
                        match result {
                            IndexingResult::Indexed(_) => {
                                crate::log_event!(handler_name, "reindexed");

                                // Save semantic search
                                if indexer.has_semantic_search() {
                                    let semantic_path = self.index_path.join("semantic");
                                    if let Err(e) = indexer.save_semantic_search(&semantic_path) {
                                        tracing::warn!(
                                            "[{handler_name}] failed to save semantic search: {e}"
                                        );
                                    }
                                }

                                // Notify
                                self.broadcaster
                                    .send(FileChangeEvent::FileReindexed { path: path.clone() });
                            }
                            IndexingResult::Cached(_) => {
                                crate::debug_event!(handler_name, "unchanged (hash match)");
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!("[{handler_name}] reindex failed: {e}");
                    }
                }
            }

            WatchAction::RemoveCode { path } => {
                let mut indexer = self.facade.write().await;
                if let Err(e) = indexer.remove_file(&path) {
                    tracing::error!("[{handler_name}] failed to remove: {e}");
                } else {
                    crate::log_event!(handler_name, "removed");
                    self.broadcaster
                        .send(FileChangeEvent::FileDeleted { path: path.clone() });
                }
            }

            WatchAction::ReindexDocument { path } => {
                if let Some(ref store) = self.document_store {
                    let mut store = store.write().await;
                    match store.reindex_file(&path, &self.chunking_config) {
                        Ok(Some(chunks)) => {
                            crate::log_event!(handler_name, "reindexed", "{chunks} chunks");
                            self.broadcaster
                                .send(FileChangeEvent::FileReindexed { path: path.clone() });
                        }
                        Ok(None) => {
                            crate::debug_event!(handler_name, "not in index, skipped");
                        }
                        Err(e) => {
                            tracing::error!("[{handler_name}] reindex failed: {e}");
                        }
                    }
                }
            }

            WatchAction::RemoveDocument { path } => {
                if let Some(ref store) = self.document_store {
                    let mut store = store.write().await;
                    match store.remove_file(&path) {
                        Ok(true) => {
                            crate::log_event!(handler_name, "removed");
                            self.broadcaster
                                .send(FileChangeEvent::FileDeleted { path: path.clone() });
                        }
                        Ok(false) => {
                            crate::debug_event!(handler_name, "was not in index");
                        }
                        Err(e) => {
                            tracing::error!("[{handler_name}] failed to remove: {e}");
                        }
                    }
                }
            }

            WatchAction::ReloadConfig { added, removed } => {
                if !added.is_empty() {
                    crate::log_event!("config", "adding directories", "{}", added.len());
                    for path in &added {
                        tracing::info!("  + {}", path.display());
                    }

                    let mut indexer = self.facade.write().await;
                    for path in &added {
                        crate::log_event!("config", "indexing", "{}", path.display());
                        match indexer.index_directory(path, false) {
                            Ok(stats) => {
                                tracing::info!(
                                    "  indexed {} files, {} symbols",
                                    stats.files_indexed,
                                    stats.symbols_found
                                );
                            }
                            Err(e) => {
                                tracing::error!("  failed: {e}");
                            }
                        }
                    }
                }

                if !removed.is_empty() {
                    crate::log_event!("config", "removed directories", "{}", removed.len());
                    for path in &removed {
                        tracing::info!("  - {}", path.display());
                    }
                    tracing::info!("Run 'codanna clean' to remove symbols from these directories");
                }

                if !added.is_empty() || !removed.is_empty() {
                    self.broadcaster.send(FileChangeEvent::IndexReloaded);
                }
            }

            WatchAction::None => {
                crate::debug_event!(handler_name, "no action needed");
            }
        }

        Ok(())
    }

    /// Handle IndexReloaded notification - refresh all handlers.
    async fn handle_index_reloaded(&mut self) {
        crate::log_event!("watcher", "index reloaded, refreshing");

        for handler in &self.handlers {
            if let Err(e) = handler.refresh_paths().await {
                tracing::warn!(
                    "[watcher] failed to refresh {} handler: {e}",
                    handler.name()
                );
            }
        }

        // Rebuild path registry
        let mut all_paths = Vec::new();
        for handler in &self.handlers {
            all_paths.extend(handler.tracked_paths().await);
        }

        let old_dirs: HashSet<PathBuf> = self.registry.watch_dirs().clone();
        self.registry.rebuild(all_paths);

        // Collect new directories before mutably borrowing self
        let dirs_to_watch: Vec<PathBuf> = self
            .registry
            .watch_dirs()
            .difference(&old_dirs)
            .cloned()
            .collect();

        // Watch any new directories
        for dir in dirs_to_watch {
            if let Err(e) = self.watch_directory(&dir) {
                tracing::warn!("[watcher] failed to watch new directory: {e}");
            }
        }

        // Config reload can add or drop roots; re-register them.
        self.register_handler_roots().await;

        crate::log_event!(
            "watcher",
            "watching",
            "{} files in {} directories",
            self.registry.path_count(),
            self.registry.dir_count()
        );
    }
}

/// Classifies why a catch-up reindex attempt did not produce a successful
/// outcome, so [`UnifiedWatcher::handle_catch_up_failure`] can distinguish a
/// benign contention rejection (another full reindex already holds the
/// facade's reindex gate) from a genuine failure without string-matching
/// the error message.
enum CatchUpFailure {
    /// The spawned task returned a typed [`WatchError`].
    Watch(WatchError),
    /// The spawned task itself did not join cleanly (e.g. it panicked).
    JoinFailed(String),
}

impl CatchUpFailure {
    /// True when this failure is a reindex-gate contention rejection
    /// (`IndexError::ReindexInProgress`, as wrapped by
    /// `WatchError::CatchUpReindexFailed`) rather than a genuine failure.
    fn is_contention(&self) -> bool {
        matches!(
            self,
            CatchUpFailure::Watch(WatchError::CatchUpReindexFailed {
                source: IndexError::ReindexInProgress
            })
        )
    }
}

impl std::fmt::Display for CatchUpFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CatchUpFailure::Watch(e) => write!(f, "{e}"),
            CatchUpFailure::JoinFailed(reason) => write!(f, "{reason}"),
        }
    }
}

/// Pure decision predicate for firing a catch-up reindex after an
/// overflow/rescan signal.
///
/// Fires exactly when the index is marked stale, there is no pending
/// (still-debouncing) file activity, and the quiet window has elapsed since
/// the last staleness signal. Callers are responsible for clearing `stale`
/// after a `true` result so the predicate does not re-fire on subsequent
/// ticks for the same episode.
fn should_catch_up(stale: bool, has_pending: bool, elapsed: Duration, window: Duration) -> bool {
    stale && !has_pending && elapsed >= window
}

/// Drops any scope that is a descendant of another scope in the same list,
/// so a wide burst of created-directory scopes (e.g. nested `a/b/c/d`)
/// shrinks to its minimal covering set before being handed to the
/// `any(starts_with)` filter in [`IndexFacade::discoverable_entries_for`] --
/// fewer scopes means a cheaper filter pass over the same single walk.
///
/// A path always `starts_with` itself, so the comparison excludes a scope
/// from being considered its own ancestor (otherwise every scope would
/// "contain" itself and the whole list would collapse to nothing).
/// Duplicate entries are preserved as ties (neither is a strict descendant
/// of the other via index-inequality), so they survive rather than
/// annihilating each other.
fn prune_nested_scopes(scopes: &[PathBuf]) -> Vec<PathBuf> {
    scopes
        .iter()
        .enumerate()
        .filter(|(i, scope)| {
            !scopes
                .iter()
                .enumerate()
                .any(|(j, other)| j != *i && scope.starts_with(other) && **scope != *other)
        })
        .map(|(_, scope)| scope.clone())
        .collect()
}

/// Minimum time between successive catch-up reindex completions, enforced
/// independent of `debounce_window` so sustained bursty git activity
/// (rebase/checkout) can't retrigger a full clear+rebuild on every quiet gap
/// just over the (much shorter) per-file debounce window.
const CATCH_UP_COOLDOWN: Duration = Duration::from_secs(5);

/// Bound on consecutive catch-up reindex failures for a single stale
/// episode before staleness tracking is cleared, to avoid hot-looping
/// forever on a permanent failure.
const MAX_CATCH_UP_ATTEMPTS: u32 = 5;

/// Threshold of *consecutive* reindex-gate contention rejections (each
/// re-fired after `CATCH_UP_COOLDOWN`) past which `handle_catch_up_failure`
/// escalates from `debug_event!` to `tracing::warn!`. 12 is roughly one
/// minute at the 5s cooldown, far beyond any legitimate reindex handoff.
const CONSECUTIVE_CONTENTION_WARN_THRESHOLD: u32 = 12;

/// Pure decision predicate combining [`should_catch_up`] with the
/// in-flight-task guard and the distinct catch-up cooldown, used by
/// `maybe_start_catch_up`.
fn should_start_catch_up(
    catch_up_in_flight: bool,
    stale: bool,
    has_pending: bool,
    stale_elapsed: Duration,
    debounce_window: Duration,
    last_completed_elapsed: Option<Duration>,
    cooldown: Duration,
) -> bool {
    if catch_up_in_flight {
        // A catch-up reindex is already running; don't double-fire.
        return false;
    }

    if !should_catch_up(stale, has_pending, stale_elapsed, debounce_window) {
        return false;
    }

    if let Some(elapsed) = last_completed_elapsed
        && elapsed < cooldown
    {
        // Throttle successive catch-up episodes independent of the
        // per-file debounce window, so sustained bursty activity (e.g.
        // rebase/checkout) doesn't trigger a brand-new full clear+rebuild
        // on every quiet gap.
        return false;
    }

    true
}

/// Pure decision predicate for whether a successfully completed catch-up
/// reindex may clear staleness tracking.
///
/// Returns `false` (staleness must remain armed) when `stale_since` is
/// strictly newer than `started_at`, meaning an overflow/rescan signal
/// arrived *after* the completed walk began and so may not be reflected in
/// its results.
fn should_clear_stale_after_success(stale_since: Option<Instant>, started_at: Instant) -> bool {
    match stale_since {
        Some(since) => since <= started_at,
        None => true,
    }
}

/// Builder for constructing a UnifiedWatcher.
pub struct UnifiedWatcherBuilder {
    handlers: Vec<Box<dyn WatchHandler>>,
    broadcaster: Option<Arc<NotificationBroadcaster>>,
    facade: Option<Arc<RwLock<IndexFacade>>>,
    document_store: Option<Arc<RwLock<DocumentStore>>>,
    chunking_config: ChunkingConfig,
    index_path: Option<PathBuf>,
    workspace_root: Option<PathBuf>,
    debounce_ms: u64,
    refresh_on_overflow: bool,
}

impl UnifiedWatcherBuilder {
    /// Create a new builder with defaults.
    pub fn new() -> Self {
        Self {
            handlers: Vec::new(),
            broadcaster: None,
            facade: None,
            document_store: None,
            chunking_config: ChunkingConfig::default(),
            index_path: None,
            workspace_root: None,
            debounce_ms: 500,
            // Mirrors `FileWatchConfig::refresh_on_overflow`'s `default_true()`
            // (config/mod.rs), so a builder-constructed watcher without an
            // explicit `.refresh_on_overflow(...)` call behaves the same as
            // one built from default config.
            refresh_on_overflow: true,
        }
    }

    /// Add a handler.
    pub fn handler(mut self, handler: impl WatchHandler + 'static) -> Self {
        self.handlers.push(Box::new(handler));
        self
    }

    /// Set the notification broadcaster.
    pub fn broadcaster(mut self, broadcaster: Arc<NotificationBroadcaster>) -> Self {
        self.broadcaster = Some(broadcaster);
        self
    }

    /// Set the facade (renamed from indexer).
    pub fn indexer(mut self, facade: Arc<RwLock<IndexFacade>>) -> Self {
        self.facade = Some(facade);
        self
    }

    /// Set the document store.
    pub fn document_store(mut self, store: Arc<RwLock<DocumentStore>>) -> Self {
        self.document_store = Some(store);
        self
    }

    /// Set the chunking config for documents.
    pub fn chunking_config(mut self, config: ChunkingConfig) -> Self {
        self.chunking_config = config;
        self
    }

    /// Set the index path for semantic search persistence.
    pub fn index_path(mut self, path: PathBuf) -> Self {
        self.index_path = Some(path);
        self
    }

    /// Set the workspace root.
    pub fn workspace_root(mut self, path: PathBuf) -> Self {
        self.workspace_root = Some(path);
        self
    }

    /// Set the debounce duration in milliseconds.
    pub fn debounce_ms(mut self, ms: u64) -> Self {
        self.debounce_ms = ms;
        self
    }

    /// Set whether to actively refresh the index when a backend
    /// overflow/rescan condition is detected.
    pub fn refresh_on_overflow(mut self, refresh: bool) -> Self {
        self.refresh_on_overflow = refresh;
        self
    }

    /// Build the UnifiedWatcher.
    pub fn build(self) -> Result<UnifiedWatcher, WatchError> {
        let broadcaster = self.broadcaster.ok_or_else(|| WatchError::InitFailed {
            reason: "Broadcaster is required".to_string(),
        })?;

        let facade = self.facade.ok_or_else(|| WatchError::InitFailed {
            reason: "Facade is required".to_string(),
        })?;

        let workspace_root = self
            .workspace_root
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

        let index_path = self
            .index_path
            .unwrap_or_else(|| workspace_root.join(".codanna/index"));

        // Create channel for events
        let (tx, rx) = mpsc::channel(100);

        // Create the notify watcher
        let watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
            let _ = tx.blocking_send(res);
        })?;

        Ok(UnifiedWatcher {
            handlers: self.handlers,
            registry: PathRegistry::new(),
            debouncer: Debouncer::new(self.debounce_ms),
            dir_debouncer: Debouncer::new(self.debounce_ms),
            event_rx: rx,
            _watcher: watcher,
            broadcaster,
            facade,
            document_store: self.document_store,
            chunking_config: self.chunking_config,
            index_path,
            workspace_root,
            stale: false,
            stale_since: None,
            refresh_on_overflow: self.refresh_on_overflow,
            debounce_window: Duration::from_millis(self.debounce_ms),
            catch_up_task: None,
            catch_up_started_at: None,
            last_catch_up_completed: None,
            catch_up_attempts: 0,
            consecutive_contention: 0,
            handler_roots: Vec::new(),
            #[cfg(test)]
            walk_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        })
    }
}

impl Default for UnifiedWatcherBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::Flag;

    /// A real kernel IN_Q_OVERFLOW (or platform-equivalent rescan condition) is
    /// not unit-testable without a live filesystem watcher, so we synthesize
    /// the `notify::Event` directly with the rescan flag set and empty paths -
    /// this mirrors exactly what a backend-driven overflow event looks like.
    fn rescan_event() -> Event {
        Event::new(EventKind::Other).set_flag(Flag::Rescan)
    }

    #[test]
    fn rescan_event_reports_need_rescan_with_empty_paths() {
        let event = rescan_event();

        assert!(event.need_rescan());
        assert!(
            event.paths.is_empty(),
            "a rescan/overflow event carries no paths"
        );
    }

    /// Build a minimal real `UnifiedWatcher` against a temp-dir-backed index
    /// using caller-supplied `Settings` (e.g. `indexing.ignore_patterns`), so
    /// `handle_event`/`process_pending_created_dirs` can be exercised
    /// directly instead of re-simulating their branching logic.
    /// `debounce_ms(0)` so `debouncer.take_ready()`/`dir_debouncer.take_ready()`
    /// return everything recorded without waiting out a real debounce window.
    fn test_watcher_with_settings(
        tempdir: &tempfile::TempDir,
        mut settings: crate::config::Settings,
    ) -> UnifiedWatcher {
        use crate::indexing::facade::IndexFacade;

        settings.index_path = tempdir.path().to_path_buf();
        settings.workspace_root = None;
        if settings.indexed_paths_cache.is_empty() {
            let root = tempdir
                .path()
                .canonicalize()
                .unwrap_or_else(|_| tempdir.path().to_path_buf());
            settings.indexed_paths_cache = vec![root];
        }
        let facade = IndexFacade::new(std::sync::Arc::new(settings))
            .expect("facade construction against a fresh temp dir must succeed");

        UnifiedWatcher::builder()
            .broadcaster(Arc::new(NotificationBroadcaster::new(16)))
            .indexer(Arc::new(RwLock::new(facade)))
            .workspace_root(tempdir.path().to_path_buf())
            .debounce_ms(0)
            .build()
            .expect("builder has all required fields")
    }

    /// Build a minimal real `UnifiedWatcher` against a temp-dir-backed index,
    /// so `handle_event` can be exercised directly instead of re-simulating
    /// its branching logic.
    fn test_watcher(tempdir: &tempfile::TempDir) -> UnifiedWatcher {
        use crate::config::Settings;

        test_watcher_with_settings(tempdir, Settings::default())
    }

    /// A directory created inside a registered handler root must be
    /// discovered and watched, and any files already inside it must be
    /// routed through the normal debounce path - exercised via
    /// `handle_event` followed by a `process_pending_created_dirs` drain
    /// (not `handle_event` alone), so the wiring from the event loop through
    /// the coalesced walk into the feature is itself under test.
    #[tokio::test]
    async fn created_directory_registers_watch_and_records_files() {
        use crate::config::Settings;
        use notify::event::CreateKind;

        let tempdir = tempfile::tempdir().unwrap();
        let root = tempdir.path().canonicalize().unwrap();
        let mut watcher = test_watcher_with_settings(&tempdir, Settings::default());
        watcher.handler_roots = vec![root.clone()];

        let newdir = root.join("newdir");
        std::fs::create_dir_all(&newdir).unwrap();
        std::fs::write(newdir.join("a.rs"), "fn a() {}").unwrap();

        let dirs_before = watcher.registry.dir_count();

        let mut event = Event::new(EventKind::Create(CreateKind::Folder));
        event.paths.push(newdir.clone());
        watcher.handle_event(event).await;

        // `handle_event` only records the scope into `dir_debouncer` now; the
        // walk that discovers `a.rs` and registers `newdir` for watching does
        // not run until the coalescing drain below.
        assert_eq!(
            watcher.registry.dir_count(),
            dirs_before,
            "handle_event must not walk synchronously; the walk is deferred to process_pending_created_dirs"
        );

        watcher.process_pending_created_dirs().await;

        assert!(
            watcher.registry.dir_count() > dirs_before,
            "the new directory must be registered for watching"
        );

        let ready = watcher.debouncer.take_ready();
        assert!(
            ready.iter().any(|p| p.ends_with("a.rs")),
            "the file already inside the created directory must be debounced for catch-up: {ready:?}"
        );
    }

    /// A subtree excluded by `ignore_patterns` must not be walked or watched
    /// when a directory is created under a registered handler root - checked
    /// at the watcher altitude (the live consumer of `discoverable_*`), not
    /// just at the facade.
    ///
    /// The drain (`process_pending_created_dirs`) is required here, not
    /// optional: without it, `dir_debouncer` still holds the scope and
    /// nothing has been walked yet, so both assertions below would be
    /// trivially true against an empty result regardless of whether ignore
    /// filtering works at all. `sibling.rs` (created alongside the ignored
    /// subtree, outside it) is the positive control proving the walk did
    /// run and did find real files - without it, an implementation that
    /// walked nothing at all would also pass.
    #[tokio::test]
    async fn created_directory_skips_ignored_subtree() {
        use crate::config::Settings;
        use notify::event::CreateKind;

        let tempdir = tempfile::tempdir().unwrap();
        let root = tempdir.path().canonicalize().unwrap();

        let mut settings = Settings {
            index_path: tempdir.path().to_path_buf(),
            workspace_root: None,
            ..Default::default()
        };
        settings.indexing.ignore_patterns = vec!["ignored/".into()];
        settings.indexed_paths_cache = vec![root.clone()];
        let mut watcher = test_watcher_with_settings(&tempdir, settings);
        watcher.handler_roots = vec![root.clone()];

        let newdir = root.join("newdir");
        let ignored_dir = newdir.join("ignored");
        std::fs::create_dir_all(&ignored_dir).unwrap();
        std::fs::write(ignored_dir.join("b.rs"), "fn b() {}").unwrap();
        std::fs::write(newdir.join("sibling.rs"), "fn sibling() {}").unwrap();

        let mut event = Event::new(EventKind::Create(CreateKind::Folder));
        event.paths.push(newdir.clone());
        watcher.handle_event(event).await;

        watcher.process_pending_created_dirs().await;

        let ready = watcher.debouncer.take_ready();
        assert!(
            ready.iter().any(|p| p.ends_with("sibling.rs")),
            "a file outside the ignored subtree must still be debounced (positive \
             control proving the walk actually ran): {ready:?}"
        );
        assert!(
            !ready.iter().any(|p| p.ends_with("b.rs")),
            "a file under an ignored subtree must not be debounced: {ready:?}"
        );
        assert!(
            !watcher.registry.watch_dirs().contains(&ignored_dir),
            "an ignored subtree must not be watch-registered"
        );
    }

    /// A directory event outside every registered handler root must be
    /// ignored entirely, preserving the upstream early-return guard.
    ///
    /// The handler-roots gate now runs at record time (inside `handle_event`,
    /// before anything is coalesced into `dir_debouncer`), not at walk time -
    /// so this asserts `dir_debouncer.has_pending()` is false straight after
    /// `handle_event`, before any drain. That is the one assertion that would
    /// catch a regression where the gate got moved/dropped and the path
    /// silently slipped into `dir_debouncer` anyway, only to be filtered out
    /// later by coincidence (e.g. `discoverable_scope_root` returning `None`
    /// for a path outside `indexed_paths_cache`) rather than by the gate
    /// actually working. The drain is still run afterward so the
    /// post-drain assertions exercise the same real path production code
    /// takes, rather than only inspecting pre-drain state.
    #[tokio::test]
    async fn created_directory_outside_handler_roots_is_ignored() {
        use notify::event::CreateKind;

        let tempdir = tempfile::tempdir().unwrap();
        let root = tempdir.path().canonicalize().unwrap();
        let mut watcher = test_watcher(&tempdir);
        watcher.handler_roots = Vec::new();

        let newdir = root.join("newdir");
        std::fs::create_dir_all(&newdir).unwrap();
        std::fs::write(newdir.join("a.rs"), "fn a() {}").unwrap();

        let dirs_before = watcher.registry.dir_count();

        let mut event = Event::new(EventKind::Create(CreateKind::Folder));
        event.paths.push(newdir.clone());
        watcher.handle_event(event).await;

        assert!(
            !watcher.dir_debouncer.has_pending(),
            "a path outside every handler root must never enter dir_debouncer at all"
        );

        watcher.process_pending_created_dirs().await;

        assert_eq!(
            watcher.registry.dir_count(),
            dirs_before,
            "no directory outside handler_roots may be registered for watching"
        );
        assert!(
            watcher.debouncer.take_ready().is_empty(),
            "no file outside handler_roots may be debounced"
        );
    }

    // -- prune_nested_scopes -------------------------------------------
    //
    // These target the degenerate cases a naive "is `a` a prefix of `b`"
    // implementation gets wrong: self-elimination via `starts_with` always
    // being true for a path against itself, string-prefix vs. component-wise
    // containment, duplicate survival, and order-independence of the result.

    /// A path always `starts_with` itself; the function must special-case
    /// that away, or every single-element (and every otherwise-unrelated)
    /// list would collapse to nothing.
    #[test]
    fn prune_nested_scopes_self_starts_with_is_not_self_elimination() {
        let scopes = vec![PathBuf::from("/a")];
        assert_eq!(prune_nested_scopes(&scopes), vec![PathBuf::from("/a")]);
    }

    /// Duplicate scopes are ties, not ancestor/descendant pairs (neither
    /// occurrence is a *strict* descendant of the other), so both must
    /// survive rather than annihilating each other.
    #[test]
    fn prune_nested_scopes_preserves_duplicates() {
        let scopes = vec![PathBuf::from("/a"), PathBuf::from("/a")];
        assert_eq!(prune_nested_scopes(&scopes), scopes);
    }

    /// `/ab` is not a descendant of `/a` despite sharing a string prefix -
    /// containment must be component-wise (`Path::starts_with`), not a raw
    /// string `starts_with`. Both must be kept.
    #[test]
    fn prune_nested_scopes_keeps_component_wise_siblings_not_string_prefix() {
        let scopes = vec![PathBuf::from("/a"), PathBuf::from("/ab")];
        let mut pruned = prune_nested_scopes(&scopes);
        pruned.sort();
        assert_eq!(pruned, vec![PathBuf::from("/a"), PathBuf::from("/ab")]);
    }

    /// A genuine ancestor/descendant pair prunes to the ancestor regardless
    /// of which order the two scopes were recorded in.
    #[test]
    fn prune_nested_scopes_prunes_descendant_regardless_of_input_order() {
        let child_first = vec![PathBuf::from("/a/b"), PathBuf::from("/a")];
        let parent_first = vec![PathBuf::from("/a"), PathBuf::from("/a/b")];

        assert_eq!(prune_nested_scopes(&child_first), vec![PathBuf::from("/a")]);
        assert_eq!(
            prune_nested_scopes(&parent_first),
            vec![PathBuf::from("/a")]
        );
    }

    #[test]
    fn prune_nested_scopes_empty_input_returns_empty() {
        assert!(prune_nested_scopes(&[]).is_empty());
    }

    /// A three-level chain collapses to just its root ancestor, in any
    /// recording order - the case the issue names explicitly (`a/b/c/d`
    /// recorded as separate events).
    #[test]
    fn prune_nested_scopes_collapses_three_level_chain() {
        let root_to_leaf = vec![
            PathBuf::from("/a"),
            PathBuf::from("/a/b"),
            PathBuf::from("/a/b/c"),
        ];
        let leaf_to_root = vec![
            PathBuf::from("/a/b/c"),
            PathBuf::from("/a/b"),
            PathBuf::from("/a"),
        ];

        assert_eq!(
            prune_nested_scopes(&root_to_leaf),
            vec![PathBuf::from("/a")]
        );
        assert_eq!(
            prune_nested_scopes(&leaf_to_root),
            vec![PathBuf::from("/a")]
        );
    }

    // -- coalescing behavior --------------------------------------------
    //
    // These are the point of the change: a burst of created-directory events
    // must settle into ONE walk per owning root, not one walk per event.
    // `UnifiedWatcher::walk_count` (a `#[cfg(test)]`-only counter incremented
    // once per `discoverable_entries_for` call on the drain path) makes that
    // walk count directly assertable, rather than only inferring "one walk"
    // from side effects a per-scope-walk regression could still satisfy.
    // Each test also proves the *batching* itself: nothing is walked until
    // the drain runs (via `process_pending_created_dirs`, called exactly
    // once), and that single drain call is sufficient to discover every
    // sibling/descendant and every file inside them.

    /// The dominant case: several sibling directories created under one
    /// root in a single burst, each holding a file. A nesting-only fix
    /// (one that only collapsed ancestor/descendant chains) would still
    /// walk once per sibling here, since none is nested inside another -
    /// this is the case that specifically requires grouping-by-root, not
    /// just `prune_nested_scopes`.
    #[tokio::test]
    async fn created_directory_burst_of_siblings_coalesces_into_one_drain() {
        use crate::config::Settings;
        use notify::event::CreateKind;

        let tempdir = tempfile::tempdir().unwrap();
        let root = tempdir.path().canonicalize().unwrap();
        let mut watcher = test_watcher_with_settings(&tempdir, Settings::default());
        watcher.handler_roots = vec![root.clone()];

        let mut siblings = Vec::new();
        for i in 0..5 {
            let dir = root.join(format!("sibling{i}"));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("f.rs"), "fn f() {}").unwrap();
            siblings.push(dir);
        }

        let dirs_before = watcher.registry.dir_count();

        for dir in &siblings {
            let mut event = Event::new(EventKind::Create(CreateKind::Folder));
            event.paths.push(dir.clone());
            watcher.handle_event(event).await;
        }

        // Nothing has been walked yet: five events recorded, zero walks run.
        assert_eq!(
            watcher.registry.dir_count(),
            dirs_before,
            "handle_event must only coalesce into dir_debouncer, never walk synchronously"
        );

        watcher.process_pending_created_dirs().await;

        assert_eq!(
            watcher.walk_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "five sibling scopes under one root must produce exactly one walk"
        );
        for dir in &siblings {
            assert!(
                watcher.registry.watch_dirs().contains(dir),
                "sibling directory {dir:?} must be watched after a single drain"
            );
        }
        let ready = watcher.debouncer.take_ready();
        for dir in &siblings {
            assert!(
                ready
                    .iter()
                    .any(|p| p.starts_with(dir) && p.ends_with("f.rs")),
                "file inside sibling {dir:?} must be debounced after a single drain: {ready:?}"
            );
        }
    }

    /// The case the issue names explicitly: a nested chain `a/b/c/d`
    /// recorded as four separate created-directory events (as a recursive
    /// mkdir -p or an extraction would generate). After one drain, the
    /// whole chain must be watched and every file throughout it picked up -
    /// this is what `prune_nested_scopes` collapsing the chain to its root
    /// is for, exercised here at the watcher altitude rather than as a bare
    /// unit test of the pruning function alone.
    #[tokio::test]
    async fn created_directory_nested_chain_coalesces_into_one_drain() {
        use crate::config::Settings;
        use notify::event::CreateKind;

        let tempdir = tempfile::tempdir().unwrap();
        let root = tempdir.path().canonicalize().unwrap();
        let mut watcher = test_watcher_with_settings(&tempdir, Settings::default());
        watcher.handler_roots = vec![root.clone()];

        let a = root.join("a");
        let b = a.join("b");
        let c = b.join("c");
        let d = c.join("d");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(a.join("fa.rs"), "fn fa() {}").unwrap();
        std::fs::write(b.join("fb.rs"), "fn fb() {}").unwrap();
        std::fs::write(c.join("fc.rs"), "fn fc() {}").unwrap();
        std::fs::write(d.join("fd.rs"), "fn fd() {}").unwrap();

        let dirs_before = watcher.registry.dir_count();

        for dir in [&a, &b, &c, &d] {
            let mut event = Event::new(EventKind::Create(CreateKind::Folder));
            event.paths.push(dir.clone());
            watcher.handle_event(event).await;
        }

        assert_eq!(
            watcher.registry.dir_count(),
            dirs_before,
            "handle_event must only coalesce into dir_debouncer, never walk synchronously"
        );

        watcher.process_pending_created_dirs().await;

        assert_eq!(
            watcher.walk_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "a nested chain under one root must collapse to exactly one walk"
        );
        for dir in [&a, &b, &c, &d] {
            assert!(
                watcher.registry.watch_dirs().contains(dir),
                "{dir:?} must be watched after a single drain of the whole chain"
            );
        }
        let ready = watcher.debouncer.take_ready();
        for name in ["fa.rs", "fb.rs", "fc.rs", "fd.rs"] {
            assert!(
                ready.iter().any(|p| p.ends_with(name)),
                "{name} must be debounced after a single drain: {ready:?}"
            );
        }
    }

    /// A burst that spans multiple scopes but all resolving to the same
    /// owning indexed root must still group into a single walk for that
    /// root - checked here via two disjoint (non-nested, non-sibling-named)
    /// subtrees under the same root, so `scopes_by_root` grouping is
    /// exercised independent of `prune_nested_scopes`.
    #[tokio::test]
    async fn created_directory_burst_spanning_same_root_groups_into_one_walk() {
        use crate::config::Settings;
        use notify::event::CreateKind;

        let tempdir = tempfile::tempdir().unwrap();
        let root = tempdir.path().canonicalize().unwrap();
        let mut watcher = test_watcher_with_settings(&tempdir, Settings::default());
        watcher.handler_roots = vec![root.clone()];

        let left = root.join("left/deep/path");
        let right = root.join("right/other/path");
        std::fs::create_dir_all(&left).unwrap();
        std::fs::create_dir_all(&right).unwrap();
        std::fs::write(left.join("left.rs"), "fn left() {}").unwrap();
        std::fs::write(right.join("right.rs"), "fn right() {}").unwrap();

        for dir in [&left, &right] {
            let mut event = Event::new(EventKind::Create(CreateKind::Folder));
            event.paths.push(dir.clone());
            watcher.handle_event(event).await;
        }

        watcher.process_pending_created_dirs().await;

        assert_eq!(
            watcher.walk_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "two disjoint scopes under the same root must group into one walk"
        );
        let ready = watcher.debouncer.take_ready();
        assert!(
            ready.iter().any(|p| p.ends_with("left.rs")),
            "file under the first scope must be debounced: {ready:?}"
        );
        assert!(
            ready.iter().any(|p| p.ends_with("right.rs")),
            "file under the second scope must be debounced: {ready:?}"
        );
    }

    #[tokio::test]
    async fn rescan_with_refresh_on_overflow_marks_stale() {
        let tempdir = tempfile::tempdir().unwrap();
        let mut watcher = test_watcher(&tempdir);
        watcher.refresh_on_overflow = true;

        assert!(!watcher.stale);
        assert!(watcher.stale_since.is_none());

        let event = rescan_event();
        assert!(event.paths.is_empty());
        watcher.handle_event(event).await;

        assert!(watcher.stale, "rescan event must flip stale to true");
        assert!(
            watcher.stale_since.is_some(),
            "rescan event must record stale_since"
        );
    }

    #[tokio::test]
    async fn rescan_without_refresh_on_overflow_leaves_stale_unset() {
        let tempdir = tempfile::tempdir().unwrap();
        let mut watcher = test_watcher(&tempdir);
        watcher.refresh_on_overflow = false;

        let event = rescan_event();
        watcher.handle_event(event).await;

        assert!(
            !watcher.stale,
            "stale must stay false when refresh_on_overflow is disabled"
        );
        assert!(watcher.stale_since.is_none());
    }

    #[tokio::test]
    async fn ordinary_event_while_stale_bumps_stale_clock() {
        let tempdir = tempfile::tempdir().unwrap();
        let mut watcher = test_watcher(&tempdir);
        watcher.refresh_on_overflow = true;

        // Enter the stale episode via a rescan/overflow signal.
        watcher.handle_event(rescan_event()).await;
        let first = watcher.stale_since.expect("rescan must set stale_since");

        // A later ordinary (non-rescan) signal must restart the quiet-window
        // clock so catch-up does not fire while activity is still arriving.
        tokio::time::sleep(Duration::from_millis(5)).await;
        let mut modify = Event::new(EventKind::Modify(notify::event::ModifyKind::Any));
        modify.paths.push(tempdir.path().join("some_file.rs"));
        watcher.handle_event(modify).await;

        let second = watcher.stale_since.expect("must still be stale");
        assert!(
            second > first,
            "an ordinary signal received while stale must advance stale_since"
        );
        assert!(watcher.stale, "an ordinary signal must not clear stale");
    }

    #[tokio::test]
    async fn ordinary_event_while_not_stale_does_not_set_stale() {
        let tempdir = tempfile::tempdir().unwrap();
        let mut watcher = test_watcher(&tempdir);
        watcher.refresh_on_overflow = true;

        let mut modify = Event::new(EventKind::Modify(notify::event::ModifyKind::Any));
        modify.paths.push(tempdir.path().join("some_file.rs"));
        watcher.handle_event(modify).await;

        assert!(
            !watcher.stale && watcher.stale_since.is_none(),
            "a non-rescan signal must not start a stale episode"
        );
    }

    #[test]
    fn should_catch_up_fires_once_when_stale_unpending_and_window_elapsed() {
        let window = Duration::from_millis(500);

        assert!(
            should_catch_up(true, false, Duration::from_millis(600), window),
            "stale + no pending + elapsed >= window must fire"
        );

        // Caller clears `stale` after firing; a second call with stale=false
        // must not fire again for the same episode.
        assert!(
            !should_catch_up(false, false, Duration::from_millis(600), window),
            "cleared stale must not re-fire"
        );
    }

    #[test]
    fn should_catch_up_does_not_fire_while_debouncer_has_pending() {
        let window = Duration::from_millis(500);

        assert!(!should_catch_up(
            true,
            true,
            Duration::from_millis(600),
            window
        ));
    }

    #[test]
    fn should_catch_up_does_not_fire_before_window_elapses() {
        let window = Duration::from_millis(500);

        assert!(!should_catch_up(
            true,
            false,
            Duration::from_millis(100),
            window
        ));
    }

    #[test]
    fn should_catch_up_does_not_refire_on_repeated_ticks_after_one_fire() {
        let window = Duration::from_millis(500);

        // First tick: fires.
        assert!(should_catch_up(
            true,
            false,
            Duration::from_millis(500),
            window
        ));

        // Caller clears stale/stale_since on fire. Subsequent ticks, even
        // with a large elapsed value (as if stale_since were never reset),
        // must not re-fire once stale is false.
        for elapsed_ms in [500, 1000, 5000] {
            assert!(!should_catch_up(
                false,
                false,
                Duration::from_millis(elapsed_ms),
                window
            ));
        }
    }

    // ── should_start_catch_up: in-flight guard + cooldown ──────────────────

    #[test]
    fn should_start_catch_up_refuses_double_spawn_while_in_flight() {
        let window = Duration::from_millis(500);

        assert!(
            !should_start_catch_up(
                true, // catch-up already in flight
                true,
                false,
                Duration::from_millis(600),
                window,
                None,
                CATCH_UP_COOLDOWN,
            ),
            "must not start a second catch-up while one is already running"
        );
    }

    #[test]
    fn should_start_catch_up_suppresses_immediate_refire_within_cooldown() {
        let window = Duration::from_millis(500);

        assert!(
            !should_start_catch_up(
                false,
                true,
                false,
                Duration::from_millis(600),
                window,
                Some(Duration::from_millis(100)), // just completed, well under cooldown
                CATCH_UP_COOLDOWN,
            ),
            "a completion inside the cooldown window must suppress an immediate re-fire"
        );
    }

    #[test]
    fn should_start_catch_up_fires_after_cooldown_elapses() {
        let window = Duration::from_millis(500);

        assert!(should_start_catch_up(
            false,
            true,
            false,
            Duration::from_millis(600),
            window,
            Some(CATCH_UP_COOLDOWN + Duration::from_millis(1)),
            CATCH_UP_COOLDOWN,
        ));
    }

    #[test]
    fn should_start_catch_up_fires_when_never_completed_before() {
        let window = Duration::from_millis(500);

        assert!(should_start_catch_up(
            false,
            true,
            false,
            Duration::from_millis(600),
            window,
            None, // no prior catch-up completion recorded
            CATCH_UP_COOLDOWN,
        ));
    }

    /// `maybe_start_catch_up` is production's only caller of
    /// `should_start_catch_up`, and composes the `has_pending` argument from
    /// *both* debouncers (`self.debouncer.has_pending() ||
    /// self.dir_debouncer.has_pending()`). The pure `should_start_catch_up_*`
    /// tests above exercise the predicate with `has_pending` passed in
    /// directly, so they cannot catch a regression that drops the
    /// `dir_debouncer` half of that composition. This exercises the
    /// composition itself: a created-directory scope is still debouncing
    /// (recorded, not yet drained) while the plain file `debouncer` is
    /// empty, staleness is armed and past the quiet window, yet
    /// `maybe_start_catch_up` must not spawn a catch-up task.
    #[tokio::test]
    async fn maybe_start_catch_up_defers_while_dir_debouncer_has_pending() {
        let tempdir = tempfile::tempdir().unwrap();
        let mut watcher = test_watcher(&tempdir);

        watcher
            .dir_debouncer
            .record(tempdir.path().join("still-settling"));
        assert!(!watcher.debouncer.has_pending());
        assert!(watcher.dir_debouncer.has_pending());

        watcher.stale = true;
        watcher.stale_since =
            Some(Instant::now() - watcher.debounce_window - Duration::from_millis(50));

        watcher.maybe_start_catch_up();

        assert!(
            watcher.catch_up_task.is_none(),
            "a created-directory scope still debouncing must defer catch-up, \
             the same race `self.dir_debouncer.has_pending()` exists to prevent"
        );
    }

    // ── should_clear_stale_after_success: the overflow-during-walk race ────

    #[test]
    fn should_clear_stale_after_success_when_no_stale_since_recorded() {
        let started_at = Instant::now();
        assert!(should_clear_stale_after_success(None, started_at));
    }

    #[test]
    fn should_clear_stale_after_success_when_signal_predates_task_start() {
        let started_at = Instant::now();
        let earlier = started_at - Duration::from_millis(10);
        assert!(should_clear_stale_after_success(Some(earlier), started_at));
    }

    #[test]
    fn should_not_clear_stale_after_success_when_signal_arrived_during_walk() {
        // Reproduces the race: overflow #2 arrives (bumping `stale_since`)
        // while a catch-up task spawned for overflow #1 is still running.
        // The completed walk (started before #2 arrived) must not be
        // allowed to clear staleness out from under the newer signal.
        let started_at = Instant::now();
        let later = started_at + Duration::from_millis(10);
        assert!(!should_clear_stale_after_success(Some(later), started_at));
    }

    // ── handle_catch_up_success / handle_catch_up_failure state machine ────

    fn dummy_outcome() -> ReindexOutcome {
        ReindexOutcome {
            reindexed: 1,
            symbol_count: 1,
            indexed_dirs: Vec::new(),
        }
    }

    #[tokio::test]
    async fn handle_catch_up_success_clears_stale_when_no_newer_signal() {
        let tempdir = tempfile::tempdir().unwrap();
        let mut watcher = test_watcher(&tempdir);

        let started_at = Instant::now();
        watcher.stale = true;
        watcher.stale_since = Some(started_at);
        watcher.catch_up_attempts = 3;

        watcher.handle_catch_up_success(dummy_outcome(), started_at);

        assert!(
            !watcher.stale,
            "success with no newer signal must clear stale"
        );
        assert!(watcher.stale_since.is_none());
        assert_eq!(watcher.catch_up_attempts, 0);
    }

    #[tokio::test]
    async fn handle_catch_up_success_keeps_stale_when_newer_signal_arrived_mid_walk() {
        let tempdir = tempfile::tempdir().unwrap();
        let mut watcher = test_watcher(&tempdir);

        let started_at = Instant::now();
        // Simulate overflow #2 bumping stale_since after the task started.
        let newer_signal = started_at + Duration::from_millis(10);
        watcher.stale = true;
        watcher.stale_since = Some(newer_signal);

        watcher.handle_catch_up_success(dummy_outcome(), started_at);

        assert!(
            watcher.stale,
            "a signal that arrived during the walk must keep the index marked stale"
        );
        assert_eq!(
            watcher.stale_since,
            Some(newer_signal),
            "the newer stale_since must be preserved so the quiet window re-measures from it"
        );
    }

    #[tokio::test]
    async fn handle_catch_up_failure_rearms_stale_below_max_attempts() {
        let tempdir = tempfile::tempdir().unwrap();
        let mut watcher = test_watcher(&tempdir);
        watcher.catch_up_attempts = MAX_CATCH_UP_ATTEMPTS - 2;
        watcher.stale = false;
        watcher.stale_since = None;

        watcher.handle_catch_up_failure(CatchUpFailure::JoinFailed("boom".to_string()));

        assert_eq!(watcher.catch_up_attempts, MAX_CATCH_UP_ATTEMPTS - 1);
        assert!(watcher.stale, "a failure must re-arm staleness for a retry");
        assert!(watcher.stale_since.is_some());
    }

    #[tokio::test]
    async fn handle_catch_up_failure_gives_up_at_max_attempts() {
        let tempdir = tempfile::tempdir().unwrap();
        let mut watcher = test_watcher(&tempdir);
        watcher.catch_up_attempts = MAX_CATCH_UP_ATTEMPTS - 1;
        watcher.stale = true;
        watcher.stale_since = Some(Instant::now());

        watcher.handle_catch_up_failure(CatchUpFailure::JoinFailed("boom".to_string()));

        assert_eq!(
            watcher.catch_up_attempts, 0,
            "attempt counter resets once the episode is abandoned"
        );
        assert!(
            !watcher.stale,
            "staleness tracking is cleared once attempts are exhausted, to avoid hot-looping forever"
        );
        assert!(watcher.stale_since.is_none());
    }

    /// A contention rejection (`ReindexInProgress`, wrapped by
    /// `WatchError::CatchUpReindexFailed`) must not consume an attempt or
    /// touch staleness tracking - another reindex is already doing the work.
    #[tokio::test]
    async fn handle_catch_up_failure_ignores_contention_rejection() {
        let tempdir = tempfile::tempdir().unwrap();
        let mut watcher = test_watcher(&tempdir);
        watcher.catch_up_attempts = 2;
        watcher.stale = true;
        let stale_since = Instant::now();
        watcher.stale_since = Some(stale_since);

        watcher.handle_catch_up_failure(CatchUpFailure::Watch(WatchError::CatchUpReindexFailed {
            source: IndexError::ReindexInProgress,
        }));

        assert_eq!(
            watcher.catch_up_attempts, 2,
            "a contention rejection must not consume an attempt"
        );
        assert!(
            watcher.stale,
            "a contention rejection must leave staleness armed"
        );
        assert_eq!(
            watcher.stale_since,
            Some(stale_since),
            "a contention rejection must not touch stale_since"
        );
    }

    /// A genuine failure (not a contention rejection) must still increment
    /// the attempt counter, even when carried as a typed `WatchError`.
    #[tokio::test]
    async fn handle_catch_up_failure_increments_on_genuine_watch_error() {
        let tempdir = tempfile::tempdir().unwrap();
        let mut watcher = test_watcher(&tempdir);
        watcher.catch_up_attempts = 0;
        watcher.stale = false;
        watcher.stale_since = None;

        watcher.handle_catch_up_failure(CatchUpFailure::Watch(WatchError::EventError {
            details: "boom".to_string(),
        }));

        assert_eq!(
            watcher.catch_up_attempts, 1,
            "a genuine failure must still increment the attempt counter"
        );
        assert!(watcher.stale, "a genuine failure must re-arm staleness");
        assert!(watcher.stale_since.is_some());
    }

    /// A contention rejection repeated more than `MAX_CATCH_UP_ATTEMPTS`
    /// times must never abandon the stale episode, since it never
    /// increments `catch_up_attempts` in the first place.
    #[tokio::test]
    async fn handle_catch_up_failure_repeated_contention_never_abandons_episode() {
        let tempdir = tempfile::tempdir().unwrap();
        let mut watcher = test_watcher(&tempdir);
        watcher.catch_up_attempts = 0;
        watcher.stale = true;
        let stale_since = Instant::now();
        watcher.stale_since = Some(stale_since);

        for _ in 0..(MAX_CATCH_UP_ATTEMPTS + 1) {
            watcher.handle_catch_up_failure(CatchUpFailure::Watch(
                WatchError::CatchUpReindexFailed {
                    source: IndexError::ReindexInProgress,
                },
            ));
        }

        assert_eq!(
            watcher.catch_up_attempts, 0,
            "repeated contention rejections must never consume attempts"
        );
        assert!(
            watcher.stale,
            "repeated contention rejections must never abandon the episode"
        );
        assert_eq!(watcher.stale_since, Some(stale_since));
    }

    /// `consecutive_contention` must track contention rejections
    /// independent of `catch_up_attempts`, incrementing on every contention
    /// rejection regardless of the escalation threshold.
    #[tokio::test]
    async fn handle_catch_up_failure_contention_increments_consecutive_counter() {
        let tempdir = tempfile::tempdir().unwrap();
        let mut watcher = test_watcher(&tempdir);
        watcher.consecutive_contention = 0;
        watcher.stale = true;
        watcher.stale_since = Some(Instant::now());

        watcher.handle_catch_up_failure(CatchUpFailure::Watch(WatchError::CatchUpReindexFailed {
            source: IndexError::ReindexInProgress,
        }));

        assert_eq!(watcher.consecutive_contention, 1);
    }

    /// A genuine failure resets the consecutive contention streak, so an
    /// isolated contention rejection followed by a real failure does not
    /// carry stale contention count forward into a later streak.
    #[tokio::test]
    async fn handle_catch_up_failure_genuine_failure_resets_consecutive_contention() {
        let tempdir = tempfile::tempdir().unwrap();
        let mut watcher = test_watcher(&tempdir);
        watcher.consecutive_contention = 5;
        watcher.stale = true;
        watcher.stale_since = Some(Instant::now());

        watcher.handle_catch_up_failure(CatchUpFailure::JoinFailed("boom".to_string()));

        assert_eq!(
            watcher.consecutive_contention, 0,
            "a genuine failure must reset the consecutive contention streak"
        );
    }

    /// A successful catch-up resets the consecutive contention streak, so a
    /// contention streak that resolves once the other reindex releases the
    /// gate does not carry over into a later, unrelated streak.
    #[tokio::test]
    async fn handle_catch_up_success_resets_consecutive_contention() {
        let tempdir = tempfile::tempdir().unwrap();
        let mut watcher = test_watcher(&tempdir);
        watcher.consecutive_contention = 7;
        watcher.stale = true;
        watcher.stale_since = Some(Instant::now());

        let outcome = ReindexOutcome {
            reindexed: 1,
            symbol_count: 1,
            indexed_dirs: Vec::new(),
        };
        watcher.handle_catch_up_success(outcome, Instant::now());

        assert_eq!(
            watcher.consecutive_contention, 0,
            "a successful catch-up must reset the consecutive contention streak"
        );
    }

    /// Below the escalation threshold, contention must never itself abandon
    /// the episode or consume a bounded attempt (repeat of the existing
    /// deliberate behavior, now with the consecutive-contention counter also
    /// under test at the boundary).
    #[tokio::test]
    async fn handle_catch_up_failure_contention_at_threshold_stays_deferred() {
        let tempdir = tempfile::tempdir().unwrap();
        let mut watcher = test_watcher(&tempdir);
        watcher.consecutive_contention = CONSECUTIVE_CONTENTION_WARN_THRESHOLD - 1;
        watcher.stale = true;
        watcher.stale_since = Some(Instant::now());

        watcher.handle_catch_up_failure(CatchUpFailure::Watch(WatchError::CatchUpReindexFailed {
            source: IndexError::ReindexInProgress,
        }));

        assert_eq!(
            watcher.consecutive_contention,
            CONSECUTIVE_CONTENTION_WARN_THRESHOLD
        );
        assert_eq!(watcher.catch_up_attempts, 0);
        assert!(watcher.stale);
    }

    /// Once consecutive contention exceeds the threshold, the counter keeps
    /// incrementing and staleness/attempt bookkeeping must remain untouched
    /// (the escalation is a logging change only, not a behavior change).
    #[tokio::test]
    async fn handle_catch_up_failure_contention_past_threshold_keeps_incrementing() {
        let tempdir = tempfile::tempdir().unwrap();
        let mut watcher = test_watcher(&tempdir);
        watcher.consecutive_contention = CONSECUTIVE_CONTENTION_WARN_THRESHOLD;
        watcher.stale = true;
        watcher.stale_since = Some(Instant::now());

        watcher.handle_catch_up_failure(CatchUpFailure::Watch(WatchError::CatchUpReindexFailed {
            source: IndexError::ReindexInProgress,
        }));

        assert_eq!(
            watcher.consecutive_contention,
            CONSECUTIVE_CONTENTION_WARN_THRESHOLD + 1
        );
        assert_eq!(watcher.catch_up_attempts, 0);
        assert!(watcher.stale);
    }

    /// End-to-end liveness proof for upstream v0.12.0's created-directory
    /// handling, driven through the real `watch()` loop over a real kernel
    /// watcher -- no hand-set state.
    ///
    /// Every other test of this feature populates `handler_roots` directly,
    /// so all of them pass even if `register_handler_roots()` is never
    /// called from `watch()`, or if `watch_roots()` returns empty because
    /// `init_cache()` ran after it instead of before. In either case the
    /// feature is dead in production and only this test notices: it asserts
    /// on the indexed result, so it fails unless the whole chain is live --
    /// refresh_paths -> init_cache -> eligibility.roots -> watch_roots ->
    /// register_handler_roots -> handle_event (records into dir_debouncer) ->
    /// process_pending_created_dirs (coalesced walk) -> debouncer ->
    /// on_modify -> reindex. Drives the real `watch()` loop over a real
    /// kernel watcher, so it reaches the select! timeout arm (which calls
    /// `process_pending_created_dirs`) on its own; no manual drain call is
    /// needed here.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn created_directory_is_indexed_through_the_real_watch_loop() {
        use crate::config::Settings;
        use crate::indexing::facade::IndexFacade;
        use crate::watcher::handlers::CodeFileHandler;

        let tempdir = tempfile::tempdir().unwrap();
        let root = tempdir
            .path()
            .canonicalize()
            .expect("temp dir must canonicalize");

        // Seed one indexed file so the watcher has something to watch and
        // `tracked_paths()` is non-empty at startup.
        std::fs::write(root.join("seed.py"), "def seed():\n    pass\n").unwrap();

        let index_dir = tempfile::tempdir().unwrap();
        // workspace_root set, as a real server's settings.toml has it, and
        // identical to the value handed to the handler below: the handler
        // relativizes paths against its own root while the facade resolves
        // them against settings, so production keeps the two the same.
        let mut settings = Settings {
            index_path: index_dir.path().to_path_buf(),
            workspace_root: Some(root.clone()),
            ..Default::default()
        };
        // `add_indexed_path` populates `indexed_paths_cache` itself, which is
        // what `init_cache` reads to build `eligibility.roots`.
        settings
            .add_indexed_path(root.clone())
            .expect("register the indexed root");

        let mut facade = IndexFacade::new(Arc::new(settings)).expect("facade over temp index");
        facade.index_directory(&root, false).expect("seed index");
        let facade = Arc::new(RwLock::new(facade));

        let handler = CodeFileHandler::new(Arc::clone(&facade), root.clone());
        let watcher = UnifiedWatcher::builder()
            .broadcaster(Arc::new(NotificationBroadcaster::new(16)))
            .indexer(Arc::clone(&facade))
            .workspace_root(root.clone())
            .handler(handler)
            .debounce_ms(0)
            .build()
            .expect("builder has all required fields");

        let watch_task = tokio::spawn(watcher.watch());

        // Let watch() finish startup (refresh_paths, tracked_paths,
        // watch_directory, register_handler_roots) before generating events.
        tokio::time::sleep(Duration::from_millis(750)).await;

        std::fs::create_dir_all(root.join("fresh")).unwrap();
        std::fs::write(
            root.join("fresh/arrival.py"),
            "def arrival_marker():\n    pass\n",
        )
        .unwrap();

        // Poll rather than sleep a fixed span: inotify delivery plus debounce
        // plus reindex has no bounded latency worth hardcoding. A generous
        // deadline keeps this from flaking on a loaded machine while still
        // failing outright if the feature is dead.
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut found = false;
        while Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let f = facade.read().await;
            if f.find_symbols_by_name("arrival_marker", None)
                .iter()
                .any(|s| s.name.as_ref() == "arrival_marker")
            {
                found = true;
                break;
            }
        }

        watch_task.abort();

        assert!(
            found,
            "a source file created inside a NEW directory under a registered \
             root must be indexed by the running watcher; not finding it means \
             the created-directory chain is not wired end to end"
        );
    }
}
