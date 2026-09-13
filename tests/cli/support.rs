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

pub fn run_cli(workspace: &Path, args: &[&str]) -> (i32, String, String) {
    let bin = codanna_binary();
    let test_home = workspace.join(".home");
    std::fs::create_dir_all(&test_home).expect("create test home");

    let output = Command::new(&bin)
        .args(args)
        .current_dir(workspace)
        .env("HOME", &test_home)
        .env("XDG_CONFIG_HOME", &test_home)
        .output()
        .expect("run codanna CLI");

    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}

/// The on-disk index root for a workspace.
///
/// Flat layout today: this *is* the index directory. Once generation
/// directories land, this will resolve to the workspace-level container
/// that holds them.
pub fn index_root(ws: &Path) -> PathBuf {
    ws.join(".codanna/index")
}

/// The currently-active generation directory for a workspace.
///
/// Flat layout today: identical to `index_root`. Kept as a distinct name
/// so callers that care about "the generation currently being read/written"
/// don't need to change when generation directories are introduced.
///
/// Not every test binary that includes this module uses this helper yet;
/// `#[allow(dead_code)]` avoids a per-binary unused-function lint since
/// `support.rs` is compiled independently into each `#[path]`-including
/// crate.
#[allow(dead_code)]
pub fn current_generation_dir(ws: &Path) -> PathBuf {
    index_root(ws)
}

/// Path to `index.meta` within the current generation.
pub fn index_meta_path(ws: &Path) -> PathBuf {
    index_root(ws).join("index.meta")
}

/// Path to the `tantivy` directory within the current generation.
#[allow(dead_code)]
pub fn tantivy_dir(ws: &Path) -> PathBuf {
    index_root(ws).join("tantivy")
}

/// File names present directly under the index root, for before/after
/// comparisons that assert an operation did or did not touch the index
/// directory contents.
#[allow(dead_code)]
pub fn index_dir_entries(ws: &Path) -> BTreeSet<String> {
    std::fs::read_dir(index_root(ws))
        .expect("read index dir")
        .map(|entry| {
            entry
                .expect("dir entry")
                .file_name()
                .to_string_lossy()
                .to_string()
        })
        .collect()
}
