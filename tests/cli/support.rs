use std::collections::BTreeSet;
use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn codanna_binary() -> PathBuf {
    if let Some(path) = option_env!("CARGO_BIN_EXE_codanna") {
        return PathBuf::from(path);
    }

    if let Ok(path) = env::var("CODANNA_BIN") {
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

/// Build the `codanna <args>` [`Command`] for `workspace`, with an isolated
/// `HOME`/`XDG_CONFIG_HOME` under the workspace so the process never reads
/// or writes a developer's real home directory. Shared by [`run_cli`]
/// (blocks on completion) and [`spawn_cli`] (returns the live child so a
/// test can observe on-disk state while the process is still running).
fn command_for(workspace: &Path, args: &[&str]) -> Command {
    let bin = codanna_binary();
    let test_home = workspace.join(".home");
    std::fs::create_dir_all(&test_home).expect("create test home");

    let mut command = Command::new(&bin);
    command
        .args(args)
        .current_dir(workspace)
        .env("HOME", &test_home)
        .env("XDG_CONFIG_HOME", &test_home);
    command
}

pub fn run_cli(workspace: &Path, args: &[&str]) -> (i32, String, String) {
    let output = command_for(workspace, args)
        .output()
        .expect("run codanna CLI");

    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}

/// Spawn `codanna <args>` in `workspace` without waiting for it to exit.
///
/// For tests that need to observe on-disk state while the process is still
/// running (e.g. poll for a `BUILDING` marker) and then kill it, rather than
/// [`run_cli`]'s block-until-exit behavior. Stdout/stderr are discarded
/// (`Stdio::null`) since nothing reads them before the process is killed.
#[allow(dead_code)]
pub fn spawn_cli(workspace: &Path, args: &[&str]) -> std::process::Child {
    command_for(workspace, args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn codanna CLI")
}

/// The on-disk index root for a workspace: the workspace-level container
/// (`current` pointer + `gen/<id>/` directories) rather than any single
/// generation's data directory -- use [`current_generation_dir`] for that.
pub fn index_root(ws: &Path) -> PathBuf {
    ws.join(".codanna/index")
}

/// The currently-active generation directory for a workspace:
/// `.codanna/index/gen/<id>/`, resolved by reading (and, for a legacy flat
/// layout, migrating) the `current` pointer at `index_root`.
///
/// Not every test binary that includes this module uses this helper yet;
/// `#[allow(dead_code)]` avoids a per-binary unused-function lint since
/// `support.rs` is compiled independently into each `#[path]`-including
/// crate.
#[allow(dead_code)]
pub fn current_generation_dir(ws: &Path) -> PathBuf {
    use codanna::storage::IndexLayout;
    use codanna::storage::generation::resolve_current;

    let root = index_root(ws);
    let layout = IndexLayout::new(root.clone());
    match resolve_current(&layout) {
        Ok(Some(id)) => layout.gen_dir(&id),
        Ok(None) | Err(_) => root,
    }
}

/// Path to `index.meta` within the current generation.
pub fn index_meta_path(ws: &Path) -> PathBuf {
    current_generation_dir(ws).join("index.meta")
}

/// Path to the `tantivy` directory within the current generation.
#[allow(dead_code)]
pub fn tantivy_dir(ws: &Path) -> PathBuf {
    current_generation_dir(ws).join("tantivy")
}

/// Every file/directory path present under the index root, recursively,
/// relative to the root and using forward slashes regardless of platform --
/// for before/after comparisons that assert an operation did or did not
/// touch the index directory contents.
///
/// Under the generation layout, the index root's top-level entries are just
/// `current` and `gen/` regardless of what mutates underneath a particular
/// generation directory; a shallow, non-recursive listing would not detect
/// e.g. an orphaned generation directory left behind by a partially-failed
/// force run. Walking recursively restores the original flat-layout
/// guarantee: any nested addition, removal, or rename shows up as a set
/// difference.
#[allow(dead_code)]
pub fn index_dir_entries(ws: &Path) -> BTreeSet<String> {
    let root = index_root(ws);
    walkdir::WalkDir::new(&root)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.path() != root)
        .map(|entry| {
            entry
                .path()
                .strip_prefix(&root)
                .expect("walkdir entries are nested under the root it walks")
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect()
}
