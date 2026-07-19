use tantivy::directory::error::OpenDirectoryError;
use tantivy::{TantivyError, query::QueryParserError};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum StorageError {
    #[error("Tantivy error: {0}")]
    Tantivy(#[from] TantivyError),

    #[error("Tantivy query parser error: {0}")]
    QueryParser(#[from] QueryParserError),

    #[error("Tantivy operation error during {operation}: {cause}")]
    TantivyOperation { operation: String, cause: String },

    #[error("Document not found: {0}")]
    DocumentNotFound(String),

    #[error("Invalid field value for {field}: {reason}")]
    InvalidFieldValue { field: String, reason: String },

    #[error("Schema error: {0}")]
    SchemaError(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Serialization error: {0}")]
    Serialization(String),

    #[error("Metadata error: {0}")]
    Metadata(String),

    #[error("No active batch. Call start_batch() first")]
    NoActiveBatch,

    /// Another operation already holds the internal write batch.
    ///
    /// In-crate callers can resolve this by calling `commit_batch()` or
    /// `rollback_batch()` on the batch currently in flight before retrying.
    #[error("Another indexing operation is already in progress; retry shortly")]
    BatchInProgress,

    #[error("Lock poisoned")]
    LockPoisoned,

    #[error("Directory error: {0}")]
    Directory(#[from] OpenDirectoryError),

    #[error("General error: {0}")]
    General(String),
}

pub type StorageResult<T> = Result<T, StorageError>;
