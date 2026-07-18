use crate::storage::{MetadataKey, StorageError, StorageResult};
use crate::{FileId, Relationship, SymbolId};
use tantivy::{
    IndexWriter, TantivyDocument as Document, Term,
    directory::error::LockError,
    query::{BooleanQuery, Occur, TermQuery},
    schema::IndexRecordOption,
};

use super::DocumentIndex;

/// Classify a `TantivyError` as transient (worth retrying) or permanent.
///
/// Transient cases:
/// - `LockFailure(LockError::LockBusy, _)`: another writer currently holds the
///   directory lock. This is the primary case `create_writer_with_retry` exists
///   to survive, and it carries no `io::Error` source, so it cannot be detected
///   by downcasting `Error::source`.
/// - `LockFailure(LockError::IoError(io_err), _)` or any other error whose
///   `source()` downcasts to an `io::Error` with kind `PermissionDenied`,
///   `TimedOut`, or `WouldBlock`.
fn is_transient_writer_error(e: &tantivy::TantivyError) -> bool {
    if let tantivy::TantivyError::LockFailure(lock_error, _) = e {
        return match lock_error {
            LockError::LockBusy => true,
            LockError::IoError(io_err) => matches!(
                io_err.kind(),
                std::io::ErrorKind::PermissionDenied
                    | std::io::ErrorKind::TimedOut
                    | std::io::ErrorKind::WouldBlock
            ),
        };
    }

    std::error::Error::source(e)
        .and_then(|s| s.downcast_ref::<std::io::Error>())
        .map(|io_err| {
            matches!(
                io_err.kind(),
                std::io::ErrorKind::PermissionDenied
                    | std::io::ErrorKind::TimedOut
                    | std::io::ErrorKind::WouldBlock
            )
        })
        .unwrap_or(false)
}

impl DocumentIndex {
    /// Create index writer with retry logic for transient errors
    fn create_writer_with_retry(&self) -> Result<IndexWriter<Document>, tantivy::TantivyError> {
        // `max_retry_attempts` is config-driven and could be set to 0; clamp to
        // at least 1 so the loop body always runs at least once and the
        // post-loop path is genuinely unreachable rather than panicking.
        let max_attempts = self.max_retry_attempts.max(1);

        for attempt in 0..max_attempts {
            match self.index.writer::<Document>(self.heap_size) {
                Ok(writer) => return Ok(writer),
                Err(e) => {
                    let is_transient = is_transient_writer_error(&e);

                    if is_transient && attempt < max_attempts - 1 {
                        let delay = 100 * (1 << attempt);
                        eprintln!(
                            "Attempt {}/{}: Transient index-lock contention, retrying after {}ms",
                            attempt + 1,
                            max_attempts,
                            delay
                        );
                        std::thread::sleep(std::time::Duration::from_millis(delay));
                    } else {
                        return Err(e);
                    }
                }
            }
        }
        unreachable!("max_attempts is clamped to at least 1, so the loop always returns")
    }

    /// Start a batch operation for adding multiple documents
    pub fn start_batch(&self) -> StorageResult<()> {
        let mut writer_lock = self
            .writer
            .write()
            .map_err(|_| StorageError::LockPoisoned)?;
        if writer_lock.is_none() {
            let writer = self.create_writer_with_retry()?;
            *writer_lock = Some(writer);

            // Initialize the pending symbol counter for this batch
            let current = self
                .query_metadata(MetadataKey::SymbolCounter)?
                .unwrap_or(0) as u32;
            if let Ok(mut pending_guard) = self.pending_symbol_counter.lock() {
                *pending_guard = Some(current + 1);
            }

            // Initialize the pending file counter for this batch
            let file_current = self.query_metadata(MetadataKey::FileCounter)?.unwrap_or(0) as u32;
            if let Ok(mut pending_guard) = self.pending_file_counter.lock() {
                *pending_guard = Some(file_current + 1);
            }
        }
        Ok(())
    }

