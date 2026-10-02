//! Exit codes for CLI operations following Unix conventions and Claude Code semantics.
//!
//! # Exit Code Semantics
//!
//! - `0`: Success - operation completed, results found (or no results is acceptable)
//! - `1`: General error - unspecified failure
//! - `2`: Blocking error - critical failure that should halt automation
//! - `3-125`: Specific recoverable errors
//! - `126-255`: Reserved by shell

use crate::error::IndexError;

/// Standard exit codes for CLI operations.
///
/// These codes follow Unix conventions where 0 indicates success,
/// and non-zero values indicate various error conditions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ExitCode {
    /// Operation succeeded (code 0)
    Success = 0,

    /// Unspecified error occurred (code 1)
    GeneralError = 1,

    /// Critical error that should halt automation (code 2)
    /// Following Claude Code hook semantics for blocking errors
    BlockingError = 2,

    /// Entity not found but command executed successfully (code 3)
    NotFound = 3,

    /// Failed to parse files (code 4)
    ParseError = 4,

    /// File I/O error (code 5)
    IoError = 5,

    /// Configuration error (code 6)
    ConfigError = 6,

    /// Index corruption detected (code 7)
    IndexCorrupted = 7,

    /// Operation not supported (code 8)
    UnsupportedOperation = 8,
}

impl From<ExitCode> for i32 {
    fn from(code: ExitCode) -> i32 {
        code as i32
    }
}

impl ExitCode {
    /// Determine exit code for a retrieve operation based on result presence.
    ///
    /// Returns `Success` if data is found, `NotFound` if empty.
    pub fn from_retrieve_result<T>(result: &Option<T>) -> Self {
        match result {
            Some(_) => ExitCode::Success,
            None => ExitCode::NotFound,
        }
    }

    /// Convert an `IndexError` to the appropriate exit code.
    ///
    /// Maps specific error types to semantic exit codes that scripts
    /// can use to determine appropriate recovery actions.
    pub fn from_error(error: &IndexError) -> Self {
        match error {
            // Not found errors are recoverable
            IndexError::SymbolNotFound { .. } | IndexError::FileNotFound { .. } => {
                ExitCode::NotFound
            }

            // Index corruption is a blocking error
            IndexError::IndexCorrupted { .. } => ExitCode::BlockingError,

            // A stale on-disk schema needs a rebuild, like corruption
            IndexError::Storage(crate::storage::StorageError::SchemaMismatch { .. }) => {
                ExitCode::IndexCorrupted
            }

            // Specific recoverable errors
            IndexError::ParseError { .. } => ExitCode::ParseError,
            IndexError::FileRead { .. } | IndexError::FileWrite { .. } => ExitCode::IoError,
            IndexError::ConfigError { .. } => ExitCode::ConfigError,
            IndexError::UnsupportedFileType { .. } => ExitCode::UnsupportedOperation,

            // ID exhaustion errors are blocking
            IndexError::FileIdExhausted | IndexError::SymbolIdExhausted => ExitCode::BlockingError,

            // Everything else is a general error
            _ => ExitCode::GeneralError,
        }
    }

    /// Check if this exit code indicates a blocking error.
    ///
    /// Blocking errors should halt automation pipelines.
    #[must_use]
    pub fn is_blocking(&self) -> bool {
        matches!(self, ExitCode::BlockingError)
    }

    /// Check if this exit code indicates success.
    #[must_use]
    pub fn is_success(&self) -> bool {
        matches!(self, ExitCode::Success)
    }

    /// Get a human-readable description of the exit code.
    pub fn description(&self) -> &str {
        match self {
            ExitCode::Success => "Success",
            ExitCode::GeneralError => "General error",
            ExitCode::BlockingError => "Blocking error - automation should halt",
            ExitCode::NotFound => "Not found",
            ExitCode::ParseError => "Parse error",
            ExitCode::IoError => "I/O error",
            ExitCode::ConfigError => "Configuration error",
            ExitCode::IndexCorrupted => "Index corrupted",
            ExitCode::UnsupportedOperation => "Unsupported operation",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_exit_code_values() {
        assert_eq!(ExitCode::Success as u8, 0);
        assert_eq!(ExitCode::GeneralError as u8, 1);
        assert_eq!(ExitCode::BlockingError as u8, 2);
        assert_eq!(ExitCode::NotFound as u8, 3);
    }

    #[test]
    fn test_from_retrieve_result() {
        let some_result = Some("data");
        assert_eq!(
            ExitCode::from_retrieve_result(&some_result),
            ExitCode::Success
        );

        let none_result: Option<&str> = None;
        assert_eq!(
            ExitCode::from_retrieve_result(&none_result),
            ExitCode::NotFound
        );
    }

    #[test]
    fn test_is_success() {
        assert!(ExitCode::Success.is_success());
        assert!(!ExitCode::NotFound.is_success());
        assert!(!ExitCode::GeneralError.is_success());
    }

    #[test]
    fn test_is_blocking() {
        assert!(ExitCode::BlockingError.is_blocking());
        assert!(!ExitCode::Success.is_blocking());
        assert!(!ExitCode::NotFound.is_blocking());
    }

    #[test]
    fn test_schema_mismatch_maps_to_index_corrupted() {
        let error = IndexError::Storage(crate::storage::StorageError::SchemaMismatch {
            path: std::path::PathBuf::from("idx"),
            detail: "field count is 1, expected 2".to_string(),
        });
        assert_eq!(ExitCode::from_error(&error), ExitCode::IndexCorrupted);
    }
}
