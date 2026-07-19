//! IndexFacade - Bridge component wrapping DocumentIndex + Pipeline + SemanticSearch
//!
//! Provides a unified API that matches SimpleIndexer's interface while using Pipeline
//! for indexing and DocumentIndex for queries. This enables gradual migration from
//! SimpleIndexer to the parallel Pipeline architecture.
//!
//! ## Architecture
//!
//! ```text
//! IndexFacade
//!   ├── DocumentIndex (Arc) - All query operations
//!   ├── Pipeline - All mutation/indexing operations
//!   ├── SimpleSemanticSearch (Option<Arc<Mutex>>) - Semantic search
//!   ├── SymbolCache (Option<Arc>) - O(1) symbol lookups
//!   └── indexed_paths (HashSet) - Directory tracking
//! ```
//!
//! ## Usage
//!
//! ```ignore
//! let facade = IndexFacade::new(settings)?;
//! facade.index_directory(&path)?;  // Uses Pipeline
//! let symbols = facade.find_symbols_by_name("main")?;  // Uses DocumentIndex
//! ```

use crate::config::Settings;
use crate::indexing::pipeline::Pipeline;
use crate::semantic::remote::run_async;
use crate::semantic::{
    EmbeddingBackend, EmbeddingPool, RemoteEmbedder, SemanticSearchError, SimpleSemanticSearch,
};
use crate::storage::{DocumentIndex, SearchResult};
use crate::symbol::context::{ContextIncludes, SymbolContext, SymbolRelationships};
use crate::{FileId, IndexError, RelationKind, Relationship, Symbol, SymbolId, SymbolKind};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Result type for facade operations
pub type FacadeResult<T> = Result<T, IndexError>;

/// Statistics for indexing operations
#[derive(Debug, Clone, Default)]
pub struct IndexingStats {
    pub files_indexed: usize,
    pub symbols_found: usize,
    pub relationships_resolved: usize,
    /// Files removed by deleted-file cleanup.
    pub files_removed: usize,
    /// Symbols removed by deleted-file cleanup (modified-file cleanup
    /// excluded — those symbols re-add in the same run).
    pub symbols_removed: usize,
}

/// Output verbosity for `index --dry-run`.
///
/// A dedicated enum instead of two more bool parameters on
/// `index_directory_with_options`: `list_all` and `json` are not independent
/// (`--json` wins over `--list-all`), so a bool pair would admit an
/// unrepresentable/ambiguous combination.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DryRunOutput {
    /// Default: human-readable summary, truncated at 5 paths.
    #[default]
    Summary,
    /// `--list-all`: every path, one per line, no truncation.
    ListAll,
    /// `--json`: a JSON array of path strings to stdout, nothing else.
    Json,
}

/// Statistics for sync operations
#[derive(Debug, Clone, Default)]
pub struct SyncStats {
    pub added_dirs: usize,
    pub removed_dirs: usize,
    pub files_indexed: usize,
    pub symbols_found: usize,
    pub files_modified: usize,
    pub files_added: usize,
}

impl SyncStats {
    pub fn has_changes(&self) -> bool {
        self.added_dirs > 0
            || self.removed_dirs > 0
            || self.files_modified > 0
            || self.files_added > 0
    }
}

/// IndexFacade - Unified interface for code intelligence operations
///
/// This facade wraps DocumentIndex (for queries) and Pipeline (for indexing),
/// providing an API compatible with SimpleIndexer for gradual migration.
pub struct IndexFacade {
    /// Document storage (Tantivy-based) - used for all queries
    document_index: Arc<DocumentIndex>,

    /// Parallel indexing pipeline - used for mutations
    pipeline: Pipeline,

    /// Optional semantic search for doc comment embeddings
    semantic_search: Option<Arc<Mutex<SimpleSemanticSearch>>>,

    /// Optional embedding pool for parallel embedding generation
    embedding_pool: Option<Arc<EmbeddingBackend>>,

    /// Configuration
    settings: Arc<Settings>,

    /// Tracked indexed directories (canonicalized paths)
    indexed_paths: HashSet<PathBuf>,

    /// Base path for index storage
    index_base: PathBuf,

    /// Set to true when load_semantic_search fails with DimensionMismatch so
    /// hot-reload and other callers do not retry on every reload cycle.
    semantic_incompatible: bool,

    /// Persisted semantic metadata for status/reporting when semantic search
    /// is not loaded into memory (for example, lite facade loads).
    semantic_metadata_snapshot: Option<crate::semantic::SemanticMetadata>,

    /// Serializes full-reindex runs through [`reindex_locked`]. Only one
    /// `reindex_locked` invocation may hold this facade's Phase 2 off-lock
    /// walk at a time; a losing caller is rejected (see
    /// [`IndexError::ReindexInProgress`]) rather than queued.
    reindex_gate: Arc<tokio::sync::Semaphore>,
}

impl IndexFacade {
    /// Create a new IndexFacade with the given settings.
    ///
    /// Creates or opens the DocumentIndex and initializes the Pipeline.
    pub fn new(settings: Arc<Settings>) -> FacadeResult<Self> {
        // Construct the full index path
        let index_base = if let Some(ref workspace_root) = settings.workspace_root {
            workspace_root.join(&settings.index_path)
        } else {
            settings.index_path.clone()
        };

        // Tantivy data goes under index_path/tantivy
        let tantivy_path = index_base.join("tantivy");

        let document_index = Arc::new(DocumentIndex::new(&tantivy_path, &settings)?);

        let pipeline = Pipeline::with_settings(settings.clone());

        Ok(Self {
            document_index,
            pipeline,
            semantic_search: None,
            embedding_pool: None,
            settings,
            indexed_paths: HashSet::new(),
            index_base,
            semantic_incompatible: false,
            semantic_metadata_snapshot: None,
            reindex_gate: Arc::new(tokio::sync::Semaphore::new(1)),
        })
    }

    /// Create facade from existing components (for server integration).
    pub fn from_components(
        document_index: Arc<DocumentIndex>,
        pipeline: Pipeline,
        semantic_search: Option<Arc<Mutex<SimpleSemanticSearch>>>,
        settings: Arc<Settings>,
    ) -> Self {
        let index_base = if let Some(ref workspace_root) = settings.workspace_root {
            workspace_root.join(&settings.index_path)
        } else {
            settings.index_path.clone()
        };

        Self {
            document_index,
            pipeline,
            semantic_search,
            embedding_pool: None,
            settings,
            indexed_paths: HashSet::new(),
            index_base,
            semantic_incompatible: false,
            semantic_metadata_snapshot: None,
            reindex_gate: Arc::new(tokio::sync::Semaphore::new(1)),
        }
    }

    /// Get a reference to the underlying DocumentIndex.
    pub fn document_index(&self) -> &Arc<DocumentIndex> {
        &self.document_index
    }

    /// Get a reference to the Pipeline.
    pub fn pipeline(&self) -> &Pipeline {
        &self.pipeline
    }

    /// Get a reference to the settings.
    pub fn settings(&self) -> &Arc<Settings> {
        &self.settings
    }

    /// Get the index base path.
    pub fn index_base(&self) -> &Path {
        &self.index_base
    }

    /// Clone the handle to this facade's reindex gate, used by
    /// [`reindex_locked`] to serialize full-reindex runs.
    pub(crate) fn reindex_gate(&self) -> Arc<tokio::sync::Semaphore> {
        Arc::clone(&self.reindex_gate)
    }

    // =========================================================================
    // Semantic Search Management
    // =========================================================================

