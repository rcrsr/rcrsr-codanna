//! `analyze_impact` `count_only`, `max_results` truncation, and `group_by`
//! (W-9), exercised end-to-end through `CodeIntelligenceServer`.
//!
//! Covers:
//! (a) `count_only: true` returns a symbol count plus a distinct-file count,
//!     with no listing in `data`.
//! (b) `max_results` truncates the listing and sets `meta.truncated: true`;
//!     omitting `max_results` (or setting it above the result size) leaves
//!     `meta.truncated` false/absent.
//! (c) `group_by: file` regroups the same impact set by file instead of by
//!     kind (the default), without changing the total count.

use std::sync::Arc;

use codanna::config::Settings;
use codanna::indexing::facade::IndexFacade;
use codanna::mcp::requests::GroupBy;
use codanna::mcp::{AnalyzeImpactRequest, CodeIntelligenceServer, OutputFormat};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::ContentBlock;
use tempfile::TempDir;

/// Index a fixture with `target()` defined once in `src/target.py` and
/// three distinct-file callers (`src/caller_a.py`, `src/caller_b.py`,
/// `src/caller_c.py`), so the impact radius spans 3 symbols across 3 files.
async fn build_server() -> CodeIntelligenceServer {
    let temp = TempDir::new().expect("create temp dir");
    let root = temp.path();
    let src_dir = root.join("src");
    std::fs::create_dir_all(&src_dir).expect("create src dir");

    std::fs::write(src_dir.join("target.py"), "def target():\n    pass\n")
        .expect("write target.py fixture");
    for (file, func) in [
        ("caller_a.py", "caller_a"),
        ("caller_b.py", "caller_b"),
        ("caller_c.py", "caller_c"),
    ] {
        std::fs::write(
            src_dir.join(file),
            format!("from target import target\n\n\ndef {func}():\n    target()\n"),
        )
        .unwrap_or_else(|e| panic!("write {file} fixture: {e}"));
    }

    let settings = Settings {
        index_path: root.join("index"),
        workspace_root: None,
        ..Default::default()
    };
    let mut facade =
        IndexFacade::new(Arc::new(settings)).expect("create facade over temp index dir");
    facade
        .index_directory(root, false)
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

fn base_request() -> AnalyzeImpactRequest {
    AnalyzeImpactRequest {
        name: Some("target".to_string()),
        symbol_id: None,
        max_depth: 3,
        count_only: false,
        max_results: 0,
        group_by: GroupBy::Kind,
        output_format: OutputFormat::Json,
    }
}

/// (a) `count_only: true` returns totals (symbol count + distinct-file
/// count) with no listing.
#[tokio::test(flavor = "current_thread")]
async fn analyze_impact_count_only_returns_counts_without_listing() {
    let server = build_server().await;

    let result = server
        .analyze_impact(Parameters(AnalyzeImpactRequest {
            count_only: true,
            ..base_request()
        }))
        .await
        .expect("analyze_impact should succeed");

    let text = text_of(&result.content);
    let envelope: serde_json::Value = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("expected parseable JSON envelope: {e}\ngot:\n{text}"));

    assert_eq!(envelope["status"], "success", "envelope: {envelope}");
    let data = &envelope["data"];
    assert!(
        data.is_object(),
        "count_only data must be an object of totals, not a listing: {data}"
    );
    assert!(
        data.get("total").is_some() && data.get("files").is_some(),
        "expected data.total and data.files, got: {data}"
    );
    let total = data["total"].as_u64().expect("data.total must be a number");
    let files = data["files"].as_u64().expect("data.files must be a number");
    assert_eq!(total, 3, "expected 3 impacted symbols: {data}");
    assert_eq!(files, 3, "expected 3 distinct files: {data}");
}

/// (b) `max_results` below the result size truncates the listing and sets
/// `meta.truncated: true`.
#[tokio::test(flavor = "current_thread")]
async fn analyze_impact_max_results_truncates_and_sets_meta_flag() {
    let server = build_server().await;

    let result = server
        .analyze_impact(Parameters(AnalyzeImpactRequest {
            max_results: 1,
            ..base_request()
        }))
        .await
        .expect("analyze_impact should succeed");

    let text = text_of(&result.content);
    let envelope: serde_json::Value = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("expected parseable JSON envelope: {e}\ngot:\n{text}"));

    let data = envelope["data"]
        .as_array()
        .unwrap_or_else(|| panic!("expected an impact listing array, got:\n{envelope}"));
    assert_eq!(
        data.len(),
        1,
        "expected the listing truncated to 1: {data:?}"
    );
    assert_eq!(
        envelope["meta"]["count"], 3,
        "meta.count must report the untruncated total: {envelope}"
    );
    assert_eq!(
        envelope["meta"]["truncated"], true,
        "expected meta.truncated: true when max_results < total: {envelope}"
    );
}

