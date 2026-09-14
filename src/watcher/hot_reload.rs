//! Hot-reload watcher for external index changes.
//!
//! Polls for changes to the index made by external processes (CI/CD, other terminals)
//! and hot-reloads them without restarting the server.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::RwLock;
use tokio::time::interval;
use tracing::{debug, info, warn};

use crate::indexing::facade::IndexFacade;
use crate::mcp::notifications::{FileChangeEvent, NotificationBroadcaster};
use crate::storage::IndexLayout;
use crate::{IndexPersistence, Settings};

/// Watches for external index changes and hot-reloads them.
///
/// This watcher compares the on-disk `current` generation pointer against the
/// generation its facade currently serves, and polls `state.json` to detect
/// when the document store is modified by external processes (e.g., `codanna
/// index` in another terminal, CI/CD pipelines). It does NOT watch source
/// files - that's handled by UnifiedWatcher.
pub struct HotReloadWatcher {
    index_path: PathBuf,
    facade: Arc<RwLock<IndexFacade>>,
    settings: Arc<Settings>,
    persistence: IndexPersistence,
    last_doc_modified: Option<SystemTime>,
    check_interval: Duration,
    broadcaster: Option<Arc<NotificationBroadcaster>>,
}

impl HotReloadWatcher {
    /// Create a new hot-reload watcher.
    pub fn new(
        facade: Arc<RwLock<IndexFacade>>,
        settings: Arc<Settings>,
        check_interval: Duration,
    ) -> Self {
        let index_path = settings.index_path.clone();
        let persistence = IndexPersistence::new(index_path.clone());

        // Get initial modification time of document store state.json
        let doc_state_path = index_path.join("documents").join("state.json");
        let last_doc_modified = std::fs::metadata(&doc_state_path)
            .ok()
            .and_then(|meta| meta.modified().ok());

        Self {
            index_path,
            facade,
            settings,
            persistence,
            last_doc_modified,
            check_interval,
            broadcaster: None,
        }
    }

    /// Set the notification broadcaster.
    pub fn with_broadcaster(mut self, broadcaster: Arc<NotificationBroadcaster>) -> Self {
        self.broadcaster = Some(broadcaster);
        self
    }

    /// Start watching for external index changes.
    pub async fn watch(mut self) {
        let mut ticker = interval(self.check_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            ticker.tick().await;

            if let Err(e) = self.check_and_reload().await {
                tracing::error!("Error checking/reloading index: {e}");
            }
        }
    }

    /// Check if the index has been modified externally and reload if necessary.
    async fn check_and_reload(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        // Check for document store changes (state.json modified externally)
        self.check_document_changes();

        // Check if index file exists
        if !self.persistence.exists() {
            debug!("Index file does not exist at {:?}", self.index_path);
            return Ok(());
        }

        // An external publish flips `current` to a generation other than
        // the one this watcher's facade serves; that flip alone drives the
        // reload.
        let served_generation = {
            let facade_guard = self.facade.read().await;
            facade_guard.generation_id().clone()
        };
        let layout = IndexLayout::new(self.index_path.clone());
        let on_disk = layout.read_current()?;
        let generation_changed = on_disk.as_ref().is_some_and(|id| id != &served_generation);

        if !generation_changed {
            tracing::trace!("Index file unchanged");
            return Ok(());
        }

        crate::log_event!(
            "hot-reload",
            "reloading",
            "{}",
            crate::parsing::paths::render_absolute_path(&self.index_path).display()
        );

        // Load the new index as a facade
        match self.persistence.load_facade(self.settings.clone()) {
            Ok(new_facade) => {
                IndexFacade::swap_in(&self.facade, new_facade, self.broadcaster.as_deref()).await;
                Ok(())
            }
            Err(e) => {
                warn!("Failed to reload index: {e}");
                Err(Box::new(std::io::Error::other(format!(
                    "Failed to reload index: {e}"
                ))))
            }
        }
    }

    /// Check if document store state.json has changed (documents indexed externally).
    fn check_document_changes(&mut self) {
        let doc_state_path = self.index_path.join("documents").join("state.json");

        // Get current modification time
        let current_modified = match std::fs::metadata(&doc_state_path) {
            Ok(meta) => match meta.modified() {
                Ok(time) => time,
                Err(_) => return,
            },
            Err(_) => return,
        };

        // Check if changed
        let changed = match self.last_doc_modified {
            Some(last) => current_modified > last,
            None => true,
        };

        if changed {
            self.last_doc_modified = Some(current_modified);
            info!("Document store changed, notifying watchers");

            // Send IndexReloaded to refresh document handler's watched files
            if let Some(ref broadcaster) = self.broadcaster {
                broadcaster.send(FileChangeEvent::IndexReloaded);
            }
        }
    }