    /// Enable semantic search with the configured model.
    pub fn enable_semantic_search(&mut self) -> FacadeResult<()> {
        let semantic_path = self.index_base.join("semantic");
        std::fs::create_dir_all(&semantic_path)?;

        let backend = build_embedding_backend(&self.settings.semantic_search)?;
        let backend = Arc::new(backend);

        // In remote mode, skip local fastembed init; use new_empty so the
        // SemanticSearch instance carries the correct dimension from the backend.
        let is_remote = self.settings.semantic_search.remote_url.is_some()
            || std::env::var("CODANNA_EMBED_URL").is_ok();
        let semantic = if is_remote {
            SimpleSemanticSearch::new_empty(
                backend.dimensions(),
                &resolve_remote_model_name(&self.settings.semantic_search),
            )
        } else {
            let model = &self.settings.semantic_search.model;
            SimpleSemanticSearch::from_model_name(model)?
        };

        self.semantic_search = Some(Arc::new(Mutex::new(semantic)));
        self.semantic_metadata_snapshot = self.get_semantic_metadata();
        self.embedding_pool = Some(backend);

        Ok(())
    }

    /// Check if semantic search is enabled.
    pub fn has_semantic_search(&self) -> bool {
        self.semantic_search.is_some()
    }

    /// Returns true if a previous load_semantic_search call failed with
    /// DimensionMismatch, meaning retrying would always fail until re-indexed.
    pub fn is_semantic_incompatible(&self) -> bool {
        self.semantic_incompatible
    }

    /// Save semantic search data to disk.
    pub fn save_semantic_search(&self, path: &Path) -> FacadeResult<()> {
        if let Some(ref semantic) = self.semantic_search {
            let sem = semantic.lock().map_err(|_| IndexError::lock_error())?;
            sem.save(path)?;
        }
        Ok(())
    }

    /// Load semantic search data from disk.
    ///
    /// This only loads pre-computed embeddings for querying.
    /// Embedding pool for generating new embeddings is initialized lazily.
    pub fn load_semantic_search(&mut self, path: &Path) -> FacadeResult<bool> {
        if path.join("metadata.json").exists() {
            let is_remote = self.settings.semantic_search.remote_url.is_some()
                || std::env::var("CODANNA_EMBED_URL").is_ok();
            let load_result = if is_remote {
                SimpleSemanticSearch::load_remote(path)
            } else {
                SimpleSemanticSearch::load(path)
            };
            match load_result {
                Ok(semantic) => {
                    // Restore the embedding backend so query-time remote embedding
                    // works immediately without waiting for a lazy reindex call.
                    if self.embedding_pool.is_none() {
                        match build_embedding_backend(&self.settings.semantic_search) {
                            Ok(b) => self.embedding_pool = Some(Arc::new(b)),
                            Err(e) => tracing::warn!("Failed to restore embedding backend: {e}"),
                        }
                    }

                    // Verify dimension and backend kind compatibility.
                    if let Some(ref pool) = self.embedding_pool {
                        let backend_dim = pool.dimensions();
                        let index_dim = semantic.dimensions();

                        if backend_dim != index_dim {
                            self.semantic_incompatible = true;
                            return Err(IndexError::SemanticSearch(
                                SemanticSearchError::DimensionMismatch {
                                    expected: backend_dim,
                                    actual: index_dim,
                                    suggestion: format!(
                                        "Index was built with {index_dim}-dimensional embeddings \
                                         but current backend produces {backend_dim}d. \
                                         Re-index with: codanna index <path> --force"
                                    ),
                                },
                            ));
                        }

                        // Warn when backend kind changed but dimensions happen to match.
                        // Embedding spaces differ between models so similarity scores may
                        // be meaningless. Only a --force re-index can fully fix this.
                        let index_is_remote = semantic.is_remote_index();
                        let backend_is_remote =
                            matches!(pool.as_ref(), EmbeddingBackend::Remote(_));
                        if index_is_remote != backend_is_remote {
                            tracing::warn!(
                                target: "semantic",
                                "Backend kind changed (index={}, current={}). \
                                 Embedding spaces may differ — similarity scores could be inaccurate. \
                                 Re-index with --force to fix.",
                                if index_is_remote { "remote" } else { "local" },
                                if backend_is_remote { "remote" } else { "local" },
                            );
                        }
                    }

                    self.semantic_search = Some(Arc::new(Mutex::new(semantic)));
                    self.semantic_metadata_snapshot = self.get_semantic_metadata();
                    return Ok(true);
                }
                Err(SemanticSearchError::DimensionMismatch {
                    expected,
                    actual,
                    ref suggestion,
                }) => {
                    // Dimension mismatch: index is structurally incompatible with the
                    // current backend. Mark this facade so callers do not retry on every
                    // cycle. The error propagates upward; callers that need the process
                    // to survive (startup, hot-reload) swallow it and continue text-only.
                    // Callers that want to fail fast can treat this Err as fatal.
                    self.semantic_incompatible = true;
                    tracing::error!(
                        target: "semantic",
                        "Semantic index dimension mismatch (expected={expected}, actual={actual}): {suggestion}"
                    );
                    return Err(IndexError::SemanticSearch(
                        SemanticSearchError::DimensionMismatch {
                            expected,
                            actual,
                            suggestion: suggestion.to_string(),
                        },
                    ));
                }
                Err(e) => {
                    // Other errors (missing file, corrupt data) — warn and continue
                    // without semantic search rather than blocking startup.
                    tracing::warn!("Failed to load semantic search, continuing without it: {e}");
                }
            }
        }
        Ok(false)
    }

    /// Load persisted semantic metadata without initializing the semantic backend.
    pub fn load_semantic_metadata_snapshot(&mut self, path: &Path) -> FacadeResult<bool> {
        if !path.join("metadata.json").exists() {
            self.semantic_metadata_snapshot = None;
            return Ok(false);
        }

        let metadata = crate::semantic::SemanticMetadata::load(path)?;
        self.semantic_metadata_snapshot = Some(metadata);
        Ok(true)
    }

    /// Ensure embedding backend is initialized for generating new embeddings.
    ///
    /// Called lazily by methods that need to compute embeddings (reindexing, watcher).
    pub fn ensure_embedding_pool(&mut self) -> FacadeResult<()> {
        if self.embedding_pool.is_some() {
            return Ok(());
        }

        let backend = build_embedding_backend(&self.settings.semantic_search)?;
        self.embedding_pool = Some(Arc::new(backend));
        tracing::debug!("Initialized embedding backend for incremental updates");
        Ok(())
    }

    /// Get semantic search embedding count.
    pub fn semantic_search_embedding_count(&self) -> usize {
        self.semantic_search
            .as_ref()
            .map(|s| s.lock().map(|sem| sem.embedding_count()).unwrap_or(0))
            .or_else(|| {
                self.semantic_metadata_snapshot
                    .as_ref()
                    .map(|m| m.embedding_count)
            })
            .unwrap_or(0)
    }

    /// Get semantic search metadata.
    pub fn get_semantic_metadata(&self) -> Option<crate::semantic::SemanticMetadata> {
        self.semantic_search
            .as_ref()
            .and_then(|s| s.lock().ok().and_then(|sem| sem.metadata().cloned()))
            .or_else(|| self.semantic_metadata_snapshot.clone())
    }

    // =========================================================================
    // Symbol Query Methods (delegate to DocumentIndex)
    // =========================================================================

    /// Find a symbol by name.
    pub fn find_symbol(&self, name: &str) -> Option<SymbolId> {
        self.document_index
            .find_symbols_by_name(name, None)
            .ok()
            .and_then(|symbols| symbols.first().map(|s| s.id))
    }

    /// Find all symbols by name with optional language filter.
    pub fn find_symbols_by_name(&self, name: &str, language_filter: Option<&str>) -> Vec<Symbol> {
        self.document_index
            .find_symbols_by_name(name, language_filter)
            .unwrap_or_default()
    }

    /// Get a symbol by ID.
    pub fn get_symbol(&self, id: SymbolId) -> Option<Symbol> {
        self.document_index.find_symbol_by_id(id).ok().flatten()
    }

    /// Get all symbols (with limit).
    ///
    /// Returns empty vec on error for SimpleIndexer API compatibility.
    pub fn get_all_symbols(&self) -> Vec<Symbol> {
        self.document_index
            .get_all_symbols(10000)
            .unwrap_or_else(|e| {
                tracing::warn!(target: "facade", "get_all_symbols error: {e}");
                Vec::new()
            })
    }

