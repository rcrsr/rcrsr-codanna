//! Common utilities shared across modules.

use chrono::Utc;

/// Get current UTC timestamp in seconds since UNIX_EPOCH.
///
/// Uses chrono for accurate cross-platform timestamp.
pub fn get_utc_timestamp() -> u64 {
    Utc::now().timestamp() as u64
}

/// Describe a `tokio::task::JoinError` from an awaited `spawn_blocking` task,
/// distinguishing cancellation (e.g. runtime shutdown, benign) from an actual
/// panic inside the task.
///
/// Shared by every `spawn_blocking(...).await` error branch across
/// `indexing/facade.rs`, `mcp/tools/search.rs`, `mcp/server.rs`, and
/// `cli/commands/mcp.rs` so operators can tell a shutdown from a genuine
/// panic regardless of which call site surfaced the error.
pub fn describe_join_error(e: &tokio::task::JoinError) -> String {
    if e.is_cancelled() {
        "task was cancelled".to_string()
    } else {
        format!("task panicked: {e}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_utc_timestamp() {
        let ts = get_utc_timestamp();
        // Should be a reasonable Unix timestamp (after 2020)
        assert!(ts > 1577836800, "Timestamp should be after 2020-01-01");
    }

    #[tokio::test]
    async fn test_describe_join_error_panic() {
        let handle = tokio::task::spawn_blocking(|| panic!("boom"));
        let err = handle
            .await
            .expect_err("panicking task must yield a JoinError");

        let message = describe_join_error(&err);

        assert!(
            message.starts_with("task panicked:"),
            "expected panic message, got: {message}"
        );
    }

    #[tokio::test]
    async fn test_describe_join_error_cancelled() {
        let handle = tokio::task::spawn_blocking(|| {
            std::thread::sleep(std::time::Duration::from_millis(50));
        });
        handle.abort();
        let err = handle
            .await
            .expect_err("aborted task must yield a JoinError");

        let message = describe_join_error(&err);

        assert_eq!(message, "task was cancelled");
    }
}
