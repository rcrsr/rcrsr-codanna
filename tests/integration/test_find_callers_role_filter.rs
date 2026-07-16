//! `find_callers` caller-role tagging, filtering, and `count_only`
//! (W-7), exercised end-to-end through `CodeIntelligenceServer`.
//!
//! Covers:
//! (a) each caller is tagged with its role (`production` for a `src/`
//!     caller, `test` for a `tests/`-path caller) via the path-heuristic
//!     classifier reading `caller_classification.test_path_patterns`.
//! (b) `filter: production` / `filter: test` partition the caller list to
//!     exactly the matching role.
//! (c) `count_only: true` returns totals with a per-role breakdown whose
//!     `production + test` sum equals the unfiltered total.
//! (d) the classifier function itself, unit-tested against each of the six
//!     default `test_path_patterns`.

use std::sync::Arc;

use codanna::config::Settings;
use codanna::indexing::facade::IndexFacade;
use codanna::mcp::requests::CallerFilter;
use codanna::mcp::service::classify_caller_role;
use codanna::mcp::{CodeIntelligenceServer, FindCallersRequest, OutputFormat};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::ContentBlock;
use tempfile::TempDir;

/// Index a fixture with:
/// - `target()` defined once in `src/target.py`.
/// - a production caller `src/prod_caller.py::caller` calling `target()`.
/// - a test caller `tests/test_caller.py::test_it` calling `target()`.
async fn build_server() -> CodeIntelligenceServer {
    let temp = TempDir::new().expect("create temp dir");
    let root = temp.path();
    let src_dir = root.join("src");
    let tests_dir = root.join("tests");
    std::fs::create_dir_all(&src_dir).expect("create src dir");
    std::fs::create_dir_all(&tests_dir).expect("create tests dir");

    std::fs::write(src_dir.join("target.py"), "def target():\n    pass\n")
        .expect("write target.py fixture");
    std::fs::write(
        src_dir.join("prod_caller.py"),
        "from target import target\n\n\ndef caller():\n    target()\n",
    )
    .expect("write prod_caller.py fixture");
    std::fs::write(
        tests_dir.join("test_caller.py"),
        "from target import target\n\n\ndef test_it():\n    target()\n",
    )
    .expect("write test_caller.py fixture");

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

/// (a) `filter: all` (default), `output_format: json`: both callers come
/// back, each tagged with the role matching its source path.
#[tokio::test(flavor = "current_thread")]
async fn find_callers_tags_each_caller_role() {
    let server = build_server().await;

    let result = server
        .find_callers(Parameters(FindCallersRequest {
            name: Some("target".to_string()),
            symbol_id: None,
            filter: CallerFilter::All,
            count_only: false,
            output_format: OutputFormat::Json,
        }))
        .await
        .expect("find_callers should succeed");

    let text = text_of(&result.content);
    let envelope: serde_json::Value = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("expected parseable JSON envelope: {e}\ngot:\n{text}"));

    assert_eq!(envelope["status"], "success", "envelope: {envelope}");
    let data = envelope["data"]
        .as_array()
        .unwrap_or_else(|| panic!("expected a caller array, got:\n{envelope}"));
    assert_eq!(
        data.len(),
        2,
        "expected both the production and test caller: {data:?}"
    );

    let roles: std::collections::BTreeSet<&str> = data
        .iter()
        .map(|c| c["role"].as_str().expect("each caller must carry a role"))
        .collect();
    assert_eq!(
        roles,
        std::collections::BTreeSet::from(["production", "test"]),
        "expected one production-tagged and one test-tagged caller: {data:?}"
    );
}

/// (b) `filter: production` returns only the `src/` caller.
#[tokio::test(flavor = "current_thread")]
async fn find_callers_filter_production_excludes_test_caller() {
    let server = build_server().await;

    let result = server
        .find_callers(Parameters(FindCallersRequest {
            name: Some("target".to_string()),
            symbol_id: None,
            filter: CallerFilter::Production,
            count_only: false,
            output_format: OutputFormat::Json,
        }))
        .await
        .expect("find_callers should succeed");

    let text = text_of(&result.content);
    let envelope: serde_json::Value = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("expected parseable JSON envelope: {e}\ngot:\n{text}"));

    let data = envelope["data"]
        .as_array()
        .unwrap_or_else(|| panic!("expected a caller array, got:\n{envelope}"));
    assert_eq!(
        data.len(),
        1,
        "expected only the production caller: {data:?}"
    );
    assert_eq!(data[0]["role"], "production");
    assert_eq!(data[0]["name"], "caller");
}

