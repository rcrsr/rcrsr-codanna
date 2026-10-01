//! A relative `indexed_paths` entry (`"."`) resolves against the workspace
//! that owns the config, not the current directory. Running a read-only
//! command from a subdirectory (with or without a relative `--config`) must
//! not treat the entry as a new root, re-index, or publish a generation.

use std::path::Path;
use std::process::Command;

use crate::support::{codanna_binary, index_root};

fn run_in(workspace: &Path, cwd: &Path, args: &[&str]) -> (i32, String) {
    let test_home = workspace.join(".home");
    std::fs::create_dir_all(&test_home).expect("create test home");
    let output = Command::new(codanna_binary())
        .args(args)
        .current_dir(cwd)
        .env("HOME", &test_home)
        .env("XDG_CONFIG_HOME", &test_home)
        .output()
        .expect("run codanna CLI");
    let combined = format!(
        "stdout:{}\nstderr:{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    (output.status.code().unwrap_or(-1), combined)
}

fn current_generation(workspace: &Path) -> String {
    std::fs::read_to_string(index_root(workspace).join("current"))
        .expect("read current pointer")
        .trim()
        .to_string()
}

fn assert_read_only_retrieve(workspace: &Path, cwd: &Path, extra: &[&str], gen_before: &str) {
    let mut args = vec!["retrieve", "symbol", "a"];
    args.extend_from_slice(extra);
    let (exit, out) = run_in(workspace, cwd, &args);
    assert_eq!(exit, 0, "retrieve from {cwd:?} must succeed\n{out}");
    assert!(
        out.contains("src/lib.rs"),
        "symbol `a` must be reported at src/lib.rs\n{out}"
    );
    assert!(
        !out.contains("Indexing directory"),
        "retrieve from {cwd:?} must not re-index\n{out}"
    );
    assert_eq!(
        current_generation(workspace),
        gen_before,
        "retrieve from {cwd:?} must not publish a generation"
    );
}

/// Writes the fixture workspace and indexes it from the workspace root.
/// Returns the canonical workspace and its `src/sub` directory.
fn indexed_workspace(temp: &tempfile::TempDir) -> (std::path::PathBuf, std::path::PathBuf) {
    let workspace = temp.path().canonicalize().expect("canonical workspace");
    let sub = workspace.join("src/sub");
    std::fs::create_dir_all(&sub).expect("create src/sub");
    std::fs::write(workspace.join("src/lib.rs"), "pub fn a() {}\n").expect("write lib.rs");
    std::fs::write(sub.join("m.rs"), "pub fn b() {}\n").expect("write m.rs");
    std::fs::create_dir_all(workspace.join(".codanna")).expect("create .codanna");
    std::fs::write(
        workspace.join(".codanna/settings.toml"),
        "index_path = \".codanna/index\"\n\n[indexing]\nindexed_paths = [\".\"]\n\n[semantic_search]\nenabled = false\n",
    )
    .expect("write settings");

    let (exit, out) = run_in(&workspace, &workspace, &["index", ".", "--no-progress"]);
    assert_eq!(exit, 0, "initial index must succeed\n{out}");
    (workspace, sub)
}

fn assert_symbol_at(workspace: &Path, symbol: &str, file: &str, context: &str) {
    let (exit, out) = run_in(workspace, workspace, &["retrieve", "symbol", symbol]);
    assert_eq!(exit, 0, "retrieve {symbol} {context}\n{out}");
    assert!(
        out.contains(file),
        "symbol `{symbol}` must be reported at {file} {context}\n{out}"
    );
}

#[test]
fn relative_indexed_path_reindex_from_subdirectory_keeps_all_roots() {
    let temp = tempfile::TempDir::new().expect("temp workspace");
    let (workspace, sub) = indexed_workspace(&temp);

    // The reindex output itself is asserted because every later CLI call
    // runs its own startup catch-up from the workspace root, which would
    // otherwise heal a wrongly scoped rebuild before `retrieve` looks.
    for (args, expected, context) in [
        (
            vec!["mcp", "reindex"],
            "Reindexed 0 files, 2 symbols",
            "after incremental reindex",
        ),
        (
            vec!["mcp", "reindex", "force:true"],
            "Reindexed 2 files, 2 symbols",
            "after forced reindex",
        ),
    ] {
        let (exit, out) = run_in(&workspace, &sub, &args);
        assert_eq!(exit, 0, "{args:?} from src/sub must succeed\n{out}");
        assert!(
            out.contains(expected),
            "{args:?} from src/sub must walk every workspace root\n{out}"
        );
        assert_symbol_at(&workspace, "a", "src/lib.rs", context);
        assert_symbol_at(&workspace, "b", "src/sub/m.rs", context);
    }
}

#[test]
fn relative_indexed_path_does_not_reindex_from_subdirectory() {
    let temp = tempfile::TempDir::new().expect("temp workspace");
    let (workspace, sub) = indexed_workspace(&temp);
    let gen_before = current_generation(&workspace);

    assert_read_only_retrieve(&workspace, &workspace, &[], &gen_before);
    assert_read_only_retrieve(&workspace, &sub, &[], &gen_before);
    assert_read_only_retrieve(&workspace, &workspace, &[], &gen_before);
    assert_read_only_retrieve(
        &workspace,
        &sub,
        &["--config", "../../.codanna/settings.toml"],
        &gen_before,
    );
}
