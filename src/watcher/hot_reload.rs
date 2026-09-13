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
/// This watcher polls `meta.json` and `state.json` to detect when the index
/// is modified by external processes (e.g., `codanna index` in another terminal,
/// CI/CD pipelines). It does NOT watch source files - that's handled by UnifiedWatcher.
pub struct HotReloadWatcher {
    index_path: PathBuf,
    facade: Arc<RwLock<IndexFacade>>,
    settings: Arc<Settings>,
    persistence: IndexPersistence,
    last_modified: Option<SystemTime>,
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

        // Get initial modification time of the index metadata file. Read
        // through the already-constructed facade's own generation
        // directory (`gen/<id>/tantivy/meta.json`) rather than assuming a
        // flat `index_path/tantivy` layout, since the facade may already
        // be resolved to a generation.
        let meta_file_path = facade
            .try_read()
            .map(|f| f.generation_dir().join("tantivy").join("meta.json"))
            .unwrap_or_else(|_| index_path.join("tantivy").join("meta.json"));
        let last_modified = std::fs::metadata(&meta_file_path)
            .ok()
            .and_then(|meta| meta.modified().ok());

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
            last_modified,
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
        // the one this watcher's facade serves. The meta.json mtime poll
        // stays alongside it for this phase because external scoped writes
        // still mutate the served generation in place; Phase 3 drops it.
        let (served_generation, served_meta_path) = {
            let facade_guard = self.facade.read().await;
            (
                facade_guard.generation_id().clone(),
                facade_guard
                    .generation_dir()
                    .join("tantivy")
                    .join("meta.json"),
            )
        };
        let layout = IndexLayout::new(self.index_path.clone());
        let on_disk = layout.read_current()?;
        let generation_changed = on_disk.as_ref().is_some_and(|id| id != &served_generation);

        // Poll the generation the reload will open, so the recorded mtime
        // belongs to the facade that ends up serving.
        let meta_file_path = match (&on_disk, generation_changed) {
            (Some(id), true) => layout.tantivy_dir(id).join("meta.json"),
            _ => served_meta_path,
        };
        let current_modified = std::fs::metadata(&meta_file_path)?.modified()?;
        let mtime_changed = match self.last_modified {
            Some(last) => current_modified > last,
            None => true,
        };

        if !generation_changed && !mtime_changed {
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
            Ok(mut new_facade) => {
                // Get write lock and replace the facade. Carry the outgoing
                // facade's reindex gate into the replacement BEFORE
                // assigning it, so a permit held by an in-flight
                // `reindex_locked` call is still respected by callers that
                // read the gate handle after this swap (see the invariant
                // documented on `IndexFacade::reindex_gate`).
                let mut facade_guard = self.facade.write().await;
                new_facade.adopt_reindex_gate(facade_guard.reindex_gate());
                *facade_guard = new_facade;

                // Update last modified time
                self.last_modified = Some(current_modified);

                // Ensure semantic search stays attached after hot reloads
                let mut restored_semantic = false;
                if !facade_guard.has_semantic_search() && !facade_guard.is_semantic_incompatible() {
                    let semantic_path = facade_guard.semantic_dir();
                    let metadata_exists = semantic_path.join("metadata.json").exists();
                    if metadata_exists {
                        match facade_guard.load_semantic_search(&semantic_path) {
                            Ok(true) => {
                                restored_semantic = true;
                            }
                            Ok(false) => {
                                crate::debug_event!(
                                    "hot-reload",
                                    "semantic metadata present but reload returned false"
                                );
                            }
                            Err(crate::IndexError::SemanticSearch(
                                crate::semantic::SemanticSearchError::DimensionMismatch {
                                    ref suggestion,
                                    ..
                                },
                            )) => {
                                warn!(
                                    "Semantic index dimension mismatch after hot-reload: {suggestion}. \
                                     Semantic search disabled until re-indexed with --force."
                                );
                            }
                            Err(e) => {
                                warn!("Failed to reload semantic search after index update: {e}");
                            }
                        }
                    } else {
                        crate::debug_event!(
                            "hot-reload",
                            "semantic metadata missing",
                            "{}",
                            crate::parsing::paths::render_absolute_path(&semantic_path).display()
                        );
                    }
                }

                let symbol_count = facade_guard.symbol_count();
                let has_semantic = facade_guard.has_semantic_search();
                if restored_semantic {
                    let count = facade_guard.semantic_search_embedding_count();
                    crate::debug_event!("hot-reload", "restored semantic", "{count} embeddings");
                }
                crate::log_event!("hot-reload", "reloaded", "{symbol_count} symbols");
                crate::debug_event!("hot-reload", "semantic search", "{has_semantic}");

                // Send notification that index was reloaded
                if let Some(ref broadcaster) = self.broadcaster {
                    broadcaster.send(FileChangeEvent::IndexReloaded);
                    crate::debug_event!("hot-reload", "broadcast", "IndexReloaded");
                }

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
            last_modified: self.last_modified,
            index_path: self.index_path.clone(),
        }
    }
}

/// Statistics about the watched index.
#[derive(Debug, Clone)]
pub struct IndexStats {
    pub symbol_count: usize,
    pub last_modified: Option<SystemTime>,
    pub index_path: PathBuf,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Settings;
    use crate::storage::generation::Complete;
    use crate::storage::{DocumentIndex, EMISSION_SEMANTICS_VERSION, GenerationId, IndexMetadata};

    fn test_settings(index_path: PathBuf) -> Arc<Settings> {
        Arc::new(Settings {
            index_path,
            workspace_root: None,
            ..Default::default()
        })
    }

    // Advances the on-disk Tantivy index in place by indexing a small source
    // tree into it through a second `IndexFacade` over the same directory.
    // Kept as a standalone helper (rather than inlined in the test) so a
    // future phase can swap in a different reload trigger without touching
    // the permit-acquisition or post-swap assertion around it.
    fn advance_index_in_place(settings: &Arc<Settings>, workspace_root: &std::path::Path) {
        let mut writer_facade = IndexFacade::new(settings.clone()).unwrap();
        let source_root = workspace_root.join("src");
        std::fs::create_dir_all(&source_root).unwrap();
        std::fs::write(source_root.join("a.rs"), "fn a() {}\n").unwrap();
        writer_facade.index_directory(&source_root, false).unwrap();
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

    // Publish a second, empty generation under `settings.index_path` by
    // hand and point `current` at it, the way an external `codanna index`
    // will once builds are staged. Returns its id.
    fn publish_empty_generation(settings: &Arc<Settings>) -> GenerationId {
        let layout = IndexLayout::new(settings.index_path.clone());
        let id = GenerationId::generate();
        drop(DocumentIndex::new(layout.tantivy_dir(&id), settings).unwrap());
        let mut meta = IndexMetadata::new();
        meta.emission_version = Some(EMISSION_SEMANTICS_VERSION);
        meta.save(&layout.gen_dir(&id)).unwrap();
        Complete {
            id: id.clone(),
            parent: None,
            started_at: 0,
            completed_at: 0,
            builder_version: "0.0.0".to_string(),
            symbol_count: 0,
            file_count: 0,
            files: Vec::new(),
        }
        .write(&layout)
        .unwrap();
        layout.write_current(&id).unwrap();
        id
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
}
