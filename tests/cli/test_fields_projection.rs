//! Behavioral lock for `--fields` projection across every JSON-emitting CLI
//! surface (`codanna mcp <tool> --json --fields ...` and `codanna retrieve
//! symbol --json --fields ...`).
//!
//! The fork previously had zero test coverage for `--fields`: this file
//! pins the observed envelope shapes (field names were confirmed by running
//! the built binary once against the fixture below — they are not guessed)
//! and asserts every success-path projection is SHAPE-DISCRIMINATING (a
//! requested key present in `data[0]` *and* a known sibling key absent from
//! the same object), never merely `exit == 0`.
//!
//! Fixture (mirrors `test_mcp_exit_code_matrix.rs`'s `write_fixture` /
//! `write_settings`, copied locally per `crate::support::run_cli` reuse
//! rules -- BASIC.4 only forbids re-copying `codanna_binary`/`run_cli`):
//! `unique_target` and `unique_caller` live in one file (`alpha.rs`, with an
//! unambiguous call edge between them); `dup_name` is defined in TWO files
//! (`alpha.rs` and `beta.rs`) so a `dup_name` lookup is genuinely ambiguous.
//! Semantic search is disabled so indexing stays fast and deterministic.

use serde_json::Value;
use std::path::Path;

use tempfile::TempDir;

use crate::support::run_cli;

fn write_fixture(workspace: &Path) {
    let src = workspace.join("src");
    std::fs::create_dir_all(&src).expect("create src dir");
    std::fs::write(
        src.join("alpha.rs"),
        r#"
pub fn unique_target() -> i32 {
    41
}

pub fn unique_caller() -> i32 {
    unique_target() + 1
}

pub fn dup_name() -> i32 {
    1
}
"#,
    )
    .expect("write alpha fixture");
    std::fs::write(
        src.join("beta.rs"),
        r#"
pub fn dup_name() -> i32 {
    2
}
"#,
    )
    .expect("write beta fixture");
}

fn write_settings(workspace: &Path) {
    let codanna_dir = workspace.join(".codanna");
    std::fs::create_dir_all(&codanna_dir).expect("create .codanna");

    let src_abs = workspace
        .join("src")
        .canonicalize()
        .expect("src dir should exist and be resolvable");
    let src_path = src_abs.to_str().expect("src path should be valid UTF-8");

    let settings = format!(
        r#"
index_path = ".codanna/index"

[indexing]
indexed_paths = ["{src_path}"]

[semantic_search]
enabled = false
"#
    );

    std::fs::write(codanna_dir.join("settings.toml"), settings).expect("write settings");
}

/// Parse stdout as JSON, panicking with the raw stdout on failure so a
/// non-JSON/panicking failure mode is loud rather than silently unwrapped.
fn parse_envelope(stdout: &str) -> Value {
    serde_json::from_str(stdout)
        .unwrap_or_else(|e| panic!("stdout is not a JSON envelope: {e}\nstdout:\n{stdout}"))
}

fn envelope_exit_code(payload: &Value) -> i64 {
    payload["exit_code"]
        .as_i64()
        .unwrap_or_else(|| panic!("envelope has no declared exit_code\npayload:\n{payload}"))
}