/// (b) `filter: test` returns only the `tests/` caller.
#[tokio::test(flavor = "current_thread")]
async fn find_callers_filter_test_excludes_production_caller() {
    let server = build_server().await;

    let result = server
        .find_callers(Parameters(FindCallersRequest {
            name: Some("target".to_string()),
            symbol_id: None,
            filter: CallerFilter::Test,
            count_only: false,
            output_format: OutputFormat::Json,
        }))
        .await
        .expect("find_callers should succeed");

    let text = text_of(&result.content);
    let envelope: serde_json::Value = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("expected parseable JSON envelope: {e}\ngot:\n{text}"));

    let data = envelope["data"]
        .as_array()
        .unwrap_or_else(|| panic!("expected a caller array, got:\n{envelope}"));
    assert_eq!(data.len(), 1, "expected only the test caller: {data:?}");
    assert_eq!(data[0]["role"], "test");
    assert_eq!(data[0]["name"], "test_it");
}

/// (c) `count_only: true` with `filter: all` returns totals whose
/// per-role breakdown sums to the unfiltered total.
#[tokio::test(flavor = "current_thread")]
async fn find_callers_count_only_totals_sum_to_all() {
    let server = build_server().await;

    let result = server
        .find_callers(Parameters(FindCallersRequest {
            name: Some("target".to_string()),
            symbol_id: None,
            filter: CallerFilter::All,
            count_only: true,
            output_format: OutputFormat::Json,
        }))
        .await
        .expect("find_callers should succeed");

    let text = text_of(&result.content);
    let envelope: serde_json::Value = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("expected parseable JSON envelope: {e}\ngot:\n{text}"));

    assert_eq!(envelope["status"], "success", "envelope: {envelope}");
    let data = &envelope["data"];
    let total = data["total"].as_u64().expect("data.total must be a number");
    let production = data["production"]
        .as_u64()
        .expect("data.production must be a number");
    let test = data["test"].as_u64().expect("data.test must be a number");

    assert_eq!(total, 2, "expected 2 total callers: {data}");
    assert_eq!(production, 1, "expected 1 production caller: {data}");
    assert_eq!(test, 1, "expected 1 test caller: {data}");
    assert_eq!(
        production + test,
        total,
        "per-role totals must sum to the overall total: {data}"
    );
}

/// (c) `count_only: true` combined with `filter: production` still reports
/// the true non-zero `test` count in the breakdown: `filter` narrows the
/// returned *listing*, never the counted breakdown, so the other role's
/// count must not be zeroed out just because a filter was applied.
#[tokio::test(flavor = "current_thread")]
async fn find_callers_count_only_with_filter_still_reports_unfiltered_breakdown() {
    let server = build_server().await;

    let result = server
        .find_callers(Parameters(FindCallersRequest {
            name: Some("target".to_string()),
            symbol_id: None,
            filter: CallerFilter::Production,
            count_only: true,
            output_format: OutputFormat::Json,
        }))
        .await
        .expect("find_callers should succeed");

    let text = text_of(&result.content);
    let envelope: serde_json::Value = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("expected parseable JSON envelope: {e}\ngot:\n{text}"));

    assert_eq!(envelope["status"], "success", "envelope: {envelope}");
    let data = &envelope["data"];
    let total = data["total"].as_u64().expect("data.total must be a number");
    let production = data["production"]
        .as_u64()
        .expect("data.production must be a number");
    let test = data["test"].as_u64().expect("data.test must be a number");

    assert_eq!(
        total, 2,
        "total must remain the UNFILTERED total, not narrowed by `filter`: {data}"
    );
    assert_eq!(production, 1, "expected 1 production caller: {data}");
    assert_eq!(
        test, 1,
        "expected the true non-zero test count even though filter:production was applied: {data}"
    );
}

/// (d) unit-level: the path-heuristic classifier tags each of the six
/// default `test_path_patterns` as `test`, and an ordinary `src/` path as
/// `production`.
#[test]
fn classify_caller_role_covers_all_default_patterns() {
    let patterns: Vec<String> = vec![
        "tests/".to_string(),
        "/test/".to_string(),
        "*_test.*".to_string(),
        "test_*.py".to_string(),
        "*.spec.*".to_string(),
        "__tests__/".to_string(),
    ];

    for path in [
        "tests/integration_test.rs",
        "src/test/helpers.rs",
        "src/widget_test.rs",
        "scripts/test_helpers.py",
        "src/widget.spec.ts",
        "src/__tests__/widget.tsx",
    ] {
        assert_eq!(
            format!("{:?}", classify_caller_role(path, &patterns)),
            "Test",
            "expected {path} to classify as Test"
        );
    }

    assert_eq!(
        format!("{:?}", classify_caller_role("src/widget.rs", &patterns)),
        "Production",
        "expected an ordinary src/ path to classify as Production"
    );
}