    /// Get symbols by file ID.
    ///
    /// Returns empty vec on error for SimpleIndexer API compatibility.
    pub fn get_symbols_by_file(&self, file_id: FileId) -> Vec<Symbol> {
        self.document_index
            .find_symbols_by_file(file_id)
            .unwrap_or_default()
    }

    // =========================================================================
    // Relationship Query Methods (delegate to DocumentIndex)
    // =========================================================================

    /// Get functions called by a symbol.
    pub fn get_called_functions(&self, symbol_id: SymbolId) -> Vec<Symbol> {
        let relationships = self
            .document_index
            .get_relationships_from(symbol_id, RelationKind::Calls)
            .unwrap_or_default();

        let mut symbols = Vec::new();
        for (_, to_id, _) in relationships {
            if let Some(symbol) = self.get_symbol(to_id) {
                symbols.push(symbol);
            }
        }
        symbols
    }

    /// Get functions called by a symbol with metadata.
    pub fn get_called_functions_with_metadata(
        &self,
        symbol_id: SymbolId,
    ) -> Vec<(Symbol, Option<crate::relationship::RelationshipMetadata>)> {
        let relationships = self
            .document_index
            .get_relationships_from(symbol_id, RelationKind::Calls)
            .unwrap_or_default();

        let mut results = Vec::new();
        for (_, to_id, rel) in relationships {
            if let Some(symbol) = self.get_symbol(to_id) {
                results.push((symbol, rel.metadata));
            }
        }
        results
    }

    /// Get functions that call a symbol.
    pub fn get_calling_functions(&self, symbol_id: SymbolId) -> Vec<Symbol> {
        let relationships = self
            .document_index
            .get_relationships_to(symbol_id, RelationKind::Calls)
            .unwrap_or_default();

        let mut symbols = Vec::new();
        for (from_id, _, _) in relationships {
            if let Some(symbol) = self.get_symbol(from_id) {
                symbols.push(symbol);
            }
        }
        symbols
    }

    /// Get functions that call a symbol with metadata.
    pub fn get_calling_functions_with_metadata(
        &self,
        symbol_id: SymbolId,
    ) -> Vec<(Symbol, Option<crate::relationship::RelationshipMetadata>)> {
        let relationships = self
            .document_index
            .get_relationships_to(symbol_id, RelationKind::Calls)
            .unwrap_or_default();

        let mut results = Vec::new();
        for (from_id, _, rel) in relationships {
            if let Some(symbol) = self.get_symbol(from_id) {
                results.push((symbol, rel.metadata));
            }
        }
        results
    }

    /// Get implementations of a trait/interface.
    pub fn get_implementations(&self, trait_id: SymbolId) -> Vec<Symbol> {
        let relationships = self
            .document_index
            .get_relationships_to(trait_id, RelationKind::Implements)
            .unwrap_or_default();

        let mut symbols = Vec::new();
        for (from_id, _, _) in relationships {
            if let Some(symbol) = self.get_symbol(from_id) {
                symbols.push(symbol);
            }
        }
        symbols
    }

    /// Get traits implemented by a type.
    pub fn get_implemented_traits(&self, type_id: SymbolId) -> Vec<Symbol> {
        let relationships = self
            .document_index
            .get_relationships_from(type_id, RelationKind::Implements)
            .unwrap_or_default();

        let mut symbols = Vec::new();
        for (_, to_id, _) in relationships {
            if let Some(symbol) = self.get_symbol(to_id) {
                symbols.push(symbol);
            }
        }
        symbols
    }

    /// Get classes/types extended by a class.
    pub fn get_extends(&self, class_id: SymbolId) -> Vec<Symbol> {
        let relationships = self
            .document_index
            .get_relationships_from(class_id, RelationKind::Extends)
            .unwrap_or_default();

        let mut symbols = Vec::new();
        for (_, to_id, _) in relationships {
            if let Some(symbol) = self.get_symbol(to_id) {
                symbols.push(symbol);
            }
        }
        symbols
    }

    /// Get classes that extend a base class.
    pub fn get_extended_by(&self, base_class_id: SymbolId) -> Vec<Symbol> {
        let relationships = self
            .document_index
            .get_relationships_to(base_class_id, RelationKind::Extends)
            .unwrap_or_default();

        let mut symbols = Vec::new();
        for (from_id, _, _) in relationships {
            if let Some(symbol) = self.get_symbol(from_id) {
                symbols.push(symbol);
            }
        }
        symbols
    }

    /// Get types/symbols used by a symbol.
    pub fn get_uses(&self, symbol_id: SymbolId) -> Vec<Symbol> {
        let relationships = self
            .document_index
            .get_relationships_from(symbol_id, RelationKind::Uses)
            .unwrap_or_default();

        let mut symbols = Vec::new();
        for (_, to_id, _) in relationships {
            if let Some(symbol) = self.get_symbol(to_id) {
                symbols.push(symbol);
            }
        }
        symbols
    }

    /// Get symbols that use a type.
    pub fn get_used_by(&self, type_id: SymbolId) -> Vec<Symbol> {
        let relationships = self
            .document_index
            .get_relationships_to(type_id, RelationKind::Uses)
            .unwrap_or_default();

        let mut symbols = Vec::new();
        for (from_id, _, _) in relationships {
            if let Some(symbol) = self.get_symbol(from_id) {
                symbols.push(symbol);
            }
        }
        symbols
    }

    /// Get relationships for a symbol (by symbol ID).
    pub fn get_relationships_for_symbol(
        &self,
        symbol_id: SymbolId,
    ) -> FacadeResult<Vec<(SymbolId, SymbolId, Relationship)>> {
        let mut all_rels = Vec::new();

        // Get outgoing relationships
        for kind in &[
            RelationKind::Calls,
            RelationKind::Uses,
            RelationKind::Implements,
            RelationKind::Extends,
            RelationKind::Defines,
        ] {
            if let Ok(rels) = self.document_index.get_relationships_from(symbol_id, *kind) {
                all_rels.extend(rels);
            }
        }

        // Get incoming relationships
        for kind in &[
            RelationKind::Calls,
            RelationKind::Uses,
            RelationKind::Implements,
            RelationKind::Extends,
        ] {
            if let Ok(rels) = self.document_index.get_relationships_to(symbol_id, *kind) {
                all_rels.extend(rels);
            }
        }

        Ok(all_rels)
    }

    // =========================================================================
    // Complex Query Methods (facade-level orchestration)
    // =========================================================================

    /// Get symbol context with configurable relationship inclusion.
    pub fn get_symbol_context(
        &self,
        symbol_id: SymbolId,
        include: ContextIncludes,
    ) -> Option<SymbolContext> {
        let symbol = self.get_symbol(symbol_id)?;
        let file_path = self
            .document_index
            .get_file_path(symbol.file_id)
            .ok()
            .flatten()
            .unwrap_or_else(|| symbol.file_path.to_string());

        let mut relationships = SymbolRelationships::default();

        if include.contains(ContextIncludes::IMPLEMENTATIONS) {
            let impls = self.get_implementations(symbol_id);
            if !impls.is_empty() {
                relationships.implemented_by = Some(impls);
            }
            // Also get what this type implements
            let implemented = self.get_implemented_traits(symbol_id);
            if !implemented.is_empty() {
                relationships.implements = Some(implemented);
            }
        }

        if include.contains(ContextIncludes::DEFINITIONS) {
            if let Ok(rels) = self
                .document_index
                .get_relationships_from(symbol_id, RelationKind::Defines)
            {
                let defines: Vec<Symbol> = rels
                    .iter()
                    .filter_map(|(_, to_id, _)| self.get_symbol(*to_id))
                    .collect();
                if !defines.is_empty() {
                    relationships.defines = Some(defines);
                }
            }
        }

        if include.contains(ContextIncludes::CALLS) {
            let calls = self.get_called_functions_with_metadata(symbol_id);
            if !calls.is_empty() {
                relationships.calls = Some(calls);
            }
        }

        if include.contains(ContextIncludes::CALLERS) {
            let callers = self.get_calling_functions_with_metadata(symbol_id);
            if !callers.is_empty() {
                relationships.called_by = Some(callers);
            }
        }

        if include.contains(ContextIncludes::EXTENDS) {
            let extends = self.get_extends(symbol_id);
            if !extends.is_empty() {
                relationships.extends = Some(extends);
            }
            let extended_by = self.get_extended_by(symbol_id);
            if !extended_by.is_empty() {
                relationships.extended_by = Some(extended_by);
            }
        }

        if include.contains(ContextIncludes::USES) {
            let uses = self.get_uses(symbol_id);
            if !uses.is_empty() {
                relationships.uses = Some(uses);
            }
            let used_by = self.get_used_by(symbol_id);
            if !used_by.is_empty() {
                relationships.used_by = Some(used_by);
            }
        }

        Some(SymbolContext {
            symbol,
            file_path,
            relationships,
        })
    }