    /// Discard the current batch: staged adds/deletes never reach a commit.
    ///
    /// Without this, an error path that abandons a started batch leaves the
    /// writer (with staged delete_terms) in place for the next start_batch,
    /// and a later commit applies deletions for files that were never
    /// reprocessed.
    pub fn rollback_batch(&self) -> StorageResult<()> {
        let mut writer_lock = match self.writer.write() {
            Ok(lock) => lock,
            Err(poisoned) => {
                eprintln!("Warning: Recovering from poisoned writer rwlock in rollback_batch");
                poisoned.into_inner()
            }
        };
        if let Some(mut writer) = writer_lock.take() {
            writer.rollback()?;
        }

        if let Ok(mut pending_guard) = self.pending_symbol_counter.lock() {
            *pending_guard = None;
        }
        if let Ok(mut pending_guard) = self.pending_file_counter.lock() {
            *pending_guard = None;
        }

        Ok(())
    }

    /// Commit the current batch and reload the reader
    pub fn commit_batch(&self) -> StorageResult<()> {
        let mut writer_lock = match self.writer.write() {
            Ok(lock) => lock,
            Err(poisoned) => {
                eprintln!("Warning: Recovering from poisoned writer rwlock in commit_batch");
                poisoned.into_inner()
            }
        };
        if let Some(mut writer) = writer_lock.take() {
            // Try to commit with better error context
            match writer.commit() {
                Ok(_opstamp) => {
                    // Successful commit
                }
                Err(e) => {
                    // Check for permission errors using ErrorKind
                    let is_permission_error = std::error::Error::source(&e)
                        .and_then(|s| s.downcast_ref::<std::io::Error>())
                        .map(|io_err| matches!(io_err.kind(), std::io::ErrorKind::PermissionDenied))
                        .unwrap_or(false);

                    if is_permission_error {
                        return Err(StorageError::General(format!(
                            "Failed to commit index due to permission error.\n\
                            This can happen when:\n\
                            1. Security software is scanning the index directory\n\
                            2. Another process has locked the files\n\
                            3. Insufficient file system permissions\n\
                            \nOriginal error: {e}\n\
                            \nTry:\n\
                            - Reducing tantivy_heap_mb in settings (15-25MB)\n\
                            - Adding .codanna to security software exclusions\n\
                            - Ensuring no other codanna processes are running"
                        )));
                    }
                    return Err(e.into());
                }
            }

            // Reload the reader to see new documents
            self.reader.reload()?;

            // Clear the pending symbol counter after commit
            if let Ok(mut pending_guard) = self.pending_symbol_counter.lock() {
                *pending_guard = None;
            }

            // Clear the pending file counter after commit
            if let Ok(mut pending_guard) = self.pending_file_counter.lock() {
                *pending_guard = None;
            }
        }
        Ok(())
    }

    /// Remove documents for a specific file
    pub fn remove_file_documents(&self, file_path: &str) -> StorageResult<()> {
        // Use existing batch writer if available, otherwise create temporary one
        let writer_lock = self.writer.read().map_err(|_| StorageError::LockPoisoned)?;
        let term = Term::from_field_text(self.schema.file_path, file_path);

        if let Some(writer) = writer_lock.as_ref() {
            // Use existing batch writer
            writer.delete_term(term);
            // Note: We don't commit here - that happens at batch end
        } else {
            // Create temporary writer for single operation
            drop(writer_lock); // Release lock before creating new writer
            let mut writer = self.index.writer::<Document>(50_000_000)?;
            writer.delete_term(term);
            writer.commit()?;
            self.reader.reload()?;
        }

        Ok(())
    }

    /// Clear all documents from the index
    pub fn clear(&self) -> StorageResult<()> {
        // Check if index has been initialized (has meta.json)
        // If not, there's nothing to clear
        let meta_path = self.index_path.join("meta.json");
        if !meta_path.exists() {
            return Ok(());
        }

        let mut writer = self.index.writer::<Document>(50_000_000)?;
        writer.delete_all_documents()?;
        writer.commit()?;
        self.reader.reload()?;
        Ok(())
    }

