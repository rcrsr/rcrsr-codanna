//! Lazy, shared slot for the document store.
//!
//! A server that starts before `codanna documents index` has run has no
//! document store to hand out. The slot lets every server session share one
//! store that is loaded on first demand after it appears, without a restart.
//! A filled slot is never replaced: a second store over the same tantivy
//! directory would leave two writers on it.

use crate::config::Settings;
use crate::documents::DocumentStore;
use std::sync::Arc;
use tokio::sync::{OnceCell, RwLock};

/// Shared handle to a [`DocumentStore`] that may be loaded after startup.
#[derive(Default)]
pub struct DocumentStoreSlot {
    cell: OnceCell<Arc<RwLock<DocumentStore>>>,
}

impl DocumentStoreSlot {
    /// An empty slot; the store is loaded by the first successful `resolve`.
    pub fn new() -> Self {
        Self::default()
    }

    /// A slot already holding `store`.
    pub fn filled(store: Arc<RwLock<DocumentStore>>) -> Self {
        Self {
            cell: OnceCell::new_with(Some(store)),
        }
    }

    /// The cached store, if any. Never loads.
    pub fn get(&self) -> Option<Arc<RwLock<DocumentStore>>> {
        self.cell.get().cloned()
    }

    /// Return the cached store, else load it from `settings` on a blocking
    /// thread (loading initializes the ONNX model). A failed load leaves the
    /// slot empty so the next call retries.
    pub async fn resolve(&self, settings: Arc<Settings>) -> Option<Arc<RwLock<DocumentStore>>> {
        self.resolve_with(move || crate::documents::load_from_settings(&settings))
            .await
    }

    /// [`Self::resolve`] with an injectable loader. Concurrent callers are
    /// serialized, so the loader runs at most once per successful fill.
    pub async fn resolve_with<F>(&self, loader: F) -> Option<Arc<RwLock<DocumentStore>>>
    where
        F: FnOnce() -> Option<Arc<RwLock<DocumentStore>>> + Send + 'static,
    {
        let loaded = self
            .cell
            .get_or_try_init(|| async {
                match tokio::task::spawn_blocking(loader).await {
                    Ok(Some(store)) => Ok(store),
                    Ok(None) => Err(()),
                    Err(e) => {
                        tracing::warn!(
                            target: "documents",
                            "document store load task failed: {}",
                            crate::utils::describe_join_error(&e)
                        );
                        Err(())
                    }
                }
            })
            .await;
        loaded.ok().cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vector::VectorDimension;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn make_store(dir: &tempfile::TempDir) -> Arc<RwLock<DocumentStore>> {
        let store = DocumentStore::new(
            dir.path().join("documents"),
            VectorDimension::dimension_384(),
        )
        .expect("create document store");
        Arc::new(RwLock::new(store))
    }

    #[tokio::test]
    async fn test_empty_slot_with_none_loader_stays_empty() {
        let slot = DocumentStoreSlot::new();
        assert!(slot.resolve_with(|| None).await.is_none());
        assert!(slot.get().is_none());
    }

    #[tokio::test]
    async fn test_second_resolve_with_some_loader_fills_slot() {
        let dir = tempfile::tempdir().unwrap();
        let slot = DocumentStoreSlot::new();
        assert!(slot.resolve_with(|| None).await.is_none());
        let store = make_store(&dir);
        let expected = Arc::clone(&store);
        let got = slot.resolve_with(move || Some(store)).await.unwrap();
        assert!(Arc::ptr_eq(&got, &expected));
    }

    #[tokio::test]
    async fn test_third_resolve_returns_same_arc_without_running_loader() {
        let dir = tempfile::tempdir().unwrap();
        let slot = DocumentStoreSlot::new();
        let first = slot
            .resolve_with(move || Some(make_store(&dir)))
            .await
            .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let c = Arc::clone(&calls);
        let again = slot
            .resolve_with(move || {
                c.fetch_add(1, Ordering::SeqCst);
                None
            })
            .await
            .unwrap();
        assert!(Arc::ptr_eq(&first, &again));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn test_filled_slot_never_runs_loader() {
        let dir = tempfile::tempdir().unwrap();
        let store = make_store(&dir);
        let slot = DocumentStoreSlot::filled(Arc::clone(&store));
        let calls = Arc::new(AtomicUsize::new(0));
        let c = Arc::clone(&calls);
        let got = slot
            .resolve_with(move || {
                c.fetch_add(1, Ordering::SeqCst);
                None
            })
            .await
            .unwrap();
        assert!(Arc::ptr_eq(&got, &store));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
}