    /// Get dependencies (what a symbol depends on).
    pub fn get_dependencies(&self, symbol_id: SymbolId) -> HashMap<RelationKind, Vec<Symbol>> {
        let mut deps: HashMap<RelationKind, Vec<Symbol>> = HashMap::new();

        for kind in &[
            RelationKind::Calls,
            RelationKind::Uses,
            RelationKind::Implements,
            RelationKind::Defines,
        ] {
            let rels = self
                .document_index
                .get_relationships_from(symbol_id, *kind)
                .unwrap_or_default();
            let symbols: Vec<Symbol> = rels
                .iter()
                .filter_map(|(_, to_id, _)| self.get_symbol(*to_id))
                .collect();
            if !symbols.is_empty() {
                deps.insert(*kind, symbols);
            }
        }

        deps
    }

    /// Get dependents (what depends on a symbol).
    pub fn get_dependents(&self, symbol_id: SymbolId) -> HashMap<RelationKind, Vec<Symbol>> {
        let mut deps: HashMap<RelationKind, Vec<Symbol>> = HashMap::new();

        for kind in &[
            RelationKind::Calls,
            RelationKind::Uses,
            RelationKind::Implements,
        ] {
            let rels = self
                .document_index
                .get_relationships_to(symbol_id, *kind)
                .unwrap_or_default();
            let symbols: Vec<Symbol> = rels
                .iter()
                .filter_map(|(from_id, _, _)| self.get_symbol(*from_id))
                .collect();
            if !symbols.is_empty() {
                deps.insert(*kind, symbols);
            }
        }

        deps
    }

    /// Get impact radius (BFS traversal of dependents).
    pub fn get_impact_radius(
        &self,
        symbol_id: SymbolId,
        max_depth: Option<usize>,
    ) -> Vec<SymbolId> {
        let max_depth = max_depth.unwrap_or(2);
        let mut visited = HashSet::new();
        let mut queue = std::collections::VecDeque::new();

        queue.push_back((symbol_id, 0usize));
        visited.insert(symbol_id);

        while let Some((current_id, depth)) = queue.pop_front() {
            if depth >= max_depth {
                continue;
            }

            // Get dependents via Calls, Uses, Implements, Extends
            for kind in &[
                RelationKind::Calls,
                RelationKind::Uses,
                RelationKind::Implements,
                RelationKind::Extends,
            ] {
                if let Ok(rels) = self.document_index.get_relationships_to(current_id, *kind) {
                    for (from_id, _, _) in rels {
                        if visited.insert(from_id) {
                            queue.push_back((from_id, depth + 1));
                        }
                    }
                }
            }
        }

        // Remove the initial symbol from results
        visited.remove(&symbol_id);
        visited.into_iter().collect()
    }

    // =========================================================================
    // Search Methods
    // =========================================================================

    /// Full-text search for symbols.
    pub fn search(
        &self,
        query: &str,
        limit: usize,
        kind_filter: Option<SymbolKind>,
        module_filter: Option<&str>,
        language_filter: Option<&str>,
    ) -> FacadeResult<Vec<SearchResult>> {
        self.document_index
            .search(query, limit, kind_filter, module_filter, language_filter)
            .map_err(Into::into)
    }

    /// Semantic search using doc comment embeddings.
    pub fn semantic_search_docs(
        &self,
        query: &str,
        limit: usize,
    ) -> FacadeResult<Vec<(Symbol, f32)>> {
        self.semantic_search_docs_with_language(query, limit, None)
    }

    /// Semantic search with language filter.
    pub fn semantic_search_docs_with_language(
        &self,
        query: &str,
        limit: usize,
        language_filter: Option<&str>,
    ) -> FacadeResult<Vec<(Symbol, f32)>> {
        let semantic = self
            .semantic_search
            .as_ref()
            .ok_or(IndexError::SemanticSearchNotEnabled)?;

        let sem = semantic.lock().map_err(|_| IndexError::lock_error())?;

        // When the semantic search has no local model (built with remote embeddings),
        // generate the query vector via the embedding backend regardless of whether
        // the backend is currently remote or local — the pool just needs to produce
        // a vector of the right dimension.
        let results = if sem.has_local_model() {
            sem.search_with_language(query, limit, language_filter)?
        } else {
            let pool = self.embedding_pool.as_ref().ok_or_else(|| {
                IndexError::General(
                    "Remote-mode index requires an embedding backend for queries. \
                     Set CODANNA_EMBED_URL or re-index with a local model."
                        .to_string(),
                )
            })?;
            let query_vec = pool.embed_one(query)?;
            sem.search_with_embedding_and_language(&query_vec, limit, language_filter)?
        };

        let mut symbols = Vec::new();
        for (symbol_id, score) in results {
            if let Some(symbol) = self.get_symbol(symbol_id) {
                symbols.push((symbol, score));
            }
        }

        Ok(symbols)
    }

    /// Semantic search with score threshold.
    pub fn semantic_search_docs_with_threshold(
        &self,
        query: &str,
        limit: usize,
        threshold: f32,
    ) -> FacadeResult<Vec<(Symbol, f32)>> {
        self.semantic_search_docs_with_threshold_and_language(query, limit, threshold, None)
    }

    /// Semantic search with threshold and language filter.
    pub fn semantic_search_docs_with_threshold_and_language(
        &self,
        query: &str,
        limit: usize,
        threshold: f32,
        language_filter: Option<&str>,
    ) -> FacadeResult<Vec<(Symbol, f32)>> {
        let results = self.semantic_search_docs_with_language(query, limit, language_filter)?;

        Ok(results
            .into_iter()
            .filter(|(_, score)| *score >= threshold)
            .collect())
    }

    // =========================================================================
    // File Operations
    // =========================================================================

    /// Get file ID for a path.
    pub fn get_file_id_for_path(&self, path: &str) -> Option<FileId> {
        self.document_index
            .get_file_info(path)
            .ok()
            .flatten()
            .map(|(id, _, _)| id)
    }

    /// Get file path for a FileId.
    ///
    /// Returns None on error for SimpleIndexer API compatibility.
    pub fn get_file_path(&self, file_id: FileId) -> Option<String> {
        self.document_index.get_file_path(file_id).ok().flatten()
    }

    /// Get the stored content hash for a file path.
    ///
    /// Delegates to `DocumentIndex::get_file_info`. Returns None on error or
    /// if the path has no indexed file-info entry, for SimpleIndexer API
    /// compatibility.
    pub fn get_file_hash_for_path(&self, path: &str) -> Option<String> {
        self.document_index
            .get_file_info(path)
            .ok()
            .flatten()
            .map(|(_, hash, _)| hash)
    }

    /// Get all indexed file paths.
    pub fn get_all_indexed_paths(&self) -> Vec<PathBuf> {
        self.document_index
            .get_all_indexed_paths()
            .unwrap_or_default()
    }

    // =========================================================================
    // Statistics Methods
    // =========================================================================

