//! Metadata tracking for index state and data sources

use crate::IndexResult;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// Metadata about the index state
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexMetadata {
    /// Version of the index format
    pub version: u32,

    /// Current data source
    pub data_source: DataSource,

    /// Number of symbols in the index
    pub symbol_count: u32,

    /// Number of files in the index
    pub file_count: u32,

    /// Last modification timestamp
    pub last_modified: u64,

    /// Directories that were indexed (canonicalized paths)
    /// Used to detect config changes and auto-sync on load
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub indexed_paths: Option<Vec<PathBuf>>,

    /// Fingerprint of the ignore-rule inputs (`.codannaignore`, `.gitignore`,
    /// `.git/info/exclude`, `indexing.ignore_patterns`, `indexing.follow_links`)
    /// as of the last index build. Compared against a freshly computed
    /// fingerprint on load to detect-and-report staleness (never reconciled
    /// automatically). `None` on metadata written before this field existed,
    /// or when the fingerprint could not be computed — both report as
    /// "unknown", not "changed".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignore_fingerprint: Option<String>,
}

/// Describes where the index data came from
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DataSource {
    /// Loaded from Tantivy index
    Tantivy {
        path: PathBuf,
        doc_count: u64,
        timestamp: u64,
    },

    /// Fresh index (not loaded)
    Fresh,
}

impl Default for IndexMetadata {
    fn default() -> Self {
        Self {
            version: 1,
            data_source: DataSource::Fresh,
            symbol_count: 0,
            file_count: 0,
            last_modified: crate::indexing::get_utc_timestamp(),
            indexed_paths: None,
            ignore_fingerprint: None,
        }
    }
}

impl IndexMetadata {
    /// Create new metadata for a fresh index
    pub fn new() -> Self {
        Self::default()
    }

    /// Update counts from the indexer
    pub fn update_counts(&mut self, symbol_count: u32, file_count: u32) {
        self.symbol_count = symbol_count;
        self.file_count = file_count;
        self.last_modified = crate::indexing::get_utc_timestamp();
    }

    /// Update indexed paths from the indexer
    pub fn update_indexed_paths(&mut self, paths: Vec<PathBuf>) {
        self.indexed_paths = Some(paths);
        self.last_modified = crate::indexing::get_utc_timestamp();
    }

    /// Record the ignore-rule fingerprint computed for the index just built.
    pub fn update_ignore_fingerprint(&mut self, fingerprint: String) {
        self.ignore_fingerprint = Some(fingerprint);
    }

    /// Save metadata to file
    pub fn save(&self, base_path: &Path) -> IndexResult<()> {
        let metadata_path = base_path.join("index.meta");
        let json = serde_json::to_string_pretty(self).map_err(|e| {
            crate::IndexError::General(format!("Failed to serialize metadata: {e}"))
        })?;

        fs::write(&metadata_path, json).map_err(|e| crate::IndexError::FileWrite {
            path: metadata_path,
            source: e,
        })?;

        Ok(())
    }

    /// Load metadata from file
    pub fn load(base_path: &Path) -> IndexResult<Self> {
        let metadata_path = base_path.join("index.meta");

        if !metadata_path.exists() {
            return Ok(Self::new());
        }

        let json = fs::read_to_string(&metadata_path).map_err(|e| crate::IndexError::FileRead {
            path: metadata_path.clone(),
            source: e,
        })?;

        serde_json::from_str(&json)
            .map_err(|e| crate::IndexError::General(format!("Failed to parse metadata: {e}")))
    }

    /// Display source information to the user
    pub fn display_source(&self) {
        match &self.data_source {
            DataSource::Tantivy {
                path, doc_count, ..
            } => {
                eprintln!(
                    "Loaded from Tantivy index: {} ({} documents)",
                    path.display(),
                    doc_count
                );
            }
            DataSource::Fresh => {
                eprintln!("Created fresh index");
            }
        }
        eprintln!(
            "Index contains {} symbols from {} files",
            self.symbol_count, self.file_count
        );
    }
}
