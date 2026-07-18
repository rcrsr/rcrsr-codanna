//! Real-process end-to-end coverage for `codanna index --dry-run`'s
//! `--list-all` and `--json` output modes (GitHub issue #29: dry-run
//! silently truncated its file listing at 5 paths with no way to see the
//! rest).
//!
//! This file drives the real `codanna` binary as a subprocess, mirroring the
//! pattern established in `tests/cli/test_serve_proxy_discovery.rs`.

#[path = "cli/support.rs"]
mod support;

use tempfile::TempDir;

use support::run_cli;

/// Number of Python fixture files written by `prepare_workspace` -- kept
/// well above the default dry-run truncation threshold (5) so every mode
/// under test (default summary, `--list-all`, `--json`) has more paths than
/// the summary would show.
const FIXTURE_FILE_COUNT: usize = 8;

/// Build a workspace with `FIXTURE_FILE_COUNT` Python fixture files and a
/// minimal `.codanna/settings.toml`, ready for `codanna index --dry-run`.
fn prepare_workspace() -> TempDir {
    let workspace = TempDir::new().expect("create temp workspace");

    let src_dir = workspace.path().join("src");
    std::fs::create_dir_all(&src_dir).expect("create src dir");
    for i in 0..FIXTURE_FILE_COUNT {
        std::fs::write(
            src_dir.join(format!("module_{i}.py")),
            format!("def fn_{i}():\n    pass\n"),
        )
        .unwrap_or_else(|e| panic!("write module_{i}.py fixture: {e}"));
    }

    let codanna_dir = workspace.path().join(".codanna");
    std::fs::create_dir_all(&codanna_dir).expect("create .codanna dir");
    std::fs::write(
        codanna_dir.join("settings.toml"),
        r#"
index_path = ".codanna/index"

[semantic_search]
enabled = false
"#,
    )
    .expect("write settings.toml");

    workspace
}

#[test]
fn dry_run_json_output_parses_as_path_array() {
    let workspace = prepare_workspace();

    let (code, stdout, stderr) = run_cli(
        workspace.path(),
        &["index", "src", "--dry-run", "--json", "--no-progress"],
    );
    assert_eq!(
        code, 0,
        "dry-run --json should succeed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    let paths: Vec<String> = serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
        panic!("--json stdout must parse as a JSON array of strings: {e}\nstdout:\n{stdout}")
    });

    assert_eq!(
        paths.len(),
        FIXTURE_FILE_COUNT,
        "--json must list every discovered file, not a truncated subset"
    );
    assert!(
        paths.len() > 5,
        "fixture must exceed the default truncation threshold for this test to be meaningful"
    );
}

#[test]
fn dry_run_json_and_list_all_agree_on_path_count() {
    let workspace = prepare_workspace();

    let (json_code, json_stdout, json_stderr) = run_cli(
        workspace.path(),
        &["index", "src", "--dry-run", "--json", "--no-progress"],
    );
    assert_eq!(
        json_code, 0,
        "dry-run --json should succeed\nstdout:\n{json_stdout}\nstderr:\n{json_stderr}"
    );
    let json_paths: Vec<String> = serde_json::from_str(json_stdout.trim())
        .unwrap_or_else(|e| panic!("--json stdout must parse as JSON: {e}"));

    let (list_all_code, list_all_stdout, list_all_stderr) = run_cli(
        workspace.path(),
        &["index", "src", "--dry-run", "--list-all", "--no-progress"],
    );
    assert_eq!(
        list_all_code, 0,
        "dry-run --list-all should succeed\nstdout:\n{list_all_stdout}\nstderr:\n{list_all_stderr}"
    );

    // --list-all prints a "Would index N files:" header line (and, on a
    // second `add_paths_to_settings` call against the same workspace, an
    // "Already indexed" notice) followed by one two-space-indented path per
    // line -- keep only the indented path lines.
    let list_all_paths: Vec<&str> = list_all_stdout
        .lines()
        .filter(|line| line.starts_with("  "))
        .collect();

    assert_eq!(
        json_paths.len(),
        list_all_paths.len(),
        "--json and --list-all must report the same number of paths\n--json stdout:\n{json_stdout}\n--list-all stdout:\n{list_all_stdout}"
    );
    assert!(
        json_paths.len() > 5,
        "--json must exceed the default truncation threshold on this fixture"
    );
    assert!(
        list_all_paths.len() > 5,
        "--list-all must exceed the default truncation threshold on this fixture"
    );
}

