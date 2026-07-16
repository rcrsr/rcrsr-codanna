//! `output_format` regression: one representative MCP tool (`get_calls`)
//! exercised end-to-end through `CodeIntelligenceServer`.
//!
//! Covers:
//! (a) omitting `output_format` yields the existing plain-text rendering.
//! (b) `output_format: json` yields a single `ContentBlock::text` whose
//!     payload parses as an `Envelope<T>` with `meta.schema_version` set
//!     and the status mapped correctly for success / not_found / ambiguous.

use std::sync::Arc;

use codanna::config::Settings;
use codanna::indexing::facade::IndexFacade;
use codanna::mcp::{CodeIntelligenceServer, GetCallsRequest, OutputFormat};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::ContentBlock;
use tempfile::TempDir;

/// Index a small fixture with:
/// - `caller` calling `helper` (the success case for `get_calls`).
/// - two same-named `dup` functions in different files (the ambiguous case).
async fn build_server() -> CodeIntelligenceServer {
    let temp = TempDir::new().expect("create temp dir");
    let src_dir = temp.path().join("src");
    std::fs::create_dir_all(&src_dir).expect("create src dir");

    std::fs::write(
        src_dir.join("caller.py"),
        "def helper():\n    pass\n\n\ndef caller():\n    helper()\n",
    )
    .expect("write caller.py fixture");

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

/// (a) Omitting `output_format` (i.e. leaving it at its `#[serde(default)]`
/// value, `OutputFormat::Text`) must still produce the existing
/// human-readable string content, not JSON.
#[tokio::test(flavor = "current_thread")]
async fn get_calls_default_output_format_is_plain_text() {
    let server = build_server().await;

    let result = server
        .get_calls(Parameters(GetCallsRequest {
            name: Some("caller".to_string()),
            symbol_id: None,
            output_format: OutputFormat::default(),
        }))
        .await
        .expect("get_calls should succeed");

    let text = text_of(&result.content);
    assert!(
        text.contains("caller calls"),
        "expected default (text) rendering, got:\n{text}"
    );
    assert!(
        serde_json::from_str::<serde_json::Value>(&text).is_err(),
        "default output_format must not produce parseable JSON, got:\n{text}"
    );
}

/// (b) `output_format: json`, success case: a resolvable, unambiguous
/// symbol with call data returns a `Success`-status envelope.
#[tokio::test(flavor = "current_thread")]
async fn get_calls_json_output_format_success() {
    let server = build_server().await;

    let result = server
        .get_calls(Parameters(GetCallsRequest {
            name: Some("caller".to_string()),
            symbol_id: None,
            output_format: OutputFormat::Json,
        }))
        .await
        .expect("get_calls should succeed");

    let text = text_of(&result.content);
    let envelope: serde_json::Value = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("expected parseable JSON envelope: {e}\ngot:\n{text}"));

    assert_eq!(envelope["status"], "success");
    assert_eq!(envelope["meta"]["schema_version"], "1.0.0");
    assert!(
        envelope["data"].as_array().is_some_and(|a| !a.is_empty()),
        "expected non-empty data array, got:\n{envelope}"
    );
}

/// (b) `output_format: json`, not-found case: a name with no matching
/// symbol returns a `NotFound`-status envelope with a null data payload.
#[tokio::test(flavor = "current_thread")]
async fn get_calls_json_output_format_not_found() {
    let server = build_server().await;

    let result = server
        .get_calls(Parameters(GetCallsRequest {
            name: Some("does_not_exist_anywhere".to_string()),
            symbol_id: None,
            output_format: OutputFormat::Json,
        }))
        .await
        .expect("get_calls should succeed (not-found is not a tool error)");

    let text = text_of(&result.content);
    let envelope: serde_json::Value = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("expected parseable JSON envelope: {e}\ngot:\n{text}"));

    assert_eq!(envelope["status"], "not_found");
    assert_eq!(envelope["meta"]["schema_version"], "1.0.0");
    assert!(envelope["data"].is_null());
}

/// (b) `output_format: json`, ambiguous case: a name matching more than
/// one symbol returns an `Ambiguous`-status envelope (W-1) listing the
/// candidates, refuse-and-list rather than aggregating relationships
/// across the unrelated same-named symbols.
#[tokio::test(flavor = "current_thread")]
async fn get_calls_json_output_format_ambiguous() {
    let server = build_server().await;

    let result = server
        .get_calls(Parameters(GetCallsRequest {
            name: Some("dup".to_string()),
            symbol_id: None,
            output_format: OutputFormat::Json,
        }))
        .await
        .expect("get_calls should succeed (ambiguity is not a tool error)");

    let text = text_of(&result.content);
    let envelope: serde_json::Value = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("expected parseable JSON envelope: {e}\ngot:\n{text}"));

    assert_eq!(envelope["status"], "ambiguous");
    assert_eq!(envelope["meta"]["schema_version"], "1.0.0");
    let candidates = envelope["data"]
        .as_array()
        .expect("ambiguous envelope must carry the candidate list in data");
    assert_eq!(
        candidates.len(),
        2,
        "expected both same-named 'dup' symbols listed as candidates, got:\n{envelope}"
    );
}
