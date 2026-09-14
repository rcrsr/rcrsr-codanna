//! `codanna index --status` prints read-only, current-plus-per-generation
//! index status without building or writing the index.

use std::path::Path;

use tempfile::TempDir;

use crate::support::run_cli;

fn write_fixture(workspace: &Path) {
    let src = workspace.join("src");
    std::fs::create_dir_all(&src).expect("create src dir");
    std::fs::write(
        src.join("alpha.rs"),
        r#"
pub fn status_gate_target() -> i32 {
    1
}
"#,
    )
    .expect("write fixture");
}

/// After a normal `codanna index` run, `--status --json` must report a
/// single-element JSON array whose one generation is `state: "current"`.
#[test]
fn status_json_reports_one_current_generation_after_normal_index_run() {
    let temp = TempDir::new().expect("temp workspace");
    let workspace = temp.path();
    write_fixture(workspace);

    let (exit, stdout, stderr) = run_cli(workspace, &["index", "src", "--no-progress"]);
    assert_eq!(
        exit, 0,
        "normal index run must succeed\nstdout:{stdout}\nstderr:{stderr}"
    );

    let (exit, stdout, stderr) = run_cli(workspace, &["index", "--status", "--json"]);
    assert_eq!(
        exit, 0,
        "--status --json must succeed\nstdout:{stdout}\nstderr:{stderr}"
    );

    let rows: Vec<serde_json::Value> = serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
        panic!("--status --json stdout must parse as a JSON array: {e}\nstdout:\n{stdout}")
    });

    assert_eq!(
        rows.len(),
        1,
        "a freshly built index must report exactly one generation\nstdout:{stdout}"
    );
    assert_eq!(
        rows[0]["state"], "current",
        "the only generation after a normal index run must be current\nstdout:{stdout}"
    );
}

/// `--status` against a workspace that has never been indexed must not
/// bootstrap an empty `current` generation on disk. This is the first
/// real-world invocation most users hit (checking status before ever
/// running `codanna index`), and it must stay read-only: no `.codanna/index`
/// directory at all, and `list_generations` reports zero rows rather than a
/// phantom `current` generation.
#[test]
fn status_on_never_indexed_workspace_does_not_write_the_index() {
    let temp = TempDir::new().expect("temp workspace");
    let workspace = temp.path();
    write_fixture(workspace);

    // Initialize the project (writes settings.toml, no index) separately so
    // the one-time "Created default configuration..." banner that `init`
    // prints to stdout doesn't get mixed into the `--status --json` output
    // parsed below -- this test cares about `--status` never having
    // indexed, not about first-run project bootstrap noise.
    let (exit, _stdout, stderr) = run_cli(workspace, &["init"]);
    assert_eq!(exit, 0, "init must succeed\nstderr:{stderr}");

    let (exit, stdout, stderr) = run_cli(workspace, &["index", "--status", "--json"]);
    assert_eq!(
        exit, 0,
        "--status --json on a never-indexed workspace must succeed\nstdout:{stdout}\nstderr:{stderr}"
    );

    let rows: Vec<serde_json::Value> = serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
        panic!("--status --json stdout must parse as a JSON array: {e}\nstdout:\n{stdout}")
    });
    assert!(
        rows.is_empty(),
        "a never-indexed workspace must report zero generations, not a phantom bootstrap\nstdout:{stdout}"
    );

    // `init` itself creates an empty `.codanna/index` directory (unrelated
    // to `--status`), so assert on the absence of a `gen/` subdirectory --
    // that is what `IndexFacade::new`'s bootstrap-on-open would write.
    assert!(
        !crate::support::index_root(workspace).join("gen").exists(),
        "--status must never bootstrap a generation on a never-indexed workspace"
    );

    let (exit, stdout, stderr) = run_cli(workspace, &["index", "--status"]);
    assert_eq!(
        exit, 0,
        "--status on a never-indexed workspace must succeed\nstdout:{stdout}\nstderr:{stderr}"
    );
    assert!(
        stdout.contains("No index found."),
        "human-readable --status on a never-indexed workspace must report no index\nstdout:{stdout}"
    );
    assert!(
        !crate::support::index_root(workspace).join("gen").exists(),
        "--status must never bootstrap a generation on a never-indexed workspace"
    );
}

