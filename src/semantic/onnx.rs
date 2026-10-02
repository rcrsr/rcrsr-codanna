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

/// Ceiling for `embedding_threads`: the host's logical CPU count (at least 1).
/// ORT takes the value as a C `int`, so an unbounded setting could wrap to 0
/// (ORT default = ncpu) and silently defeat the cap.
fn embedding_threads_ceiling() -> usize {
    std::thread::available_parallelism().map_or(1, |n| n.get())
}

fn clamp_threads(requested: usize, ceiling: usize) -> usize {
    requested.clamp(1, ceiling.max(1))
}

/// Clamp a configured `embedding_threads` value to `1..=available_parallelism`,
/// warning when it is changed. Used for both the shared ORT pool size and the
/// model instance count.
pub(crate) fn clamp_embedding_threads(requested: usize) -> usize {
    let clamped = clamp_threads(requested, embedding_threads_ceiling());
    if clamped != requested {
        tracing::warn!(
            target: "semantic",
            "semantic_search.embedding_threads = {requested} is outside 1..={}; using {clamped}",
            embedding_threads_ceiling()
        );
    }
    clamped
}

/// Record the intra-op thread cap (clamped to `1..=available_parallelism`) for
/// the ORT environment. Cheap: nothing is initialized until the first embedding
/// session is created, so commands that never embed pay nothing. Has no
/// effect once the first embedding session has been created. Safe to call
/// repeatedly.
pub fn set_onnx_thread_cap(intra_threads: usize) {
    INTRA_THREADS.store(clamp_embedding_threads(intra_threads), Ordering::Relaxed);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clamp_threads_raises_zero_to_one() {
        assert_eq!(clamp_threads(0, 8), 1);
    }

    #[test]
    fn test_clamp_threads_caps_wrapping_values_at_ceiling() {
        assert_eq!(clamp_threads(1usize << 32, 8), 8);
        assert_eq!(clamp_threads(usize::MAX, 8), 8);
    }

    #[test]
    fn test_clamp_threads_keeps_in_range_value() {
        assert_eq!(clamp_threads(3, 8), 3);
    }

    #[test]
    fn test_clamp_threads_tolerates_zero_ceiling() {
        assert_eq!(clamp_threads(5, 0), 1);
    }
}
