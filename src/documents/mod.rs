//! Document chunking and embedding for RAG use cases.
//!
//! This module provides:
//! - Document chunking with configurable strategies
//! - Vector embeddings for document chunks
//! - Collection-based organization and filtering
//! - Semantic search within document collections

pub mod chunker;
pub mod config;
pub mod schema;
pub mod slot;
pub mod store;
pub mod types;

pub use chunker::{Chunker, HybridChunker, RawChunk};
pub use config::{
    ChunkingConfig, ChunkingStrategy, CollectionConfig, DocumentsConfig, PreviewMode, SearchConfig,
};
pub use schema::DocumentSchema;
pub use slot::DocumentStoreSlot;
pub use store::{CollectionStats, DocumentStore, IndexProgress, SearchQuery, SearchResult};
pub use types::{ChunkId, CollectionId, DocumentChunk, FileState};

use crate::config::Settings;
use crate::vector::{EmbeddingGenerator, FastEmbedGenerator};
use std::sync::Arc;
use tokio::sync::RwLock;

/// Load document store from settings if enabled and indexed.
///
/// Returns None if documents are disabled, index doesn't exist, or loading fails.
/// The returned Arc can be shared between MCP server and file watcher.
pub fn load_from_settings(settings: &Settings) -> Option<Arc<RwLock<DocumentStore>>> {
    if !settings.documents.enabled {
        tracing::debug!(target: "documents", "document store disabled in settings");
        return None;
    }

    let doc_path = settings.index_path.join("documents");
    if !doc_path.exists() {
        tracing::debug!(target: "documents", "document index not found at {}", crate::parsing::paths::render_absolute_path(&doc_path).display());
        return None;
    }

    let generator = match FastEmbedGenerator::from_settings(&settings.semantic_search.model, false)
    {
        Ok(g) => g,
        Err(e) => {
            tracing::warn!(target: "documents", "failed to create embedding generator: {e}");
            return None;
        }
    };

    let dimension = generator.dimension();
    let store = match DocumentStore::new(&doc_path, dimension) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(target: "documents", "failed to open document store: {e}");
            return None;
        }
    };

    let store_with_emb = match store.with_embeddings(Box::new(generator)) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(target: "documents", "failed to attach embeddings to store: {e}");
            return None;
        }
    };

    tracing::info!(target: "documents", "loaded document store from {}", crate::parsing::paths::render_absolute_path(&doc_path).display());
    Some(Arc::new(RwLock::new(store_with_emb)))
}

#[cfg(test)]
mod load_tests {
    use super::*;

    fn settings_in(dir: &tempfile::TempDir, enabled: bool) -> Settings {
        let mut settings = Settings {
            index_path: dir.path().join("index"),
            ..Default::default()
        };
        settings.documents.enabled = enabled;
        settings
    }

    #[test]
    fn test_load_from_settings_returns_none_when_documents_disabled() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("index/documents")).unwrap();

        assert!(load_from_settings(&settings_in(&dir, false)).is_none());
    }

    #[test]
    fn test_load_from_settings_returns_none_when_no_store_exists_yet() {
        let dir = tempfile::tempdir().unwrap();

        assert!(load_from_settings(&settings_in(&dir, true)).is_none());
    }

    #[test]
    #[ignore = "needs the embedding model (~150MB)"]
    fn test_load_from_settings_returns_store_once_documents_store_is_created() {
        let dir = tempfile::tempdir().unwrap();
        let settings = settings_in(&dir, true);
        assert!(load_from_settings(&settings).is_none());

        let dimension = FastEmbedGenerator::from_settings(&settings.semantic_search.model, false)
            .unwrap()
            .dimension();
        DocumentStore::new(settings.index_path.join("documents"), dimension).unwrap();

        assert!(load_from_settings(&settings).is_some());
    }
}
