//! `find_symbols` (batch symbol lookup) exercised end-to-end through
//! `CodeIntelligenceServer`.
//!
//! Covers:
//! (a) a single batch call whose per-name result map contains one `found`,
//!     one `not_found`, and one `ambiguous` entry.
//! (b) a batch over the 1024-name cap is rejected with a cap-message error
//!     envelope rather than being silently truncated or processed.

use std::sync::Arc;

use codanna::config::Settings;
use codanna::indexing::facade::IndexFacade;
use codanna::mcp::{CodeIntelligenceServer, FindSymbolsRequest, OutputFormat};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::ContentBlock;
use tempfile::TempDir;

/// Index a small fixture with:
/// - `helper` defined once (the `found` case).
/// - `dup` defined in two different files (the `ambiguous` case).
/// - no definition at all for `missing_symbol` (the `not_found` case).
async fn build_server() -> CodeIntelligenceServer {
    let temp = TempDir::new().expect("create temp dir");
    let src_dir = temp.path().join("src");
    std::fs::create_dir_all(&src_dir).expect("create src dir");

    std::fs::write(src_dir.join("helper.py"), "def helper():\n    pass\n")
        .expect("write helper.py fixture");
    std::fs::write(src_dir.join("dup_a.py"), "def dup():\n    pass\n")
        .expect("write dup_a.py fixture");
    std::fs::write(src_dir.join("dup_b.py"), "def dup():\n    pass\n")
        .expect("write dup_b.py fixture");

    let settings = Settings {
        index_path: temp.path().join("index"),
        workspace_root: None,
        ..Default::default()
    };
    let mut facade =
        IndexFacade::new(Arc::new(settings)).expect("create facade over temp index dir");
    facade
        .index_directory(&src_dir, false)
        .expect("index fixture directory");

    // Keep the temp dir alive for the duration of the test by leaking it:
    // the facade only needs the on-disk index, not the source directory,
    // once indexing has completed, but `TempDir` deletes on drop.
    std::mem::forget(temp);

    CodeIntelligenceServer::new(facade)
}

fn text_of(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|c| match c {
            ContentBlock::Text(block) => Some(block.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// One `find_symbols` call over `["helper", "missing_symbol", "dup"]` must
/// classify each name independently: `helper` -> found, `missing_symbol` ->
/// not_found, `dup` -> ambiguous with two candidates. This also guards
/// against a "flatten to one status" bug: all three statuses must coexist
/// in the same response map.
#[tokio::test(flavor = "current_thread")]
async fn find_symbols_batch_returns_found_not_found_and_ambiguous() {
    let server = build_server().await;

    let result = server
        .find_symbols(Parameters(FindSymbolsRequest {
            names: vec![
                "helper".to_string(),
                "missing_symbol".to_string(),
                "dup".to_string(),
            ],
            lang: None,
            output_format: OutputFormat::Json,
        }))
        .await
        .expect("find_symbols should succeed");

    let text = text_of(&result.content);
    let envelope: serde_json::Value =
        serde_json::from_str(&text).expect("find_symbols JSON output must parse");

    assert_eq!(
        envelope["status"], "success",
        "batch call itself must succeed even though individual names miss/ambiguous: {envelope}"
    );

    let data = &envelope["data"];

    assert_eq!(
        data["helper"]["status"], "found",
        "helper must resolve as found: {data}"
    );
    assert!(
        data["helper"]["location"].is_string(),
        "found entry must carry a location: {data}"
    );
    assert!(
        data["helper"]["kind"].is_string(),
        "found entry must carry a kind: {data}"
    );
    assert!(
        data["helper"]["line_range"].is_array(),
        "found entry must carry a line_range: {data}"
    );

    assert_eq!(
        data["missing_symbol"]["status"], "not_found",
        "missing_symbol must resolve as not_found: {data}"
    );

    assert_eq!(
        data["dup"]["status"], "ambiguous",
        "dup must resolve as ambiguous (defined in two files): {data}"
    );
    let candidates = data["dup"]["candidates"]
        .as_array()
        .expect("ambiguous entry must carry a candidates array");
    assert_eq!(
        candidates.len(),
        2,
        "dup must surface both candidate definitions: {data}"
    );
}

/// A batch of 1025 names (one over `MAX_FIND_SYMBOLS_NAMES`) must be
/// rejected outright with a cap-referencing error message, mirroring
/// `MAX_REINDEX_PATHS` in `server.rs`. It must never be silently truncated
/// to the first 1024 names and processed.
#[tokio::test(flavor = "current_thread")]
async fn find_symbols_batch_over_cap_is_rejected() {
    let server = build_server().await;

    let names: Vec<String> = (0..1025).map(|i| format!("name_{i}")).collect();

    let result = server
        .find_symbols(Parameters(FindSymbolsRequest {
            names,
            lang: None,
            output_format: OutputFormat::Json,
        }))
        .await
        .expect("find_symbols must return a tool result, not an MCP protocol error");

    let text = text_of(&result.content);
    let envelope: serde_json::Value =
        serde_json::from_str(&text).expect("find_symbols JSON output must parse");

    assert_eq!(
        envelope["status"], "error",
        "an over-cap batch must be rejected as an error envelope: {envelope}"
    );
    let message = envelope["message"]
        .as_str()
        .expect("error envelope must carry a message");
    assert!(
        message.contains("1025") && message.contains("1024"),
        "error message must reference both the requested count and the cap: {message}"
    );
    assert!(
        envelope["data"].is_null(),
        "an over-cap batch must not carry a partial data payload: {envelope}"
    );
}
