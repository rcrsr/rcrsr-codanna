//! Lazy, shared slot for the document store.
//!
//! A server that starts before `codanna documents index` has run has no
//! document store to hand out. The slot lets every server session share one
//! store that is loaded on first demand after it appears, without a restart.
//! A filled slot is never replaced: a second store over the same tantivy
//! directory would leave two writers on it.

use crate::config::Settings;
use crate::documents::DocumentStore;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};
use tokio::sync::{OnceCell, RwLock};

/// How long after a failed load `resolve` answers `None` without calling the
/// loader again. A failed load initializes the ONNX model and opens the
/// stores, which costs seconds, so a persistent failure (model not cached on
/// an offline host, corrupt or stale-schema documents store) must not be
/// re-run by every `search_documents` call. The first call after the
/// cooldown elapses retries.
const LOAD_FAILURE_COOLDOWN: Duration = Duration::from_secs(10);

/// Shared handle to a [`DocumentStore`] that may be loaded after startup.
pub struct DocumentStoreSlot {
    cell: OnceCell<Arc<RwLock<DocumentStore>>>,
    /// When the last load failed; never held across an await.
    last_failure: Mutex<Option<Instant>>,
    cooldown: Duration,
}

impl Default for DocumentStoreSlot {
    fn default() -> Self {
        Self {
            cell: OnceCell::new(),
            last_failure: Mutex::new(None),
            cooldown: LOAD_FAILURE_COOLDOWN,
        }
    }
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
            ..Self::default()
        }
    }

    /// An empty slot with a custom failure cooldown.
    #[cfg(test)]
    fn with_cooldown(cooldown: Duration) -> Self {
        Self {
            cooldown,
            ..Self::default()
        }
    }

    /// The cached store, if any. Never loads.
    pub fn get(&self) -> Option<Arc<RwLock<DocumentStore>>> {
        self.cell.get().cloned()
    }

    /// Return the cached store, else load it from `settings` on a blocking
    /// thread (loading initializes the ONNX model). A failed load leaves the
    /// slot empty; further calls return `None` without loading until the
    /// failure cooldown elapses, after which the next call retries. Nothing
    /// is spawned when documents are disabled in `settings` or the documents
    /// directory is absent, and that answer starts no cooldown, so a store
    /// indexed later is picked up on the next call.
    pub async fn resolve(&self, settings: Arc<Settings>) -> Option<Arc<RwLock<DocumentStore>>> {
        if self.cell.get().is_none()
            && (!settings.documents.enabled || !settings.index_path.join("documents").exists())
        {
            return None;
        }
        self.resolve_with(move || crate::documents::load_from_settings(&settings))
            .await
    }

    /// [`Self::resolve`] with an injectable loader. Concurrent callers are
    /// serialized, so the loader runs at most once per successful fill, and
    /// callers queued behind a failed load see its cooldown instead of
    /// running their own load.
    pub async fn resolve_with<F>(&self, loader: F) -> Option<Arc<RwLock<DocumentStore>>>
    where
        F: FnOnce() -> Option<Arc<RwLock<DocumentStore>>> + Send + 'static,
    {
        let loaded = self
            .cell
            .get_or_try_init(|| async {
                if self.in_cooldown() {
                    return Err(());
                }
                let result = match tokio::task::spawn_blocking(loader).await {
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
                };
                if result.is_err() {
                    *self
                        .last_failure
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner) = Some(Instant::now());
                }
                result
            })
            .await;
        loaded.ok().cloned()
    }

    /// True while the last load failure is younger than the cooldown.
    fn in_cooldown(&self) -> bool {
        self.last_failure
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some_and(|failed_at| failed_at.elapsed() < self.cooldown)
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
        let slot = DocumentStoreSlot::with_cooldown(Duration::ZERO);
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

    fn counting_failing_loader(
        calls: &Arc<AtomicUsize>,
    ) -> impl FnOnce() -> Option<Arc<RwLock<DocumentStore>>> + Send + 'static {
        let c = Arc::clone(calls);
        move || {
            c.fetch_add(1, Ordering::SeqCst);
            None
        }
    }

    #[tokio::test]
    async fn test_failed_load_is_not_retried_within_cooldown() {
        let slot = DocumentStoreSlot::with_cooldown(Duration::from_secs(3600));
        let calls = Arc::new(AtomicUsize::new(0));
        for _ in 0..4 {
            assert!(
                slot.resolve_with(counting_failing_loader(&calls))
                    .await
                    .is_none()
            );
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_failed_load_is_retried_after_cooldown() {
        let slot = DocumentStoreSlot::with_cooldown(Duration::from_millis(20));
        let calls = Arc::new(AtomicUsize::new(0));
        assert!(
            slot.resolve_with(counting_failing_loader(&calls))
                .await
                .is_none()
        );
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(
            slot.resolve_with(counting_failing_loader(&calls))
                .await
                .is_none()
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn test_first_load_success_is_unaffected_by_cooldown() {
        let dir = tempfile::tempdir().unwrap();
        let slot = DocumentStoreSlot::with_cooldown(Duration::from_secs(3600));
        let store = make_store(&dir);
        let expected = Arc::clone(&store);
        let got = slot.resolve_with(move || Some(store)).await.unwrap();
        assert!(Arc::ptr_eq(&got, &expected));
    }

    #[tokio::test]
    async fn test_resolve_skips_loader_when_documents_dir_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        let settings = Settings {
            index_path: dir.path().to_path_buf(),
            ..Settings::default()
        };
        let slot = DocumentStoreSlot::new();
        assert!(slot.resolve(Arc::new(settings)).await.is_none());
        assert!(!slot.in_cooldown(), "an absent store is not a load failure");
    }
}