#[test]
fn dry_run_json_stdout_stays_clean_when_path_already_indexed() {
    let workspace = prepare_workspace();

    // First invocation adds `src` to settings.toml (clean "Added" path).
    let (first_code, first_stdout, first_stderr) = run_cli(
        workspace.path(),
        &["index", "src", "--dry-run", "--json", "--no-progress"],
    );
    assert_eq!(
        first_code, 0,
        "first dry-run --json should succeed\nstdout:\n{first_stdout}\nstderr:\n{first_stderr}"
    );
    serde_json::from_str::<Vec<String>>(first_stdout.trim()).unwrap_or_else(|e| {
        panic!("first --json stdout must parse as JSON: {e}\nstdout:\n{first_stdout}")
    });

    // Second invocation against the same workspace/path hits the
    // `SkipReason::CoveredBy`/`AlreadyPresent` notice path in
    // `add_paths_to_settings`. Those notices must go to stderr, not stdout,
    // so --json stdout still parses as a clean JSON array with no
    // contamination.
    let (second_code, second_stdout, second_stderr) = run_cli(
        workspace.path(),
        &["index", "src", "--dry-run", "--json", "--no-progress"],
    );
    assert_eq!(
        second_code, 0,
        "second dry-run --json should succeed\nstdout:\n{second_stdout}\nstderr:\n{second_stderr}"
    );

    let second_paths: Vec<String> =
        serde_json::from_str(second_stdout.trim()).unwrap_or_else(|e| {
            panic!(
                "second --json stdout must parse as clean JSON with zero non-JSON contamination \
                 (e.g. an 'Already indexed' notice): {e}\nstdout:\n{second_stdout}"
            )
        });

    assert_eq!(
        second_paths.len(),
        FIXTURE_FILE_COUNT,
        "second --json invocation must still list every discovered file"
    );
}

/// Guard against a 4th `ignore::WalkBuilder` site regrowing silently.
///
/// `walk_config::build_walker` is the ONLY place in the crate permitted to
/// call `WalkBuilder::new` (see its doc comment). There is no compiler
/// mechanism to enforce this -- reading the tracked `src/` tree and counting
/// occurrences is the only backstop.
#[test]
fn walk_builder_new_has_exactly_one_call_site() {
    let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let src_dir = repo_root.join("src");

    let mut call_sites: Vec<(std::path::PathBuf, usize)> = Vec::new();
    let mut stack = vec![src_dir];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap_or_else(|e| {
            panic!("read_dir({}): {e}", dir.display());
        }) {
            let entry = entry.expect("dir entry");
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let content = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
            for (lineno, line) in content.lines().enumerate() {
                // Match the actual call `WalkBuilder::new(...)`, not prose
                // mentions of the symbol in doc comments (e.g. "...permitted
                // to call `WalkBuilder::new`.").
                if line.contains("WalkBuilder::new(") {
                    call_sites.push((path.clone(), lineno + 1));
                }
            }
        }
    }

    assert_eq!(
        call_sites.len(),
        1,
        "expected exactly one `WalkBuilder::new` call site (in walk_config.rs), found: {call_sites:?}"
    );
    assert!(
        call_sites[0].0.ends_with("indexing/walk_config.rs"),
        "the sole `WalkBuilder::new` call site must live in src/indexing/walk_config.rs, found: {:?}",
        call_sites[0].0
    );
}

// ---------------------------------------------------------------------------
// GitHub issue #22: `ignore_patterns` was deserialized but never consulted by
// any walk. The tests below drive the real `codanna` binary end-to-end and
// assert on walk outcome / index contents (never on the config value itself
// -- that was exactly the gap that let #22 survive undetected for months,
// see `test_load_from_toml` in `src/config/mod.rs`).
// ---------------------------------------------------------------------------

/// Write a minimal workspace with a `settings.toml` carrying the given
/// `[indexing]` body verbatim (e.g. `ignore_patterns = ["skipme/**"]`).
/// Semantic search is disabled so tests never touch the embedding model.
fn init_ignore_workspace(indexing_toml_body: &str) -> TempDir {
    let workspace = TempDir::new().expect("create temp workspace");

    let codanna_dir = workspace.path().join(".codanna");
    std::fs::create_dir_all(&codanna_dir).expect("create .codanna dir");
    std::fs::write(
        codanna_dir.join("settings.toml"),
        format!(
            r#"
index_path = ".codanna/index"

[semantic_search]
enabled = false

[indexing]
{indexing_toml_body}
"#
        ),
    )
    .expect("write settings.toml");

    workspace
}

/// Look up a symbol by exact name via `codanna retrieve symbol --json` and
/// return `true` if it is present in the index.
fn symbol_is_indexed(workspace: &std::path::Path, name: &str) -> (bool, String, String) {
    let (code, stdout, stderr) = run_cli(workspace, &["retrieve", "symbol", name, "--json"]);
    // ExitCode::NotFound == 3 (src/io/exit_code.rs); ExitCode::Success == 0.
    (code == 0, stdout, stderr)
}

