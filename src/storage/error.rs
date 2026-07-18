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

    #[error("Batch already in progress. Call commit_batch() or rollback_batch() first")]
    BatchInProgress,

    #[error("Lock poisoned")]
    LockPoisoned,

    #[error("Directory error: {0}")]
    Directory(#[from] OpenDirectoryError),

    #[error("General error: {0}")]
    General(String),
}

pub type StorageResult<T> = Result<T, StorageError>;