    /// Update the pending symbol counter (for cross-file symbol ID continuity in batches)
    pub fn update_pending_symbol_counter(&self, new_value: u32) -> StorageResult<()> {
        if let Ok(mut pending_guard) = self.pending_symbol_counter.lock() {
            if let Some(ref mut counter) = *pending_guard {
                *counter = new_value;
            }
        }
        Ok(())
    }

    /// Delete a symbol
    pub fn delete_symbol(&self, id: SymbolId) -> StorageResult<()> {
        let writer_lock = match self.writer.read() {
            Ok(lock) => lock,
            Err(poisoned) => {
                eprintln!("Warning: Recovering from poisoned writer rwlock in delete_symbol");
                poisoned.into_inner()
            }
        };
        let writer = writer_lock.as_ref().ok_or(StorageError::NoActiveBatch)?;

        let term = Term::from_field_u64(self.schema.symbol_id, id.0 as u64);
        writer.delete_term(term);
        Ok(())
    }

    /// Delete relationships for a symbol
    pub fn delete_relationships_for_symbol(&self, id: SymbolId) -> StorageResult<()> {
        let writer_lock = match self.writer.read() {
            Ok(lock) => lock,
            Err(poisoned) => {
                eprintln!(
                    "Warning: Recovering from poisoned writer rwlock in delete_relationships"
                );
                poisoned.into_inner()
            }
        };
        let writer = writer_lock.as_ref().ok_or(StorageError::NoActiveBatch)?;

        // Delete where from_symbol_id = id
        let from_term = Term::from_field_u64(self.schema.from_symbol_id, id.0 as u64);
        writer.delete_term(from_term);

        // Delete where to_symbol_id = id
        let to_term = Term::from_field_u64(self.schema.to_symbol_id, id.0 as u64);
        writer.delete_term(to_term);

        Ok(())
    }

    /// Store a relationship between two symbols
    pub(crate) fn store_relationship(
        &self,
        from: SymbolId,
        to: SymbolId,
        rel: &Relationship,
    ) -> StorageResult<()> {
        let writer_lock = match self.writer.read() {
            Ok(lock) => lock,
            Err(poisoned) => {
                eprintln!("Warning: Recovering from poisoned writer rwlock in store_relationship");
                poisoned.into_inner()
            }
        };
        let writer = writer_lock.as_ref().ok_or(StorageError::NoActiveBatch)?;

        let mut doc = Document::new();
        doc.add_text(self.schema.doc_type, "relationship");
        doc.add_u64(self.schema.from_symbol_id, from.value() as u64);
        doc.add_u64(self.schema.to_symbol_id, to.value() as u64);
        doc.add_text(self.schema.relation_kind, format!("{:?}", rel.kind));
        doc.add_f64(self.schema.relation_weight, rel.weight as f64);

        if let Some(ref metadata) = rel.metadata {
            if let Some(line) = metadata.line {
                doc.add_u64(self.schema.relation_line, line as u64);
            }
            if let Some(column) = metadata.column {
                doc.add_u64(self.schema.relation_column, column as u64);
            }
            if let Some(ref context) = metadata.context {
                doc.add_text(self.schema.relation_context, context.as_ref());
            }
            if let Some(ref receiver) = metadata.receiver {
                doc.add_text(self.schema.relation_receiver, receiver.as_ref());
            }
            if metadata.static_call {
                doc.add_u64(self.schema.relation_static_call, 1);
            }
        }

        writer.add_document(doc)?;
        Ok(())
    }

    /// Index a symbol from a Symbol struct
    pub fn index_symbol(&self, symbol: &crate::Symbol, file_path: &str) -> StorageResult<()> {
        self.add_document(symbol, file_path)
    }

