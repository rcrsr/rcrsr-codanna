//! Simplified persistence layer for Tantivy-only storage
//!
//! This module manages metadata and ensures Tantivy index exists.
//! All actual data is stored in Tantivy.

use crate::indexing::facade::{IndexFacade, SemanticRestore};
use crate::indexing::walk_config;
use crate::storage::generation::layout::{self, free_space_preflight_against};
use crate::storage::generation::markers::{Building, Complete};
use crate::storage::generation::{
    GenerationId, clone_generation, gc_logged, generation_size, migrate_flat_layout,
    resolve_current,
};
use crate::storage::{DataSource, IndexLayout, IndexMetadata};
use crate::{IndexError, IndexResult, Settings};
use std::ops::{Deref, DerefMut};
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

/// How [`IndexPersistence::open_build`] should seed a newly allocated
/// generation before handing it to the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildMode {
    /// Start from nothing: an empty generation with no parent.
    Fresh,
    /// Seed the new generation from the current generation's on-disk data
    /// (hardlinked Tantivy segments, copied semantic/metadata files) before
    /// handing it to the caller, so an incremental build only has to touch
    /// what actually changed.
    CloneCurrent,
}

/// An in-progress generation build: an [`IndexFacade`] opened onto a freshly
/// allocated generation directory, plus the [`Building`] ownership guard
/// that must stay alive for as long as this build is in progress, and the
/// parent generation (if any) it was seeded from.
///
/// Derefs to [`IndexFacade`] so callers can drive the build the same way
/// they would drive any other facade.
pub struct BuildFacade {
    facade: IndexFacade,
    // Held only to keep this generation's `BUILDING` ownership lock alive
    // for the build's lifetime; never read directly (mirrors
    // `markers::Building`'s own `_lock` field for the same reason).
    #[allow(dead_code)]
    guard: Building,
    parent: Option<GenerationId>,
}

impl BuildFacade {
    /// The generation this build was seeded from, or `None` for a
    /// [`BuildMode::Fresh`] build (including a [`BuildMode::CloneCurrent`]
    /// build that fell back to fresh semantics because no generation was
    /// current yet).
    pub fn parent(&self) -> Option<&GenerationId> {
        self.parent.as_ref()
    }
}

impl Deref for BuildFacade {
    type Target = IndexFacade;

    fn deref(&self) -> &IndexFacade {
        &self.facade
    }
}

impl DerefMut for BuildFacade {
    fn deref_mut(&mut self) -> &mut IndexFacade {
        &mut self.facade
    }
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

        // Re-attach persisted semantic-search state (full embeddings if
        // requested, otherwise just the lightweight metadata snapshot) and
        // restore indexed_paths from metadata.
        let restore_mode = if load_semantic {
            SemanticRestore::Embeddings
        } else {
            SemanticRestore::MetadataSnapshotOnly
        };
        facade.attach_persisted_state(restore_mode, metadata.as_ref());

