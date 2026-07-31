//! Line-convention and location-composition contract for `codanna mcp`.
//!
//! Scalar `line` fields are 1-indexed editor coordinates on every channel
//! (search rows, tuple relationshipMetadata, inline call_line); text
//! locations name real places (callee def + explicit call site). Search
//! rows report the symbol's true kind via the one kind vocabulary.

use serde_json::Value;
use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

fn codanna_binary() -> PathBuf {
    if let Some(path) = option_env!("CARGO_BIN_EXE_codanna") {
        let bin = PathBuf::from(path);
        if bin.exists() {
            return bin;
        }
    }

    let manifest_dir = env::var("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| env::current_dir().expect("current dir"));

    let debug_bin = if cfg!(windows) {
        manifest_dir.join("target/debug/codanna.exe")
    } else {
        manifest_dir.join("target/debug/codanna")
    };
    if debug_bin.exists() {
        return debug_bin;
    }

    let status = Command::new("cargo")
        .args(["build", "--bin", "codanna"])
        .current_dir(&manifest_dir)
        .status()
        .expect("build codanna binary");
    assert!(status.success(), "cargo build failed");
    debug_bin
}

fn run_cli(workspace: &Path, args: &[&str]) -> (i32, String, String) {
    let bin = codanna_binary();
    let test_home = workspace.join(".home");
    std::fs::create_dir_all(&test_home).expect("create test home");

    let output = Command::new(&bin)
        .args(args)
        .current_dir(workspace)
        .env("HOME", &test_home)
        .output()
        .expect("run codanna CLI");

    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}

// Fixture layout is load-bearing: assertions below name these exact
// 1-indexed lines.
const RUST_FIXTURE: &str = "\
pub struct MarkerStructKind;

pub fn line_target() -> i32 {
    7
}

pub fn line_caller() -> i32 {
    line_target() + 1
}
";
const TARGET_DEF_LINE: i64 = 3;
/// Line where `line_caller` is DEFINED. Independent of [`CALLER_CALL_LINE`]
/// below: the two coincide-by-one only because this fixture's body happens
/// to be a single line, so deriving one from the other would silently assert
/// a wrong constant the moment a line is added to `line_caller`.
const CALLER_DEF_LINE: i64 = 7;
/// Line of the `line_target()` CALL SITE inside `line_caller`'s body.
const CALLER_CALL_LINE: i64 = 8;

const JAVA_FIXTURE: &str = "\
public class MarkerClassKind {
    public int markerMethodKind() {
        return 7;
    }
}
";

