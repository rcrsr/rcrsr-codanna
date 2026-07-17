pub mod facade;
pub mod file_info;
pub mod progress;
pub mod walk_config;
pub mod walker;

// Parallel pipeline for high-performance indexing
pub mod pipeline;

// Re-exports
pub use file_info::{FileInfo, calculate_hash, get_utc_timestamp};
pub use progress::IndexStats;
pub use walk_config::build_walker;
pub use walker::FileWalker;

// Pipeline exports
pub use pipeline::{Pipeline, PipelineConfig};

// Facade - primary API for indexing operations
pub use facade::{DryRunOutput, FacadeResult, IndexFacade, IndexingStats, SyncStats};

// `reindex_locked` is the shared reindex seam and is internal-only (see its
// doc comment for the path-containment precondition callers must uphold).
// `ReindexOutcome` is re-exported alongside it for callers naming the
// return type, but is not itself internal-only: it's already publicly
// reachable via `facade::ReindexOutcome` since `ReindexHandles::run` (a
// public method) returns it.
pub use facade::ReindexOutcome;
pub(crate) use facade::reindex_locked;