/// `--status` must never build or write the index: a second `--status`
/// call must leave the persisted index byte-identical.
#[test]
fn status_does_not_modify_the_persisted_index() {
    let temp = TempDir::new().expect("temp workspace");
    let workspace = temp.path();
    write_fixture(workspace);

    let (exit, stdout, stderr) = run_cli(workspace, &["index", "src", "--no-progress"]);
    assert_eq!(
        exit, 0,
        "normal index run must succeed\nstdout:{stdout}\nstderr:{stderr}"
    );

    let meta = crate::support::index_meta_path(workspace);
    let meta_before = std::fs::read(&meta).expect("read index.meta");
    let entries_before = crate::support::index_dir_entries(workspace);

    let (exit, stdout, stderr) = run_cli(workspace, &["index", "--status"]);
    assert_eq!(
        exit, 0,
        "--status must succeed\nstdout:{stdout}\nstderr:{stderr}"
    );

    let meta_after = std::fs::read(&meta).expect("read index.meta");
    assert_eq!(
        meta_before, meta_after,
        "--status must never modify index.meta"
    );
    assert_eq!(
        entries_before,
        crate::support::index_dir_entries(workspace),
        "--status must never add, remove, or modify index directory entries"
    );
}

/// `--status` on a legacy flat layout (pre-generations) must stay
/// read-only: it reports the layout as not yet migrated and leaves the
/// migration to the first command that actually opens the index.
#[test]
fn status_on_a_legacy_flat_layout_is_read_only_and_says_so() {
    let temp = TempDir::new().expect("temp workspace");
    let workspace = temp.path();
    write_fixture(workspace);

    let (exit, stdout, stderr) = run_cli(workspace, &["index", "src", "--no-progress"]);
    assert_eq!(
        exit, 0,
        "normal index run must succeed\nstdout:{stdout}\nstderr:{stderr}"
    );

    // Un-migrate by hand: move the current generation's artifacts back up
    // to the index root and drop the pointer, reproducing what an older
    // binary leaves behind.
    let root = crate::support::index_root(workspace);
    let gen_dir = crate::support::current_generation_dir(workspace);
    for name in ["tantivy", "semantic", "index.meta"] {
        let from = gen_dir.join(name);
        if from.exists() {
            std::fs::rename(&from, root.join(name)).expect("un-migrate artifact");
        }
    }
    std::fs::remove_file(root.join("current")).expect("drop current pointer");
    std::fs::remove_dir_all(root.join("gen")).expect("drop gen/");
    assert!(
        root.join("tantivy").join("meta.json").is_file(),
        "fixture precondition: a flat layout"
    );

    let (exit, stdout, stderr) = run_cli(workspace, &["index", "--status"]);
    assert_eq!(
        exit, 0,
        "--status on a flat layout must succeed\nstdout:{stdout}\nstderr:{stderr}"
    );
    assert!(
        stdout.contains("Legacy flat index layout"),
        "--status must report the unmigrated layout\nstdout:{stdout}"
    );
    assert!(
        !root.join("current").exists() && root.join("tantivy").join("meta.json").is_file(),
        "--status must not migrate the flat layout"
    );

    // The first command that opens the index migrates it.
    let (exit, stdout, stderr) = run_cli(workspace, &["retrieve", "symbol", "status_gate_target"]);
    assert_eq!(
        exit, 0,
        "retrieve must open (and migrate) the index\nstdout:{stdout}\nstderr:{stderr}"
    );
    assert!(
        root.join("current").is_file() && !root.join("tantivy").exists(),
        "opening the index must migrate the flat layout into gen/"
    );

    let (exit, stdout, stderr) = run_cli(workspace, &["index", "--status", "--json"]);
    assert_eq!(exit, 0, "stdout:{stdout}\nstderr:{stderr}");
    let rows: Vec<serde_json::Value> =
        serde_json::from_str(stdout.trim()).expect("--status --json parses as an array");
    assert_eq!(rows.len(), 1, "one migrated generation\nstdout:{stdout}");
    assert_eq!(rows[0]["state"], "current");
}