        Ok(facade)
    }

    /// The on-disk size in bytes of generation `id` under this persistence's
    /// [`IndexLayout`], or `0` if `id` has no generation directory. Path
    /// arithmetic and the walk itself live in
    /// [`crate::storage::generation::generation_size`], which walks only
    /// this one generation's directory rather than every generation under
    /// `gen/`.
    fn generation_size_bytes(&self, id: &GenerationId) -> IndexResult<u64> {
        if !self.layout.gen_dir(id).is_dir() {
            return Ok(0);
        }
        generation_size(&self.layout, id)
    }

    /// Allocate and open a fresh generation to build into.
    ///
    /// Runs a best-effort [`gc()`](crate::storage::generation::gc::gc) pass first to reclaim headroom, resolves
    /// `mode`'s parent generation, checks free disk space for a
    /// [`BuildMode::Fresh`] build, then claims ownership of the new
    /// generation via [`Building::start`] before seeding and opening it. See
    /// [`BuildMode`] for what "seeding" means for each mode.
    #[must_use = "Build errors should be handled appropriately"]
    pub fn open_build(&self, settings: Arc<Settings>, mode: BuildMode) -> IndexResult<BuildFacade> {
        let disks = sysinfo::Disks::new_with_refreshed_list();
        let candidates = disks
            .list()
            .iter()
            .map(|disk| (disk.mount_point(), disk.available_space()));
        self.open_build_in(settings, mode, candidates)
    }

    /// Testable core of [`Self::open_build`]: takes the candidate
    /// `(mount_point, available_bytes)` pairs as a plain iterator, exactly
    /// like [`crate::storage::generation::layout::free_space_preflight_against`]
    /// does for the crate's real disk-backed preflight, so tests can inject
    /// a fake tiny disk instead of depending on the real machine's disk
    /// layout being in any particular state.
    pub(crate) fn open_build_in<'a>(
        &self,
        settings: Arc<Settings>,
        mode: BuildMode,
        disks: impl Iterator<Item = (&'a Path, u64)>,
    ) -> IndexResult<BuildFacade> {
        // (1) Reclaim headroom before allocating a new generation. A run
        // that finds the GC lock already held by a concurrent process is
        // not an error for this build -- `skipped_locked` is simply noted.
        gc_logged(
            &self.layout,
            settings.indexing.previous_generation_max_age(),
            "pre-build",
        )?;

        // (2) Resolve the parent generation for `mode`. A `CloneCurrent`
        // build over an index with no current generation yet has nothing to
        // clone, so it silently degrades to `Fresh` semantics: `parent`
        // stays `None`, and the seed step below becomes a no-op.
        let parent = match mode {
            BuildMode::Fresh => None,
            BuildMode::CloneCurrent => resolve_current(&self.layout)?,
        };

        // (3) Free-space preflight, `Fresh` builds only: a `CloneCurrent`
        // build's new generation starts out hardlinked to the parent's
        // Tantivy segments, so it does not need headroom equal to the
        // parent's full on-disk size the way a from-scratch build does.
        if matches!(mode, BuildMode::Fresh) {
            let needed = match resolve_current(&self.layout)? {
                Some(ref current_id) => self.generation_size_bytes(current_id)?,
                None => 0,
            };
            free_space_preflight_against(self.layout.root(), needed, disks)?;
        }

        // (4) Allocate the new generation's id.
        let id = GenerationId::generate();

        // (5) Claim ownership before touching the new generation directory
        // any further, so a concurrent builder racing the same id is
        // rejected rather than silently clobbered.
        let guard = Building::start(&self.layout, &id, parent.clone())?;

        let seeded_from_parent = matches!(mode, BuildMode::CloneCurrent) && parent.is_some();

        // (6) Seed the new generation from its parent, `CloneCurrent` only.
        if seeded_from_parent {
            let parent_id = parent.as_ref().expect("checked by seeded_from_parent");
            clone_generation(&self.layout, parent_id, &id)?;
        }

        // (7) Open the facade onto the (possibly seeded) generation.
        let mut facade = IndexFacade::open(settings, self.layout.clone(), id.clone())?;

        // (8) `CloneCurrent` only: replicate `load_facade_impl`'s post-open
        // steps exactly, so a cloned build starts with the same semantic
        // search and indexed-paths state a normal load of that data would
        // produce -- a semantic load failure never fails the build, it just
        // continues without semantic search.
        if seeded_from_parent {
            facade.attach_persisted_state(
                SemanticRestore::Embeddings,
                IndexMetadata::load(&self.layout.gen_dir(&id)).ok().as_ref(),
            );
        }

        // (9)
        Ok(BuildFacade {
            facade,
            guard,
            parent,
        })
    }

    /// Abandon `build` without publishing it and remove its generation
    /// directory right away, instead of leaving an orphan for the next
    /// `gc` pass to find. Best-effort: a directory that cannot be removed
    /// (Windows sharing violation while a mapping is still closing) is left
    /// as an orphan, which `gc` reclaims later.
    pub fn discard(&self, build: BuildFacade) {
        let id = build.generation_id().clone();
        drop(build);
        let dir = self.layout.gen_dir(&id);
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => tracing::debug!("[persistence] discarded unpublished build {id}"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::debug!(
                "[persistence] could not remove discarded build {id} ({e}); left for gc"
            ),
        }
    }

    /// Publish a finished build: persist its metadata, write a `COMPLETE`
    /// manifest, and atomically flip `current` to it -- but only if `current`
    /// still names the generation this build was seeded from.
    ///
    /// Order of operations:
    /// 1. [`Self::save_facade`] writes `index.meta` and semantic data into
    ///    the build's own generation directory (never the index root).
    /// 2. A [`Complete`] manifest is written for the build's generation,
    ///    recording every file the build actually produced.
    /// 3. A short critical section under [`IndexLayout::publish_lock`] --
    ///    deliberately never nested inside [`gc()`](crate::storage::generation::gc::gc)'s lock, so the two can
    ///    never deadlock against each other -- re-reads `current` and
    ///    compares it against the parent this build was seeded from. For an
    ///    incremental build (`parent.is_some()`), if `current` no longer
    ///    matches, this returns [`IndexError::GenerationSuperseded`] and
    ///    leaves the build's `BUILDING` marker in place: the build is
    ///    orphaned, to be reclaimed by a later [`gc()`](crate::storage::generation::gc::gc) run. A
    ///    [`BuildMode::Fresh`] build (`parent.is_none()`) always wins the
    ///    race, since it never depended on any particular starting state.
    ///    Otherwise, the [`Building`] guard is finished (removing the
    ///    `BUILDING` marker) and `current` is flipped to the new generation.
    /// 4. A trailing best-effort [`gc()`](crate::storage::generation::gc::gc) pass reclaims the generation this
    ///    build superseded. Its failure is logged only, never propagated --
    ///    the publish itself already succeeded once `current` was flipped.
    #[must_use = "Publish errors should be handled appropriately"]
    pub fn publish(&self, build: BuildFacade) -> IndexResult<GenerationId> {
        self.publish_into_facade(build).map(|(id, _)| id)
    }

    /// Same as [`Self::publish`], but also returns the warm [`IndexFacade`]
    /// that performed the phase-2 writes, so a caller that already holds it
    /// can keep serving from the same `document_index` `Arc` instead of
    /// re-opening the generation it just published.
    #[must_use = "Publish errors should be handled appropriately"]
    pub fn publish_into_facade(
        &self,
        build: BuildFacade,
    ) -> IndexResult<(GenerationId, IndexFacade)> {
        let BuildFacade {
            facade,
            guard,
            parent,
        } = build;

        // (1) Persist index.meta + semantic data into the build's own
        // generation directory.
        self.save_facade(&facade)?;

        let id = facade.generation_id().clone();
        let gen_dir = facade.generation_dir();

        // (2) Write the COMPLETE manifest for this generation.
        let manifest = Complete {
            id: id.clone(),
            parent: parent.clone(),
            started_at: guard.started_at(),
            completed_at: layout::unix_millis_now(),
            builder_version: env!("CARGO_PKG_VERSION").to_string(),
            symbol_count: facade.symbol_count() as u64,
            file_count: u64::from(facade.file_count()),
            files: layout::manifest_files(&gen_dir)?,
        };
        manifest.write(&self.layout)?;

        // (3) Compare-and-swap `current` under the publish lock.
        std::fs::create_dir_all(self.layout.root()).map_err(|e| IndexError::FileWrite {
            path: self.layout.root().to_path_buf(),
            source: e,
        })?;
        let lock_path = self.layout.publish_lock();
        let lock_file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .map_err(|e| IndexError::FileWrite {
                path: lock_path.clone(),
                source: e,
            })?;
        // Blocking: publish's critical section is short (a read, and either
        // a rename or nothing), so waiting out a concurrent publisher is
        // preferable to the skip-on-contention behavior `gc` uses for its
        // much coarser, best-effort pass.
        lock_file.lock().map_err(|e| IndexError::FileWrite {
            path: lock_path.clone(),
            source: e,
        })?;

        let observed = self.layout.read_current()?;
        let cas_result = if parent.is_some() && observed != parent {
            Err(IndexError::GenerationSuperseded {
                expected: parent
                    .as_ref()
                    .map(GenerationId::to_string)
                    .unwrap_or_default(),
                actual: observed
                    .as_ref()
                    .map(GenerationId::to_string)
                    .unwrap_or_else(|| "none".to_string()),
            })
        } else {
            guard.finish(&self.layout, &id)?;
            self.layout.write_current(&id)?;
            Ok(())
        };

        let _ = lock_file.unlock();

        cas_result?;

        // (4) Best-effort trailing GC: never fails a publish that already
        // landed. `gc_logged` owns the conditional-log gating (INFO only
        // when something was actually removed); this call site never
        // duplicates that logic.
        let _ = gc_logged(
            &self.layout,
            facade.settings().indexing.previous_generation_max_age(),
            "post-publish",
        );

        Ok((id, facade))
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

    /// Save `facade`'s metadata into its bound generation only if that
    /// generation is still `current`.
    ///
    /// Mirrors the compare-and-swap [`Self::publish_into_facade`] already
    /// performs around `current`: `facade` was bound to a generation by an
    /// earlier `load_facade`/`load_facade_lite` call, and a save that
    /// blindly writes into that generation's directory races a concurrent
    /// publish that has since moved `current` elsewhere (the write would
    /// silently land in a stale, possibly soon-to-be-GC'd generation while
    /// reporting success). This holds the same `publish_lock` around a
    /// `read_current`/compare/save sequence so a save either lands in the
    /// generation that is still current, or is refused with
    /// [`IndexError::GenerationSuperseded`].
    pub(crate) fn save_facade_current_checked(&self, facade: &IndexFacade) -> IndexResult<()> {
        std::fs::create_dir_all(self.layout.root()).map_err(|e| IndexError::FileWrite {
            path: self.layout.root().to_path_buf(),
            source: e,
        })?;
        let lock_path = self.layout.publish_lock();
        let lock_file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .map_err(|e| IndexError::FileWrite {
                path: lock_path.clone(),
                source: e,
            })?;
        lock_file.lock().map_err(|e| IndexError::FileWrite {
            path: lock_path.clone(),
            source: e,
        })?;

        let observed = self.layout.read_current()?;
        let result = if observed.as_ref() == Some(facade.generation_id()) {
            self.save_facade(facade)
        } else {
            Err(IndexError::GenerationSuperseded {
                expected: facade.generation_id().to_string(),
                actual: observed
                    .as_ref()
                    .map(GenerationId::to_string)
                    .unwrap_or_else(|| "none".to_string()),
            })
        };

        let _ = lock_file.unlock();

        result
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

    /// Settings configured with a remote embedding backend, so semantic
    /// save/load round-trips in these tests never touch a local fastembed
    /// model or the network: `SimpleSemanticSearch::new_empty` always tags
    /// its metadata `remote`, and `load`/`load_semantic_search` delegate a
    /// `remote`-tagged index straight to `load_remote`, which only reads
    /// vector storage off disk.
    fn remote_settings_for(temp_dir: &TempDir) -> Arc<Settings> {
        let mut settings = Settings {
            index_path: temp_dir.path().to_path_buf(),
            ..Settings::default()
        };
        settings.semantic_search.remote_url = Some("http://127.0.0.1:0".to_string());
        settings.semantic_search.remote_dim = Some(8);
        Arc::new(settings)
    }

    #[test]
    fn open_build_clone_current_shares_tantivy_inodes_and_loads_semantics() {
        use std::os::unix::fs::MetadataExt;

        let temp_dir = TempDir::new().unwrap();
        let persistence = IndexPersistence::new(temp_dir.path().to_path_buf());
        let settings = remote_settings_for(&temp_dir);

        let facade = IndexFacade::new(settings.clone()).unwrap();
        persistence.save_facade(&facade).unwrap();
        let current_id = facade.generation_id().clone();

        // A full (albeit empty) semantic index on the current generation, so
        // the build's clone step has something real to load.
        crate::semantic::SimpleSemanticSearch::new_empty(8, "test-model")
            .save(&facade.semantic_dir())
            .unwrap();

        let build = persistence
            .open_build(settings, BuildMode::CloneCurrent)
            .expect("clone-current build over an existing generation must succeed");

        assert_eq!(build.parent(), Some(&current_id));
        assert_ne!(build.generation_id(), &current_id);
        assert!(
            build.has_semantic_search(),
            "cloned build must load the parent's semantic index"
        );

        // Tantivy's meta.json must be hardlinked (same inode), not copied,
        // from the parent generation into the build's generation.
        let parent_meta = facade.generation_dir().join("tantivy").join("meta.json");
        let build_meta = build.generation_dir().join("tantivy").join("meta.json");
        assert_eq!(
            std::fs::metadata(&parent_meta).unwrap().ino(),
            std::fs::metadata(&build_meta).unwrap().ino(),
            "cloned tantivy files must share inodes with the parent generation"
        );
    }

    #[test]
    fn open_build_clone_current_over_no_index_falls_back_to_fresh() {
        let temp_dir = TempDir::new().unwrap();
        let persistence = IndexPersistence::new(temp_dir.path().to_path_buf());
        let settings = settings_for(&temp_dir);

        let build = persistence
            .open_build(settings, BuildMode::CloneCurrent)
            .expect("clone-current over an empty root must fall back to a fresh build");

        assert!(
            build.parent().is_none(),
            "no current generation means no parent to report"
        );
        assert_eq!(build.symbol_count(), 0);
        assert_eq!(build.file_count(), 0);
    }

    #[test]
    fn open_build_fresh_preflight_refuses_via_injected_disks() {
        let temp_dir = TempDir::new().unwrap();
        let persistence = IndexPersistence::new(temp_dir.path().to_path_buf());
        let settings = settings_for(&temp_dir);

        // A real current generation with nonzero on-disk size, so `Fresh`'s
        // preflight has a positive `needed_bytes` to compare against the
        // injected disk's tiny reported availability.
        let facade = IndexFacade::new(settings.clone()).unwrap();
        persistence.save_facade(&facade).unwrap();

        let root = temp_dir.path().canonicalize().unwrap();
        let fake_disks = vec![(root.as_path(), 1u64)];

        let result = persistence.open_build_in(settings, BuildMode::Fresh, fake_disks.into_iter());

        match result {
            Err(IndexError::IndexNotSpaceForBuild { .. }) => {}
            Err(e) => panic!("expected IndexNotSpaceForBuild, got {e:?}"),
            Ok(_) => panic!("a tiny injected disk must refuse the fresh build"),
        }
    }

    #[test]
    fn publish_flips_current_and_classifies_current_not_building() {
        let temp_dir = TempDir::new().unwrap();
        let persistence = IndexPersistence::new(temp_dir.path().to_path_buf());
        let settings = settings_for(&temp_dir);

        let build = persistence
            .open_build(settings, BuildMode::Fresh)
            .expect("open fresh build");
        let id = build.generation_id().clone();

        let published_id = persistence.publish(build).expect("publish must succeed");
        assert_eq!(published_id, id);

        assert_eq!(
            persistence.layout.read_current().expect("read current"),
            Some(id.clone())
        );
        assert!(
            !persistence.layout.building_marker(&id).is_file(),
            "BUILDING marker must be removed once published"
        );
        assert_eq!(
            crate::storage::generation::classify(&persistence.layout, &id),
            crate::storage::generation::GenerationState::Current
        );
    }

    #[test]
    fn publish_incremental_refuses_when_current_moved_off_parent() {
        let temp_dir = TempDir::new().unwrap();
        let persistence = IndexPersistence::new(temp_dir.path().to_path_buf());
        let settings = settings_for(&temp_dir);

        // Base generation P, published and current.
        let base = persistence
            .open_build(settings.clone(), BuildMode::Fresh)
            .expect("open fresh base build");
        let base_id = persistence.publish(base).expect("publish base");

        // Two independent CloneCurrent builds, both cloned from P.
        let b1 = persistence
            .open_build(settings.clone(), BuildMode::CloneCurrent)
            .expect("open b1");
        let b2 = persistence
            .open_build(settings.clone(), BuildMode::CloneCurrent)
            .expect("open b2");
        assert_eq!(b1.parent(), Some(&base_id));
        assert_eq!(b2.parent(), Some(&base_id));

        let id1 = b1.generation_id().clone();
        let b2_id = b2.generation_id().clone();
        let published1 = persistence.publish(b1).expect("publish b1 must succeed");
        assert_eq!(published1, id1);

        let err = persistence
            .publish(b2)
            .expect_err("publish b2 must be superseded");
        match err {
            IndexError::GenerationSuperseded { expected, actual } => {
                assert_eq!(expected, base_id.to_string());
                assert_eq!(actual, id1.to_string());
            }
            other => panic!("expected GenerationSuperseded, got {other:?}"),
        }

        // A superseded publish never removes the build's BUILDING marker --
        // that only happens once `guard.finish` runs, which the CAS failure
        // path deliberately skips so the build is left to be reclaimed by a
        // later gc run rather than torn down here.
        let b2_dir = persistence.layout.gen_dir(&b2_id);
        assert!(
            persistence.layout.building_marker(&b2_id).is_file(),
            "the superseded build's BUILDING marker must survive publish's CAS failure"
        );
        assert!(b2_dir.is_dir());
    }

    #[test]
    fn publish_fresh_wins_regardless_of_current() {
        let temp_dir = TempDir::new().unwrap();
        let persistence = IndexPersistence::new(temp_dir.path().to_path_buf());
        let settings = settings_for(&temp_dir);

        let base = persistence
            .open_build(settings.clone(), BuildMode::Fresh)
            .expect("open fresh base build");
        persistence.publish(base).expect("publish base");

        // Open a fresh build (no parent) before `current` moves again.
        let fresh = persistence
            .open_build(settings.clone(), BuildMode::Fresh)
            .expect("open fresh build");
        assert!(fresh.parent().is_none());
        let fresh_id = fresh.generation_id().clone();

        // Move `current` out from under it before publishing.
        let other = persistence
            .open_build(settings.clone(), BuildMode::Fresh)
            .expect("open other fresh build");
        persistence.publish(other).expect("publish other");

        let published = persistence
            .publish(fresh)
            .expect("a Fresh build (no parent) must win regardless of current");
        assert_eq!(published, fresh_id);
    }

    #[test]
    fn save_facade_current_checked_refuses_when_current_moved_off_bound_generation() {
        let temp_dir = TempDir::new().unwrap();
        let persistence = IndexPersistence::new(temp_dir.path().to_path_buf());
        let settings = settings_for(&temp_dir);

        // Publish G1, current == G1.
        let base = persistence
            .open_build(settings.clone(), BuildMode::Fresh)
            .expect("open fresh base build");
        let g1 = persistence.publish(base).expect("publish base");

        // Bind a facade to G1 (mirrors `load_facade_lite` in
        // `run_prune_indexed_paths`).
        let mut facade = persistence
            .load_facade_lite(settings.clone())
            .expect("load facade bound to G1");
        assert_eq!(facade.generation_id(), &g1);

        let g1_meta_path = persistence.layout.gen_dir(&g1).join("index.meta");
        let g1_meta_before =
            std::fs::read(&g1_meta_path).expect("G1 index.meta must exist before the race");

        // Out-of-band: a concurrent full reindex publishes G2, moving
        // `current` off G1 from under the bound facade.
        let other = persistence
            .open_build(settings.clone(), BuildMode::Fresh)
            .expect("open fresh other build");
        persistence.publish(other).expect("publish other as G2");
        assert_ne!(
            persistence.layout.read_current().unwrap().unwrap(),
            g1,
            "current must have moved off G1"
        );

        facade.set_indexed_paths(vec![temp_dir.path().to_path_buf()]);

        let err = persistence
            .save_facade_current_checked(&facade)
            .expect_err("save must be refused once current moved off the bound generation");
        match err {
            IndexError::GenerationSuperseded { expected, actual } => {
                assert_eq!(expected, g1.to_string());
                assert_ne!(actual, g1.to_string());
            }
            other => panic!("expected GenerationSuperseded, got {other:?}"),
        }

        // The discriminating assertion: a plain `save_facade` would have
        // written into G1 despite it no longer being current. Confirm no
        // stale write landed.
        let g1_meta_after =
            std::fs::read(&g1_meta_path).expect("G1 index.meta must still exist after the race");
        assert_eq!(
            g1_meta_before, g1_meta_after,
            "G1's index.meta must be byte-unchanged: no stale write must land after current moved on"
        );
    }

    #[test]
    fn save_facade_current_checked_saves_when_current_is_unchanged() {
        let temp_dir = TempDir::new().unwrap();
        let persistence = IndexPersistence::new(temp_dir.path().to_path_buf());
        let settings = settings_for(&temp_dir);

        let build = persistence
            .open_build(settings.clone(), BuildMode::Fresh)
            .expect("open fresh build");
        persistence.publish(build).expect("publish");

        let mut facade = persistence
            .load_facade_lite(settings.clone())
            .expect("load facade bound to current");

        let new_paths = vec![temp_dir.path().to_path_buf()];
        facade.set_indexed_paths(new_paths.clone());

        persistence
            .save_facade_current_checked(&facade)
            .expect("save must succeed when current has not moved");

        let reloaded = persistence
            .load_facade_lite(settings)
            .expect("reload facade");
        let reloaded_paths: Vec<PathBuf> = reloaded.get_indexed_paths().iter().cloned().collect();
        assert_eq!(reloaded_paths, new_paths);
    }

    #[test]
    fn publish_writes_index_meta_into_the_generation() {
        let temp_dir = TempDir::new().unwrap();
        let persistence = IndexPersistence::new(temp_dir.path().to_path_buf());
        let settings = settings_for(&temp_dir);

        let build = persistence
            .open_build(settings, BuildMode::Fresh)
            .expect("open fresh build");
        let gen_dir = build.generation_dir();

        persistence.publish(build).expect("publish must succeed");

        let meta_path = gen_dir.join("index.meta");
        assert!(
            meta_path.is_file(),
            "index.meta must be written into the generation directory"
        );

        let metadata = IndexMetadata::load(&gen_dir).expect("load index.meta");
        match metadata.data_source {
            DataSource::Tantivy { path, .. } => {
                assert!(
                    path.is_relative(),
                    "tantivy data_source path must be relative, got {path:?}"
                );
                assert_eq!(path, PathBuf::from("tantivy"));
            }
            DataSource::Fresh => panic!("expected DataSource::Tantivy after publish"),
        }
    }

    #[test]
    fn publish_into_facade_returns_the_warm_facade_that_was_published() {
        let temp_dir = TempDir::new().unwrap();
        let persistence = IndexPersistence::new(temp_dir.path().to_path_buf());
        let settings = settings_for(&temp_dir);

        let build = persistence
            .open_build(settings, BuildMode::Fresh)
            .expect("open fresh build");
        let expected_symbol_count = build.symbol_count();

        let (id, facade) = persistence
            .publish_into_facade(build)
            .expect("publish_into_facade must succeed");

        assert_eq!(facade.generation_id(), &id);
        assert_eq!(facade.symbol_count(), expected_symbol_count);
    }

    #[test]
    fn discard_removes_the_unpublished_build_directory_and_leaves_current_alone() {
        let temp_dir = TempDir::new().unwrap();
        let persistence = IndexPersistence::new(temp_dir.path().to_path_buf());
        let settings = settings_for(&temp_dir);

        let current = persistence
            .publish(
                persistence
                    .open_build(Arc::clone(&settings), BuildMode::Fresh)
                    .expect("open fresh build"),
            )
            .expect("publish current");

        let build = persistence
            .open_build(settings, BuildMode::CloneCurrent)
            .expect("open clone-current build");
        let build_id = build.generation_id().clone();
        let build_dir = persistence.layout.gen_dir(&build_id);
        assert!(
            build_dir.is_dir(),
            "build directory must exist before discard"
        );

        persistence.discard(build);

        assert!(
            !build_dir.exists(),
            "discard must remove the build directory"
        );
        assert!(
            persistence.layout.gen_dir(&current).is_dir(),
            "discard must not touch the current generation"
        );
        assert_eq!(
            persistence.layout.read_current().expect("read current"),
            Some(current),
            "discard must not move the current pointer"
        );
    }
}