    /// Get the number of indexed symbols.
    pub fn symbol_count(&self) -> usize {
        self.document_index.count_symbols().unwrap_or(0)
    }

    /// Get the number of indexed files.
    pub fn file_count(&self) -> u32 {
        self.document_index.count_files().unwrap_or(0) as u32
    }

    /// Get the number of relationships.
    pub fn relationship_count(&self) -> usize {
        self.document_index.count_relationships().unwrap_or(0)
    }

    /// Get total Tantivy document count.
    pub fn document_count(&self) -> FacadeResult<u64> {
        self.document_index.document_count().map_err(Into::into)
    }

    // =========================================================================
    // Directory Tracking
    // =========================================================================

    /// Add a directory to tracked indexed paths.
    pub fn add_indexed_path(&mut self, dir_path: &Path) {
        if let Ok(canonical) = dir_path.canonicalize() {
            // Skip if already covered by an existing parent directory
            let already_covered = self
                .indexed_paths
                .iter()
                .any(|p| canonical.starts_with(p) && canonical != *p);
            if already_covered {
                return;
            }

            // Remove any child paths that would be covered by this directory
            self.indexed_paths
                .retain(|p| !p.starts_with(&canonical) || *p == canonical);
            self.indexed_paths.insert(canonical);
        } else {
            self.indexed_paths.insert(dir_path.to_path_buf());
        }
    }

    /// Get tracked indexed paths.
    pub fn get_indexed_paths(&self) -> &HashSet<PathBuf> {
        &self.indexed_paths
    }

    /// Update indexed paths from a vector.
    pub fn set_indexed_paths(&mut self, paths: Vec<PathBuf>) {
        self.indexed_paths = paths.into_iter().collect();
    }

    // =========================================================================
    // Mutation Methods (delegate to Pipeline)
    // =========================================================================

    /// Index a single file using the parallel pipeline.
    ///
    /// Returns `IndexingResult::Indexed` with the file ID on success.
    pub fn index_file(
        &mut self,
        path: impl AsRef<std::path::Path>,
    ) -> crate::IndexResult<crate::IndexingResult> {
        let path = path.as_ref();
        if self.has_semantic_search() {
            if let Err(e) = self.ensure_embedding_pool() {
                tracing::warn!("Failed to initialize embedding pool: {e}");
            }
        }
        let stats = self.pipeline.index_file_single(
            path,
            Arc::clone(&self.document_index),
            self.semantic_search.clone(),
            self.embedding_pool.clone(),
        )?;

        Ok(crate::IndexingResult::Indexed(stats.file_id))
    }

    /// Index a single file with optional force re-indexing.
    ///
    /// When `force` is true, removes the file first to ensure a fresh re-index.
    pub fn index_file_with_force(
        &mut self,
        path: impl AsRef<std::path::Path>,
        force: bool,
    ) -> crate::IndexResult<crate::IndexingResult> {
        let path = path.as_ref();

        if force {
            // Remove first to force re-index. Not-indexed files return Ok,
            // so any error here is a real cleanup failure and must not be
            // masked: swallowing it desyncs the semantic store from Tantivy.
            self.remove_file(path)?;
        }

        self.index_file(path)
    }

    /// Remove a file from the index.
    ///
    /// Uses the Pipeline's cleanup stage to remove symbols and embeddings.
    pub fn remove_file(&mut self, path: impl AsRef<std::path::Path>) -> crate::IndexResult<()> {
        let path = path.as_ref();
        let semantic_path = self.settings.index_path.join("semantic");

        use crate::indexing::pipeline::stages::CleanupStage;
        let cleanup_stage = if let Some(ref sem) = self.semantic_search {
            CleanupStage::new(Arc::clone(&self.document_index), &semantic_path)
                .with_semantic(Arc::clone(sem))
        } else {
            CleanupStage::new(Arc::clone(&self.document_index), &semantic_path)
        };

        cleanup_stage.cleanup_files(&[path.to_path_buf()])?;
        Ok(())
    }

    /// Clear all documents from the index.
    ///
    /// Reuses the already-open `DocumentIndex`/Tantivy writer handle and the
    /// in-memory semantic search store rather than removing files on disk or
    /// constructing new writers. Resets directory tracking so a subsequent
    /// `index_directory` call re-populates `indexed_paths` from scratch.
    pub fn clear_index(&mut self) -> FacadeResult<()> {
        self.document_index.clear()?;

        if let Some(ref semantic) = self.semantic_search {
            let mut sem = semantic
                .lock()
                .map_err(|e| IndexError::LockError(format!("semantic search: {e}")))?;
            sem.clear();
        }

        self.indexed_paths.clear();

        Ok(())
    }

    /// Index a directory using the parallel pipeline.
    ///
    /// This is the primary indexing entry point using Pipeline.
    pub fn index_directory(&mut self, path: &Path, force: bool) -> FacadeResult<IndexingStats> {
        if self.has_semantic_search() {
            if let Err(e) = self.ensure_embedding_pool() {
                tracing::warn!("Failed to initialize embedding pool: {e}");
            }
        }
        let stats = self.pipeline.index_incremental(
            path,
            Arc::clone(&self.document_index),
            self.semantic_search.clone(),
            self.embedding_pool.clone(),
            force,
        )?;

        // Update tracked paths
        self.add_indexed_path(path);

        Ok(IndexingStats {
            files_indexed: stats.new_files + stats.modified_files,
            symbols_found: stats.index_stats.symbols_found,
            relationships_resolved: stats.phase2_stats.defines_resolved
                + stats.phase2_stats.calls_resolved
                + stats.phase2_stats.other_resolved,
            files_removed: stats.deleted_files,
            symbols_removed: stats.deleted_symbols,
        })
    }

    /// Index a directory with advanced options.
    ///
    /// Provides options for progress reporting, dry-run mode, force re-indexing,
    /// and limiting the number of files.
    pub fn index_directory_with_options(
        &mut self,
        dir: impl AsRef<Path>,
        progress: bool,
        dry_run: bool,
        force: bool,
        max_files: Option<usize>,
        dry_run_output: DryRunOutput,
    ) -> crate::IndexResult<crate::indexing::progress::IndexStats> {
        use crate::indexing::FileWalker;
        use crate::indexing::progress::IndexStats;

        let dir = dir.as_ref();
        let walker = FileWalker::new(Arc::clone(&self.settings));
        let files: Vec<_> = walker.walk(dir)?.collect();

        // Apply max_files limit if specified
        let files = if let Some(max) = max_files {
            files.into_iter().take(max).collect()
        } else {
            files
        };

        let total_files = files.len();

        // Handle dry-run mode
        if dry_run {
            match dry_run_output {
                DryRunOutput::Json => {
                    // `--json` prints nothing but the array itself: a truncated
                    // JSON array would repeat the very bug this flag exists to fix.
                    let paths: Vec<String> =
                        files.iter().map(|p| p.display().to_string()).collect();
                    // Never substitute an empty array on failure: printing "no
                    // files" when the walk found some is the class of silent lie
                    // this flag exists to eliminate.
                    let json = serde_json::to_string(&paths).map_err(|e| {
                        IndexError::General(format!(
                            "failed to serialize dry-run file list as JSON: {e}"
                        ))
                    })?;
                    println!("{json}");
                }
                DryRunOutput::ListAll => {
                    println!("Would index {total_files} files:");
                    for file_path in &files {
                        println!("  {}", file_path.display());
                    }
                }
                DryRunOutput::Summary => {
                    println!("Would index {total_files} files:");
                    for (i, file_path) in files.iter().enumerate() {
                        if i < 5 {
                            println!("  {}", file_path.display());
                        } else if i == 5 && total_files > 5 {
                            println!("  ... and {} more files", total_files - 5);
                            break;
                        }
                    }
                }
            }

            let mut stats = IndexStats::new();
            stats.files_indexed = total_files;
            return Ok(stats);
        }

        // Auto-force mode for empty indexes (clean index behaves like --force)
        let force = force || self.document_count().unwrap_or(0) == 0;

        if self.has_semantic_search() {
            if let Err(e) = self.ensure_embedding_pool() {
                tracing::warn!("Failed to initialize embedding pool: {e}");
            }
        }

        // Use Pipeline for indexing with progress flag
        // The pipeline manages progress bars internally for clean sequential display
        let pipeline_stats = self.pipeline.index_incremental_with_progress_flag(
            dir,
            Arc::clone(&self.document_index),
            self.semantic_search.clone(),
            self.embedding_pool.clone(),
            force,
            progress && total_files > 0,
            total_files,
        )?;

        // Update tracked paths
        self.add_indexed_path(dir);

        // Convert to IndexStats format using pipeline's actual timing
        let mut stats = IndexStats::default();
        stats.files_indexed = pipeline_stats.new_files + pipeline_stats.modified_files;
        stats.symbols_found = pipeline_stats.index_stats.symbols_found;
        stats.files_removed = pipeline_stats.deleted_files;
        stats.symbols_removed = pipeline_stats.deleted_symbols;
        stats.elapsed = pipeline_stats.elapsed;

        Ok(stats)
    }

