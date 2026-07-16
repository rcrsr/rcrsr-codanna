//! `get_file_outline` and `read_symbol` (W-8), exercised end-to-end through
//! `CodeIntelligenceServer`.
//!
//! Covers:
//! (a) `get_file_outline` returns one entry per symbol defined in the file,
//!     with the expected kind for each.
//! (b) `read_symbol` returns a source span that equals the known source
//!     text of the target function, sliced by line/column (not byte offset).
//! (c) mutating the indexed file on disk after indexing, then calling
//!     `read_symbol` again, trips the staleness guard (an `Error` envelope
//!     mentioning the mismatch) instead of returning a possibly-shifted
//!     span — this also validates the W-3 file-hash accessor is wired up.

use std::sync::Arc;

use codanna::config::Settings;
use codanna::indexing::facade::IndexFacade;
use codanna::mcp::requests::{GetFileOutlineRequest, ReadSymbolRequest};
use codanna::mcp::{CodeIntelligenceServer, OutputFormat};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::ContentBlock;
use tempfile::TempDir;

const FIXTURE_SOURCE: &str = "def helper():\n    pass\n\n\nclass Widget:\n    pass\n";

/// Index a fixture file (`src/outline_target.py`) containing one function
/// (`helper`) and one class (`Widget`). Returns the server plus the
/// on-disk path of the fixture file so tests can mutate it later.
async fn build_server() -> (CodeIntelligenceServer, std::path::PathBuf) {
    let temp = TempDir::new().expect("create temp dir");
    let src_dir = temp.path().join("src");
    std::fs::create_dir_all(&src_dir).expect("create src dir");

    let fixture_path = src_dir.join("outline_target.py");
    std::fs::write(&fixture_path, FIXTURE_SOURCE).expect("write fixture file");

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
    // the facade only needs the on-disk index (and, for read_symbol, the
    // fixture file itself, which must survive until the mutate-and-assert
    // step below), but `TempDir` deletes on drop.
    std::mem::forget(temp);

    (CodeIntelligenceServer::new(facade), fixture_path)
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

/// `get_file_outline`'s JSON payload must contain one entry per indexed
/// symbol in the file (including the language's implicit module symbol),
/// with kinds matching the fixture: one `Function` (`helper`) and one
/// `Class` (`Widget`).
#[tokio::test(flavor = "current_thread")]
async fn get_file_outline_lists_symbol_count_and_kinds() {
    let (server, fixture_path) = build_server().await;
    let path = fixture_path.to_string_lossy().to_string();

    let result = server
        .get_file_outline(Parameters(GetFileOutlineRequest {
            path,
            output_format: OutputFormat::Json,
        }))
        .await
        .expect("get_file_outline call succeeds");

    let text = text_of(&result.content);
    let envelope: serde_json::Value =
        serde_json::from_str(&text).expect("get_file_outline returns valid JSON envelope");

    assert_eq!(envelope["status"], "success", "envelope: {envelope}");
    let data = envelope["data"]
        .as_array()
        .expect("data is an array of outline entries");
    assert!(
        data.len() >= 2,
        "expected at least 2 symbols (helper, Widget), got: {data:?}"
    );

    let kinds: Vec<&str> = data
        .iter()
        .map(|entry| entry["kind"].as_str().expect("kind is a string"))
        .collect();
    assert!(
        kinds.contains(&"Function"),
        "expected a Function entry, got: {kinds:?}"
    );
    assert!(
        kinds.contains(&"Class"),
        "expected a Class entry, got: {kinds:?}"
    );

    let helper_entry = data
        .iter()
        .find(|entry| entry["name"] == "helper")
        .expect("outline includes the helper function entry");
    assert_eq!(helper_entry["kind"], "Function");
    assert_eq!(helper_entry["start_line"], 1);
    assert_eq!(helper_entry["end_line"], 2);
}

/// `read_symbol`'s returned span must equal the exact known source text of
/// `helper`, sliced by the indexed `Range`'s line/column (not a byte
/// offset into the whole file).
#[tokio::test(flavor = "current_thread")]
async fn read_symbol_span_equals_known_source() {
    let (server, _fixture_path) = build_server().await;

    let result = server
        .read_symbol(Parameters(ReadSymbolRequest {
            name: Some("helper".to_string()),
            symbol_id: None,
            output_format: OutputFormat::Json,
        }))
        .await
        .expect("read_symbol call succeeds");

    let text = text_of(&result.content);
    let envelope: serde_json::Value =
        serde_json::from_str(&text).expect("read_symbol returns valid JSON envelope");

    assert_eq!(envelope["status"], "success", "envelope: {envelope}");
    let source = envelope["data"]["source"]
        .as_str()
        .expect("data.source is a string");
    assert_eq!(source, "def helper():\n    pass");
}

/// Mutating the indexed file on disk after indexing (without reindexing)
/// must trip the staleness guard on the next `read_symbol` call: an
/// `Error`-status envelope whose message flags the mismatch, never a span
/// sliced against the now-shifted file. This exercises the W-3
/// `get_file_hash_for_path` accessor end-to-end.
#[tokio::test(flavor = "current_thread")]
async fn read_symbol_refuses_stale_index_after_on_disk_mutation() {
    let (server, fixture_path) = build_server().await;

    // Mutate the file on disk without reindexing: prepend a line so every
    // line/column offset below `helper` shifts by one line.
    let mutated = format!("# mutated\n{FIXTURE_SOURCE}");
    std::fs::write(&fixture_path, mutated).expect("mutate fixture file on disk");

    let result = server
        .read_symbol(Parameters(ReadSymbolRequest {
            name: Some("helper".to_string()),
            symbol_id: None,
            output_format: OutputFormat::Json,
        }))
        .await
        .expect("read_symbol call succeeds (envelope carries the error, not a transport failure)");

    let text = text_of(&result.content);
    let envelope: serde_json::Value =
        serde_json::from_str(&text).expect("read_symbol returns valid JSON envelope");

    assert_eq!(
        envelope["status"], "error",
        "stale file must not yield a success envelope: {envelope}"
    );
    let message = envelope["message"]
        .as_str()
        .expect("message is a string")
        .to_ascii_uppercase();
    assert!(
        message.contains("STALE_INDEX") || message.contains("STALE"),
        "expected a staleness-flagged message, got: {message}"
    );
    assert!(
        envelope["data"].is_null(),
        "stale envelope must carry no data (no possibly-shifted span): {envelope}"
    );
}