#[test]
fn fields_projection_behavioral_lock() {
    let workspace = TempDir::new().expect("temp dir");
    write_fixture(workspace.path());
    write_settings(workspace.path());

    let (index_code, index_stdout, index_stderr) = run_cli(
        workspace.path(),
        &["index", "src", "--force", "--no-progress"],
    );
    assert_eq!(
        index_code, 0,
        "index should succeed\nstdout:\n{index_stdout}\nstderr:\n{index_stderr}"
    );

    // -----------------------------------------------------------------
    // A1 -- find_symbol --fields symbol.name: exit 0, projected key
    // present, a known sibling absent, envelope scaffolding intact.
    //
    // Valid path pinned by running the binary once against this fixture:
    // `codanna mcp find_symbol name:unique_target --json` returns
    // `data[0]` shaped as `{ symbol: {...}, file_path: ..., relationships:
    // {...} }`; `--fields symbol.name` projects down to
    // `data[0].symbol.name` while dropping the `file_path` and
    // `relationships` siblings from `data[0]`.
    //
    // Kills: a projector that ignores the requested field list and always
    // emits the full envelope (present-key check alone would pass; the
    // sibling-absent check would not).
    // -----------------------------------------------------------------
    {
        let (code, stdout, stderr) = run_cli(
            workspace.path(),
            &[
                "mcp",
                "find_symbol",
                "name:unique_target",
                "--json",
                "--fields",
                "symbol.name",
            ],
        );
        assert_eq!(code, 0, "A1 exit\nstdout:\n{stdout}\nstderr:\n{stderr}");
        let payload = parse_envelope(&stdout);

        let entry = &payload["data"][0];
        assert_eq!(
            entry["symbol"]["name"], "unique_target",
            "A1 requested key symbol.name must be present\npayload:\n{payload}"
        );
        assert!(
            entry.get("file_path").is_none(),
            "A1 sibling key file_path must be projected away\npayload:\n{payload}"
        );
        assert!(
            entry.get("relationships").is_none(),
            "A1 sibling key relationships must be projected away\npayload:\n{payload}"
        );

        // Envelope-level scaffolding survives projection: only `data` is
        // narrowed, never status/code/exit_code/meta.
        assert_eq!(
            payload["status"], "success",
            "A1 envelope status\npayload:\n{payload}"
        );
        assert_eq!(
            payload["code"], "OK",
            "A1 envelope code\npayload:\n{payload}"
        );
        assert_eq!(
            payload["exit_code"], 0,
            "A1 envelope exit_code\npayload:\n{payload}"
        );
        assert!(
            payload.get("meta").is_some_and(|m| !m.is_null()),
            "A1 envelope meta must survive projection\npayload:\n{payload}"
        );
    }

    // -----------------------------------------------------------------
    // A2 -- find_symbol --fields bogus_field: process exit 2 AND (as a
    // separate assertion) the envelope's own declared exit_code == 2;
    // stdout parses as JSON; code == INVALID_QUERY; hint names the
    // available-fields listing; stderr carries no panic.
    //
    // Kills: a literal `std::process::exit(2)` inside the projection
    // helper that bypasses `emit_envelope_and_exit` -- such code would
    // still deliver process exit 2, but the two assertions below are
    // checked independently against two different reads (process exit
    // vs. the envelope's own field), so a helper that hardcodes the
    // process exit without also setting `exit_code` on the emitted
    // envelope is still caught.
    // -----------------------------------------------------------------
    {
        let (code, stdout, stderr) = run_cli(
            workspace.path(),
            &[
                "mcp",
                "find_symbol",
                "name:unique_target",
                "--json",
                "--fields",
                "bogus_field",
            ],
        );
        assert_eq!(
            code, 2,
            "A2 process exit\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
        let payload = parse_envelope(&stdout);
        assert_eq!(
            envelope_exit_code(&payload),
            2,
            "A2 envelope's own declared exit_code, checked separately from process exit\npayload:\n{payload}"
        );
        assert_eq!(
            payload["code"], "INVALID_QUERY",
            "A2 code\npayload:\n{payload}"
        );
        let hint = payload["hint"].as_str().unwrap_or("");
        assert!(
            hint.contains("Available top-level fields"),
            "A2 hint must name the available-fields listing\npayload:\n{payload}"
        );
        assert!(
            !stderr.contains("panicked"),
            "A2 stderr must carry no panic\nstderr:\n{stderr}"
        );
    }

    // -----------------------------------------------------------------
    // A3 -- find_symbols (fork-only tool) --fields bogus_field: exit 2,
    // INVALID_QUERY, no panic. `names` is passed as a real JSON array via
    // `--args` so the request reaches the fields validator rather than
    // failing earlier on argument shape.
    //
    // Kills: a leftover raw `to_json_with_fields(..).expect(..)` call site
    // at the find_symbols arm -- such a call site would panic (unwrap on
    // an `Err`) instead of producing a clean INVALID_QUERY envelope, which
    // the "no panicked in stderr" + parses-as-JSON checks below catch.
    // -----------------------------------------------------------------
    {
        let (code, stdout, stderr) = run_cli(
            workspace.path(),
            &[
                "mcp",
                "find_symbols",
                "--args",
                r#"{"names":["unique_target","unique_caller"]}"#,
                "--json",
                "--fields",
                "bogus_field",
            ],
        );
        assert_eq!(
            code, 2,
            "A3 process exit\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
        let payload = parse_envelope(&stdout);
        assert_eq!(
            payload["code"], "INVALID_QUERY",
            "A3 code\npayload:\n{payload}"
        );
        assert!(
            !stderr.contains("panicked"),
            "A3 stderr must carry no panic\nstderr:\n{stderr}"
        );
    }

    // -----------------------------------------------------------------
    // A4 -- reindex (fork-only tool) --fields bogus_field: exit 2,
    // INVALID_QUERY, no panic.
    //
    // Kills: a leftover raw `to_json_with_fields(..).expect(..)` call site
    // at the reindex arm, for the same reason as A3.
    // -----------------------------------------------------------------
    {
        let (code, stdout, stderr) = run_cli(
            workspace.path(),
            &["mcp", "reindex", "--json", "--fields", "bogus_field"],
        );
        assert_eq!(
            code, 2,
            "A4 process exit\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
        let payload = parse_envelope(&stdout);
        assert_eq!(
            payload["code"], "INVALID_QUERY",
            "A4 code\npayload:\n{payload}"
        );
        assert!(
            !stderr.contains("panicked"),
            "A4 stderr must carry no panic\nstderr:\n{stderr}"
        );
    }

    // -----------------------------------------------------------------
    // A5/A6/A7 -- the find_callers and analyze_impact clusters.
    //
    // DESIGN DEVIATION, encoded from observed binary behavior. The design
    // assumed `count_only:true` reaches the `--fields` path. It does not:
    // the shared parameter whitelist (`crate::mcp::service::tool_param_spec`,
    // consulted by this surface's single argument-validation block) accepts
    // only `function_name, symbol_id` for `find_callers`, so `count_only` is
    // rejected as an unknown PARAMETER long before any `--fields` code runs.
    // Asserting on that rejection would be vacuous with respect to this
    // cutover: it passes identically whether or not the call sites were ever
    // converted. (The unreachable-from-CLI `count_only` dispatch arm is a
    // real pre-existing gap, but it is orthogonal to `--fields` and out of
    // scope here.)
    //
    // These assertions therefore drive the LISTING arms, which are reachable
    // and do route through `render_envelope_json`.
    // -----------------------------------------------------------------
    {
        // A5: projection applies on the find_callers listing arm.
        // Kills a shadow render (projection computed, unprojected JSON
        // printed) and an over-broad rewrite that filters envelope keys
        // rather than only `data`.
        let (code, stdout, stderr) = run_cli(
            workspace.path(),
            &[
                "mcp",
                "find_callers",
                "function_name:unique_target",
                "--json",
                "--fields",
                "name",
            ],
        );
        assert_eq!(
            code, 0,
            "A5 process exit\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
        let payload = parse_envelope(&stdout);
        assert_eq!(
            payload["data"][0]["name"], "unique_caller",
            "A5 requested field must be present\npayload:\n{payload}"
        );
        assert!(
            payload["data"][0].get("file_path").is_none(),
            "A5 a known sibling key must be ABSENT -- otherwise the projection \
             did not apply and the unprojected payload was printed\npayload:\n{payload}"
        );
        for key in ["status", "code", "exit_code", "meta"] {
            assert!(
                payload.get(key).is_some(),
                "A5 envelope-level key '{key}' must survive projection -- only \
                 `data` is filtered\npayload:\n{payload}"
            );
        }
    }

    {
        // A6: the available-field list is derived from the LIVE payload, so
        // it differs between tools. `role` is a caller-record field: valid
        // on find_callers, absent from analyze_impact's symbol records.
        //
        // Kills a hardcoded or hoisted available-field list -- any such list
        // would answer identically for both tools and one of these two
        // assertions would fail.
        let (code, stdout, stderr) = run_cli(
            workspace.path(),
            &[
                "mcp",
                "find_callers",
                "function_name:unique_target",
                "--json",
                "--fields",
                "role",
            ],
        );
        assert_eq!(
            code, 0,
            "A6 'role' must be a valid field on find_callers\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );

        // The find_callers listing arm's REJECTION path. Without this, a
        // revert of that one call site to the raw
        // `to_json_with_fields(..).expect(..)` pattern would go unnoticed:
        // every other find_callers assertion here passes a VALID field name,
        // and the raw pattern renders those identically. Only an unknown
        // field separates the two -- the raw form panics, this envelope is
        // what the converted form produces.
        let (code, stdout, stderr) = run_cli(
            workspace.path(),
            &[
                "mcp",
                "find_callers",
                "function_name:unique_target",
                "--json",
                "--fields",
                "bogus_field",
            ],
        );
        assert_eq!(
            code, 2,
            "A6 find_callers listing arm must reject an unknown field\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
        assert!(
            !stderr.contains("panicked"),
            "A6 find_callers listing arm must emit an envelope, not panic -- a panic \
             here means this call site still carries the raw \
             to_json_with_fields(..).expect(..) pattern\nstderr:\n{stderr}"
        );
        let payload = parse_envelope(&stdout);
        assert_eq!(
            payload["code"], "INVALID_QUERY",
            "A6 find_callers rejection code\npayload:\n{payload}"
        );

        let (code, stdout, stderr) = run_cli(
            workspace.path(),
            &[
                "mcp",
                "analyze_impact",
                "symbol_name:unique_target",
                "--json",
                "--fields",
                "role",
            ],
        );
        assert_eq!(
            code, 2,
            "A6 the SAME field name must be rejected on analyze_impact, proving the \
             available-field list is payload-derived rather than hardcoded\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
        let payload = parse_envelope(&stdout);
        assert_eq!(
            payload["code"], "INVALID_QUERY",
            "A6 rejection code\npayload:\n{payload}"
        );
        let hint = payload["hint"].as_str().unwrap_or("");
        assert!(
            hint.contains("Available top-level fields"),
            "A6 hint must come from the --fields validator, not the parameter \
             whitelist\npayload:\n{payload}"
        );
        assert!(
            !hint.contains("role"),
            "A6 hint must not offer the very field it just rejected\npayload:\n{payload}"
        );
    }

    {
        // A7: analyze_impact listing arm rejects an unknown field cleanly.
        // Kills a leftover raw `to_json_with_fields(..).expect(..)` at this
        // call site, which would panic instead of emitting this envelope.
        let (code, stdout, stderr) = run_cli(
            workspace.path(),
            &[
                "mcp",
                "analyze_impact",
                "symbol_name:unique_target",
                "--json",
                "--fields",
                "bogus_field",
            ],
        );
        assert_eq!(
            code, 2,
            "A7 process exit\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
        let payload = parse_envelope(&stdout);
        assert_eq!(
            payload["code"], "INVALID_QUERY",
            "A7 code\npayload:\n{payload}"
        );
        assert_eq!(
            payload["exit_code"], 2,
            "A7 declared envelope exit_code must equal the process exit\npayload:\n{payload}"
        );
        assert!(
            !stderr.contains("panicked"),
            "A7 stderr must carry no panic\nstderr:\n{stderr}"
        );
    }

    // -----------------------------------------------------------------
    // A8 -- get_calls function_name:dup_name --fields bogus_field: EXIT 3,
    // code AMBIGUOUS. `dup_name` is defined in both fixture files, so
    // resolution is genuinely ambiguous.
    //
    // Kills: anyone routing the ambiguous-name path through the new
    // `--fields` validator (`render_envelope_json`) instead of the
    // dedicated `exit_ambiguous` -- ambiguity resolution happens before
    // any `--fields` projection is attempted, so exit 3/AMBIGUOUS must
    // hold regardless of the (here, deliberately invalid) `--fields`
    // value.
    // -----------------------------------------------------------------
    {
        let (code, stdout, stderr) = run_cli(
            workspace.path(),
            &[
                "mcp",
                "get_calls",
                "function_name:dup_name",
                "--json",
                "--fields",
                "bogus_field",
            ],
        );
        assert_eq!(
            code, 3,
            "A8 process exit\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
        let payload = parse_envelope(&stdout);
        assert_eq!(payload["code"], "AMBIGUOUS", "A8 code\npayload:\n{payload}");
        assert_eq!(
            envelope_exit_code(&payload),
            3,
            "A8 envelope's own declared exit_code\npayload:\n{payload}"
        );
    }

    // -----------------------------------------------------------------
    // A9 -- retrieve symbol unique_target --fields bogus_field: exit 2.
    //
    // Kills: a `codanna retrieve` path that silently ignores an unknown
    // `--fields` value instead of rejecting it (that path shares
    // `Envelope::to_json_with_fields` with the `mcp` surface, so a
    // regression here would indicate the two surfaces drifted apart).
    // -----------------------------------------------------------------
    {
        let (code, stdout, stderr) = run_cli(
            workspace.path(),
            &[
                "retrieve",
                "symbol",
                "unique_target",
                "--json",
                "--fields",
                "bogus_field",
            ],
        );
        assert_eq!(code, 2, "A9 exit\nstdout:\n{stdout}\nstderr:\n{stderr}");
        let payload = parse_envelope(&stdout);
        assert_eq!(
            payload["code"], "INVALID_QUERY",
            "A9 code\npayload:\n{payload}"
        );
    }

    // -----------------------------------------------------------------
    // A10 -- retrieve symbol unique_target --fields symbol.name: exit 0,
    // projection applied, present/absent pair exactly as A1.
    //
    // Kills: a `codanna retrieve` path that renders the full envelope
    // regardless of `--fields` (the sibling-absent half of this check
    // fails if projection is a no-op).
    // -----------------------------------------------------------------
    {
        let (code, stdout, stderr) = run_cli(
            workspace.path(),
            &[
                "retrieve",
                "symbol",
                "unique_target",
                "--json",
                "--fields",
                "symbol.name",
            ],
        );
        assert_eq!(code, 0, "A10 exit\nstdout:\n{stdout}\nstderr:\n{stderr}");
        let payload = parse_envelope(&stdout);

        let entry = &payload["data"][0];
        assert_eq!(
            entry["symbol"]["name"], "unique_target",
            "A10 requested key symbol.name must be present\npayload:\n{payload}"
        );
        assert!(
            entry.get("file_path").is_none(),
            "A10 sibling key file_path must be projected away\npayload:\n{payload}"
        );
        assert!(
            entry.get("relationships").is_none(),
            "A10 sibling key relationships must be projected away\npayload:\n{payload}"
        );
    }
}