    /// Sync with configuration (compare stored vs config paths).
    ///
    /// Returns (added_dirs, removed_dirs, files_indexed, symbols_found).
    pub fn sync_with_config(
        &mut self,
        stored_paths: Option<Vec<PathBuf>>,
        config_paths: &[PathBuf],
        progress: bool,
    ) -> FacadeResult<SyncStats> {
        let stored = stored_paths.unwrap_or_default();
        let stored_set: HashSet<PathBuf> = stored.iter().cloned().collect();
        let config_set: HashSet<PathBuf> = config_paths.iter().cloned().collect();

        // Determine what to add and remove
        let to_add: Vec<&PathBuf> = config_set.difference(&stored_set).collect();
        let to_remove: Vec<&PathBuf> = stored_set.difference(&config_set).collect();

        let mut stats = SyncStats::default();

        if self.has_semantic_search() && !to_add.is_empty() {
            if let Err(e) = self.ensure_embedding_pool() {
                tracing::warn!("Failed to initialize embedding pool: {e}");
            }
        }

        // Index new directories with progress if enabled
        // Use force=true since these are new directories being indexed for the first time
        for path in &to_add {
            // Visual separator and directory label (stderr syncs with progress bars)
            eprintln!();
            eprintln!("Indexing directory: {}", path.display());

            // Count files first for accurate progress bar. Uses `walk_quiet`
            // rather than `walk` because `index_incremental_with_progress_flag`
            // below performs its own full walk of the same directory via
            // `DiscoverStage`; both walk sites call
            // `warn_if_skipped_symlink_dir` per entry, so warning here too
            // would log a symlinked-directory skip twice per run.
            let file_count = if progress {
                use crate::indexing::FileWalker;
                let walker = FileWalker::new(Arc::clone(&self.settings));
                walker.walk_quiet(path)?.count()
            } else {
                0
            };

            let result = self.pipeline.index_incremental_with_progress_flag(
                path,
                Arc::clone(&self.document_index),
                self.semantic_search.clone(),
                self.embedding_pool.clone(),
                true, // force: new directories should be fully indexed
                progress,
                file_count,
            )?;
            stats.files_indexed += result.new_files + result.modified_files;
            stats.symbols_found += result.index_stats.symbols_found;
        }
        stats.added_dirs = to_add.len();

        // Remove files from removed directories
        for path in &to_remove {
            self.remove_directory_files(path)?;
        }
        stats.removed_dirs = to_remove.len();

        // Update tracked paths
        self.indexed_paths = config_set;

        Ok(stats)
    }

    /// Remove all files from a directory.
    fn remove_directory_files(&self, _dir: &Path) -> FacadeResult<()> {
        // TODO: Implement using CleanupStage
        // For now, this is a placeholder
        Ok(())
    }

    /// Captures cloneable handles under the caller's lock so the heavy walk
    /// can run with no facade lock held; mirrors the lock-acquire-then-swap
    /// pattern used by `watcher::hot_reload` to keep the write lock window
    /// short.
    pub fn snapshot_reindex_handles(&mut self) -> FacadeResult<ReindexHandles> {
        if self.has_semantic_search() {
            self.ensure_embedding_pool()?;
        }

        Ok(ReindexHandles {
            pipeline: self.pipeline.clone(),
            document_index: Arc::clone(&self.document_index),
            semantic_search: self.semantic_search.clone(),
            embedding_pool: self.embedding_pool.clone(),
        })
    }
}

// =========================================================================
// Off-lock reindex seam
// =========================================================================

/// Move-only bundle of cloned handles needed to run a reindex walk without
/// holding the `IndexFacade` lock.
///
/// Captured via [`IndexFacade::snapshot_reindex_handles`] and consumed once
/// by [`ReindexHandles::run`].
pub struct ReindexHandles {
    pipeline: Pipeline,
    document_index: Arc<DocumentIndex>,
    semantic_search: Option<Arc<Mutex<SimpleSemanticSearch>>>,
    embedding_pool: Option<Arc<EmbeddingBackend>>,
}

/// Outcome of an off-lock reindex walk.
#[derive(Debug, Clone)]
pub struct ReindexOutcome {
    pub reindexed: usize,
    pub symbol_count: usize,
    pub indexed_dirs: Vec<PathBuf>,
}