fn setup(workspace: &Path) {
    let src = workspace.join("src");
    std::fs::create_dir_all(&src).expect("create src dir");
    std::fs::write(src.join("fixture.rs"), RUST_FIXTURE).expect("write rust fixture");
    std::fs::write(src.join("Marker.java"), JAVA_FIXTURE).expect("write java fixture");

    let codanna_dir = workspace.join(".codanna");
    std::fs::create_dir_all(&codanna_dir).expect("create .codanna");
    let src_abs = src.canonicalize().expect("resolvable src");
    let settings = format!(
        r#"
index_path = ".codanna/index"

[indexing]
indexed_paths = ["{}"]

[semantic_search]
enabled = false
"#,
        src_abs.to_str().expect("utf-8 path")
    );
    std::fs::write(codanna_dir.join("settings.toml"), settings).expect("write settings");

    let (code, stdout, stderr) = run_cli(workspace, &["index", "src", "--force", "--no-progress"]);
    assert_eq!(
        code, 0,
        "index should succeed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

fn json_data(stdout: &str) -> Value {
    let payload: Value = serde_json::from_str(stdout)
        .unwrap_or_else(|e| panic!("stdout is not JSON: {e}\nstdout:\n{stdout}"));
    payload["data"].clone()
}

#[test]
fn search_rows_carry_one_indexed_lines_and_true_kinds() {
    let workspace = TempDir::new().expect("temp dir");
    setup(workspace.path());

    let (code, stdout, _) = run_cli(
        workspace.path(),
        &["mcp", "search_symbols", "query:line_target", "--json"],
    );
    assert_eq!(code, 0);
    let row = &json_data(&stdout)[0]["symbol"];
    assert_eq!(
        row["line"].as_i64(),
        Some(TARGET_DEF_LINE),
        "search line must be the 1-indexed def line\nrow: {row}"
    );

    for (query, expected_kind, expected_lang) in [
        ("MarkerStructKind", "Struct", "rust"),
        ("MarkerClassKind", "Class", "java"),
        ("markerMethodKind", "Method", "java"),
    ] {
        let (code, stdout, _) = run_cli(
            workspace.path(),
            &["mcp", "search_symbols", &format!("query:{query}"), "--json"],
        );
        assert_eq!(code, 0, "search for {query}");
        let row = &json_data(&stdout)[0]["symbol"];
        assert_eq!(
            row["kind"].as_str(),
            Some(expected_kind),
            "search must report the true kind for {query}\nrow: {row}"
        );
        assert_eq!(
            row["language_id"].as_str(),
            Some(expected_lang),
            "search rows must carry language_id\nrow: {row}"
        );
    }
}

#[test]
fn index_info_languages_map_partitions_symbol_count() {
    let workspace = TempDir::new().expect("temp dir");
    setup(workspace.path());

    let (code, stdout, _) = run_cli(workspace.path(), &["mcp", "get_index_info", "--json"]);
    assert_eq!(code, 0);
    let data = json_data(&stdout);
    let languages = data["languages"]
        .as_object()
        .expect("languages map present");
    assert!(languages.contains_key("rust") && languages.contains_key("java"));
    let sum: i64 = languages.values().filter_map(|v| v.as_i64()).sum();
    assert_eq!(
        Some(sum),
        data["symbol_count"].as_i64(),
        "languages map must partition symbol_count"
    );

    let (code, stdout, _) = run_cli(workspace.path(), &["mcp", "get_index_info"]);
    assert_eq!(code, 0);
    assert!(
        stdout.contains("Languages:") && stdout.contains("- java:"),
        "text rendering must show the languages section\nstdout:\n{stdout}"
    );
}

#[test]
fn tuple_metadata_line_matches_inline_call_line() {
    let workspace = TempDir::new().expect("temp dir");
    setup(workspace.path());

    // Inline channel: find_callers call_line
    let (code, stdout, _) = run_cli(
        workspace.path(),
        &["mcp", "find_callers", "function_name:line_target", "--json"],
    );
    assert_eq!(code, 0);
    let inline = json_data(&stdout)[0]["call_line"]
        .as_i64()
        .expect("caller row carries call_line");
    assert_eq!(inline, CALLER_CALL_LINE, "inline call_line is 1-indexed");

    // Tuple channel: find_symbol relationships.called_by metadata line
    let (code, stdout, _) = run_cli(
        workspace.path(),
        &["mcp", "find_symbol", "name:line_target", "--json"],
    );
    assert_eq!(code, 0);
    let called_by = &json_data(&stdout)[0]["relationships"]["called_by"][0];
    let tuple_line = called_by[1]["line"]
        .as_i64()
        .expect("tuple metadata carries line");
    assert_eq!(
        tuple_line, inline,
        "tuple relationshipMetadata.line must equal inline call_line on the same edge"
    );
}

// --- W-1: single-point 1-indexing for the serialized `Range` object ---
//
// `read_symbol` and `get_file_outline` have no `--json` wiring through the
// `codanna mcp <tool> --json` CLI shortcut (the shortcut's `let result = if
// json { .. }` stub never dispatches to those two tools; see
// `src/cli/commands/mcp.rs`) -- a pre-existing gap out of this task's scope
// (only `src/symbol/mod.rs` and this file). The `range_agrees_with_scalar_line_fields`
// and `range_columns_are_unshifted` tests below therefore call
// `CodeIntelligenceServer` directly, matching the precedent in
// `tests/integration/test_read_symbol_and_outline_mcp.rs`, instead of
// shelling out to the CLI for those two tools.

/// `data[].range.start_line` on the `Ambiguous`-status envelope must be
/// 1-indexed like every other line field at the JSON boundary. The cheapest
/// wrong implementation -- forgetting the shift entirely -- reports
/// `start_line - 1`, which this test catches.
///
/// Scope, stated precisely because it is narrower than it looks: in CLI
/// `--json` mode, `get_calls`/`find_callers`/`analyze_impact` ambiguity IS
/// `exit_ambiguous` (`src/cli/commands/mcp.rs` routes each of them into it),
/// which in turn calls `ambiguous_envelope`. So the loop below exercises ONE
/// mechanism reached by three tool names -- not the CLI path plus a separate
/// tool path. The in-process MCP-handler ambiguous path (what a real MCP
/// client hits, including `read_symbol`, the fifth `ambiguous_envelope`
/// caller) is a genuinely different entry point and is covered separately by
/// `read_symbol_ambiguous_candidate_range_is_1_indexed` below.
#[test]
fn ambiguous_candidate_range_is_1_indexed() {
    let workspace = TempDir::new().expect("temp dir");
    let src = workspace.path().join("src");
    std::fs::create_dir_all(&src).expect("create src dir");
    // dup_name is defined on line 2 (1-indexed) in both files.
    std::fs::write(
        src.join("alpha.rs"),
        "\npub fn dup_name() -> i32 {\n    1\n}\n",
    )
    .expect("write alpha fixture");
    std::fs::write(
        src.join("beta.rs"),
        "\npub fn dup_name() -> i32 {\n    2\n}\n",
    )
    .expect("write beta fixture");

    let codanna_dir = workspace.path().join(".codanna");
    std::fs::create_dir_all(&codanna_dir).expect("create .codanna");
    let src_abs = src.canonicalize().expect("resolvable src");
    let settings = format!(
        r#"
index_path = ".codanna/index"

[indexing]
indexed_paths = ["{}"]

[semantic_search]
enabled = false
"#,
        src_abs.to_str().expect("utf-8 path")
    );
    std::fs::write(codanna_dir.join("settings.toml"), settings).expect("write settings");

    let (code, stdout, stderr) = run_cli(
        workspace.path(),
        &["index", "src", "--force", "--no-progress"],
    );
    assert_eq!(
        code, 0,
        "index should succeed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    const DUP_NAME_DEF_LINE: i64 = 2;

    for tool in ["get_calls", "find_callers", "analyze_impact"] {
        let arg = if tool == "analyze_impact" {
            "symbol_name:dup_name"
        } else {
            "function_name:dup_name"
        };
        let (code, stdout, stderr) = run_cli(workspace.path(), &["mcp", tool, arg, "--json"]);
        assert_eq!(code, 3, "{tool} ambiguous exit\nstderr:\n{stderr}");
        let data = json_data(&stdout);
        let candidates = data.as_array().expect("ambiguous data is an array");
        assert_eq!(candidates.len(), 2, "{tool}: expected 2 candidates");
        for candidate in candidates {
            assert_eq!(
                candidate["range"]["start_line"].as_i64(),
                Some(DUP_NAME_DEF_LINE),
                "{tool}: ambiguous candidate range.start_line must be 1-indexed\ncandidate: {candidate}"
            );
        }
    }
}

/// The fifth `ambiguous_envelope` caller: `read_symbol`'s ambiguous branch,
/// reached through the in-process MCP handler rather than the CLI.
///
/// This is a distinct entry point from the test above, not a repeat of it.
/// `codanna mcp read_symbol --json` cannot exercise it -- the CLI has no
/// pre-collection block for `read_symbol` in JSON mode, so it never
/// dispatches the handler -- and it is exactly the path a real MCP client
/// takes. Without this, one of the five surfaces the systemic `Symbol::range`
/// projection was meant to fix would ship with no assertion at all.
#[tokio::test(flavor = "current_thread")]
async fn read_symbol_ambiguous_candidate_range_is_1_indexed() {
    use codanna::config::Settings;
    use codanna::indexing::facade::IndexFacade;
    use codanna::mcp::requests::ReadSymbolRequest;
    use codanna::mcp::{CodeIntelligenceServer, OutputFormat};
    use rmcp::handler::server::wrapper::Parameters;
    use rmcp::model::ContentBlock;
    use std::sync::Arc;

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

    let temp = TempDir::new().expect("temp dir");
    let src_dir = temp.path().join("src");
    std::fs::create_dir_all(&src_dir).expect("create src dir");
    // `dup_name` is defined on line 2 (1-indexed) in both files, so
    // `read_symbol` by name resolves to two candidates and takes the
    // ambiguous branch.
    std::fs::write(
        src_dir.join("alpha.rs"),
        "\npub fn dup_name() -> i32 {\n    1\n}\n",
    )
    .expect("write alpha fixture");
    std::fs::write(
        src_dir.join("beta.rs"),
        "\npub fn dup_name() -> i32 {\n    2\n}\n",
    )
    .expect("write beta fixture");

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
    let server = CodeIntelligenceServer::new(facade);

    const DUP_NAME_DEF_LINE: i64 = 2;

    let result = server
        .read_symbol(Parameters(ReadSymbolRequest {
            name: Some("dup_name".to_string()),
            symbol_id: None,
            output_format: OutputFormat::Json,
        }))
        .await
        .expect("read_symbol call succeeds");
    let envelope: Value =
        serde_json::from_str(&text_of(&result.content)).expect("read_symbol JSON envelope");

    assert_eq!(
        envelope["status"], "ambiguous",
        "two same-named symbols must take read_symbol's ambiguous branch\nenvelope: {envelope}"
    );
    let candidates = envelope["data"]
        .as_array()
        .expect("ambiguous data is an array");
    assert_eq!(candidates.len(), 2, "expected 2 candidates: {envelope}");
    for candidate in candidates {
        assert_eq!(
            candidate["range"]["start_line"].as_i64(),
            Some(DUP_NAME_DEF_LINE),
            "read_symbol ambiguous candidate range.start_line must be 1-indexed -- a caller \
             following the envelope's own `re-run with symbol_id` hint and slicing by this \
             range would otherwise land one line early\ncandidate: {candidate}"
        );
    }
}

/// Success payloads that embed a raw `Symbol` (`CallRelation` via
/// `find_callers`, `SymbolContext` via `find_symbol`) must carry the same
/// 1-indexed `range.start_line` as the ambiguous path above -- there is
/// exactly one `Symbol::range` serde impl, so a regression here or there
/// cannot happen independently, but both are asserted to pin the contract
/// at each embed site named in the work item.
#[test]
fn success_payload_range_is_1_indexed() {
    let workspace = TempDir::new().expect("temp dir");
    setup(workspace.path());

    let (code, stdout, stderr) = run_cli(
        workspace.path(),
        &["mcp", "find_callers", "function_name:line_target", "--json"],
    );
    assert_eq!(code, 0, "stderr:\n{stderr}");
    let data = json_data(&stdout);
    assert_eq!(
        data[0]["range"]["start_line"].as_i64(),
        Some(CALLER_DEF_LINE),
        "find_callers CallRelation range.start_line must be the caller's 1-indexed def line\ndata: {data}"
    );

    let (code, stdout, stderr) = run_cli(
        workspace.path(),
        &["mcp", "find_symbol", "name:line_target", "--json"],
    );
    assert_eq!(code, 0, "stderr:\n{stderr}");
    let data = json_data(&stdout);
    assert_eq!(
        data[0]["symbol"]["range"]["start_line"].as_i64(),
        Some(TARGET_DEF_LINE),
        "find_symbol SymbolContext.symbol.range.start_line must be 1-indexed\ndata: {data}"
    );
}

/// The same symbol's `range.start_line` (Symbol's serde field) must equal
/// its independently-computed scalar `start_line` fields on
/// `get_file_outline` and `read_symbol` -- three different code paths
/// converging on one number. A blanket, uncoordinated +1 landing on only
/// one of the three would fail this test even though each field looks
/// individually plausible.
#[tokio::test(flavor = "current_thread")]
async fn range_agrees_with_scalar_line_fields() {
    use codanna::config::Settings;
    use codanna::indexing::facade::IndexFacade;
    use codanna::mcp::requests::{FindSymbolRequest, GetFileOutlineRequest, ReadSymbolRequest};
    use codanna::mcp::{CodeIntelligenceServer, OutputFormat};
    use rmcp::handler::server::wrapper::Parameters;
    use rmcp::model::ContentBlock;
    use std::sync::Arc;

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

    let temp = TempDir::new().expect("temp dir");
    let src_dir = temp.path().join("src");
    std::fs::create_dir_all(&src_dir).expect("create src dir");
    let fixture_path = src_dir.join("agree.rs");
    // `agreed_fn` is defined on line 2 (1-indexed).
    std::fs::write(&fixture_path, "\npub fn agreed_fn() -> i32 {\n    1\n}\n")
        .expect("write fixture");

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
    let server = CodeIntelligenceServer::new(facade);

    const AGREED_FN_LINE: i64 = 2;

    let result = server
        .find_symbol(Parameters(FindSymbolRequest {
            name: "agreed_fn".to_string(),
            symbol_id: None,
            lang: None,
            output_format: OutputFormat::Json,
        }))
        .await
        .expect("find_symbol succeeds");
    let envelope: Value =
        serde_json::from_str(&text_of(&result.content)).expect("find_symbol JSON");
    assert_eq!(
        envelope["data"][0]["symbol"]["range"]["start_line"].as_i64(),
        Some(AGREED_FN_LINE),
        "find_symbol range.start_line\nenvelope: {envelope}"
    );

    let result = server
        .get_file_outline(Parameters(GetFileOutlineRequest {
            path: fixture_path.to_string_lossy().to_string(),
            max_results: 0,
            output_format: OutputFormat::Json,
        }))
        .await
        .expect("get_file_outline succeeds");
    let envelope: Value =
        serde_json::from_str(&text_of(&result.content)).expect("get_file_outline JSON");
    let entry = envelope["data"]
        .as_array()
        .expect("outline data array")
        .iter()
        .find(|e| e["name"] == "agreed_fn")
        .expect("agreed_fn entry present");
    assert_eq!(
        entry["start_line"].as_i64(),
        Some(AGREED_FN_LINE),
        "get_file_outline start_line\nentry: {entry}"
    );

    let result = server
        .read_symbol(Parameters(ReadSymbolRequest {
            name: Some("agreed_fn".to_string()),
            symbol_id: None,
            output_format: OutputFormat::Json,
        }))
        .await
        .expect("read_symbol succeeds");
    let envelope: Value =
        serde_json::from_str(&text_of(&result.content)).expect("read_symbol JSON");
    assert_eq!(
        envelope["data"]["start_line"].as_i64(),
        Some(AGREED_FN_LINE),
        "read_symbol start_line\nenvelope: {envelope}"
    );
}

/// Pins the lines-only decision (USER DECISION on this work item): columns
/// on the serialized `range` object stay 0-indexed and untouched by the
/// shift. Uses an indented symbol so a later blanket "+1 everything in
/// range" change fails this test even though it would still pass every
/// start_line/end_line assertion above.
#[tokio::test(flavor = "current_thread")]
async fn range_columns_are_unshifted() {
    use codanna::config::Settings;
    use codanna::indexing::facade::IndexFacade;
    use codanna::mcp::requests::FindSymbolRequest;
    use codanna::mcp::{CodeIntelligenceServer, OutputFormat};
    use rmcp::handler::server::wrapper::Parameters;
    use rmcp::model::ContentBlock;
    use std::sync::Arc;

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

    let temp = TempDir::new().expect("temp dir");
    let src_dir = temp.path().join("src");
    std::fs::create_dir_all(&src_dir).expect("create src dir");
    let fixture_path = src_dir.join("indented.py");
    // `helper` is indented 4 columns (0-indexed start_column == 4).
    std::fs::write(
        &fixture_path,
        "class Widget:\n    def helper(self):\n        pass\n",
    )
    .expect("write fixture");

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
    let server = CodeIntelligenceServer::new(facade);

    let result = server
        .find_symbol(Parameters(FindSymbolRequest {
            name: "helper".to_string(),
            symbol_id: None,
            lang: None,
            output_format: OutputFormat::Json,
        }))
        .await
        .expect("find_symbol succeeds");
    let envelope: Value =
        serde_json::from_str(&text_of(&result.content)).expect("find_symbol JSON");
    assert_eq!(
        envelope["data"][0]["symbol"]["range"]["start_column"].as_i64(),
        Some(4),
        "start_column must stay 0-indexed (lines-only convention)\nenvelope: {envelope}"
    );
}

#[test]
fn get_calls_text_names_def_and_call_site() {
    let workspace = TempDir::new().expect("temp dir");
    setup(workspace.path());

    let (code, stdout, _) = run_cli(
        workspace.path(),
        &["mcp", "get_calls", "function_name:line_caller"],
    );
    assert_eq!(code, 0, "stdout:\n{stdout}");
    let line = stdout
        .lines()
        .find(|l| l.contains("line_target"))
        .unwrap_or_else(|| panic!("no callee row\nstdout:\n{stdout}"));
    assert!(
        line.contains(&format!("fixture.rs:{TARGET_DEF_LINE}")),
        "callee location must be its def line\nrow: {line}"
    );
    assert!(
        line.contains("(called at ") && line.contains(&format!(":{CALLER_CALL_LINE})")),
        "call site must be named explicitly in the caller's file\nrow: {line}"
    );
}