/// `codanna index <path1> <path2> ... --dry-run --json` prints one JSON
/// array per positional path argument (concatenated on stdout, not merged
/// into a single array). Parse and flatten all of them into one `Vec`.
fn parse_concatenated_json_arrays(stdout: &str) -> Vec<String> {
    let mut all = Vec::new();
    for value in serde_json::Deserializer::from_str(stdout).into_iter::<Vec<String>>() {
        let value = value.unwrap_or_else(|e| {
            panic!("each dry-run --json segment must parse as a JSON array of strings: {e}\nstdout:\n{stdout}")
        });
        all.extend(value);
    }
    all
}

#[test]
fn v1_ignore_patterns_excludes_matched_files_from_the_written_index() {
    let workspace = init_ignore_workspace(r#"ignore_patterns = ["skipme/**"]"#);

    let skip_dir = workspace.path().join("skipme");
    std::fs::create_dir_all(&skip_dir).expect("create skipme dir");
    std::fs::write(
        skip_dir.join("x.rs"),
        "pub fn v1_marker_should_be_excluded() {}\n",
    )
    .expect("write skipme/x.rs");

    let keep_dir = workspace.path().join("normal");
    std::fs::create_dir_all(&keep_dir).expect("create normal dir");
    std::fs::write(
        keep_dir.join("keep.rs"),
        "pub fn v1_marker_should_be_kept() {}\n",
    )
    .expect("write normal/keep.rs");

    let (code, stdout, stderr) = run_cli(
        workspace.path(),
        &["index", "normal", "skipme", "--no-progress"],
    );
    assert_eq!(
        code, 0,
        "index should succeed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    let (skip_found, skip_stdout, skip_stderr) =
        symbol_is_indexed(workspace.path(), "v1_marker_should_be_excluded");
    assert!(
        !skip_found,
        "symbol from a file under an ignore_patterns match must not be indexed\nstdout:\n{skip_stdout}\nstderr:\n{skip_stderr}"
    );

    let (keep_found, keep_stdout, keep_stderr) =
        symbol_is_indexed(workspace.path(), "v1_marker_should_be_kept");
    assert!(
        keep_found,
        "symbol from a file not matched by ignore_patterns must be indexed\nstdout:\n{keep_stdout}\nstderr:\n{keep_stderr}"
    );
}

#[test]
fn v2_dry_run_and_real_index_agree_on_indexed_file_count_full_and_incremental() {
    let workspace = init_ignore_workspace(r#"ignore_patterns = ["skipme/**"]"#);

    let skip_dir = workspace.path().join("skipme");
    let keep_dir = workspace.path().join("normal");
    std::fs::create_dir_all(&skip_dir).expect("create skipme dir");
    std::fs::create_dir_all(&keep_dir).expect("create normal dir");
    std::fs::write(skip_dir.join("a.rs"), "pub fn v2_marker_skip_a() {}\n")
        .expect("write skipme/a.rs");
    std::fs::write(keep_dir.join("a.rs"), "pub fn v2_marker_keep_a() {}\n")
        .expect("write normal/a.rs");
    std::fs::write(keep_dir.join("b.rs"), "pub fn v2_marker_keep_b() {}\n")
        .expect("write normal/b.rs");

    // --- Full index: dry-run path count must match the real index's
    // symbol count for the same fixture (one uniquely named symbol per
    // kept file, none for ignored files).
    let (dry_code, dry_stdout, dry_stderr) = run_cli(
        workspace.path(),
        &[
            "index",
            "normal",
            "skipme",
            "--dry-run",
            "--json",
            "--no-progress",
        ],
    );
    assert_eq!(
        dry_code, 0,
        "dry-run --json should succeed\nstdout:\n{dry_stdout}\nstderr:\n{dry_stderr}"
    );
    let dry_paths = parse_concatenated_json_arrays(&dry_stdout);
    assert_eq!(
        dry_paths.len(),
        2,
        "dry-run must list exactly the two non-ignored files\ndry_paths: {dry_paths:?}"
    );

    let (index_code, index_stdout, index_stderr) = run_cli(
        workspace.path(),
        &["index", "normal", "skipme", "--no-progress"],
    );
    assert_eq!(
        index_code, 0,
        "real index should succeed\nstdout:\n{index_stdout}\nstderr:\n{index_stderr}"
    );

    let (search_code, search_stdout, search_stderr) = run_cli(
        workspace.path(),
        &[
            "retrieve",
            "search",
            "v2_marker_keep_",
            "--limit",
            "10",
            "--json",
        ],
    );
    assert_eq!(
        search_code, 0,
        "search for kept-file markers should succeed\nstdout:\n{search_stdout}\nstderr:\n{search_stderr}"
    );
    let search_json: serde_json::Value = serde_json::from_str(search_stdout.trim())
        .unwrap_or_else(|e| panic!("search --json stdout must parse as JSON: {e}"));
    let real_count = search_json["meta"]["count"]
        .as_u64()
        .expect("meta.count must be present after a successful search");
    assert_eq!(
        real_count as usize,
        dry_paths.len(),
        "the real index's symbol count for kept files must agree with the dry-run file count\nsearch json: {search_json}"
    );

    let (skip_found, skip_stdout, skip_stderr) =
        symbol_is_indexed(workspace.path(), "v2_marker_skip_a");
    assert!(
        !skip_found,
        "ignored file's symbol must never reach the real index\nstdout:\n{skip_stdout}\nstderr:\n{skip_stderr}"
    );

    // --- Incremental index: add one more kept and one more ignored file,
    // then re-run dry-run and the real (non-force) incremental index, and
    // check they still agree.
    std::fs::write(keep_dir.join("c.rs"), "pub fn v2_marker_keep_c() {}\n")
        .expect("write normal/c.rs");
    std::fs::write(skip_dir.join("d.rs"), "pub fn v2_marker_skip_d() {}\n")
        .expect("write skipme/d.rs");

    let (dry2_code, dry2_stdout, dry2_stderr) = run_cli(
        workspace.path(),
        &[
            "index",
            "normal",
            "skipme",
            "--dry-run",
            "--json",
            "--no-progress",
        ],
    );
    assert_eq!(
        dry2_code, 0,
        "second dry-run --json should succeed\nstdout:\n{dry2_stdout}\nstderr:\n{dry2_stderr}"
    );
    let dry2_paths = parse_concatenated_json_arrays(&dry2_stdout);
    assert_eq!(
        dry2_paths.len(),
        3,
        "dry-run must list all three non-ignored files after the incremental addition\ndry2_paths: {dry2_paths:?}"
    );

    let (inc_code, inc_stdout, inc_stderr) = run_cli(
        workspace.path(),
        &["index", "normal", "skipme", "--no-progress"],
    );
    assert_eq!(
        inc_code, 0,
        "incremental index should succeed\nstdout:\n{inc_stdout}\nstderr:\n{inc_stderr}"
    );

    let (search2_code, search2_stdout, search2_stderr) = run_cli(
        workspace.path(),
        &[
            "retrieve",
            "search",
            "v2_marker_keep_",
            "--limit",
            "10",
            "--json",
        ],
    );
    assert_eq!(
        search2_code, 0,
        "search after incremental index should succeed\nstdout:\n{search2_stdout}\nstderr:\n{search2_stderr}"
    );
    let search2_json: serde_json::Value = serde_json::from_str(search2_stdout.trim())
        .unwrap_or_else(|e| panic!("search --json stdout must parse as JSON: {e}"));
    let real2_count = search2_json["meta"]["count"]
        .as_u64()
        .expect("meta.count must be present after a successful search");
    assert_eq!(
        real2_count as usize,
        dry2_paths.len(),
        "after an incremental reindex, the real index's kept-file symbol count must still agree with dry-run\nsearch json: {search2_json}"
    );

    let (skip2_found, skip2_stdout, skip2_stderr) =
        symbol_is_indexed(workspace.path(), "v2_marker_skip_d");
    assert!(
        !skip2_found,
        "newly added ignored file's symbol must never reach the real index after incremental reindex\nstdout:\n{skip2_stdout}\nstderr:\n{skip2_stderr}"
    );
}

#[test]
fn v3_ignore_patterns_bare_directory_form_matches_codannaignore_identically() {
    // Workspace A: exclude via `ignore_patterns` using the bare directory
    // form (no glob suffix), matching a `.gitignore`/`.codannaignore` style
    // directory pattern.
    let workspace_a = init_ignore_workspace(r#"ignore_patterns = ["skipme/"]"#);
    // `skipme/` is nested under a `proj` parent directory (rather than being
    // the walk root itself) because the ignore crate never applies ignore
    // rules to the exact root path a walk starts from -- only to its
    // descendants -- so this fixture must exercise a real descendant match.
    let skip_dir_a = workspace_a.path().join("proj").join("skipme");
    std::fs::create_dir_all(&skip_dir_a).expect("create proj/skipme dir (a)");
    std::fs::write(
        skip_dir_a.join("x.rs"),
        "pub fn v3_marker_dir_excluded() {}\n",
    )
    .expect("write proj/skipme/x.rs (a)");

    let (code_a, stdout_a, stderr_a) =
        run_cli(workspace_a.path(), &["index", "proj", "--no-progress"]);
    assert_eq!(
        code_a, 0,
        "index (ignore_patterns bare dir form) should succeed\nstdout:\n{stdout_a}\nstderr:\n{stderr_a}"
    );
    let (found_a, stdout_a2, stderr_a2) =
        symbol_is_indexed(workspace_a.path(), "v3_marker_dir_excluded");

    // Workspace B: identical fixture, excluded via `.codannaignore` using
    // the same bare directory pattern instead of `ignore_patterns`.
    let workspace_b = init_ignore_workspace("");
    std::fs::write(workspace_b.path().join(".codannaignore"), "skipme/\n")
        .expect("write .codannaignore (b)");
    let skip_dir_b = workspace_b.path().join("proj").join("skipme");
    std::fs::create_dir_all(&skip_dir_b).expect("create proj/skipme dir (b)");
    std::fs::write(
        skip_dir_b.join("x.rs"),
        "pub fn v3_marker_dir_excluded() {}\n",
    )
    .expect("write proj/skipme/x.rs (b)");

    let (code_b, stdout_b, stderr_b) =
        run_cli(workspace_b.path(), &["index", "proj", "--no-progress"]);
    assert_eq!(
        code_b, 0,
        "index (.codannaignore bare dir form) should succeed\nstdout:\n{stdout_b}\nstderr:\n{stderr_b}"
    );
    let (found_b, stdout_b2, stderr_b2) =
        symbol_is_indexed(workspace_b.path(), "v3_marker_dir_excluded");

    assert!(
        !found_a,
        "ignore_patterns bare directory form must exclude the directory\nstdout:\n{stdout_a2}\nstderr:\n{stderr_a2}"
    );
    assert!(
        !found_b,
        ".codannaignore bare directory form must exclude the directory\nstdout:\n{stdout_b2}\nstderr:\n{stderr_b2}"
    );
    assert_eq!(
        found_a, found_b,
        "ignore_patterns and .codannaignore must produce identical exclusion behavior for the same bare-directory pattern"
    );
}

#[test]
fn v4_leading_bang_re_includes_within_ignore_patterns() {
    let workspace = init_ignore_workspace(r#"ignore_patterns = ["gen/*", "!gen/keep.rs"]"#);

    let gen_dir = workspace.path().join("gen");
    std::fs::create_dir_all(&gen_dir).expect("create gen dir");
    std::fs::write(gen_dir.join("keep.rs"), "pub fn v4_marker_keep() {}\n")
        .expect("write gen/keep.rs");
    std::fs::write(gen_dir.join("other.rs"), "pub fn v4_marker_other() {}\n")
        .expect("write gen/other.rs");

    let (code, stdout, stderr) = run_cli(workspace.path(), &["index", "gen", "--no-progress"]);
    assert_eq!(
        code, 0,
        "index should succeed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    let (keep_found, keep_stdout, keep_stderr) =
        symbol_is_indexed(workspace.path(), "v4_marker_keep");
    assert!(
        keep_found,
        "gen/keep.rs must be re-included by the trailing '!gen/keep.rs' pattern\nstdout:\n{keep_stdout}\nstderr:\n{keep_stderr}"
    );

    let (other_found, other_stdout, other_stderr) =
        symbol_is_indexed(workspace.path(), "v4_marker_other");
    assert!(
        !other_found,
        "gen/other.rs must remain excluded by 'gen/*'\nstdout:\n{other_stdout}\nstderr:\n{other_stderr}"
    );
}

#[test]
fn v5_malformed_ignore_pattern_fails_the_run_naming_the_pattern() {
    let workspace = init_ignore_workspace(r#"ignore_patterns = ["[z-a]"]"#);

    std::fs::write(
        workspace.path().join("main.rs"),
        "pub fn v5_marker_unreachable() {}\n",
    )
    .expect("write main.rs");

    let (code, stdout, stderr) = run_cli(workspace.path(), &["index", ".", "--no-progress"]);
    assert_ne!(
        code, 0,
        "a malformed ignore_patterns entry must fail the run\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("[z-a]"),
        "the error must name the offending pattern\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("ignore_patterns"),
        "the error must name the settings key\nstderr:\n{stderr}"
    );
}

// ---------------------------------------------------------------------------
// GitHub issue #23: symlinked directories were silently skipped during
// indexing, with no way to opt into following them and no signal that
// anything had been excluded. These tests exercise `[indexing]
// follow_links` (default false, preserving prior behavior exactly) and the
// warning that must now fire whenever a symlinked directory is skipped.
// ---------------------------------------------------------------------------

#[cfg(unix)]
mod follow_links_tests {
    use super::{init_ignore_workspace, run_cli, support, symbol_is_indexed};
    use std::os::unix::fs::symlink;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};
    use tempfile::TempDir;

    /// Build a workspace with a real target directory *outside* the indexed
    /// `src/` root, and a `src/link_dir` symlink pointing at it. The marker
    /// symbol is reachable only through the symlink, so it lands in the
    /// index if and only if the symlinked directory was followed.
    fn prepare_symlink_workspace(follow_links: bool) -> TempDir {
        let workspace = init_ignore_workspace(&format!("follow_links = {follow_links}"));

        let target_dir = workspace.path().join("outside").join("target_dir");
        std::fs::create_dir_all(&target_dir).expect("create outside/target_dir");
        std::fs::write(
            target_dir.join("marker.py"),
            "def w5_marker_via_symlink():\n    pass\n",
        )
        .expect("write marker.py");

        let src_dir = workspace.path().join("src");
        std::fs::create_dir_all(&src_dir).expect("create src dir");
        symlink(&target_dir, src_dir.join("link_dir")).expect("create symlink");

        workspace
    }

    #[test]
    fn symlinked_directory_skipped_by_default_and_warns() {
        let workspace = prepare_symlink_workspace(false);

        let (code, stdout, stderr) = run_cli(workspace.path(), &["index", "src", "--no-progress"]);
        assert_eq!(
            code, 0,
            "index should succeed\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );

        let (found, sym_stdout, sym_stderr) =
            symbol_is_indexed(workspace.path(), "w5_marker_via_symlink");
        assert!(
            !found,
            "a symbol reachable only via a symlinked directory must not be indexed \
             when follow_links is disabled\nstdout:\n{sym_stdout}\nstderr:\n{sym_stderr}"
        );

        assert!(
            stderr.contains("skipping symlinked directory"),
            "must warn when a symlinked directory is skipped\nstderr:\n{stderr}"
        );
        assert!(
            stderr.contains("link_dir"),
            "warning must name the skipped path\nstderr:\n{stderr}"
        );
        assert!(
            stderr.contains("follow_links = true"),
            "warning must name the remedy\nstderr:\n{stderr}"
        );
    }

    #[test]
    fn symlinked_directory_followed_when_enabled() {
        let workspace = prepare_symlink_workspace(true);

        let (code, stdout, stderr) = run_cli(workspace.path(), &["index", "src", "--no-progress"]);
        assert_eq!(
            code, 0,
            "index should succeed\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );

        let (found, sym_stdout, sym_stderr) =
            symbol_is_indexed(workspace.path(), "w5_marker_via_symlink");
        assert!(
            found,
            "a symbol reachable via a symlinked directory must be indexed \
             when follow_links is enabled\nstdout:\n{sym_stdout}\nstderr:\n{sym_stderr}"
        );
    }

    /// Regression for the symlink-escape containment check: unlike
    /// `prepare_symlink_workspace` (whose symlink target lives under the
    /// same workspace, inside `outside/`, so it never exercises an escape),
    /// this points `src/escape_link` at a directory in an entirely separate
    /// `TempDir`. With `follow_links = true`, a malicious repo could ship a
    /// symlink like this pointing at `~/.ssh` or similar; the walker must
    /// refuse to follow it even though `follow_links` is on, and must not
    /// index anything reachable only through it.
    #[test]
    fn symlinked_directory_outside_workspace_root_is_not_followed_even_when_enabled() {
        let workspace = init_ignore_workspace("follow_links = true");

        let outside = TempDir::new().expect("create outside-workspace temp dir");
        let escape_target = outside.path().join("escape_target");
        std::fs::create_dir_all(&escape_target).expect("create escape_target dir");
        std::fs::write(
            escape_target.join("marker.py"),
            "def w5_marker_via_escaping_symlink():\n    pass\n",
        )
        .expect("write marker.py");

        let src_dir = workspace.path().join("src");
        std::fs::create_dir_all(&src_dir).expect("create src dir");
        symlink(&escape_target, src_dir.join("escape_link")).expect("create escaping symlink");

        let (code, stdout, stderr) = run_cli(workspace.path(), &["index", "src", "--no-progress"]);
        assert_eq!(
            code, 0,
            "index should succeed\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );

        let (found, sym_stdout, sym_stderr) =
            symbol_is_indexed(workspace.path(), "w5_marker_via_escaping_symlink");
        assert!(
            !found,
            "a symbol reachable only via a symlink that escapes the workspace root \
             must never be indexed, even with follow_links enabled\
             \nstdout:\n{sym_stdout}\nstderr:\n{sym_stderr}"
        );

        assert!(
            stderr.contains("skipping symlink") && stderr.contains("escapes the workspace root"),
            "must warn when a symlink escaping the workspace root is skipped\nstderr:\n{stderr}"
        );
        assert!(
            stderr.contains("escape_link"),
            "warning must name the skipped symlink\nstderr:\n{stderr}"
        );
    }

    #[test]
    fn root_symlink_is_walked_even_when_follow_links_disabled() {
        let workspace = prepare_symlink_workspace(false);

        // Indexing the symlink itself as the walk root is a depth-0 entry,
        // which `ignore::WalkBuilder` always descends into regardless of
        // `follow_links` -- the asymmetry the underlying issue describes.
        let (code, stdout, stderr) = run_cli(
            workspace.path(),
            &["index", "src/link_dir", "--no-progress"],
        );
        assert_eq!(
            code, 0,
            "index should succeed\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );

        let (found, sym_stdout, sym_stderr) =
            symbol_is_indexed(workspace.path(), "w5_marker_via_symlink");
        assert!(
            found,
            "indexing a symlink directly as the walk root must still descend into it, \
             even with follow_links disabled\nstdout:\n{sym_stdout}\nstderr:\n{sym_stderr}"
        );

        // The root symlink was walked, so a "skipping" warning naming it would
        // be a false report -- and a warning that lies is worse than silence,
        // because it trains the reader to ignore the true ones.
        assert!(
            !stderr.contains("skipping symlinked directory"),
            "must not warn about skipping a root symlink that was in fact walked\
             \nstderr:\n{stderr}"
        );
    }

    #[test]
    fn symlink_loop_terminates_with_follow_links_enabled() {
        let workspace = init_ignore_workspace("follow_links = true");

        let loop_dir = workspace.path().join("loop_root");
        std::fs::create_dir_all(&loop_dir).expect("create loop_root dir");
        std::fs::write(
            loop_dir.join("marker.py"),
            "def w5_marker_loop():\n    pass\n",
        )
        .expect("write marker.py");
        // Self-referential symlink: following it recursively without cycle
        // detection would revisit loop_root forever.
        symlink(&loop_dir, loop_dir.join("self")).expect("create self-referential symlink");

        let test_home = workspace.path().join(".home");
        std::fs::create_dir_all(&test_home).expect("create test home");

        let bin = support::codanna_binary();
        let mut child = Command::new(&bin)
            .args(["index", "loop_root", "--no-progress"])
            .current_dir(workspace.path())
            .env("HOME", &test_home)
            .env("XDG_CONFIG_HOME", &test_home)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn codanna CLI");

        let deadline = Instant::now() + Duration::from_secs(30);
        let status = loop {
            if let Some(status) = child.try_wait().expect("poll child status") {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!(
                    "codanna index did not terminate within 30s on a symlink loop with \
                     follow_links = true (expected the `ignore` crate's cycle detection \
                     to bound the walk)"
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        };

        assert!(
            status.success(),
            "index over a symlink loop with follow_links enabled should still succeed \
             (cycle detected, walk bounded)"
        );

        let (found, sym_stdout, sym_stderr) = symbol_is_indexed(workspace.path(), "w5_marker_loop");
        assert!(
            found,
            "the real file inside the looped directory must still be indexed once\n\
             stdout:\n{sym_stdout}\nstderr:\n{sym_stderr}"
        );
    }
}

// ---------------------------------------------------------------------------
// GitHub issue #28: ignore-rules staleness fingerprint. `get_index_info`
// reports whether the ignore-rule inputs (`.codannaignore`, `.gitignore`,
// `.git/info/exclude`, `indexing.ignore_patterns`, `indexing.follow_links`)
// have changed since the last index build. Detect-and-report only: no
// reconciliation or automatic reindexing happens as a result.
// ---------------------------------------------------------------------------

/// Read `data.ignore_rules_changed` from a `codanna mcp get_index_info
/// --json` invocation's stdout. `Value::Null` (metadata predates this field,
/// or the fingerprint could not be recomputed) round-trips as `None`.
fn ignore_rules_changed_field(stdout: &str) -> Option<bool> {
    let payload: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
        panic!("get_index_info --json stdout must parse as JSON: {e}\nstdout:\n{stdout}")
    });
    payload["data"]["ignore_rules_changed"].as_bool()
}

#[test]
fn staleness_round_trip_detects_ignore_rule_changes_after_index() {
    let workspace = init_ignore_workspace("");

    std::fs::write(
        workspace.path().join("main.rs"),
        "pub fn staleness_marker() {}\n",
    )
    .expect("write main.rs");

    let (index_code, index_stdout, index_stderr) =
        run_cli(workspace.path(), &["index", ".", "--no-progress"]);
    assert_eq!(
        index_code, 0,
        "index should succeed\nstdout:\n{index_stdout}\nstderr:\n{index_stderr}"
    );

    // Immediately after indexing, the fingerprint just written must match
    // one recomputed from the current (unchanged) inputs.
    let (info_code, info_stdout, info_stderr) =
        run_cli(workspace.path(), &["mcp", "get_index_info", "--json"]);
    assert_eq!(
        info_code, 0,
        "get_index_info should succeed\nstdout:\n{info_stdout}\nstderr:\n{info_stderr}"
    );
    assert_eq!(
        ignore_rules_changed_field(&info_stdout),
        Some(false),
        "immediately after indexing, ignore_rules_changed must be false\nstdout:\n{info_stdout}"
    );

    // Add a .gitignore without reindexing: the walk-yielding inputs have
    // now changed relative to what was fingerprinted at index time.
    std::fs::write(workspace.path().join(".gitignore"), "*.log\n")
        .expect("write .gitignore after indexing");

    let (info2_code, info2_stdout, info2_stderr) =
        run_cli(workspace.path(), &["mcp", "get_index_info", "--json"]);
    assert_eq!(
        info2_code, 0,
        "get_index_info should succeed\nstdout:\n{info2_stdout}\nstderr:\n{info2_stderr}"
    );
    assert_eq!(
        ignore_rules_changed_field(&info2_stdout),
        Some(true),
        "adding a .gitignore after indexing must be detected as a change\nstdout:\n{info2_stdout}"
    );

    // Text-format output must surface the same signal as a human-readable
    // warning.
    let (info3_code, info3_stdout, info3_stderr) =
        run_cli(workspace.path(), &["mcp", "get_index_info"]);
    assert_eq!(
        info3_code, 0,
        "get_index_info (text) should succeed\nstdout:\n{info3_stdout}\nstderr:\n{info3_stderr}"
    );
    assert!(
        info3_stdout.contains("index may be stale: ignore rules changed since last index"),
        "text output must warn about stale ignore rules\nstdout:\n{info3_stdout}"
    );
}

#[test]
fn old_metadata_without_fingerprint_loads_clean_as_unknown() {
    let workspace = init_ignore_workspace("");

    std::fs::write(
        workspace.path().join("main.rs"),
        "pub fn old_metadata_marker() {}\n",
    )
    .expect("write main.rs");

    let (index_code, index_stdout, index_stderr) =
        run_cli(workspace.path(), &["index", ".", "--no-progress"]);
    assert_eq!(
        index_code, 0,
        "index should succeed\nstdout:\n{index_stdout}\nstderr:\n{index_stderr}"
    );

    // Simulate metadata written before the `ignore_fingerprint` field
    // existed by stripping it from the freshly persisted index.meta.
    let meta_path = workspace.path().join(".codanna/index/index.meta");
    let mut meta: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&meta_path).expect("read index.meta"))
            .expect("parse index.meta as JSON");
    meta.as_object_mut()
        .expect("index.meta must be a JSON object")
        .remove("ignore_fingerprint");
    std::fs::write(
        &meta_path,
        serde_json::to_string_pretty(&meta).expect("serialize edited index.meta"),
    )
    .expect("write edited index.meta");

    let (info_code, info_stdout, info_stderr) =
        run_cli(workspace.path(), &["mcp", "get_index_info", "--json"]);
    assert_eq!(
        info_code, 0,
        "get_index_info should succeed against pre-upgrade metadata\nstdout:\n{info_stdout}\nstderr:\n{info_stderr}"
    );
    assert_eq!(
        ignore_rules_changed_field(&info_stdout),
        None,
        "metadata missing ignore_fingerprint must report unknown (null), never 'changed'\nstdout:\n{info_stdout}"
    );

    // The text-format warning must never fire from unknown state, even
    // though the walk inputs have not actually changed here.
    let (info2_code, info2_stdout, info2_stderr) =
        run_cli(workspace.path(), &["mcp", "get_index_info"]);
    assert_eq!(
        info2_code, 0,
        "get_index_info (text) should succeed\nstdout:\n{info2_stdout}\nstderr:\n{info2_stderr}"
    );
    assert!(
        !info2_stdout.contains("index may be stale"),
        "unknown staleness state must never render as a warning\nstdout:\n{info2_stdout}"
    );
}

#[test]
fn dry_run_default_output_truncates_with_more_files_message() {
    let workspace = prepare_workspace();

    let (code, stdout, stderr) = run_cli(
        workspace.path(),
        &["index", "src", "--dry-run", "--no-progress"],
    );
    assert_eq!(
        code, 0,
        "default dry-run should succeed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    let expected_more = FIXTURE_FILE_COUNT - 5;
    let expected_suffix = format!("... and {expected_more} more files");
    assert!(
        stdout.contains(&expected_suffix),
        "default dry-run output must truncate at 5 with a '... and N more files' message \
         (expected to contain {expected_suffix:?})\nstdout:\n{stdout}"
    );
}