    /// Get current index statistics.
    pub async fn get_stats(&self) -> IndexStats {
        let indexer = self.facade.read().await;
        IndexStats {
            symbol_count: indexer.symbol_count(),
            index_path: self.index_path.clone(),
        }
    }
}

/// Statistics about the watched index.
#[derive(Debug, Clone)]
pub struct IndexStats {
    pub symbol_count: usize,
    pub index_path: PathBuf,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Settings;
    use crate::storage::GenerationId;
    use crate::storage::persistence::BuildMode;

    fn test_settings(index_path: PathBuf) -> Arc<Settings> {
        Arc::new(Settings {
            index_path,
            workspace_root: None,
            ..Default::default()
        })
    }

    // Advances the on-disk index by publishing a new generation cloned from
    // `current`, so `check_and_reload` observes a real `current` flip
    // instead of an in-place file mutation. Kept as a standalone helper
    // (rather than inlined in the test) so a future phase can swap in a
    // different reload trigger without touching the permit-acquisition or
    // post-swap assertion around it.
    fn advance_index_in_place(settings: &Arc<Settings>, workspace_root: &std::path::Path) {
        let persistence = IndexPersistence::new(settings.index_path.clone());
        let mut build = persistence
            .open_build(settings.clone(), BuildMode::CloneCurrent)
            .expect("open clone-current build");
        let source_root = workspace_root.join("src");
        std::fs::create_dir_all(&source_root).unwrap();
        std::fs::write(source_root.join("a.rs"), "fn a() {}\n").unwrap();
        build.index_directory(&source_root, false).unwrap();
        persistence.publish(build).expect("publish new generation");
    }

    // Regression for the hot-reload facade-swap race: this drives the real
    // `HotReloadWatcher::check_and_reload` wiring end-to-end (real on-disk
    // Tantivy index, real reload), not just the `adopt_reindex_gate`
    // primitive in isolation. It fails against a build where the gate
    // carry-over call above is missing or reordered after the assignment.
    #[tokio::test]
    async fn check_and_reload_preserves_permit_held_across_swap() {
        let dir = tempfile::tempdir().unwrap();
        let settings = test_settings(dir.path().join("index"));

        let facade = Arc::new(RwLock::new(IndexFacade::new(settings.clone()).unwrap()));
        let mut watcher =
            HotReloadWatcher::new(facade.clone(), settings.clone(), Duration::from_secs(3600));

        // Simulate an in-flight `reindex_locked` call holding the permit
        // against the facade that is about to be replaced by the reload.
        let held_permit = {
            let indexer = facade.read().await;
            indexer.reindex_gate()
        };
        let _permit = held_permit.try_acquire_owned().unwrap();

        // Advance the on-disk index (via a second facade over the same
        // Tantivy directory) so `check_and_reload` observes a newer
        // meta.json and actually performs the reload/swap below.
        advance_index_in_place(&settings, dir.path());

        watcher
            .check_and_reload()
            .await
            .expect("check_and_reload should reload the on-disk index");

        // A concurrent caller reading the gate handle after the swap must
        // still observe the permit as held.
        let gate_after_swap = {
            let indexer = facade.read().await;
            indexer.reindex_gate()
        };
        assert!(
            gate_after_swap.try_acquire_owned().is_err(),
            "permit held before the hot-reload swap must still gate callers after it"
        );
    }

    // Publish a second, empty generation under `settings.index_path` and
    // point `current` at it, the way an external `codanna index` will once
    // builds are staged. Returns its id.
    fn publish_empty_generation(settings: &Arc<Settings>) -> GenerationId {
        let persistence = IndexPersistence::new(settings.index_path.clone());
        let build = persistence
            .open_build(settings.clone(), BuildMode::Fresh)
            .expect("open fresh build");
        persistence.publish(build).expect("publish new generation")
    }

    // When `current` still names the served generation and meta.json is
    // untouched, a tick must neither swap the facade nor broadcast.
    #[tokio::test]
    async fn check_and_reload_is_a_no_op_when_current_matches_served_generation() {
        let dir = tempfile::tempdir().unwrap();
        let settings = test_settings(dir.path().join("index"));
        let facade = Arc::new(RwLock::new(IndexFacade::new(settings.clone()).unwrap()));
        let broadcaster = Arc::new(NotificationBroadcaster::new(8));
        let mut events = broadcaster.subscribe();
        let mut watcher =
            HotReloadWatcher::new(facade.clone(), settings.clone(), Duration::from_secs(3600))
                .with_broadcaster(broadcaster);
        let served = facade.read().await.generation_id().clone();
        assert_eq!(
            IndexLayout::new(settings.index_path.clone())
                .read_current()
                .unwrap(),
            Some(served.clone()),
            "the bootstrap publishes the served generation"
        );

        watcher.check_and_reload().await.expect("tick");

        assert_eq!(facade.read().await.generation_id(), &served);
        assert!(
            events.try_recv().is_err(),
            "a no-op tick must not broadcast IndexReloaded"
        );
    }

