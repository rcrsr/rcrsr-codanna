//! End-to-end CLI coverage of the index-generation publish/retention
//! lifecycle: two consecutive `codanna index` runs share Tantivy segment
//! inodes and publish distinct generations, `--force` retains the pre-force
//! generation as `Previous`, a build killed mid-flight leaves an orphaned
//! `BUILDING` generation that `--gc` reclaims while the prior generation
//! keeps serving, and `--rollback` restores a previous generation's content
//! (including its symbol count).
//!
//! The ENOSPC free-space preflight (`open_build`'s `Fresh`-only disk check)
//! is intentionally not re-tested here: it is already covered end to end,
//! down to the specific error variant, by
//! `open_build_fresh_preflight_refuses_via_injected_disks` in
//! `src/storage/persistence.rs`, via the same `open_build_in` injected-disk
//! test seam a CLI-level test would have no way to drive more precisely.
//! Duplicating it at the CLI layer would only re-assert the same
//! `IndexError::IndexNotSpaceForBuild` classification through slower,
//! flakier process-spawning machinery.

use std::collections::BTreeSet;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::support::{index_root, run_cli, spawn_cli, tantivy_dir};

/// Write a single-symbol Rust fixture file into `workspace/src/<file_name>`.
fn write_symbol_file(workspace: &Path, file_name: &str, symbol_name: &str, value: i32) {
    let src = workspace.join("src");
    std::fs::create_dir_all(&src).expect("create src dir");
    std::fs::write(
        src.join(file_name),
        format!("pub fn {symbol_name}() -> i32 {{ {value} }}\n"),
    )
    .expect("write fixture file");
}

/// `codanna index --status --json`, parsed into rows.
fn status_rows(workspace: &Path) -> Vec<serde_json::Value> {
    let (exit, stdout, stderr) = run_cli(workspace, &["index", "--status", "--json"]);
    assert_eq!(
        exit, 0,
        "--status --json must succeed\nstdout:{stdout}\nstderr:{stderr}"
    );
    serde_json::from_str(stdout.trim()).expect("--status --json parses as an array")
}

/// The current generation id recorded at `workspace`'s `current` pointer.
fn current_id(workspace: &Path) -> String {
    std::fs::read_to_string(index_root(workspace).join("current"))
        .expect("read current pointer")
        .trim()
        .to_string()
}

/// Parse the symbol count out of `--info`'s
/// `"Loaded existing index (total: N symbols)"` stderr line, written by
/// [`crate::support`]'s CLI invocations of any read command
/// (`retrieve`/`serve`/`dump`) that loads the current generation.
fn parse_loaded_symbol_count(stderr: &str) -> u64 {
    let marker = "total: ";
    let start = stderr.find(marker).unwrap_or_else(|| {
        panic!("--info output must report a loaded symbol count\nstderr:{stderr}")
    }) + marker.len();
    let rest = &stderr[start..];
    let end = rest.find(' ').unwrap_or_else(|| {
        panic!("--info output must be followed by ' symbols)'\nstderr:{stderr}")
    });
    rest[..end]
        .parse()
        .unwrap_or_else(|e| panic!("--info symbol count must be a number: {e}\nstderr:{stderr}"))
}

