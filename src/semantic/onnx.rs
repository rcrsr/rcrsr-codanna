//! Process-wide ONNX Runtime thread cap for local fastembed models.
//!
//! fastembed 5.6 builds every `TextEmbedding` session with
//! `with_intra_threads(available_parallelism())` and ONNX Runtime's default
//! spin-waiting. With `embedding_threads` pool instances that is
//! `embedding_threads × ncpu` busy threads during embedding — on a 28-core
//! host, three sessions kept ~84 threads running and wedged the machine
//! regardless of `indexing.parallelism`.
//!
//! Committing an ort environment with a global thread pool makes every
//! session created afterwards call `DisablePerSessionThreads` and share that
//! one capped, non-spinning pool. The environment is one-shot per process, so
//! the first commit wins; later calls are no-ops.

use std::sync::Once;
use std::sync::atomic::{AtomicUsize, Ordering};

static INIT: Once = Once::new();

/// Intra-op thread cap applied at the first session creation. Defaults to the
/// default `semantic_search.embedding_threads`; `main` overrides it from config.
static INTRA_THREADS: AtomicUsize = AtomicUsize::new(3);

/// Record the intra-op thread cap (clamped to at least 1) for the ORT
/// environment. Cheap: nothing is initialized until the first embedding
/// session is created, so commands that never embed pay nothing. Has no
/// effect once the first embedding session has been created.
pub fn set_onnx_thread_cap(intra_threads: usize) {
    INTRA_THREADS.store(intra_threads.max(1), Ordering::Relaxed);
}

/// Commit the global ORT environment with the recorded intra-op cap and
/// spin-waiting disabled. Idempotent; must run before the first
/// `TextEmbedding::try_new` to take effect, so every session-creation site
/// calls it.
pub(crate) fn init_onnx_runtime() {
    INIT.call_once(|| {
        let intra_threads = INTRA_THREADS.load(Ordering::Relaxed);
        let committed = ort::environment::GlobalThreadPoolOptions::default()
            .with_intra_threads(intra_threads)
            .and_then(|o| o.with_inter_threads(1))
            .and_then(|o| o.with_spin_control(false))
            .and_then(|opts| ort::init().with_global_thread_pool(opts).commit());
        match committed {
            Ok(true) => tracing::debug!(
                target: "semantic",
                "ONNX Runtime global thread pool: {intra_threads} intra-op threads, spinning off"
            ),
            Ok(false) => tracing::warn!(
                target: "semantic",
                "ONNX Runtime environment already initialized; embedding thread cap not applied"
            ),
            Err(e) => tracing::warn!(
                target: "semantic",
                "Failed to cap ONNX Runtime threads ({e}); sessions use per-session pools"
            ),
        }
    });
}
