//! Simplified persistence layer for Tantivy-only storage
//!
//! This module manages metadata and ensures Tantivy index exists.
//! All actual data is stored in Tantivy.

use crate::indexing::facade::IndexFacade;
use crate::indexing::walk_config;
use crate::storage::generation::{GenerationId, migrate_flat_layout, resolve_current};
use crate::storage::{DataSource, IndexLayout, IndexMetadata};
use crate::{IndexError, IndexResult, Settings};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Root used for ignore-fingerprint file lookups (`.gitignore`,
/// `.codannaignore`, `.git/info/exclude`): the workspace root when known,
/// otherwise `fallback` (the actual indexed root, when the caller has one),
/// otherwise the current directory.
///
/// [`walk_config::build_walker`]/[`walk_config::ignore_fingerprint`] fall
/// back to the actual walk root (the directory being indexed) rather than
/// the process CWD when `workspace_root` is unset; passing the caller's
/// best-known indexed root as `fallback` keeps this in step with that,
/// instead of confidently fingerprinting the wrong directory.
fn ignore_fingerprint_root(settings: &Settings, fallback: Option<&Path>) -> PathBuf {
    settings
        .workspace_root
        .clone()
        .or_else(|| fallback.map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Manages persistence of the index
#[derive(Debug)]
pub struct IndexPersistence {
    layout: IndexLayout,
}

impl IndexPersistence {
    /// Create a new persistence manager
    pub fn new(base_path: PathBuf) -> Self {
        Self {
            layout: IndexLayout::new(base_path),
        }
    }

    /// Migrate a legacy flat layout (if present), then resolve which
    /// generation `current` names. Every entry point that answers a
    /// question about the on-disk index goes through this, so the
    /// migrate-then-resolve sequence lives in exactly one place and a probe
    /// (`exists`, `current_metadata`) sees the same generation a subsequent
    /// load opens.
    fn resolve_generation(&self) -> IndexResult<Option<GenerationId>> {
        migrate_flat_layout(&self.layout)?;
        resolve_current(&self.layout)
    }

    // =========================================================================
    // IndexFacade Persistence Methods
    // =========================================================================

    /// Load an IndexFacade from disk
    #[must_use = "Load errors should be handled appropriately"]
    pub fn load_facade(&self, settings: Arc<Settings>) -> IndexResult<IndexFacade> {
        self.load_facade_impl(settings, true)
    }

    /// Load an IndexFacade without semantic search (faster for text-only queries)
    ///
    /// Use this for commands that only need Tantivy text search (e.g., retrieve).
    #[must_use = "Load errors should be handled appropriately"]
    pub fn load_facade_lite(&self, settings: Arc<Settings>) -> IndexResult<IndexFacade> {
        self.load_facade_impl(settings, false)
    }

    /// Save `metadata` as `index.meta` inside `gen_dir` (a generation
    /// directory under this persistence's [`IndexLayout`]), then
    /// best-effort refresh the project registry.
    fn persist_metadata(&self, metadata: &IndexMetadata, gen_dir: &Path) -> IndexResult<()> {
        metadata.save(gen_dir)?;

        if let Err(err) = self.update_project_registry(metadata) {
            tracing::debug!(
                target: "persistence",
                "Skipped project registry update: {err}"
            );
        }

        Ok(())
    }

    /// Internal implementation with configurable semantic search loading
    fn load_facade_impl(
        &self,
        settings: Arc<Settings>,
        load_semantic: bool,
    ) -> IndexResult<IndexFacade> {
        // Migrate a legacy flat layout (if present) and resolve which
        // generation `current` names, recovering from a missing/torn
        // pointer the same way `IndexFacade::new` would.
        let id = self
            .resolve_generation()?
            .ok_or_else(|| IndexError::GenerationNotFound {
                id: "current".to_string(),
            })?;
        let gen_dir = self.layout.gen_dir(&id);

        // Load metadata to understand data sources
        let metadata = IndexMetadata::load(&gen_dir).ok();

        // Detect-and-report staleness (issue #28) is surfaced on demand via
        // `mcp::service::ignore_rules_changed`/`get_index_info`, not here:
        // computing the fingerprint at every load just to emit a log-only
        // warning duplicated that work (an extra `index.meta` read plus
        // SHA256-of-3-files) for no externally visible effect beyond a
        // `tracing::warn!` line.

        // Open the already-resolved generation directly, rather than
        // re-deriving it via `IndexFacade::new` (which would re-run
        // migration/resolution a second time).
        let mut facade = IndexFacade::open(settings, self.layout.clone(), id.clone())?;

        // Display source info with fresh counts
        if let Some(ref meta) = metadata {
            let fresh_symbol_count = facade.symbol_count();
            let fresh_file_count = facade.file_count();

            match &meta.data_source {
                DataSource::Tantivy {
                    path, doc_count, ..
                } => {
                    tracing::info!(
                        "[persistence] loaded facade from Tantivy index: {} ({} documents)",
                        crate::parsing::paths::render_absolute_path(path).display(),
                        doc_count
                    );
                }
                DataSource::Fresh => {
                    tracing::info!("[persistence] created fresh facade");
                }
            }
            tracing::info!(
                "[persistence] facade contains {fresh_symbol_count} symbols from {fresh_file_count} files"
            );
        }

        // Load semantic search if available and requested
        let semantic_dir = self.layout.semantic_dir(&id);
        if load_semantic {
            tracing::debug!(
                "[persistence] semantic path computed as: {}",
                crate::parsing::paths::render_absolute_path(&semantic_dir).display()
            );
            match facade.load_semantic_search(&semantic_dir) {
                Ok(true) => {
                    tracing::debug!("[persistence] loaded semantic search for facade");
                }
                Ok(false) => {
                    tracing::debug!("[persistence] no semantic data found (this is optional)");
                }
                Err(IndexError::SemanticSearch(
                    crate::semantic::SemanticSearchError::DimensionMismatch {
                        ref suggestion, ..
                    },
                )) => {
                    // Semantic index is structurally incompatible with the current backend.
                    // Log at error level so it is visible, but continue without semantic
                    // search rather than failing the whole facade load and discarding the
                    // valid text index.
                    tracing::error!(
                        "[persistence] semantic search disabled — index incompatible: {suggestion}"
                    );
                }
                Err(e) => {
                    tracing::warn!("[persistence] failed to load semantic search: {e}");
                }
            }
        } else {
            tracing::debug!("[persistence] skipping semantic search (lite mode)");
            if semantic_dir.join("metadata.json").exists() {
                match facade.load_semantic_metadata_snapshot(&semantic_dir) {
                    Ok(true) => {
                        tracing::debug!(
                            "[persistence] loaded semantic metadata snapshot for lite facade"
                        );
                    }
                    Ok(false) => {}
                    Err(e) => {
                        tracing::warn!(
                            "[persistence] failed to load semantic metadata snapshot: {e}"
                        );
                    }
                }
            }
        }

        // Restore indexed_paths from metadata
        if let Some(ref meta) = metadata {
            if let Some(ref stored_paths) = meta.indexed_paths {
                facade.set_indexed_paths(stored_paths.clone());
                tracing::debug!(
                    "[persistence] restored {} indexed paths from metadata",
                    stored_paths.len()
                );
            }
        }

        Ok(facade)
    }

    /// Save metadata for an IndexFacade
    #[must_use = "Save errors should be handled to ensure data is persisted"]
    pub fn save_facade(&self, facade: &IndexFacade) -> IndexResult<()> {
        let gen_dir = facade.generation_dir();

        // Update metadata
        let mut metadata = IndexMetadata::load(&gen_dir).unwrap_or_else(|_| IndexMetadata::new());

        metadata.update_counts(facade.symbol_count() as u32, facade.file_count());
        // The gate upstream guarantees an existing index only reaches an
        // incremental save when its stamp already matches; a fresh seed is
        // this binary's output by construction. Stamping here is truthful
        // in both cases.
        metadata.emission_version = Some(crate::storage::metadata::EMISSION_SEMANTICS_VERSION);
        metadata.builder_commit = crate::storage::metadata::builder_commit().map(str::to_string);

        // Update indexed paths for sync detection on next load
        let indexed_paths: Vec<PathBuf> = facade.get_indexed_paths().iter().cloned().collect();
        tracing::debug!(
            "[persistence] saving {} indexed paths to metadata",
            indexed_paths.len()
        );
        // The first indexed directory is the closest available stand-in for
        // "the actual walk root" when `workspace_root` is unset, mirroring
        // `build_walker`'s own fallback more closely than the process CWD.
        let root_fallback = indexed_paths.first().cloned();
        metadata.update_indexed_paths(indexed_paths);

        // Record the ignore-rule fingerprint for staleness detection on next
        // load (issue #28, detect-and-report only). A computation failure
        // (e.g. an unreadable ignore file) is logged and otherwise
        // non-fatal: the save still succeeds, and the field is simply left
        // at its previous value, which loaders already treat as "unknown"
        // rather than "changed" when absent.
        let root = ignore_fingerprint_root(facade.settings(), root_fallback.as_deref());
        match walk_config::ignore_fingerprint(facade.settings(), &root) {
            Ok(fingerprint) => metadata.update_ignore_fingerprint(fingerprint),
            Err(e) => {
                tracing::warn!("[persistence] failed to compute ignore fingerprint: {e}");
            }
        }

        // Update metadata to reflect Tantivy. The recorded path is relative
        // to the generation directory it lives in: only a `tracing::info!`
        // line in `load_facade_impl` renders it (for a human-readable log),
        // so a relative path is both safe and serde-compatible with readers
        // built against the pre-generation flat-layout `index.meta` format.
        metadata.data_source = DataSource::Tantivy {
            path: PathBuf::from("tantivy"),
            doc_count: facade.document_count().unwrap_or(0),
            timestamp: crate::indexing::get_utc_timestamp(),
        };

        self.persist_metadata(&metadata, &gen_dir)?;

        // Save semantic search if enabled
        if facade.has_semantic_search() {
            let semantic_path = facade.semantic_dir();
            std::fs::create_dir_all(&semantic_path).map_err(|e| {
                IndexError::General(format!("Failed to create semantic directory: {e}"))
            })?;

            facade
                .save_semantic_search(&semantic_path)
                .map_err(|e| IndexError::General(format!("Failed to save semantic search: {e}")))?;
        }

        Ok(())
    }

    /// Check if an index exists: whether `current` resolves to a valid
    /// generation under this persistence's [`IndexLayout`] (after migrating
    /// a legacy flat layout).
    pub fn exists(&self) -> bool {
        self.resolve_generation().ok().flatten().is_some()
    }

    /// The current generation's `index.meta`, or `None` when no generation
    /// resolves or the file fails to parse. Callers that only need the
    /// metadata (the emission gate, config sync, `dump`) use this instead of
    /// reading `index.meta` off the index root, which no longer holds one.
    pub fn current_metadata(&self) -> Option<IndexMetadata> {
        let id = self.resolve_generation().ok().flatten()?;
        IndexMetadata::load(&self.layout.gen_dir(&id)).ok()
    }

    /// Update the project registry with latest metadata
    fn update_project_registry(&self, metadata: &IndexMetadata) -> IndexResult<()> {
        // Try to read the project ID file
        let local_dir = crate::init::local_dir_name();
        let project_id_path = PathBuf::from(local_dir).join(".project-id");

        if !project_id_path.exists() {
            // No project ID file means project wasn't registered during init
            // This is fine for legacy projects
            return Ok(());
        }

        let project_id =
            std::fs::read_to_string(&project_id_path).map_err(|e| IndexError::FileRead {
                path: project_id_path.clone(),
                source: e,
            })?;

        // Load the registry
        let mut registry = crate::init::ProjectRegistry::load()
            .map_err(|e| IndexError::General(format!("Failed to load project registry: {e}")))?;

        // Update the project metadata
        if let Some(project) = registry.find_project_by_id_mut(&project_id) {
            project.symbol_count = metadata.symbol_count;
            project.file_count = metadata.file_count;
            project.last_modified = metadata.last_modified;

            // Get doc count from data source
            if let DataSource::Tantivy { doc_count, .. } = &metadata.data_source {
                project.doc_count = *doc_count;
            }

            // Save the updated registry
            registry.save().map_err(|e| {
                IndexError::General(format!("Failed to save project registry: {e}"))
            })?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::semantic::SemanticMetadata;
    use tempfile::TempDir;

    fn settings_for(temp_dir: &TempDir) -> Arc<Settings> {
        Arc::new(Settings {
            index_path: temp_dir.path().to_path_buf(),
            ..Settings::default()
        })
    }

    #[test]
    fn exists_is_false_on_an_empty_root() {
        let temp_dir = TempDir::new().unwrap();
        let persistence = IndexPersistence::new(temp_dir.path().to_path_buf());

        assert!(!persistence.exists());
    }

    #[test]
    fn exists_is_true_after_a_publish() {
        let temp_dir = TempDir::new().unwrap();
        let persistence = IndexPersistence::new(temp_dir.path().to_path_buf());
        let settings = settings_for(&temp_dir);

        // Bootstrapping a facade over an empty root allocates a fresh
        // generation and publishes it via `IndexLayout::write_current`.
        let facade = IndexFacade::new(settings).unwrap();
        persistence.save_facade(&facade).unwrap();

        assert!(persistence.exists());
    }

    #[test]
    fn save_then_load_round_trips_through_a_generation() {
        let temp_dir = TempDir::new().unwrap();
        let persistence = IndexPersistence::new(temp_dir.path().to_path_buf());
        let settings = settings_for(&temp_dir);

        let facade = IndexFacade::new(settings.clone()).unwrap();
        let generation_id = facade.generation_id().clone();
        persistence.save_facade(&facade).unwrap();

        // `index.meta` lives under the generation directory, never
        // directly under the persistence root.
        let gen_dir = facade.generation_dir();
        let meta_path = gen_dir.join("index.meta");
        assert!(meta_path.is_file());

        // The saved `data_source` path is relative to the generation
        // directory, not an absolute path baked in at save time.
        let saved_metadata = IndexMetadata::load(&gen_dir).unwrap();
        match saved_metadata.data_source {
            DataSource::Tantivy { path, .. } => {
                assert_eq!(path, PathBuf::from("tantivy"));
                assert!(path.is_relative());
            }
            DataSource::Fresh => panic!("expected DataSource::Tantivy after save_facade"),
        }

        let loaded = persistence.load_facade_lite(settings).unwrap();
        assert_eq!(loaded.generation_id(), &generation_id);
        assert_eq!(loaded.symbol_count(), facade.symbol_count());
        assert_eq!(loaded.file_count(), facade.file_count());
    }

    #[test]
    fn load_facade_lite_preserves_semantic_metadata_snapshot() {
        let temp_dir = TempDir::new().unwrap();
        let persistence = IndexPersistence::new(temp_dir.path().to_path_buf());
        let settings = settings_for(&temp_dir);

        let facade = IndexFacade::new(settings.clone()).unwrap();
        persistence.save_facade(&facade).unwrap();

        let semantic_path = facade.semantic_dir();
        std::fs::create_dir_all(&semantic_path).unwrap();
        let metadata =
            SemanticMetadata::new_remote("snowflake-arctic-embed:latest".to_string(), 1024, 42);
        metadata.save(&semantic_path).unwrap();

        let loaded = persistence.load_facade_lite(settings).unwrap();

        let snapshot = loaded
            .get_semantic_metadata()
            .expect("snapshot should load in lite mode");

        assert_eq!(snapshot.backend, metadata.backend);
        assert_eq!(snapshot.model_name, metadata.model_name);
        assert_eq!(snapshot.dimension, metadata.dimension);
        assert_eq!(
            loaded.semantic_search_embedding_count(),
            metadata.embedding_count
        );
        assert!(!loaded.has_semantic_search());
    }
}