    /// Store file registration from the indexing pipeline.
    ///
    /// Takes FileRegistration directly and handles all field conversions.
    pub fn store_file_registration(
        &self,
        registration: &crate::indexing::pipeline::FileRegistration,
    ) -> StorageResult<()> {
        let writer_lock = match self.writer.read() {
            Ok(lock) => lock,
            Err(poisoned) => {
                eprintln!(
                    "Warning: Recovering from poisoned writer rwlock in store_file_registration"
                );
                poisoned.into_inner()
            }
        };
        let writer = writer_lock.as_ref().ok_or(StorageError::NoActiveBatch)?;

        let mut doc = Document::new();
        doc.add_text(self.schema.doc_type, "file_info");
        doc.add_u64(self.schema.file_id, registration.file_id.value() as u64);
        doc.add_text(
            self.schema.file_path,
            registration.path.to_string_lossy().as_ref(),
        );
        // Hash is already a SHA256 hex string
        doc.add_text(self.schema.file_hash, &registration.content_hash);
        doc.add_u64(self.schema.file_timestamp, registration.timestamp);
        doc.add_u64(self.schema.file_mtime, registration.mtime);
        // Store language for incremental indexing (parser selection)
        doc.add_text(self.schema.language, registration.language_id.as_str());

        writer.add_document(doc)?;
        Ok(())
    }

    /// Store an import document in the index
    ///
    /// This is a pure storage operation storing raw import metadata.
    /// Resolution logic happens in the resolution layer.
    pub fn store_import(&self, import: &crate::parsing::Import) -> StorageResult<()> {
        let writer_lock = match self.writer.read() {
            Ok(lock) => lock,
            Err(poisoned) => {
                eprintln!("Warning: Recovering from poisoned writer rwlock in store_import");
                poisoned.into_inner()
            }
        };
        let writer = writer_lock.as_ref().ok_or(StorageError::NoActiveBatch)?;

        let mut doc = Document::new();

        // Document type
        doc.add_text(self.schema.doc_type, "import");

        // Import metadata fields
        doc.add_u64(self.schema.import_file_id, import.file_id.value() as u64);
        doc.add_text(self.schema.import_path, &import.path);

        if let Some(alias) = &import.alias {
            doc.add_text(self.schema.import_alias, alias);
        }

        doc.add_u64(
            self.schema.import_is_glob,
            if import.is_glob { 1 } else { 0 },
        );
        doc.add_u64(
            self.schema.import_is_type_only,
            if import.is_type_only { 1 } else { 0 },
        );

        writer.add_document(doc)?;
        Ok(())
    }