/// Poll for a `BUILDING` marker under `root/gen/*/BUILDING`, returning the
/// generation id it belongs to once found. Panics if none appears before
/// `deadline` elapses.
fn wait_for_building_generation(root: &Path, deadline: Duration) -> String {
    let started = Instant::now();
    loop {
        if let Ok(entries) = std::fs::read_dir(root.join("gen")) {
            for entry in entries.filter_map(Result::ok) {
                if entry.path().join("BUILDING").is_file() {
                    return entry.file_name().to_string_lossy().to_string();
                }
            }
        }
        assert!(
            started.elapsed() <= deadline,
            "no BUILDING marker appeared under {}/gen within {deadline:?}",
            root.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// `(file name, inode)` for every `*.store` Tantivy segment file directly
/// under `tantivy_dir`.
///
/// Tantivy may merge some segments away between two builds (which segments
/// survive unmerged is an implementation detail, not something a test
/// should pin down), so an inode-sharing assertion must check "at least one
/// segment file survived unmerged and shares its inode", not any single
/// named file picked in advance.
fn segment_store_files(tantivy_dir: &Path) -> Vec<(String, u64)> {
    std::fs::read_dir(tantivy_dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", tantivy_dir.display()))
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            name.ends_with(".store").then_some(name)
        })
        .map(|name| {
            let ino = std::fs::metadata(tantivy_dir.join(&name))
                .unwrap_or_else(|e| panic!("stat {}: {e}", tantivy_dir.join(&name).display()))
                .ino();
            (name, ino)
        })
        .collect()
}

/// (1) Two consecutive `codanna index` runs in the same workspace: the
/// second run's generation shares Tantivy segment inodes with the first
/// (a `CloneCurrent` build hardlinks the parent's segments), a distinct new
/// generation id is published, and after `--gc` `--status` lists exactly
/// `current` + `previous` -- not more.
#[test]
fn second_run_shares_tantivy_inodes_and_publishes_a_new_generation() {
    let temp = tempfile::TempDir::new().expect("temp workspace");
    let workspace = temp.path();
    write_symbol_file(workspace, "alpha.rs", "publish_first_symbol", 1);

    let (exit, stdout, stderr) = run_cli(workspace, &["index", "src", "--no-progress"]);
    assert_eq!(
        exit, 0,
        "first index run must succeed\nstdout:{stdout}\nstderr:{stderr}"
    );

    let first_gen = current_id(workspace);
    let first_segments = segment_store_files(&tantivy_dir(workspace));
    assert!(
        !first_segments.is_empty(),
        "the first generation must have at least one Tantivy segment"
    );

    write_symbol_file(workspace, "beta.rs", "publish_second_symbol", 2);
    let (exit, stdout, stderr) = run_cli(workspace, &["index", "src", "--no-progress"]);
    assert_eq!(
        exit, 0,
        "second index run must succeed\nstdout:{stdout}\nstderr:{stderr}"
    );

    let second_gen = current_id(workspace);
    assert_ne!(
        first_gen, second_gen,
        "second run must publish a generation distinct from the first"
    );

    let second_tantivy = tantivy_dir(workspace);
    let shared_unmerged_segment = first_segments.iter().find_map(|(name, first_ino)| {
        let second_path = second_tantivy.join(name);
        let second_meta = std::fs::metadata(&second_path).ok()?;
        (second_meta.ino() == *first_ino).then_some((name.clone(), second_meta.nlink()))
    });
    let (shared_name, shared_nlink) = shared_unmerged_segment.unwrap_or_else(|| {
        panic!(
            "expected at least one of the first generation's segment files \
             ({first_segments:?}) to survive unmerged and share its inode with \
             the second generation's clone under {}",
            second_tantivy.display()
        )
    });
    assert!(
        shared_nlink >= 2,
        "segment file {shared_name} shared across generations must have nlink >= 2, got {shared_nlink}"
    );

    let (exit, stdout, stderr) = run_cli(workspace, &["index", "--gc"]);
    assert_eq!(
        exit, 0,
        "--gc must succeed\nstdout:{stdout}\nstderr:{stderr}"
    );

    let rows = status_rows(workspace);
    assert_eq!(
        rows.len(),
        2,
        "gc after two runs must leave exactly current+previous, not more\nrows:{rows:?}"
    );
    let states: BTreeSet<&str> = rows
        .iter()
        .map(|row| row["state"].as_str().expect("state is a string"))
        .collect();
    assert_eq!(
        states,
        BTreeSet::from(["current", "previous"]),
        "gc must leave exactly one current and one previous generation\nrows:{rows:?}"
    );
}

/// (2) `codanna index --force` publishes a fresh generation while the
/// pre-force generation is retained on disk as `Previous`, not deleted.
#[test]
fn force_reindex_retains_the_previous_generation_on_disk() {
    let temp = tempfile::TempDir::new().expect("temp workspace");
    let workspace = temp.path();
    write_symbol_file(workspace, "alpha.rs", "force_publish_target", 1);

    let (exit, stdout, stderr) = run_cli(workspace, &["index", "src", "--no-progress"]);
    assert_eq!(
        exit, 0,
        "initial index run must succeed\nstdout:{stdout}\nstderr:{stderr}"
    );
    let pre_force_gen = current_id(workspace);

    let (exit, stdout, stderr) = run_cli(workspace, &["index", "--force", "--no-progress"]);
    assert_eq!(
        exit, 0,
        "force reindex must succeed\nstdout:{stdout}\nstderr:{stderr}"
    );

    let post_force_gen = current_id(workspace);
    assert_ne!(
        pre_force_gen, post_force_gen,
        "--force must publish a generation distinct from the pre-force one"
    );

    let pre_force_dir = index_root(workspace).join("gen").join(&pre_force_gen);
    assert!(
        pre_force_dir.is_dir(),
        "the pre-force generation directory must still exist on disk after --force"
    );

    let rows = status_rows(workspace);
    let pre_force_row = rows
        .iter()
        .find(|row| row["id"] == pre_force_gen)
        .unwrap_or_else(|| {
            panic!("pre-force generation {pre_force_gen} must still be listed\nrows:{rows:?}")
        });
    assert_eq!(
        pre_force_row["state"], "previous",
        "the pre-force generation must be classified Previous, not deleted\nrows:{rows:?}"
    );
}

/// (3) SIGKILL-ing the CLI process shortly after its `BUILDING` marker
/// appears leaves an orphaned generation on disk: the prior (`current`)
/// generation keeps serving retrieve requests untouched, and a later
/// `codanna index --gc` reclaims the orphan.
#[test]
fn killed_build_orphans_its_generation_and_old_generation_keeps_serving() {
    let temp = tempfile::TempDir::new().expect("temp workspace");
    let workspace = temp.path();
    write_symbol_file(workspace, "alpha.rs", "survives_killed_build", 1);

    let (exit, stdout, stderr) = run_cli(workspace, &["index", "src", "--no-progress"]);
    assert_eq!(
        exit, 0,
        "initial index run must succeed\nstdout:{stdout}\nstderr:{stderr}"
    );
    let old_gen = current_id(workspace);

    write_symbol_file(workspace, "beta.rs", "never_completes_indexing", 2);

    let root = index_root(workspace);
    let mut child = spawn_cli(workspace, &["index", "src", "--no-progress"]);
    let building_gen = wait_for_building_generation(&root, Duration::from_secs(30));
    assert_ne!(
        building_gen, old_gen,
        "the killed build's generation must be a new one, not the already-published one"
    );

    child.kill().expect("SIGKILL the in-flight build");
    child.wait().expect("wait for the killed build to exit");

    // `current` must be untouched: the killed build never reached publish.
    assert_eq!(
        current_id(workspace),
        old_gen,
        "current must still name the pre-existing generation after the build was killed"
    );

    let (exit, stdout, stderr) =
        run_cli(workspace, &["retrieve", "symbol", "survives_killed_build"]);
    assert_eq!(
        exit, 0,
        "the old generation must still serve retrieve after the newer build was killed\nstdout:{stdout}\nstderr:{stderr}"
    );

    let orphan_dir = root.join("gen").join(&building_gen);
    assert!(
        orphan_dir.is_dir(),
        "the killed build's generation directory must still exist before gc runs"
    );

    let (exit, stdout, stderr) = run_cli(workspace, &["index", "--gc"]);
    assert_eq!(
        exit, 0,
        "--gc must succeed\nstdout:{stdout}\nstderr:{stderr}"
    );
    assert!(
        stdout.contains("removed=1"),
        "--gc must report reclaiming exactly the one orphaned build\nstdout:{stdout}"
    );
    assert!(
        !orphan_dir.exists(),
        "gc must remove the orphaned BUILDING generation left by the killed build"
    );

    let old_dir = root.join("gen").join(&old_gen);
    assert!(
        old_dir.is_dir(),
        "gc must never remove the current generation"
    );
}

/// (4) `codanna index --rollback` restores `current` to the previous
/// generation, and a subsequent retrieve reports that generation's own
/// (older, smaller) symbol count -- not the newer generation's.
#[test]
fn rollback_restores_previous_generation_and_reports_its_symbol_count() {
    let temp = tempfile::TempDir::new().expect("temp workspace");
    let workspace = temp.path();
    write_symbol_file(workspace, "alpha.rs", "rollback_old_symbol", 1);

    let (exit, stdout, stderr) = run_cli(workspace, &["index", "src", "--no-progress"]);
    assert_eq!(
        exit, 0,
        "first index run must succeed\nstdout:{stdout}\nstderr:{stderr}"
    );

    let (exit, _stdout, stderr) = run_cli(
        workspace,
        &["--info", "retrieve", "symbol", "rollback_old_symbol"],
    );
    assert_eq!(exit, 0, "retrieve must find the first generation's symbol");
    let old_symbol_count = parse_loaded_symbol_count(&stderr);

    write_symbol_file(workspace, "beta.rs", "rollback_new_symbol", 2);
    write_symbol_file(workspace, "gamma.rs", "rollback_new_symbol_two", 3);
    let (exit, stdout, stderr) = run_cli(workspace, &["index", "src", "--no-progress"]);
    assert_eq!(
        exit, 0,
        "second index run must succeed\nstdout:{stdout}\nstderr:{stderr}"
    );

    let (exit, _stdout, stderr) = run_cli(
        workspace,
        &["--info", "retrieve", "symbol", "rollback_new_symbol"],
    );
    assert_eq!(exit, 0, "retrieve must find the second generation's symbol");
    let new_symbol_count = parse_loaded_symbol_count(&stderr);
    assert!(
        new_symbol_count > old_symbol_count,
        "the second generation must have strictly more symbols than the first \
         (old={old_symbol_count}, new={new_symbol_count})"
    );

    let (exit, stdout, stderr) = run_cli(workspace, &["index", "--rollback"]);
    assert_eq!(
        exit, 0,
        "--rollback must succeed\nstdout:{stdout}\nstderr:{stderr}"
    );

    let (exit, _stdout, _stderr) =
        run_cli(workspace, &["retrieve", "symbol", "rollback_new_symbol"]);
    assert_eq!(
        exit, 3,
        "the newer generation's content must no longer be visible after rollback"
    );

    let (exit, _stdout, stderr) = run_cli(
        workspace,
        &["--info", "retrieve", "symbol", "rollback_old_symbol"],
    );
    assert_eq!(
        exit, 0,
        "rollback must restore the old generation's content"
    );
    let restored_symbol_count = parse_loaded_symbol_count(&stderr);
    assert_eq!(
        restored_symbol_count, old_symbol_count,
        "rollback must restore exactly the old generation's symbol count"
    );
}
