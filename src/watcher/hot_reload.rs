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

        // Lock before loading: a commit through the active facade between
        // the load and the swap would be missing from the replacement, whose
        // stale id counters then reissue ids already in the index. The
        // active facade also owns the live settings: a settings reload
        // swaps in a facade over the merged root list, and the snapshot
        // taken at construction would revert it.
        let mut facade_guard = self.facade.write().await;
        match self
            .persistence
            .load_facade(Arc::clone(facade_guard.settings()))
        {
            Ok(new_facade) => {
                // Under the held write guard: adopt the reindex gate, re-attach
                // semantic state, and broadcast. `swap_in` would re-take the
                // lock and deadlock (tokio RwLock is not re-entrant).
                IndexFacade::swap_in_locked(
                    &mut facade_guard,
                    new_facade,
                    self.broadcaster.as_deref(),
                );
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
    use std::collections::HashSet;

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

    // Publish a new generation cloned from the current one, so the on-disk
    // `current` pointer flips away from the generation a facade serves. This
    // is the reload trigger: an in-place file mutation no longer reloads.
    fn publish_clone_of_current(settings: &Arc<Settings>) -> GenerationId {
        let persistence = IndexPersistence::new(settings.index_path.clone());
        let build = persistence
            .open_build(settings.clone(), BuildMode::CloneCurrent)
            .expect("open clone-current build");
        persistence.publish(build).expect("publish new generation")
    }

    fn write_pair(dir: &std::path::Path, name: &str) -> PathBuf {
        let path = dir.join(format!("{name}.py"));
        std::fs::write(
            &path,
            format!("def {name}_helper():\n    return 1\n\n\ndef {name}():\n    return {name}_helper()\n"),
        )
        .unwrap();
        path
    }

    // A same-process commit made while a writer holds the facade must be in
    // the replacement the reload swaps in, and ids stay unique afterwards.
    // Scope: the reload parks on its initial `read()` of the facade behind
    // the held write guard, so this does not isolate the "write guard held
    // across `load_facade`" ordering (a load-before-lock regression would
    // pass); that ordering cannot be pinned deterministically because any
    // commit needs the write lock the reload is queued on.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reload_after_a_writer_commit_keeps_symbol_ids_unique() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("src");
        std::fs::create_dir_all(&root).unwrap();
        let mut settings = Settings {
            index_path: dir.path().join("index"),
            workspace_root: None,
            ..Default::default()
        };
        settings.add_indexed_path(root.clone()).unwrap();
        let settings = Arc::new(settings);
        let mut facade = IndexFacade::new(Arc::clone(&settings)).unwrap();
        facade.index_file(write_pair(&root, "a")).unwrap();
        let facade = Arc::new(RwLock::new(facade));
        let mut watcher = HotReloadWatcher::new(
            Arc::clone(&facade),
            Arc::clone(&settings),
            Duration::from_secs(3600),
        );

        let mut writer = facade.write().await;
        let reload =
            tokio::spawn(
                async move { watcher.check_and_reload().await.map_err(|e| e.to_string()) },
            );
        writer.index_file(write_pair(&root, "b")).unwrap();
        // Flip `current` to a generation holding both commits so the waiting
        // reload has something to swap to once the writer is released.
        publish_clone_of_current(&settings);
        drop(writer);
        reload.await.unwrap().unwrap();
        facade
            .write()
            .await
            .index_file(write_pair(&root, "c"))
            .unwrap();

        let facade = facade.read().await;
        let symbols = facade.get_all_symbols();
        let ids: HashSet<_> = symbols.iter().map(|s| s.id).collect();
        assert_eq!(
            ids.len(),
            symbols.len(),
            "duplicate symbol ids: {symbols:?}"
        );
        for name in ["a", "b", "c"] {
            let caller = facade.find_symbols_by_name(name, None);
            assert_eq!(caller.len(), 1, "{name}");
            let callees: Vec<String> = facade
                .get_called_functions(caller[0].id)
                .iter()
                .map(|s| s.name.to_string())
                .collect();
            assert_eq!(callees, vec![format!("{name}_helper")], "{name}");
        }
    }

    // A settings reload swaps in a facade over the merged root list and
    // its own commit lands in the served generation. The reloader must
    // rebuild from that facade's settings, not from the snapshot captured
    // at construction, or the next tick reverts the merge.
    #[tokio::test]
    async fn reload_keeps_the_active_facade_settings() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().canonicalize().unwrap();
        let root_a = workspace.join("a");
        let root_b = workspace.join("b");
        std::fs::create_dir_all(&root_a).unwrap();
        std::fs::create_dir_all(&root_b).unwrap();
        let index_path = workspace.join("index");
        let mut settings = Settings {
            index_path: index_path.clone(),
            workspace_root: None,
            ..Default::default()
        };
        settings.add_indexed_path(root_a.clone()).unwrap();
        let startup = Arc::new(settings);
        let mut facade = IndexFacade::new(Arc::clone(&startup)).unwrap();
        facade.index_file(write_pair(&root_a, "a")).unwrap();
        let facade = Arc::new(RwLock::new(facade));
        let mut watcher = HotReloadWatcher::new(
            Arc::clone(&facade),
            Arc::clone(&startup),
            Duration::from_secs(3600),
        );

        let mut merged = Settings::clone(&startup);
        merged.add_indexed_path(root_b.clone()).unwrap();
        let merged = Arc::new(merged);
        {
            let mut guard = facade.write().await;
            *guard = IndexPersistence::new(index_path)
                .load_facade(Arc::clone(&merged))
                .unwrap();
            guard.index_file(write_pair(&root_b, "b")).unwrap();
        }
        publish_clone_of_current(&merged);

        watcher.check_and_reload().await.unwrap();

        let facade = facade.read().await;
        assert!(
            facade.settings().indexed_paths_cache.contains(&root_b),
            "reload reverted to the startup roots: {:?}",
            facade.settings().indexed_paths_cache
        );
        assert_eq!(facade.find_symbols_by_name("b", None).len(), 1);
    }
}