impl ReindexHandles {
    /// Runs the reindex walk without holding the facade lock, consuming the
    /// handles captured by [`IndexFacade::snapshot_reindex_handles`].
    ///
    /// Preserves the branch behavior previously implemented in the MCP
    /// server's request handler:
    /// - An explicit file path is indexed via `Pipeline::index_file_single`.
    ///   When `force` is true, the file's existing symbols/embeddings are
    ///   removed first so a re-parse always runs even if its content hash
    ///   is unchanged (mirrors `IndexFacade::index_file_with_force`).
    /// - An explicit directory path is indexed via `Pipeline::index_incremental`
    ///   with the caller-supplied `force` flag.
    /// - When `paths` is `None`, every directory in `indexing.indexed_paths`
    ///   (from the pipeline's settings) is indexed with the caller-supplied
    ///   `force` flag. For the `paths: None` case this is redundant with any
    ///   clear the caller already ran under lock (force mode does a full
    ///   walk of an already-empty index either way), but passing it through
    ///   keeps this call site honoring `force` rather than reading as if it
    ///   were silently dropped.
    ///
    /// Per-path failures are logged with `tracing::warn!` and skipped rather
    /// than aborting the whole walk. Successfully indexed directories are
    /// collected into `ReindexOutcome::indexed_dirs` for the caller to record
    /// via `IndexFacade::add_indexed_path`.
    pub fn run(self, paths: Option<Vec<String>>, force: bool) -> FacadeResult<ReindexOutcome> {
        let ReindexHandles {
            pipeline,
            document_index,
            semantic_search,
            embedding_pool,
        } = self;

        // A malformed `ignore_patterns` entry is a deterministic misconfig,
        // not a transient per-path failure: it fails identically on every
        // path in the loop below. Validate once, up front, and propagate a
        // hard error rather than letting the per-path catch-and-warn below
        // reduce it to `tracing::warn!` while still reporting "reindexed 0
        // files" as if nothing were wrong.
        crate::indexing::walk_config::validate_ignore_patterns(pipeline.settings())?;

        let mut indexed_dirs = Vec::new();

        let reindexed = if let Some(paths) = paths {
            let mut total_reindexed = 0;
            for path in &paths {
                let path = Path::new(path);
                if path.is_file() {
                    if force {
                        // `index_file_single` no-ops (unchanged-hash skip)
                        // when the file's content hash matches what's
                        // already indexed, which would silently drop
                        // `force` for an explicit file path. Remove the
                        // file's existing symbols/embeddings first so the
                        // subsequent call always re-parses, mirroring
                        // `IndexFacade::index_file_with_force`.
                        use crate::indexing::pipeline::stages::CleanupStage;
                        let semantic_path = pipeline.settings().index_path.join("semantic");
                        let cleanup_stage = if let Some(ref sem) = semantic_search {
                            CleanupStage::new(Arc::clone(&document_index), &semantic_path)
                                .with_semantic(Arc::clone(sem))
                        } else {
                            CleanupStage::new(Arc::clone(&document_index), &semantic_path)
                        };
                        if let Err(e) = cleanup_stage.cleanup_files(&[path.to_path_buf()]) {
                            tracing::warn!(
                                "Failed to clear {} before force reindex: {e}",
                                path.display()
                            );
                        }
                    }
                    match pipeline.index_file_single(
                        path,
                        Arc::clone(&document_index),
                        semantic_search.clone(),
                        embedding_pool.clone(),
                    ) {
                        Ok(_stats) => {
                            // Mirrors the original `run_reindex` handler
                            // (server.rs), which counted any successfully
                            // processed explicit file path as reindexed
                            // regardless of cache status, since
                            // `IndexFacade::index_file` never actually
                            // produced `IndexingResult::Cached`.
                            total_reindexed += 1;
                        }
                        Err(e) => {
                            tracing::warn!("Failed to reindex {}: {e}", path.display());
                        }
                    }
                } else if path.is_dir() {
                    match pipeline.index_incremental(
                        path,
                        Arc::clone(&document_index),
                        semantic_search.clone(),
                        embedding_pool.clone(),
                        force,
                    ) {
                        Ok(stats) => {
                            total_reindexed += stats.new_files + stats.modified_files;
                            indexed_dirs.push(path.to_path_buf());
                        }
                        Err(e) => {
                            tracing::warn!("Failed to reindex {}: {e}", path.display());
                        }
                    }
                }
            }
            total_reindexed
        } else {
            let indexed_paths = pipeline.settings().indexing.indexed_paths.clone();
            let mut total_reindexed = 0;
            for path in &indexed_paths {
                if path.is_dir() {
                    match pipeline.index_incremental(
                        path,
                        Arc::clone(&document_index),
                        semantic_search.clone(),
                        embedding_pool.clone(),
                        force,
                    ) {
                        Ok(stats) => {
                            total_reindexed += stats.new_files + stats.modified_files;
                            indexed_dirs.push(path.clone());
                        }
                        Err(e) => {
                            tracing::warn!("Failed to reindex {}: {e}", path.display());
                        }
                    }
                }
            }
            total_reindexed
        };

        let symbol_count = document_index.count_symbols().unwrap_or(0);

        Ok(ReindexOutcome {
            reindexed,
            symbol_count,
            indexed_dirs,
        })
    }
}

/// Runs the full 3-phase reindex orchestration (brief write lock ->
/// off-lock walk -> brief write lock) against a shared, lock-guarded
/// facade.
///
/// This is the single seam through which both the MCP server's reindex
/// request handler and the file-watcher's catch-up path drive a reindex, so
/// the phase ordering (snapshot handles under lock, run the heavy walk with
/// no lock held, then record indexed directories under lock again) is
/// guaranteed regardless of caller.
///
/// - Phase 1: acquires a brief write lock. When `paths` is `None` and
///   `force` is `true`, clears the index first; then snapshots cloneable
///   reindex handles via [`IndexFacade::snapshot_reindex_handles`].
/// - Phase 2: with the write guard already dropped, runs the heavy reindex
///   walk off-lock via [`ReindexHandles::run`] on a blocking thread.
/// - Phase 3: acquires a brief write lock again to record any newly
///   indexed directories via [`IndexFacade::add_indexed_path`].
///
/// `phase2_started`, when provided, is signaled the instant phase 1's write
/// guard has been dropped and before the off-lock walk begins; this exists
/// for test synchronization and is `None` in production call sites.
///
/// Callers MUST validate that every entry in `paths` is contained within the
/// workspace root before calling; this seam does not re-check path
/// containment itself (the MCP handler validates before calling; the
/// watcher's catch-up path always passes `paths: None`).
///
/// A per-facade [`tokio::sync::Semaphore`] (see
/// [`IndexFacade::reindex_gate`]) serializes full reindex runs: only one
/// `reindex_locked` invocation may be in flight against a given facade at a
/// time. The permit is acquired strictly before phase 1's write lock and
/// held across all three phases, including the off-lock phase 2 walk, so a
/// concurrent caller (e.g. an MCP `reindex(force: true)` racing the
/// watcher's overflow catch-up reindex) cannot observe phase 1's
/// `clear_index()` mid-way through another run's phase 2 batch. A caller
/// that loses the race is rejected immediately with
/// [`IndexError::ReindexInProgress`] rather than queued, since a queued
/// duplicate force-reindex would be wasted work that pins the caller open
/// for the duration of someone else's multi-minute run.
pub(crate) async fn reindex_locked(
    facade: &Arc<tokio::sync::RwLock<IndexFacade>>,
    paths: Option<Vec<String>>,
    force: bool,
    phase2_started: Option<tokio::sync::oneshot::Sender<()>>,
) -> FacadeResult<ReindexOutcome> {
    let should_clear = paths.is_none() && force;

    // Take a brief read lock purely to clone the gate handle, then drop it
    // before acquiring the write lock below (mirrors the brief-read-lock
    // pattern in src/mcp/server.rs around the workspace-root containment
    // check) so there is no deadlock between this read and phase 1's write.
    let gate = {
        let indexer = facade.read().await;
        indexer.reindex_gate()
    };
    let _reindex_permit = gate.try_acquire_owned().map_err(|_| {
        tracing::warn!("Rejecting reindex request: another full reindex is already in progress");
        IndexError::ReindexInProgress
    })?;

    // Phase 1: brief write lock to optionally clear the index and snapshot
    // cloneable handles for the off-lock reindex walk. `clear_index()`
    // performs blocking Tantivy IO (commit, reader reload), so the owned
    // guard is moved into `spawn_blocking` rather than doing that work
    // directly on the async worker while the write lock is held.
    let owned_guard = Arc::clone(facade).write_owned().await;
    let handles = tokio::task::spawn_blocking(move || -> FacadeResult<ReindexHandles> {
        let mut indexer = owned_guard;

        if should_clear {
            // Log per-phase context for on-call readers, but propagate the
            // original typed `IndexError` variant (e.g. `LockError`,
            // `TantivyError`) unchanged rather than flattening it into a
            // `General(String)`, so `status_code()`/`recovery_suggestions()`
            // remain available to callers.
            indexer.clear_index().inspect_err(|e| {
                tracing::error!("Failed to clear index before force reindex: {e}");
            })?;
        }

        indexer.snapshot_reindex_handles().inspect_err(|e| {
            tracing::error!("Failed to snapshot reindex handles: {e}");
        })
        // `indexer` (the owned write guard) is dropped here, releasing the
        // lock before phase 2's off-lock walk begins.
    })
    .await
    .map_err(map_reindex_join_error)??;

    // The write guard above is dropped at the end of the blocking closure,
    // strictly before this point. Signal test observers that phase 2 (the
    // off-lock walk) is about to begin.
    if let Some(tx) = phase2_started {
        let _ = tx.send(());
    }

    // Phase 2: run the heavy reindex walk with no facade lock held.
    let outcome = tokio::task::spawn_blocking(move || handles.run(paths, force))
        .await
        .map_err(map_reindex_join_error)??;

    // Phase 3: brief write lock to record any newly indexed directories.
    {
        let mut indexer = facade.write().await;
        for dir in &outcome.indexed_dirs {
            indexer.add_indexed_path(dir);
        }
    }

    Ok(outcome)
}

/// Maps a `tokio::task::JoinError` from a `reindex_locked` blocking stage to
/// an `IndexError`, distinguishing cancellation (e.g. runtime shutdown) from
/// an actual panic inside the task.
fn map_reindex_join_error(e: tokio::task::JoinError) -> IndexError {
    IndexError::General(format!("reindex {}", crate::utils::describe_join_error(&e)))
}