    /// Delete all import documents for a file
    ///
    /// Used during file updates and deletions.
    pub fn delete_imports_for_file(&self, file_id: FileId) -> StorageResult<()> {
        let writer_lock = match self.writer.read() {
            Ok(lock) => lock,
            Err(poisoned) => {
                eprintln!(
                    "Warning: Recovering from poisoned writer rwlock in delete_imports_for_file"
                );
                poisoned.into_inner()
            }
        };
        let writer = writer_lock.as_ref().ok_or(StorageError::NoActiveBatch)?;

        let query = BooleanQuery::new(vec![
            (
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_text(self.schema.doc_type, "import"),
                    IndexRecordOption::Basic,
                )),
            ),
            (
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_u64(self.schema.import_file_id, file_id.value() as u64),
                    IndexRecordOption::Basic,
                )),
            ),
        ]);

        writer.delete_query(Box::new(query))?;
        Ok(())
    }

    /// Store metadata (counters, etc.)
    pub(crate) fn store_metadata(&self, key: MetadataKey, value: u64) -> StorageResult<()> {
        let writer_lock = match self.writer.read() {
            Ok(lock) => lock,
            Err(poisoned) => {
                eprintln!("Warning: Recovering from poisoned writer rwlock in store_metadata");
                poisoned.into_inner()
            }
        };
        let writer = writer_lock.as_ref().ok_or(StorageError::NoActiveBatch)?;

        // First delete any existing metadata with this key
        let key_str = key.as_str();
        let term = Term::from_field_text(self.schema.meta_key, key_str);
        writer.delete_term(term);

        let mut doc = Document::new();
        doc.add_text(self.schema.doc_type, "metadata");
        doc.add_text(self.schema.meta_key, key_str);
        doc.add_u64(self.schema.meta_value, value);

        writer.add_document(doc)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc as StdArc;
    use std::time::{Duration, Instant};
    use tantivy::directory::error::LockError;
    use tempfile::TempDir;

    #[test]
    fn test_lock_busy_is_classified_as_transient() {
        let err = tantivy::TantivyError::LockFailure(LockError::LockBusy, None);
        assert!(
            is_transient_writer_error(&err),
            "LockBusy must be treated as transient so the retry loop actually retries"
        );
    }

    #[test]
    fn test_lock_io_error_with_transient_kinds_is_classified_as_transient() {
        for kind in [
            std::io::ErrorKind::PermissionDenied,
            std::io::ErrorKind::TimedOut,
            std::io::ErrorKind::WouldBlock,
        ] {
            let io_err = std::io::Error::new(kind, "simulated transient io error");
            let err =
                tantivy::TantivyError::LockFailure(LockError::IoError(StdArc::new(io_err)), None);
            assert!(
                is_transient_writer_error(&err),
                "io error kind {kind:?} wrapped in LockError::IoError must remain transient"
            );
        }
    }

    #[test]
    fn test_non_transient_error_is_not_classified_as_transient() {
        let err = tantivy::TantivyError::SchemaError("field does not exist".to_string());
        assert!(
            !is_transient_writer_error(&err),
            "a schema error is a permanent failure and must not be retried"
        );

        let err = tantivy::TantivyError::IndexAlreadyExists;
        assert!(
            !is_transient_writer_error(&err),
            "IndexAlreadyExists is a permanent failure and must not be retried"
        );
    }

    #[test]
    fn test_zero_max_retry_attempts_does_not_panic() {
        let temp_dir = TempDir::new().unwrap();
        let mut settings = crate::config::Settings::default();
        settings.indexing.max_retry_attempts = 0;
        let index = DocumentIndex::new(temp_dir.path(), &settings).unwrap();

        // With no contention this should simply succeed on the single
        // (clamped-to-1) attempt, but the point of this test is that it
        // must not panic via the old `unreachable!()` path.
        let result = index.start_batch();
        assert!(result.is_ok());
    }

    #[test]
    fn test_start_batch_retries_through_real_lock_contention() {
        let temp_dir = TempDir::new().unwrap();
        let settings = crate::config::Settings::default();
        let index = DocumentIndex::new(temp_dir.path(), &settings).unwrap();

        // Hold a real writer lock on the same underlying tantivy Index,
        // independent of `index.writer`, so `start_batch` must contend for
        // the on-disk lock file exactly like a second process/thread would.
        let held_writer = index.index.writer::<Document>(index.heap_size).unwrap();

        // Release the lock shortly after start_batch begins its attempts,
        // comfortably inside the retry budget (100ms, 200ms, 400ms
        // backoffs => ~700ms total before attempts are exhausted).
        let release_after = Duration::from_millis(150);
        std::thread::spawn(move || {
            std::thread::sleep(release_after);
            drop(held_writer);
        });

        let start = Instant::now();
        let result = index.start_batch();
        let elapsed = start.elapsed();

        assert!(
            result.is_ok(),
            "start_batch should succeed once the contending writer is dropped"
        );
        assert!(
            elapsed >= Duration::from_millis(100),
            "start_batch returned in {elapsed:?}, which is faster than the first backoff \
             delay (100ms); it should have retried at least once instead of failing instantly"
        );
    }

    #[test]
    fn test_rollback_batch_discards_staged_deletes() {
        let temp_dir = TempDir::new().unwrap();
        let settings = crate::config::Settings::default();
        let index = DocumentIndex::new(temp_dir.path(), &settings).unwrap();

        index.start_batch().unwrap();
        let from_id = SymbolId::new(1).unwrap();
        let to_id = SymbolId::new(2).unwrap();
        let rel = crate::Relationship::new(crate::RelationKind::Calls);
        index.store_relationship(from_id, to_id, &rel).unwrap();
        index.commit_batch().unwrap();

        // Stage a delete, then roll back: the delete must not survive into
        // a later batch's commit.
        index.start_batch().unwrap();
        index.delete_relationships_for_symbol(from_id).unwrap();
        index.rollback_batch().unwrap();

        index.start_batch().unwrap();
        index.commit_batch().unwrap();

        let relationships = index.query_relationships().unwrap();
        assert_eq!(
            relationships.len(),
            1,
            "staged delete must be discarded by rollback"
        );
    }
}
