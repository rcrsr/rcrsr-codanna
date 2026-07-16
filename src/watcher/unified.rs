//! Unified file watcher that routes events to pluggable handlers.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use notify::{Event, EventKind, RecursiveMode, Watcher};
use tokio::sync::{RwLock, mpsc};
use tokio::time::{Duration, sleep};

use crate::documents::DocumentStore;
use crate::documents::config::ChunkingConfig;
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
                    let ready = self.debouncer.take_ready();
                    for path in ready {
                        self.process_modification(&path).await;
                    }

                    // After draining debounced work, check whether the quiet
                    // window for a prior overflow/rescan signal has elapsed.
                    // If so, fire exactly one catch-up reindex.
                    if let Some(since) = self.stale_since {
                        let window = self.debounce_window;
                        if should_catch_up(
                            self.stale,
                            self.debouncer.has_pending(),
                            since.elapsed(),
                            window,
                        ) {
                            crate::log_event!(
                                "watcher",
                                "catch-up reindex",
                                "quiet window elapsed after overflow/rescan; reindexing"
                            );

                            let result = crate::indexing::facade::reindex_locked(
                                &self.facade,
                                None,
                                true,
                                None,
                            )
                            .await
                            .map_err(|source| WatchError::CatchUpReindexFailed { source });

                            match result {
                                Ok(outcome) => {
                                    crate::log_event!(
                                        "watcher",
                                        "catch-up reindex complete",
                                        "{} files reindexed, {} symbols",
                                        outcome.reindexed,
                                        outcome.symbol_count
                                    );
                                    self.broadcaster.send(FileChangeEvent::IndexReloaded);
                                }
                                Err(e) => {
                                    tracing::error!("[watcher] {e}");
                                }
                            }

                            // Fire exactly once per overflow episode, whether
                            // the catch-up reindex succeeded or failed.
                            self.stale = false;
                            self.stale_since = None;
                        }
                    }
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

        for path in event.paths {
            // Check if any handler cares about this path
            let matched = self.handlers.iter().any(|h| h.matches(&path));
            if !matched {
                crate::trace_event!(
                    "watcher",
                    "unmatched",
                    "{:?} {}",
                    event.kind,
                    path.display()
                );
                continue;
            }

            match event.kind {
                EventKind::Modify(_) => {
                    // Debounce modifications
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
        // catch-up reindex rather than firing mid-burst.
        self.bump_stale_clock();
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

        crate::log_event!(
            "watcher",
            "watching",
            "{} files in {} directories",
            self.registry.path_count(),
            self.registry.dir_count()
        );
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
            refresh_on_overflow: false,
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

    /// Build a minimal real `UnifiedWatcher` against a temp-dir-backed index,
    /// so `handle_event` can be exercised directly instead of re-simulating
    /// its branching logic.
    fn test_watcher(tempdir: &tempfile::TempDir) -> UnifiedWatcher {
        use crate::config::Settings;
        use crate::indexing::facade::IndexFacade;

        let settings = Settings {
            index_path: tempdir.path().to_path_buf(),
            workspace_root: None,
            ..Default::default()
        };
        let facade = IndexFacade::new(std::sync::Arc::new(settings))
            .expect("facade construction against a fresh temp dir must succeed");

        UnifiedWatcher::builder()
            .broadcaster(Arc::new(NotificationBroadcaster::new(16)))
            .indexer(Arc::new(RwLock::new(facade)))
            .workspace_root(tempdir.path().to_path_buf())
            .build()
            .expect("builder has all required fields")
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
}
