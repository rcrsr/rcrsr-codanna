//! `codanna index --force` builds a fresh generation and publishes it
//! alongside the pre-force generation, rather than clearing the existing
//! generation in place. The pre-force generation must survive as
//! `Previous`, not be deleted -- it is only reclaimed later by `--gc`
//! (subject to `keep_previous`), giving `--rollback` something to restore.

use std::path::Path;

use crate::support::run_cli;

fn write_fixture(workspace: &Path) {
    let src = workspace.join("src");
    std::fs::create_dir_all(&src).expect("create src dir");
    std::fs::write(
        src.join("alpha.rs"),
        r#"
pub fn force_generation_target() -> i32 {
    1
}
"#,
    )
    .expect("write fixture");
}

#[test]
fn force_retains_previous_generation_as_previous_state() {
    let temp = tempfile::TempDir::new().expect("temp workspace");
    let workspace = temp.path();
    write_fixture(workspace);

    let (exit, stdout, stderr) = run_cli(workspace, &["index", "src", "--no-progress"]);
    assert_eq!(
        exit, 0,
        "initial index must succeed\nstdout:{stdout}\nstderr:{stderr}"
    );

    let root = crate::support::index_root(workspace);
    let pre_force_current = std::fs::read_to_string(root.join("current"))
        .expect("read current pointer after initial index")
        .trim()
        .to_string();

    let (exit, stdout, stderr) = run_cli(workspace, &["index", "--force", "--no-progress"]);
    assert_eq!(
        exit, 0,
        "force reindex must succeed\nstdout:{stdout}\nstderr:{stderr}"
    );
    assert!(
        stdout.contains("Index saved to"),
        "force rebuild must index and save:\nstdout:{stdout}\nstderr:{stderr}"
    );

    let (exit, stdout, stderr) = run_cli(workspace, &["index", "--status", "--json"]);
    assert_eq!(exit, 0, "stdout:{stdout}\nstderr:{stderr}");
    let rows: Vec<serde_json::Value> =
        serde_json::from_str(stdout.trim()).expect("--status --json parses as an array");

    let pre_force_row = rows
        .iter()
        .find(|row| row["id"] == pre_force_current)
        .unwrap_or_else(|| {
            panic!(
                "the pre-force generation ({pre_force_current}) must still be listed\nrows:{rows:?}"
            )
        });
    assert_eq!(
        pre_force_row["state"], "previous",
        "the pre-force generation must be Previous, not deleted\nrows:{rows:?}"
    );

    let current_row = rows
        .iter()
        .find(|row| row["state"] == "current")
        .expect("exactly one generation must be current after force");
    assert_ne!(
        current_row["id"], pre_force_current,
        "force must publish a new generation distinct from the pre-force one"
    );
}
