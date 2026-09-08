//! Force-mode composition: full walk plus resolution.

use super::{
    CleanupStats, EmbedOptions, FileSource, IncrementalStats, Phase1Options, Phase2Stats, Pipeline,
    PipelineResult, ProgressSink,
};
use crate::indexing::IndexStats;
use crate::semantic::SimpleSemanticSearch;
use crate::storage::DocumentIndex;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;

impl Pipeline {
    /// Run full pipeline: Phase 1 (indexing) + Phase 2 (resolution).
    ///
    /// Convenience method that runs both phases in sequence.
    pub fn index_and_resolve(
        &self,
        root: &Path,
        index: Arc<DocumentIndex>,
    ) -> PipelineResult<(IndexStats, Phase2Stats)> {
        // Phase 1: Index files
        let (index_stats, unresolved, bindings, barriers, symbol_cache) =
            self.index_directory(root, Arc::clone(&index))?;

        // Phase 2: Resolve relationships
        let symbol_cache = Arc::new(symbol_cache);
        let phase2_stats = self.run_phase2(unresolved, bindings, barriers, symbol_cache, index)?;

        Ok((index_stats, phase2_stats))
    }

    /// Full index (force mode): index all files without incremental detection.
    ///
    /// `single_root_batch` must be true only when this walk is BOTH the
    /// sole directory being processed in the current caller's batch AND
    /// the sole registered root overall (`settings.indexing.indexed_paths`
    /// has exactly one entry). A caller processing several explicit
    /// sub-paths of one registered root (e.g. a scoped force reindex)
    /// must pass `false`, even though `indexed_paths.len() == 1`, or
    /// cross-directory symbols outside the current walk are hidden from
    /// resolution.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn index_full(
        &self,
        root: &Path,
        index: Arc<DocumentIndex>,
        semantic: Option<Arc<Mutex<SimpleSemanticSearch>>>,
        embedding_pool: Option<Arc<crate::semantic::EmbeddingBackend>>,
        semantic_path: &Path,
        progress: Option<Arc<crate::io::status_line::ProgressBar>>,
        single_root_batch: bool,
    ) -> PipelineResult<IncrementalStats> {
        let start = Instant::now();
        let show_progress = progress.is_some();

        // Run Phase 1 with embedding iff semantic search and pool are both present
        let embed = match (&semantic, &embedding_pool) {
            (Some(sem), Some(pool)) => Some(EmbedOptions {
                pool: Arc::clone(pool),
                semantic: Arc::clone(sem),
            }),
            _ => None,
        };
        let (index_stats, unresolved, bindings, barriers, run_cache, metrics) = self.run_phase1(
            FileSource::Walk(root.to_path_buf()),
            Arc::clone(&index),
            Phase1Options {
                progress: progress.map_or(ProgressSink::Silent, ProgressSink::Bar),
                embed,
                ..Default::default()
            },
        )?;

        // Log pipeline metrics (no StatusLine in this path, safe to log immediately)
        if let Some(m) = metrics {
            m.log();
        }

        // Run Phase 2 resolution with progress if Phase 1 had progress.
        // Unless this walk is the sole directory in the current batch
        // *and* the sole registered root overall, the run-scoped cache
        // holds only this walk's files, hiding other roots'/paths'
        // symbols from resolution -- seed from the persisted index
        // instead. Only the true single-root, single-walk case already
        // has everything resolution needs in the free in-memory cache
        // Phase 1 just built, so only then can we skip the unbounded
        // Tantivy re-scan.
        let symbol_cache = if single_root_batch {
            Arc::new(run_cache)
        } else {
            Arc::new(super::SymbolLookupCache::from_index(&index)?)
        };
        let phase2_stats = self.run_phase2_maybe_bar(
            unresolved,
            bindings,
            barriers,
            symbol_cache,
            Arc::clone(&index),
            show_progress,
        )?;

        // Save embeddings
        self.persist_embeddings(semantic.as_ref(), semantic_path)?;

        Ok(IncrementalStats {
            new_files: index_stats.files_indexed,
            modified_files: 0,
            deleted_files: 0,
            renamed_files: 0,
            invalidated_caller_files: 0,
            deleted_symbols: 0,
            index_stats,
            cleanup_stats: CleanupStats::default(),
            phase2_stats,
            elapsed: start.elapsed(),
        })
    }
}