// ── Embedding backend factory ──────────────────────────────────────────────

/// Resolve the effective remote model name, applying env-var-first precedence.
///
/// Both `build_embedding_backend` and `new_empty` call sites use this so that
/// the model name embedded in saved metadata always matches what the backend uses.
pub fn resolve_remote_model_name(cfg: &crate::config::SemanticSearchConfig) -> String {
    std::env::var("CODANNA_EMBED_MODEL")
        .ok()
        .or_else(|| cfg.remote_model.clone())
        .unwrap_or_else(|| "text-embedding-ada-002".to_string())
}

/// Format a human-readable semantic search status line for CLI output.
pub fn format_semantic_status(cfg: &crate::config::SemanticSearchConfig) -> String {
    let is_remote = std::env::var("CODANNA_EMBED_URL").is_ok() || cfg.remote_url.is_some();
    let threshold = cfg.threshold;

    if is_remote {
        let model = resolve_remote_model_name(cfg);
        format!("Semantic search enabled (backend: remote, model: {model}, threshold: {threshold})")
    } else {
        let model = &cfg.model;
        format!("Semantic search enabled (model: {model}, threshold: {threshold})")
    }
}

pub fn build_embedding_backend(
    cfg: &crate::config::SemanticSearchConfig,
) -> FacadeResult<EmbeddingBackend> {
    // Env vars override config file
    let remote_url = std::env::var("CODANNA_EMBED_URL")
        .ok()
        .or_else(|| cfg.remote_url.clone());

    if let Some(url) = remote_url {
        let model = resolve_remote_model_name(cfg);

        let dim: Option<usize> = match std::env::var("CODANNA_EMBED_DIM") {
            Ok(s) => {
                let parsed = s.parse::<usize>().map_err(|_| {
                    IndexError::General(format!(
                        "CODANNA_EMBED_DIM must be a positive integer, got: {s:?}"
                    ))
                })?;
                if parsed == 0 {
                    return Err(IndexError::General(
                        "CODANNA_EMBED_DIM must be greater than zero".to_string(),
                    ));
                }
                Some(parsed)
            }
            Err(_) => cfg.remote_dim,
        };

        // API key from env var only -- secrets must not live in config files.
        let api_key = std::env::var("CODANNA_EMBED_API_KEY").ok();

        tracing::info!(
            target: "semantic",
            "Using remote embedding backend: url={url} model={model} auth={}",
            if api_key.is_some() { "bearer" } else { "none" }
        );

        let url_owned = url.clone();
        let model_owned = model.clone();
        let embedder =
            run_async(
                async move { RemoteEmbedder::new(&url_owned, &model_owned, dim, api_key).await },
            )
            .map_err(|e| IndexError::General(format!("Remote embedder init failed: {e}")))?;

        return Ok(EmbeddingBackend::Remote(Arc::new(embedder)));
    }

    // Local fastembed pool
    let pool_size = cfg.embedding_threads;
    let embedding_model = crate::vector::parse_embedding_model(&cfg.model)
        .map_err(|e| IndexError::General(format!("Failed to parse embedding model: {e}")))?;
    let pool = EmbeddingPool::new(pool_size, embedding_model)
        .map_err(|e| IndexError::General(format!("Local embedding pool init failed: {e}")))?;

    Ok(EmbeddingBackend::Local(pool))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Regression: facade construction on a corrupt tantivy dir must return
    // Err, not panic. The CLI/server fallback paths call this exactly when
    // the index dir failed to load.
    #[test]
    fn new_returns_err_on_corrupt_tantivy_dir() {
        let dir = tempfile::tempdir().unwrap();
        let tantivy_dir = dir.path().join("tantivy");
        std::fs::create_dir_all(&tantivy_dir).unwrap();
        std::fs::write(tantivy_dir.join("meta.json"), b"not valid json").unwrap();

        let settings = Settings {
            index_path: dir.path().to_path_buf(),
            workspace_root: None,
            ..Default::default()
        };

        let result = IndexFacade::new(std::sync::Arc::new(settings));
        assert!(result.is_err());
    }

    // Regression: every symbol-card surface requests
    // ContextIncludes::SYMBOL_CARD. The CLI JSON paths used to request a
    // subset, rendering extends/extended_by/uses null while the MCP text
    // handler showed the same store's edges.
    #[test]
    fn symbol_card_context_carries_extends_both_directions() {
        use crate::symbol::context::ContextIncludes;

        let dir = tempfile::tempdir().unwrap();
        let settings = Settings {
            index_path: dir.path().join("index"),
            workspace_root: None,
            ..Default::default()
        };

        let source = dir.path().join("classes.py");
        std::fs::write(
            &source,
            "class Base:\n    def m(self):\n        pass\n\n\nclass Derived(Base):\n    def m(self):\n        pass\n",
        )
        .unwrap();

        let mut facade = IndexFacade::new(std::sync::Arc::new(settings)).unwrap();
        facade.index_file(&source).unwrap();

        let derived = facade
            .find_symbols_by_name("Derived", None)
            .pop()
            .expect("Derived indexed");
        let ctx = facade
            .get_symbol_context(derived.id, ContextIncludes::SYMBOL_CARD)
            .expect("context for Derived");
        let extends = ctx
            .relationships
            .extends
            .expect("extends fetched under SYMBOL_CARD");
        assert!(
            extends.iter().any(|s| s.name.as_ref() == "Base"),
            "Derived extends Base"
        );

        let base = facade
            .find_symbols_by_name("Base", None)
            .pop()
            .expect("Base indexed");
        let ctx = facade
            .get_symbol_context(base.id, ContextIncludes::SYMBOL_CARD)
            .expect("context for Base");
        let extended_by = ctx
            .relationships
            .extended_by
            .expect("extended_by fetched under SYMBOL_CARD");
        assert!(
            extended_by.iter().any(|s| s.name.as_ref() == "Derived"),
            "Base extended by Derived"
        );
    }

    // Regression: a deletion-only incremental run must surface removal
    // counts across the facade stats boundary instead of reading as a
    // no-op ("Index up to date"). Modified-file cleanup must NOT count:
    // its symbols re-add in the same run.
    #[test]
    fn deletion_only_run_reports_removal_counts() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("fixture");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("alpha.py"), "def alpha():\n    pass\n").unwrap();
        std::fs::write(
            root.join("beta.py"),
            "def beta_one():\n    pass\n\n\ndef beta_two():\n    pass\n",
        )
        .unwrap();

        let settings = Settings {
            index_path: dir.path().join("index"),
            workspace_root: None,
            ..Default::default()
        };
        let mut facade = IndexFacade::new(std::sync::Arc::new(settings)).unwrap();

        let seed = facade.index_directory(&root, false).unwrap();
        assert_eq!(seed.files_indexed, 2);
        assert_eq!(seed.files_removed, 0);

        std::fs::remove_file(root.join("beta.py")).unwrap();
        let stats = facade.index_directory(&root, false).unwrap();
        assert_eq!(stats.files_indexed, 0, "no files re-indexed");
        assert_eq!(stats.files_removed, 1, "deletion must surface");
        assert_eq!(
            stats.symbols_removed, 3,
            "beta.py carried <module> + two functions"
        );
    }

    // Regression: force re-index of a not-yet-indexed file must still
    // succeed after remove_file errors stopped being swallowed.
    #[test]
    fn index_file_with_force_succeeds_on_unindexed_file() {
        let dir = tempfile::tempdir().unwrap();
        let settings = Settings {
            index_path: dir.path().join("index"),
            workspace_root: None,
            ..Default::default()
        };

        let source = dir.path().join("sample.rs");
        std::fs::write(&source, "fn main() {}\n").unwrap();

        let mut facade = IndexFacade::new(std::sync::Arc::new(settings)).unwrap();
        let result = facade.index_file_with_force(&source, true);
        assert!(result.is_ok(), "force on unindexed file: {result:?}");
    }
}