    // When `current` names a different published generation, a tick swaps
    // the facade to it even though the served generation's meta.json never
    // changed.
    #[tokio::test]
    async fn check_and_reload_swaps_when_current_names_another_generation() {
        let dir = tempfile::tempdir().unwrap();
        let settings = test_settings(dir.path().join("index"));
        let facade = Arc::new(RwLock::new(IndexFacade::new(settings.clone()).unwrap()));
        let broadcaster = Arc::new(NotificationBroadcaster::new(8));
        let mut events = broadcaster.subscribe();
        let mut watcher =
            HotReloadWatcher::new(facade.clone(), settings.clone(), Duration::from_secs(3600))
                .with_broadcaster(broadcaster);
        let old = facade.read().await.generation_id().clone();

        let published = publish_empty_generation(&settings);
        assert_ne!(old, published);

        watcher.check_and_reload().await.expect("tick");

        assert_eq!(facade.read().await.generation_id(), &published);
        assert!(
            matches!(events.try_recv(), Ok(FileChangeEvent::IndexReloaded)),
            "a generation flip must broadcast IndexReloaded"
        );
    }

    // No-double-fire regression: once an in-process `reindex_locked(paths:
    // None, force: true, ...)` call has published-and-swapped a new
    // generation into the SAME facade a `HotReloadWatcher` is watching, the
    // on-disk `current` pointer already names the facade's new generation.
    // A subsequent `check_and_reload` tick must therefore be a genuine
    // no-op -- no second swap, no second broadcast -- reusing the
    // assertion style of
    // `check_and_reload_is_a_no_op_when_current_matches_served_generation`
    // above. This guards against a regression where the in-process publish
    // path and the hot-reload watcher's `current`-pointer check could
    // disagree about what "already served" means, causing every reindex to
    // be immediately followed by a redundant reload.
    #[tokio::test]
    async fn reindex_locked_publish_leaves_check_and_reload_a_no_op() {
        // See `crate::mcp::server::REINDEX_TEST_SERIAL`: this test drives a
        // real `reindex_locked` walk and must not race `mcp::server`'s own
        // reindex-driving tests over the shared process-wide
        // failure-injection flag.
        let _serial_guard = crate::mcp::server::REINDEX_TEST_SERIAL.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let index_path = dir.path().join("index");
        let source_dir = dir.path().join("workspace").join("src");
        std::fs::create_dir_all(&source_dir).unwrap();
        std::fs::write(source_dir.join("a.rs"), "pub fn a() {}\n").unwrap();

        let mut settings = Settings {
            index_path: index_path.clone(),
            workspace_root: None,
            ..Default::default()
        };
        settings.indexing.indexed_paths = vec![source_dir.clone()];
        let settings = Arc::new(settings);

        let facade = Arc::new(RwLock::new(IndexFacade::new(settings.clone()).unwrap()));
        let broadcaster = Arc::new(NotificationBroadcaster::new(8));
        let mut events = broadcaster.subscribe();
        let mut watcher =
            HotReloadWatcher::new(facade.clone(), settings.clone(), Duration::from_secs(3600))
                .with_broadcaster(broadcaster);

        // Drive a real in-process full reindex through the same seam the
        // MCP server uses (`reindex_locked`), letting it build, publish,
        // and swap the new generation into `facade`.
        crate::indexing::reindex_locked(&facade, None, true, None, None, None)
            .await
            .expect("in-process force reindex must succeed");

        let published_generation = facade.read().await.generation_id().clone();
        let layout = IndexLayout::new(index_path);
        assert_eq!(
            layout.read_current().unwrap(),
            Some(published_generation.clone()),
            "an in-process publish must flip the on-disk `current` pointer to the facade's new \
             generation before check_and_reload's next tick observes it"
        );

        watcher
            .check_and_reload()
            .await
            .expect("tick after in-process publish");

        assert_eq!(facade.read().await.generation_id(), &published_generation);
        assert!(
            events.try_recv().is_err(),
            "a tick immediately after an in-process publish must be a no-op: no second swap, \
             no second broadcast"
        );
    }
}