/// (b) no `max_results` (0, the default = unlimited): the full listing
/// comes back and `meta.truncated` is false or absent.
#[tokio::test(flavor = "current_thread")]
async fn analyze_impact_without_max_results_is_not_truncated() {
    let server = build_server().await;

    let result = server
        .analyze_impact(Parameters(base_request()))
        .await
        .expect("analyze_impact should succeed");

    let text = text_of(&result.content);
    let envelope: serde_json::Value = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("expected parseable JSON envelope: {e}\ngot:\n{text}"));

    let data = envelope["data"]
        .as_array()
        .unwrap_or_else(|| panic!("expected an impact listing array, got:\n{envelope}"));
    assert_eq!(
        data.len(),
        3,
        "expected the full untruncated listing: {data:?}"
    );

    let truncated = &envelope["meta"]["truncated"];
    assert!(
        truncated.is_null() || truncated == false,
        "expected meta.truncated false/absent when not truncated: {envelope}"
    );
}

/// (c) `group_by: file` regroups the same impact set by file (contiguous
/// runs of identical `file_path`) instead of the default kind-ordered
/// (BFS) listing, without changing the total count.
#[tokio::test(flavor = "current_thread")]
async fn analyze_impact_group_by_file_regroups_listing() {
    let server = build_server().await;

    let result = server
        .analyze_impact(Parameters(AnalyzeImpactRequest {
            group_by: GroupBy::File,
            ..base_request()
        }))
        .await
        .expect("analyze_impact should succeed");

    let text = text_of(&result.content);
    let envelope: serde_json::Value = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("expected parseable JSON envelope: {e}\ngot:\n{text}"));

    let data = envelope["data"]
        .as_array()
        .unwrap_or_else(|| panic!("expected an impact listing array, got:\n{envelope}"));
    assert_eq!(
        data.len(),
        3,
        "grouping must not change the total count: {data:?}"
    );

    let file_paths: Vec<&str> = data
        .iter()
        .map(|s| {
            s["file_path"]
                .as_str()
                .expect("each symbol has a file_path")
        })
        .collect();
    let mut sorted = file_paths.clone();
    sorted.sort_unstable();
    assert_eq!(
        file_paths, sorted,
        "group_by: file must order the listing by file_path: {file_paths:?}"
    );

    let distinct: std::collections::BTreeSet<&str> = file_paths.into_iter().collect();
    assert_eq!(
        distinct.len(),
        3,
        "expected each caller's distinct file represented: {distinct:?}"
    );
}

/// `group_by: file` combined with `max_results` must truncate the SAME
/// grouped/ordered subset in both the JSON and text renderings: `group_by`
/// ordering is applied first, then `max_results` truncation, in both paths
/// (`service::group_and_truncate_impact`). Previously the JSON path grouped
/// then truncated while the text path truncated the raw BFS order before
/// grouping, so the two could return different symbols for an identical
/// request.
#[tokio::test(flavor = "current_thread")]
async fn analyze_impact_group_by_file_max_results_truncates_same_subset_in_json_and_text() {
    let server = build_server().await;

    let json_result = server
        .analyze_impact(Parameters(AnalyzeImpactRequest {
            group_by: GroupBy::File,
            max_results: 1,
            output_format: OutputFormat::Json,
            ..base_request()
        }))
        .await
        .expect("analyze_impact (json) should succeed");
    let json_text = text_of(&json_result.content);
    let envelope: serde_json::Value = serde_json::from_str(&json_text)
        .unwrap_or_else(|e| panic!("expected parseable JSON envelope: {e}\ngot:\n{json_text}"));

    let data = envelope["data"]
        .as_array()
        .unwrap_or_else(|| panic!("expected an impact listing array, got:\n{envelope}"));
    assert_eq!(
        data.len(),
        1,
        "expected the listing truncated to 1: {data:?}"
    );
    assert_eq!(
        envelope["meta"]["truncated"], true,
        "expected meta.truncated: true: {envelope}"
    );
    let json_symbol_name = data[0]["name"]
        .as_str()
        .expect("each symbol has a name")
        .to_string();

    let text_result = server
        .analyze_impact(Parameters(AnalyzeImpactRequest {
            group_by: GroupBy::File,
            max_results: 1,
            output_format: OutputFormat::Text,
            ..base_request()
        }))
        .await
        .expect("analyze_impact (text) should succeed");
    let text_output = text_of(&text_result.content);

    assert!(
        text_output.contains(&json_symbol_name),
        "expected the text-mode truncated listing to contain the same symbol \
         ('{json_symbol_name}') the JSON path truncated to (group-then-truncate order \
         must match between the two renderings):\n{text_output}"
    );
    assert!(
        text_output.contains("truncated to 1 of 3 symbol(s)"),
        "expected the text output to report the truncation: {text_output}"
    );
}
