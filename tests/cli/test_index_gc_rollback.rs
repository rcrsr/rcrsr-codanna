//! `codanna index --gc` and `codanna index --rollback` operate on
//! on-disk generations directly (no facade), mirroring `--status`'s
//! read-first pattern.

use std::path::Path;
use std::sync::Arc;

use tempfile::TempDir;

use codanna::config::Settings;
use codanna::storage::IndexPersistence;
use codanna::storage::persistence::BuildMode;

use crate::support::run_cli;

fn write_fixture(workspace: &Path) {
    let src = workspace.join("src");
    std::fs::create_dir_all(&src).expect("create src dir");
    std::fs::write(
        src.join("alpha.rs"),
        r#"
pub fn old_only_symbol() -> i32 {
    1
}
"#,
    )
    .expect("write fixture");
}

fn settings_for(workspace: &Path) -> Arc<Settings> {
    Arc::new(Settings {
        index_path: workspace.join(".codanna/index"),
        workspace_root: Some(workspace.to_path_buf()),
        ..Settings::default()
    })
}

/// `--gc` removes an orphaned generation directory (no `COMPLETE` manifest,
/// not named by `current`) and leaves the real current generation intact.
#[test]
fn gc_removes_an_orphan_generation() {
    let temp = TempDir::new().expect("temp workspace");
    let workspace = temp.path();
    write_fixture(workspace);

    let (exit, stdout, stderr) = run_cli(workspace, &["index", "src", "--no-progress"]);
    assert_eq!(
        exit, 0,
        "normal index run must succeed\nstdout:{stdout}\nstderr:{stderr}"
    );

    // Manufacture an orphan: a `gen/<id>/` directory with no `COMPLETE`
    // manifest and not named by `current`.
    let root = crate::support::index_root(workspace);
    let orphan_id = "0000000000000000001";
    let orphan_dir = root.join("gen").join(orphan_id);
    std::fs::create_dir_all(&orphan_dir).expect("create orphan generation dir");

    let (exit, stdout, stderr) = run_cli(workspace, &["index", "--gc"]);
    assert_eq!(
        exit, 0,
        "--gc must succeed\nstdout:{stdout}\nstderr:{stderr}"
    );
    assert!(
        stdout.contains("removed=1"),
        "--gc summary must report the removed orphan\nstdout:{stdout}"
    );
    assert!(
        !orphan_dir.exists(),
        "gc must remove the orphan generation directory"
    );

    let (exit, stdout, stderr) = run_cli(workspace, &["index", "--status", "--json"]);
    assert_eq!(exit, 0, "stdout:{stdout}\nstderr:{stderr}");
    let rows: Vec<serde_json::Value> =
        serde_json::from_str(stdout.trim()).expect("--status --json parses as an array");
    assert_eq!(
        rows.len(),
        1,
        "the real current generation must survive gc\nstdout:{stdout}"
    );
    assert_eq!(rows[0]["state"], "current");
}

/// `--rollback` (with no id) flips `current` back to the newest `Previous`
/// generation, and a subsequent `retrieve` sees that generation's content
/// again.
#[test]
fn rollback_restores_the_previous_generation_and_retrieve_sees_the_old_count() {
    let temp = TempDir::new().expect("temp workspace");
    let workspace = temp.path();
    write_fixture(workspace);

    let (exit, stdout, stderr) = run_cli(workspace, &["index", "src", "--no-progress"]);
    assert_eq!(
        exit, 0,
        "normal index run must succeed\nstdout:{stdout}\nstderr:{stderr}"
    );

    // Manufacture a second, newer generation directly through the
    // persistence/build API (the CLI `index` command does not yet build
    // through `open_build`/`publish`, so this is the only way to get two
    // real generations on disk for this test).
    let settings = settings_for(workspace);
    let persistence = IndexPersistence::new(settings.index_path.clone());

    std::fs::write(
        workspace.join("src").join("beta.rs"),
        r#"
pub fn new_only_symbol() -> i32 {
    2
}
"#,
    )
    .expect("write second fixture file");

    let mut build = persistence
        .open_build(settings.clone(), BuildMode::Fresh)
        .expect("open a fresh build");
    build
        .index_file_with_force(workspace.join("src").join("beta.rs"), true)
        .expect("index the new fixture into the build");
    let new_id = build.generation_id().clone();
    let published_id = persistence.publish(build).expect("publish the new build");
    assert_eq!(published_id, new_id);

    let (exit, stdout, stderr) = run_cli(workspace, &["index", "--rollback"]);
    assert_eq!(
        exit, 0,
        "--rollback must succeed\nstdout:{stdout}\nstderr:{stderr}"
    );
    assert!(
        stdout.contains(&new_id.to_string()),
        "rollback output must name the generation it moved current away from\nstdout:{stdout}"
    );

    // The old generation's content (`old_only_symbol`) must be visible
    // again, and the newer generation's content (`new_only_symbol`) must
    // no longer be.
    let (exit, stdout, stderr) = run_cli(workspace, &["retrieve", "symbol", "old_only_symbol"]);
    assert_eq!(
        exit, 0,
        "rollback must restore the old generation's content\nstdout:{stdout}\nstderr:{stderr}"
    );

    let (exit, _stdout, _stderr) = run_cli(workspace, &["retrieve", "symbol", "new_only_symbol"]);
    assert_eq!(
        exit, 3,
        "the newer generation's content must no longer be visible after rollback"
    );
}

/// `--rollback` must refuse a damaged target generation rather than
/// silently flipping `current` onto broken data.
#[test]
fn rollback_refuses_a_damaged_target() {
    let temp = TempDir::new().expect("temp workspace");
    let workspace = temp.path();
    write_fixture(workspace);

    let (exit, stdout, stderr) = run_cli(workspace, &["index", "src", "--no-progress"]);
    assert_eq!(
        exit, 0,
        "normal index run must succeed\nstdout:{stdout}\nstderr:{stderr}"
    );

    let current_dir = crate::support::current_generation_dir(workspace);
    let current_id = current_dir
        .file_name()
        .expect("current generation dir has a name")
        .to_string_lossy()
        .to_string();

    // Damage the only generation by corrupting its `index.meta`, then
    // roll back onto it explicitly by id.
    std::fs::write(current_dir.join("index.meta"), "not valid json").expect("corrupt index.meta");

    let (exit, stdout, stderr) = run_cli(workspace, &["index", "--rollback", &current_id]);
    assert_ne!(
        exit, 0,
        "rollback onto a damaged generation must fail loudly\nstdout:{stdout}\nstderr:{stderr}"
    );
    assert!(
        stderr.contains("damaged") || stderr.contains("Damaged"),
        "the refusal must surface the damage reason\nstderr:{stderr}"
    );

    // `current` must still point at the (damaged) generation unchanged --
    // never silently flipped.
    let root = crate::support::index_root(workspace);
    let recorded_current =
        std::fs::read_to_string(root.join("current")).expect("read current pointer");
    assert_eq!(
        recorded_current.trim(),
        current_id,
        "a refused rollback must never move the current pointer"
    );
}
