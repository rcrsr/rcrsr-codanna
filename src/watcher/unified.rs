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
    /// Whether to arm a one-time catch-up reindex at watcher startup, to
    /// re-converge with changes made while no watcher process was running.
    /// Independent of `refresh_on_overflow`: the two flags name two
    /// distinct triggers for the same catch-up machinery, not one trigger
    /// gated by two conditions.
    startup_catch_up: bool,
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
    /// When the last contention WARN was emitted in the current contention
    /// streak, if any. `None` means no WARN has been emitted yet in this
    /// streak, so the first past-threshold contention rejection still WARNs
    /// immediately. Reset alongside `consecutive_contention` (see
    /// [`UnifiedWatcher::reset_contention_streak`]).
    contention_warn_last_at: Option<Instant>,
    /// Quiet interval that must elapse before the next contention WARN may
    /// be emitted, widening geometrically after each WARN (see
    /// [`widen_contention_warn_interval`]) and starting at
    /// `CONTENTION_WARN_BASE_INTERVAL`.
    contention_warn_interval: Duration,
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

        // Arm a catch-up reindex now, after handler roots are registered but
        // before the event loop begins, so `stale_since` measures quiet time
        self.arm_startup_catch_up();

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

    /// Arm a one-time catch-up reindex at watcher startup, if enabled.
    ///
    /// Files may have changed while the watcher was not running (process
    /// restart, machine sleep, etc.), so the index can be stale the moment
    /// the event loop begins even before any filesystem event is observed.
    /// Marking staleness up front reuses the exact same debounce/cooldown/
    /// bounded-retry machinery that already handles the overflow case (see
    /// `mark_stale` above and the overflow signal handling in `watch()`):
    /// there is no separate "startup catch-up" code path, only a different
    /// trigger for the same one.
    ///
    /// Gated solely by `startup_catch_up`, which defaults to `false`. This
    /// is a distinct, independent trigger from `refresh_on_overflow`: the
    /// two keys each name a different condition ("watcher just started" vs.
    /// "backend reported overflow/rescan") that can arm the same underlying
    /// machinery, not one trigger gated by both flags conjunctively. Startup
    /// catch-up is opt-in because arming it means a full clear-and-rebuild
    /// of the index on every process start, not just on detected staleness.
    fn arm_startup_catch_up(&mut self) {
        if self.startup_catch_up {
            crate::log_event!(
                "watcher",
                "startup catch-up",
                "arming a catch-up reindex to re-converge with changes made while the watcher was down"
            );
            self.mark_stale();
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

    /// Reset the contention-streak triple to its start-of-streak state.
    ///
    /// `consecutive_contention`, `contention_warn_last_at` and
    /// `contention_warn_interval` move as one unit: they are all scoped to a
    /// single *continuous* streak of reindex-gate contention rejections, and
    /// must be reset together whenever that streak ends, whether because a
    /// catch-up reindex finally succeeded or because it failed for a genuine
    /// (non-contention) reason. Resetting only `consecutive_contention` while
    /// leaving `contention_warn_last_at`/`contention_warn_interval` behind
    /// would let a brand-new streak inherit a stale WARN timestamp and a
    /// widened interval from the *previous* streak, suppressing or
    /// mis-timing the next escalation. Deliberately does not touch
    /// `catch_up_attempts`: that counter has different lifecycle rules (it is
    /// bounded by `MAX_CATCH_UP_ATTEMPTS` and cleared on give-up, not on
    /// every contention-streak boundary).
    fn reset_contention_streak(&mut self) {
        self.consecutive_contention = 0;
        self.contention_warn_last_at = None;
        self.contention_warn_interval = CONTENTION_WARN_BASE_INTERVAL;
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
        self.reset_contention_streak();

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
    ///
    /// Once past the threshold, re-emission of that WARN is rate-limited by
    /// [`should_log_contention_warning`] rather than firing on every
    /// past-threshold rejection: the interval starts at
    /// `CONTENTION_WARN_BASE_INTERVAL` and widens geometrically (see
    /// [`widen_contention_warn_interval`]) each time a WARN actually fires,
    /// capped at [`contention_warn_backoff_cap`]. Past-threshold rejections
    /// that are suppressed by the rate limit still fall through to
    /// `crate::debug_event!` so nothing goes silent at any log level.
    fn handle_catch_up_failure(&mut self, failure: CatchUpFailure) {
        if failure.is_contention() {
            self.consecutive_contention += 1;

            let since_last_warn = self.contention_warn_last_at.map(|t| t.elapsed());
            if should_log_contention_warning(
                self.consecutive_contention,
                CONSECUTIVE_CONTENTION_WARN_THRESHOLD,
                since_last_warn,
                self.contention_warn_interval,
            ) {
                // Widen only when this streak has already consumed an
                // interval (i.e. a previous WARN was emitted); the first
                // past-threshold WARN fires immediately without widening, so
                // the schedule is base, 2x, 4x, cap, cap... (10m, 20m, 40m,
                // 60m, 60m...) rather than skipping straight to 2x base.
                //
                // The cadence named in the WARN below must be the interval
                // that will actually gate the *next* report, not the one
                // that gated this one, so compute it up front and use it in
                // the message before performing the real state mutation.
                let next_report_interval = if self.contention_warn_last_at.is_some() {
                    widen_contention_warn_interval(
                        self.contention_warn_interval,
                        CONTENTION_WARN_BASE_INTERVAL,
                    )
                } else {
                    self.contention_warn_interval
                };

                tracing::warn!(
                    "[watcher] catch-up reindex has been rejected by reindex-gate contention {} times in a row; \
                     another reindex may be wedged. A restart may be needed if this persists; \
                     next report in at most {} minute(s) while this persists.",
                    self.consecutive_contention,
                    next_report_interval.as_secs().div_ceil(60)
                );

                self.contention_warn_interval = next_report_interval;
                self.contention_warn_last_at = Some(Instant::now());
            } else {
                crate::debug_event!(
                    "watcher",
                    "catch-up reindex deferred",
                    "another full reindex is already in progress; will retry after cooldown"
                );
            }
            return;
        }

        self.reset_contention_streak();
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
///
/// Crossing the threshold does not WARN on every single rejection
/// thereafter: re-emission is rate-limited by
/// [`should_log_contention_warning`] against `contention_warn_interval`
/// (starting at `CONTENTION_WARN_BASE_INTERVAL` and widening after each
/// WARN, see [`widen_contention_warn_interval`]), so a wedged gate holder
/// pages at a decreasing cadence instead of flooding the log once past
/// threshold.
const CONSECUTIVE_CONTENTION_WARN_THRESHOLD: u32 = 12;

/// Base quiet interval that must elapse between consecutive contention WARN
/// log lines once a streak has passed
/// `CONSECUTIVE_CONTENTION_WARN_THRESHOLD`, before backoff (see
/// [`widen_contention_warn_interval`]) widens it further.
///
/// The most likely root cause of a sustained contention streak is a wedged
/// reindex phase-2 walk, which is *already* reported at `tracing::error!` by
/// `spawn_reindex_phase2_watchdog` (src/indexing/facade.rs:1940) on a
/// 10m/20m/40m/hourly schedule. Using the same 600s base for this WARN keeps
/// the two signals proportionate -- roughly two log lines an hour at steady
/// state -- instead of stacking a faster WARN cadence on top of an
/// already-visible ERROR for the same underlying wedge.
const CONTENTION_WARN_BASE_INTERVAL: Duration = Duration::from_secs(600);

/// The interval between contention WARN log lines widens by this factor
/// after each WARN (10m -> 20m -> 40m -> ...), capped by
/// [`contention_warn_backoff_cap`], mirroring
/// `REINDEX_WATCHDOG_BACKOFF_MULTIPLIER` (src/indexing/facade.rs:1893) for
/// the same reason: a sustained wedge should page loudly at first, then
/// settle to a low, non-flooding cadence.
const CONTENTION_WARN_BACKOFF_MULTIPLIER: u32 = 2;

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

/// Caps the widening contention-WARN interval at 6x `base`. Expressed
/// relative to `base` rather than a minute literal so the unit tests can
/// drive the same logic with a millisecond base; at the sole production
/// base of 600s (`CONTENTION_WARN_BASE_INTERVAL`) this yields a 10m -> 20m
/// -> 40m -> hourly-thereafter schedule.
///
/// This deliberately mirrors `watchdog_backoff_cap`
/// (src/indexing/facade.rs:1900), the phase-2 reindex watchdog's identical
/// cap formula, so the two independent backoff schedules stay proportionate
/// without coupling `watcher/` to `indexing/` for a two-line formula (see
/// §CDNA.1: extraction is disproportionate at just two call sites).
fn contention_warn_backoff_cap(base: Duration) -> Duration {
    base.saturating_mul(6)
}

/// Widens the contention-WARN interval by `CONTENTION_WARN_BACKOFF_MULTIPLIER`
/// after each WARN, capped at [`contention_warn_backoff_cap`] relative to
/// `base`. A fixed point at the cap: `widen(cap, base) == cap`. Never resets
/// on its own -- only a fresh contention streak resets the interval back to
/// `base`.
///
/// For any non-zero `base`, this never returns zero (the cap is
/// `base.saturating_mul(6)`, also non-zero, and `min` of two non-zero
/// durations is non-zero). It *can* return zero if `base` itself is zero
/// (`contention_warn_backoff_cap(0) == 0`, so `min(current * 2, 0) == 0`).
/// The sole production caller always passes `CONTENTION_WARN_BASE_INTERVAL`
/// (600s), a fixed non-zero constant, so a zero `base` is unreachable in
/// practice; this is documented rather than enforced with a newtype or
/// constructor validation, since introducing either for a two-line helper
/// with one production call site would be disproportionate.
fn widen_contention_warn_interval(current: Duration, base: Duration) -> Duration {
    std::cmp::min(
        current.saturating_mul(CONTENTION_WARN_BACKOFF_MULTIPLIER),
        contention_warn_backoff_cap(base),
    )
}

/// Pure decision predicate for whether a contention rejection should log a
/// `tracing::warn!` line now.
///
/// Fires exactly when the consecutive-contention `streak` has passed
/// `threshold` (the same strict `>` gate `handle_catch_up_failure` already
/// applies) *and* either no WARN has been emitted yet in this streak
/// (`since_last_warn` is `None`, so the first past-threshold rejection still
/// WARNs immediately, as today) or at least `interval` has elapsed since the
/// last WARN (`>=`, matching the `elapsed >= window` convention already used
/// by [`should_catch_up`]).
fn should_log_contention_warning(
    streak: u32,
    threshold: u32,
    since_last_warn: Option<Duration>,
    interval: Duration,
) -> bool {
    streak > threshold && since_last_warn.is_none_or(|since| since >= interval)
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
    startup_catch_up: bool,
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
            // Mirrors `FileWatchConfig::startup_catch_up`'s default of
            // `false`, so a builder-constructed watcher without an explicit
            // `.startup_catch_up(...)` call behaves the same as one built
            // from default config: startup catch-up is opt-in.
            startup_catch_up: false,
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

    /// Set whether to arm a one-time catch-up reindex at watcher startup,
    /// to re-converge with changes made while no watcher process was
    /// running.
    pub fn startup_catch_up(mut self, enabled: bool) -> Self {
        self.startup_catch_up = enabled;
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
            startup_catch_up: self.startup_catch_up,
            debounce_window: Duration::from_millis(self.debounce_ms),
            catch_up_task: None,
            catch_up_started_at: None,
            last_catch_up_completed: None,
            catch_up_attempts: 0,
            consecutive_contention: 0,
            contention_warn_last_at: None,
            contention_warn_interval: CONTENTION_WARN_BASE_INTERVAL,
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

    /// `arm_startup_catch_up` takes `&mut self`, so unlike `watch()` (which
    /// consumes `self`) it is directly callable from the module's own tests
    /// -- the same direct-state shape already used by
    /// `rescan_with_refresh_on_overflow_marks_stale` /
    /// `rescan_without_refresh_on_overflow_leaves_stale_unset` above. This
    /// covers only "does arming mark stale", not "is arming wired into
    /// `watch()`"; that second property is covered by the e2e positive
    /// `startup_catch_up_indexes_files_added_while_watcher_was_down` below.
    #[tokio::test]
    async fn arm_startup_catch_up_marks_stale_when_enabled() {
        let tempdir = tempfile::tempdir().unwrap();
        let mut watcher = test_watcher(&tempdir);
        watcher.startup_catch_up = true;

        assert!(!watcher.stale);
        assert!(watcher.stale_since.is_none());

        watcher.arm_startup_catch_up();

        assert!(watcher.stale, "startup_catch_up=true must mark stale");
        assert!(
            watcher.stale_since.is_some(),
            "startup_catch_up=true must record stale_since"
        );
    }

    /// `refresh_on_overflow` is deliberately set to `true` here (the
    /// OR-gate trap): `startup_catch_up` and `refresh_on_overflow` are two
    /// independent triggers for the same machinery, so a stray
    /// `self.refresh_on_overflow || self.startup_catch_up` (or any other
    /// accidental OR) in `arm_startup_catch_up` would arm here even though
    /// `startup_catch_up` itself is `false`.
    #[tokio::test]
    async fn arm_startup_catch_up_is_noop_when_disabled() {
        let tempdir = tempfile::tempdir().unwrap();
        let mut watcher = test_watcher(&tempdir);
        watcher.startup_catch_up = false;
        watcher.refresh_on_overflow = true;

        watcher.arm_startup_catch_up();

        assert!(
            !watcher.stale,
            "startup_catch_up=false must not mark stale even with refresh_on_overflow=true; \
             an OR-gate on `refresh_on_overflow` would arm here."
        );
        assert!(
            watcher.stale_since.is_none(),
            "startup_catch_up=false must not record stale_since even with refresh_on_overflow=true; \
             an OR-gate on `refresh_on_overflow` would arm here."
        );
    }

    #[test]
    fn builder_defaults_startup_catch_up_off() {
        let tempdir = tempfile::tempdir().unwrap();
        let watcher = test_watcher(&tempdir);

        assert!(
            !watcher.startup_catch_up,
            "startup_catch_up must default to false without an explicit \
             `.startup_catch_up(true)` builder call"
        );
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

    // ── contention-WARN backoff: contention_warn_backoff_cap /
    //    widen_contention_warn_interval / should_log_contention_warning ────

    #[test]
    fn contention_warn_backoff_cap_is_six_times_base() {
        let base = Duration::from_millis(10);
        assert_eq!(contention_warn_backoff_cap(base), Duration::from_millis(60));
    }

    #[test]
    fn widen_contention_warn_interval_doubles_then_caps() {
        let base = Duration::from_millis(10);
        let cap = contention_warn_backoff_cap(base);

        let widened_once = widen_contention_warn_interval(base, base);
        assert_eq!(widened_once, Duration::from_millis(20), "base -> 2x base");

        let widened_twice = widen_contention_warn_interval(widened_once, base);
        assert_eq!(widened_twice, Duration::from_millis(40), "2x -> 4x base");

        let widened_thrice = widen_contention_warn_interval(widened_twice, base);
        assert_eq!(
            widened_thrice, cap,
            "4x -> 8x base would exceed the cap, so it clamps to the cap"
        );

        let widened_again = widen_contention_warn_interval(widened_thrice, base);
        assert_eq!(
            widened_again, cap,
            "widening a value already at the cap must stay fixed at the cap, \
             never growing unbounded and never resetting on its own"
        );
    }

    #[test]
    fn should_log_contention_warning_respects_existing_threshold_semantics() {
        let threshold = CONSECUTIVE_CONTENTION_WARN_THRESHOLD;
        let interval = CONTENTION_WARN_BASE_INTERVAL;

        assert!(
            !should_log_contention_warning(threshold, threshold, None, interval),
            "streak == threshold must not fire, matching the existing strict `>` gate"
        );
        assert!(
            should_log_contention_warning(threshold + 1, threshold, None, interval),
            "the first rejection past threshold must WARN immediately, as today, \
             even with no prior WARN recorded in this streak"
        );
    }

    #[test]
    fn should_log_contention_warning_suppresses_inside_interval() {
        let threshold = CONSECUTIVE_CONTENTION_WARN_THRESHOLD;
        let interval = CONTENTION_WARN_BASE_INTERVAL;
        let since_last_warn = interval - Duration::from_millis(1);

        assert!(!should_log_contention_warning(
            threshold + 1,
            threshold,
            Some(since_last_warn),
            interval,
        ));
    }

    #[test]
    fn should_log_contention_warning_fires_at_interval_boundary() {
        let threshold = CONSECUTIVE_CONTENTION_WARN_THRESHOLD;
        let interval = CONTENTION_WARN_BASE_INTERVAL;

        assert!(should_log_contention_warning(
            threshold + 1,
            threshold,
            Some(interval),
            interval,
        ));
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

    // ── contention-WARN backoff state machine (contention_warn_last_at /
    //    contention_warn_interval), including a log-capture provenance
    //    test that the WARN cadence genuinely changes emitted output and
    //    not merely internal state ─────────────────────────────────────

    /// The very first past-threshold contention rejection in a streak must
    /// stamp `contention_warn_last_at` and increment the streak, but must
    /// not widen `contention_warn_interval` -- widening only applies once a
    /// previous WARN has already consumed an interval (see
    /// `handle_catch_up_failure`'s "base, 2x, 4x, cap, cap..." schedule).
    #[tokio::test]
    async fn contention_first_warn_past_threshold_stamps_without_widening() {
        let tempdir = tempfile::tempdir().unwrap();
        let mut watcher = test_watcher(&tempdir);
        watcher.consecutive_contention = CONSECUTIVE_CONTENTION_WARN_THRESHOLD;
        watcher.contention_warn_last_at = None;
        watcher.contention_warn_interval = CONTENTION_WARN_BASE_INTERVAL;
        watcher.stale = true;
        let stale_since = Instant::now();
        watcher.stale_since = Some(stale_since);

        watcher.handle_catch_up_failure(CatchUpFailure::Watch(WatchError::CatchUpReindexFailed {
            source: IndexError::ReindexInProgress,
        }));

        assert_eq!(
            watcher.consecutive_contention,
            CONSECUTIVE_CONTENTION_WARN_THRESHOLD + 1
        );
        assert!(
            watcher.contention_warn_last_at.is_some(),
            "the first past-threshold rejection must stamp the WARN timestamp"
        );
        assert_eq!(
            watcher.contention_warn_interval, CONTENTION_WARN_BASE_INTERVAL,
            "the first WARN must not widen the interval it just consumed"
        );
        assert_eq!(watcher.catch_up_attempts, 0);
        assert_eq!(watcher.stale_since, Some(stale_since));
    }

    /// A contention rejection arriving before `contention_warn_interval` has
    /// elapsed since the last WARN must neither restamp
    /// `contention_warn_last_at` nor widen `contention_warn_interval`, even
    /// though the streak itself keeps incrementing.
    #[tokio::test]
    async fn contention_inside_interval_neither_restamps_nor_widens() {
        let tempdir = tempfile::tempdir().unwrap();
        let mut watcher = test_watcher(&tempdir);
        watcher.consecutive_contention = CONSECUTIVE_CONTENTION_WARN_THRESHOLD + 1;
        let last_warn = Instant::now();
        watcher.contention_warn_last_at = Some(last_warn);
        watcher.contention_warn_interval = CONTENTION_WARN_BASE_INTERVAL;

        watcher.handle_catch_up_failure(CatchUpFailure::Watch(WatchError::CatchUpReindexFailed {
            source: IndexError::ReindexInProgress,
        }));

        assert_eq!(
            watcher.consecutive_contention,
            CONSECUTIVE_CONTENTION_WARN_THRESHOLD + 2,
            "the streak counter must keep incrementing even while suppressed"
        );
        assert_eq!(
            watcher.contention_warn_last_at,
            Some(last_warn),
            "a suppressed rejection must not restamp the WARN timestamp"
        );
        assert_eq!(
            watcher.contention_warn_interval, CONTENTION_WARN_BASE_INTERVAL,
            "a suppressed rejection must not widen the interval"
        );
    }

    /// Once `contention_warn_interval` has fully elapsed since the last
    /// WARN, the next contention rejection must restamp
    /// `contention_warn_last_at` and widen `contention_warn_interval` to 2x
    /// its previous value.
    #[tokio::test]
    async fn contention_after_interval_elapses_restamps_and_widens() {
        let tempdir = tempfile::tempdir().unwrap();
        let mut watcher = test_watcher(&tempdir);
        watcher.consecutive_contention = CONSECUTIVE_CONTENTION_WARN_THRESHOLD + 1;
        watcher.contention_warn_interval = CONTENTION_WARN_BASE_INTERVAL;
        let stale_last_warn =
            Instant::now() - watcher.contention_warn_interval - Duration::from_millis(50);
        watcher.contention_warn_last_at = Some(stale_last_warn);

        watcher.handle_catch_up_failure(CatchUpFailure::Watch(WatchError::CatchUpReindexFailed {
            source: IndexError::ReindexInProgress,
        }));

        assert!(
            watcher
                .contention_warn_last_at
                .is_some_and(|t| t > stale_last_warn),
            "an elapsed interval must advance the WARN timestamp"
        );
        assert_eq!(
            watcher.contention_warn_interval,
            CONTENTION_WARN_BASE_INTERVAL * CONTENTION_WARN_BACKOFF_MULTIPLIER,
            "an elapsed interval must widen to 2x the previous interval"
        );
    }

    /// Once `contention_warn_interval` has widened to
    /// `contention_warn_backoff_cap`, further elapsed-interval rejections
    /// must keep restamping the WARN timestamp (the signal never stops)
    /// while leaving the interval capped rather than doubling past it.
    #[tokio::test]
    async fn contention_warn_interval_stays_capped_and_never_stops() {
        let tempdir = tempfile::tempdir().unwrap();
        let mut watcher = test_watcher(&tempdir);
        watcher.consecutive_contention = CONSECUTIVE_CONTENTION_WARN_THRESHOLD + 1;
        let cap = contention_warn_backoff_cap(CONTENTION_WARN_BASE_INTERVAL);
        watcher.contention_warn_interval = cap;
        let stale_last_warn = Instant::now() - cap - Duration::from_millis(50);
        watcher.contention_warn_last_at = Some(stale_last_warn);

        watcher.handle_catch_up_failure(CatchUpFailure::Watch(WatchError::CatchUpReindexFailed {
            source: IndexError::ReindexInProgress,
        }));

        assert_eq!(
            watcher.contention_warn_interval, cap,
            "the interval must stay capped rather than doubling past it"
        );
        assert!(
            watcher
                .contention_warn_last_at
                .is_some_and(|t| t > stale_last_warn),
            "a capped interval must still restamp on every elapsed WARN -- the signal must never stop"
        );
    }

    /// A genuine (non-contention) failure must reset the full
    /// contention-warn backoff triple, not just the streak counter --
    /// otherwise a brand-new streak would inherit a widened interval and a
    /// stale timestamp from the *previous* streak, suppressing its first
    /// WARN for up to an hour.
    #[tokio::test]
    async fn genuine_failure_resets_contention_warn_backoff() {
        let tempdir = tempfile::tempdir().unwrap();
        let mut watcher = test_watcher(&tempdir);
        watcher.consecutive_contention = 20;
        watcher.contention_warn_last_at = Some(Instant::now());
        watcher.contention_warn_interval =
            contention_warn_backoff_cap(CONTENTION_WARN_BASE_INTERVAL);

        watcher.handle_catch_up_failure(CatchUpFailure::JoinFailed("boom".to_string()));

        assert_eq!(watcher.consecutive_contention, 0);
        assert!(
            watcher.contention_warn_last_at.is_none(),
            "a genuine failure must clear the WARN timestamp, not just the streak"
        );
        assert_eq!(
            watcher.contention_warn_interval, CONTENTION_WARN_BASE_INTERVAL,
            "a genuine failure must reset the interval back to base, not just the streak"
        );
    }

    /// A successful catch-up must reset the full contention-warn backoff
    /// triple identically to a genuine failure -- the same partial-fix
    /// hazard applies: resetting only the streak would suppress the next
    /// streak's first WARN for up to an hour.
    #[tokio::test]
    async fn success_resets_contention_warn_backoff() {
        let tempdir = tempfile::tempdir().unwrap();
        let mut watcher = test_watcher(&tempdir);
        watcher.consecutive_contention = 20;
        watcher.contention_warn_last_at = Some(Instant::now());
        watcher.contention_warn_interval =
            contention_warn_backoff_cap(CONTENTION_WARN_BASE_INTERVAL);

        watcher.handle_catch_up_success(dummy_outcome(), Instant::now());

        assert_eq!(watcher.consecutive_contention, 0);
        assert!(
            watcher.contention_warn_last_at.is_none(),
            "success must clear the WARN timestamp, not just the streak"
        );
        assert_eq!(
            watcher.contention_warn_interval, CONTENTION_WARN_BASE_INTERVAL,
            "success must reset the interval back to base, not just the streak"
        );
    }

    /// Minimal `MakeWriter` that clones the shared buffer handle on every
    /// write-site lookup, so a single `Arc<Mutex<Vec<u8>>>` captures every
    /// line a scoped `tracing::subscriber::with_default` subscriber emits
    /// during a test -- state-only assertions above cannot distinguish a
    /// correct gated-WARN implementation from one that gates only the
    /// *stamp* while still calling `tracing::warn!` unconditionally; only
    /// capturing real output falsifies that shadow-run outcome.
    struct VecWriter(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for VecWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[derive(Clone)]
    struct SharedVecMakeWriter(Arc<std::sync::Mutex<Vec<u8>>>);

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedVecMakeWriter {
        type Writer = VecWriter;

        fn make_writer(&'a self) -> Self::Writer {
            VecWriter(Arc::clone(&self.0))
        }
    }

    /// Proof for issue #47: a sustained contention streak emits exactly one
    /// WARN per rate-limit interval, not one per rejection (which would
    /// flood the log) and not zero after the first (which would silently
    /// go dark, reintroducing the #46 blind spot). `handle_catch_up_failure`
    /// is synchronous, so a current-thread `#[tokio::test]` keeps the
    /// scoped subscriber installed via `tracing::subscriber::with_default`
    /// live across the whole drive loop below.
    ///
    /// Phase 1 drives 20 consecutive contention rejections with no
    /// back-dating: the 13th (first past `CONSECUTIVE_CONTENTION_WARN_THRESHOLD`
    /// = 12) rejection must WARN, and none of the remaining 7 may, since
    /// `CONTENTION_WARN_BASE_INTERVAL` (10 minutes) cannot have elapsed in
    /// real wall-clock time within a unit test.
    ///
    /// Phase 2 back-dates `contention_warn_last_at` past the (still-base)
    /// interval and drives one further rejection: this must produce a
    /// SECOND WARN line, proving the cadence resumes rather than
    /// permanently capping at one line and going silent again.
    #[tokio::test]
    async fn sustained_contention_emits_exactly_one_warn_per_interval() {
        let tempdir = tempfile::tempdir().unwrap();
        let mut watcher = test_watcher(&tempdir);
        watcher.consecutive_contention = 0;
        watcher.contention_warn_last_at = None;
        watcher.contention_warn_interval = CONTENTION_WARN_BASE_INTERVAL;

        let buf = Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(SharedVecMakeWriter(Arc::clone(&buf)))
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .without_time()
            .finish();

        tracing::subscriber::with_default(subscriber, || {
            for _ in 0..20 {
                watcher.handle_catch_up_failure(CatchUpFailure::Watch(
                    WatchError::CatchUpReindexFailed {
                        source: IndexError::ReindexInProgress,
                    },
                ));
            }
        });

        let captured = String::from_utf8(buf.lock().unwrap().clone())
            .expect("tracing-subscriber's fmt output must be valid UTF-8");
        let warn_lines: Vec<&str> = captured.lines().filter(|l| l.contains("WARN")).collect();

        assert_eq!(
            warn_lines.len(),
            1,
            "exactly one WARN must be emitted across 20 rejections inside a single \
             un-elapsed interval, got: {captured:?}"
        );
        assert!(
            warn_lines[0].contains("wedged"),
            "the WARN must keep naming the recovery guidance, got: {}",
            warn_lines[0]
        );

        // Phase 2: force the interval to have elapsed and prove the signal
        // resumes rather than going permanently silent after the first line.
        watcher.contention_warn_last_at =
            Some(Instant::now() - watcher.contention_warn_interval - Duration::from_millis(50));

        tracing::subscriber::with_default(subscriber_from(&buf), || {
            watcher.handle_catch_up_failure(CatchUpFailure::Watch(
                WatchError::CatchUpReindexFailed {
                    source: IndexError::ReindexInProgress,
                },
            ));
        });

        let captured_after = String::from_utf8(buf.lock().unwrap().clone())
            .expect("tracing-subscriber's fmt output must be valid UTF-8");
        let warn_lines_after: Vec<&str> = captured_after
            .lines()
            .filter(|l| l.contains("WARN"))
            .collect();

        assert_eq!(
            warn_lines_after.len(),
            2,
            "a second WARN must be emitted once the interval has elapsed again -- \
             the cadence must never stop, got: {captured_after:?}"
        );
    }

    /// Builds a fresh capturing subscriber writing into the same shared
    /// buffer as an earlier one, so [`sustained_contention_emits_exactly_one_warn_per_interval`]'s
    /// second phase can install a new scoped subscriber without losing the
    /// first phase's captured output (the buffer, not the subscriber, is
    /// what persists across phases).
    fn subscriber_from(
        buf: &Arc<std::sync::Mutex<Vec<u8>>>,
    ) -> impl tracing::Subscriber + Send + Sync {
        tracing_subscriber::fmt()
            .with_writer(SharedVecMakeWriter(Arc::clone(buf)))
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .without_time()
            .finish()
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

    /// End-to-end liveness proof that `arm_startup_catch_up` re-converges
    /// the index with changes made while no watcher process was running, by
    /// driving the real `watch()` loop over a real kernel watcher -- no
    /// hand-set state.
    ///
    /// `while_down.py` is written to disk *before* `watch()` is ever
    /// called, so no `notify` event exists for it: the file's creation
    /// predates the watcher's own existence. A hand-set `stale`/
    /// `stale_since` (as most other tests in this module use) would prove
    /// nothing about whether `watch()` itself arms the catch-up; only
    /// driving the real loop from a cold start, as this test does, can show
    /// that `arm_startup_catch_up` is actually wired into `watch()`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn startup_catch_up_indexes_files_added_while_watcher_was_down() {
        use crate::config::Settings;
        use crate::indexing::facade::IndexFacade;
        use crate::watcher::handlers::CodeFileHandler;

        let tempdir = tempfile::tempdir().unwrap();
        let root = tempdir
            .path()
            .canonicalize()
            .expect("temp dir must canonicalize");

        std::fs::write(root.join("seed.py"), "def seed():\n    pass\n").unwrap();

        let index_dir = tempfile::tempdir().unwrap();
        let mut settings = Settings {
            index_path: index_dir.path().to_path_buf(),
            workspace_root: Some(root.clone()),
            ..Default::default()
        };
        // A force reindex (which the catch-up path runs) rebuilds from
        // `indexed_paths_cache`, not from whatever happens to already be in
        // the index -- without this, the walk covers nothing and the test
        // would fail for the wrong reason.
        settings
            .add_indexed_path(root.clone())
            .expect("register the indexed root");

        let mut facade = IndexFacade::new(Arc::new(settings)).expect("facade over temp index");
        facade.index_directory(&root, false).expect("seed index");
        let facade = Arc::new(RwLock::new(facade));

        // Written to disk before the watcher exists: no notify event will
        // ever fire for this file. Only startup catch-up can discover it.
        std::fs::write(
            root.join("while_down.py"),
            "def while_down_marker():\n    pass\n",
        )
        .unwrap();

        let handler = CodeFileHandler::new(Arc::clone(&facade), root.clone());
        let watcher = UnifiedWatcher::builder()
            .broadcaster(Arc::new(NotificationBroadcaster::new(16)))
            .indexer(Arc::clone(&facade))
            .workspace_root(root.clone())
            .handler(handler)
            .debounce_ms(0)
            .startup_catch_up(true)
            .build()
            .expect("builder has all required fields");

        let watch_task = tokio::spawn(watcher.watch());

        let deadline = Instant::now() + Duration::from_secs(30);
        let mut found = false;
        while Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let f = facade.read().await;
            if f.find_symbols_by_name("while_down_marker", None)
                .iter()
                .any(|s| s.name.as_ref() == "while_down_marker")
            {
                found = true;
                break;
            }
        }

        watch_task.abort();

        assert!(
            found,
            "a file created on disk before the watcher started must be picked \
             up by a startup catch-up reindex; not finding it means \
             `arm_startup_catch_up` is not wired into `watch()`"
        );
    }

    /// End-to-end liveness proof that startup catch-up also drops symbols
    /// for files deleted while no watcher process was running, driven
    /// through the real `watch()` loop.
    ///
    /// `gone_marker` is indexed, then its file is deleted from disk before
    /// `watch()` is ever called -- no notify event exists for the
    /// deletion either. Only a force/clear+rebuild reindex removes symbols
    /// for a vanished file; a relocated or "cheaper" incremental walk would
    /// leave them behind. `seed_marker` is the positive control proving a
    /// rebuild actually ran rather than the index simply having been wiped
    /// wholesale: without it, a bug that dropped ALL symbols (not just the
    /// deleted file's) would also make this test pass.
    ///
    /// The poll loop below requires `gone_marker` absent AND `seed_marker`
    /// present *in the same sample*, then breaks. This is deliberate:
    /// `reindex_locked`'s phase 1 commits an emptied index before phase 2
    /// rebuilds it (see the `clear_index()` call gated by
    /// `paths_is_none && force` in `reindex_locked`, `src/indexing/facade.rs`
    /// -- deliberately cited without line numbers, which rot), so there is a
    /// real, observable window where both markers are absent at once. A poll loop that reads
    /// `gone_marker` in one sample, latches on its absence, and only *then*
    /// reads `seed_marker` (possibly in the very same sample, but treating
    /// the two reads as independent) can land inside that transient
    /// clear-window and record `seed_marker` as absent even though the
    /// rebuild is still in flight and will restore it moments later --
    /// producing a flaky false negative on the positive control. Requiring
    /// both conditions jointly, and continuing to poll otherwise, makes the
    /// loop wait out that window rather than sampling inside it. Do not
    /// split this back into two independent reads across different
    /// samples; that reintroduces the same race in a different shape.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn startup_catch_up_drops_symbols_for_files_deleted_while_watcher_was_down() {
        use crate::config::Settings;
        use crate::indexing::facade::IndexFacade;
        use crate::watcher::handlers::CodeFileHandler;

        let tempdir = tempfile::tempdir().unwrap();
        let root = tempdir
            .path()
            .canonicalize()
            .expect("temp dir must canonicalize");

        std::fs::write(root.join("seed.py"), "def seed_marker():\n    pass\n").unwrap();
        let gone_path = root.join("gone.py");
        std::fs::write(&gone_path, "def gone_marker():\n    pass\n").unwrap();

        let index_dir = tempfile::tempdir().unwrap();
        let mut settings = Settings {
            index_path: index_dir.path().to_path_buf(),
            workspace_root: Some(root.clone()),
            ..Default::default()
        };
        settings
            .add_indexed_path(root.clone())
            .expect("register the indexed root");

        let mut facade = IndexFacade::new(Arc::new(settings)).expect("facade over temp index");
        facade.index_directory(&root, false).expect("seed index");
        let facade = Arc::new(RwLock::new(facade));

        // Deleted from disk before the watcher exists: no notify event will
        // ever fire for this removal. Only startup catch-up can converge it.
        std::fs::remove_file(&gone_path).unwrap();

        let handler = CodeFileHandler::new(Arc::clone(&facade), root.clone());
        let watcher = UnifiedWatcher::builder()
            .broadcaster(Arc::new(NotificationBroadcaster::new(16)))
            .indexer(Arc::clone(&facade))
            .workspace_root(root.clone())
            .handler(handler)
            .debounce_ms(0)
            .startup_catch_up(true)
            .build()
            .expect("builder has all required fields");

        let watch_task = tokio::spawn(watcher.watch());

        let deadline = Instant::now() + Duration::from_secs(30);
        let mut gone_absent = false;
        let mut seed_present_at_convergence = false;
        while Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let f = facade.read().await;
            let gone_found = f
                .find_symbols_by_name("gone_marker", None)
                .iter()
                .any(|s| s.name.as_ref() == "gone_marker");
            let seed_found = f
                .find_symbols_by_name("seed_marker", None)
                .iter()
                .any(|s| s.name.as_ref() == "seed_marker");
            // Both conditions must hold in this same sample (see the doc
            // comment above): a rebuild is only "converged" once the
            // deletion has taken effect AND the rest of the index has been
            // restored, not merely once the deletion has taken effect.
            if !gone_found && seed_found {
                gone_absent = true;
                seed_present_at_convergence = true;
                break;
            }
        }

        watch_task.abort();

        assert!(
            gone_absent,
            "symbols for a file deleted while the watcher was down must be \
             dropped by a startup catch-up reindex; still finding them means \
             the rebuild never ran or never reached this file"
        );
        assert!(
            seed_present_at_convergence,
            "seed_marker (positive control) must still be present at the \
             moment gone_marker disappears; its absence would mean the index \
             was wiped wholesale rather than genuinely rebuilt"
        );
    }
}
