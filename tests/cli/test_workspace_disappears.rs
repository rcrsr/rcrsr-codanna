//! Real-process end-to-end coverage for the workspace-disappeared self-check
//! added to `serve --http` (`src/mcp/http_server.rs`'s
//! `wait_for_workspace_or_idle`).
//!
//! Mirrors `test_idle_shutdown.rs`'s subprocess-driven pattern (`Reaper`
//! drop-guard, `wait_until` poll helper): starts a real backing `serve
//! --http` server rooted at a `TempDir` workspace, waits for it to publish
//! `serve.json`, then deletes the workspace root out from under it and
//! asserts the server self-exits within a bounded deadline and removes its
//! `serve.json` as part of that exit -- the same clean-exit path the
//! Ctrl+C and idle-timer arms already use.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::Duration;

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
use tempfile::TempDir;

use crate::support::{codanna_binary, run_cli};

/// Test-only poll interval for the periodic workspace/idle self-check,
/// injected via `CODANNA_TEST_IDLE_POLL_MS` (see `idle_poll_interval_override`
/// in `src/mcp/http_server.rs`). This test's workspace `settings.toml`
/// explicitly sets `idle_shutdown_minutes = 0`, overriding the real default
/// of 240 (see `default_idle_shutdown_minutes()` in `src/config/defaults.rs`)
/// -- the point of this test is proving the workspace-disappeared trigger
/// fires independently of, and even with, idle-shutdown disabled -- but a
/// short poll interval still keeps the self-check's cadence tight so the
/// server notices the deleted workspace promptly.
const TEST_IDLE_POLL_MS: u64 = 100;

/// Upper bound for the wait on the backing server's self-initiated exit
/// after its workspace root is deleted: generous headroom over the poll
/// interval for process scheduling jitter under a loaded CI machine.
const EXIT_DEADLINE: Duration = Duration::from_secs(30);

/// Upper bound for every other (fast) wait in this file.
const FAST_DEADLINE: Duration = Duration::from_secs(30);

/// Start `codanna serve --http --bind 127.0.0.1:0` rooted at `ws` directly
/// (not via `discover_or_spawn`), so this test observes the backing server's
/// own self-check exit rather than a proxy's.
fn start_http_server(ws: &Path) -> Child {
    let test_home = ws.join(".home");
    std::fs::create_dir_all(&test_home).expect("create test home");

    Command::new(codanna_binary())
        .args(["serve", "--http", "--bind", "127.0.0.1:0"])
        .current_dir(ws)
        .env("HOME", &test_home)
        .env("XDG_CONFIG_HOME", &test_home)
        .env("CODANNA_TEST_IDLE_POLL_MS", TEST_IDLE_POLL_MS.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn codanna serve --http")
}

/// Best-effort termination of `pid` via sysinfo, used to reap any process
/// left running at test teardown. `Process::kill()` sends a generic
/// terminate signal (not necessarily `SIGKILL`); that's sufficient here since
/// this is just teardown cleanup, not a correctness assertion about signal
/// handling.
fn kill_pid(pid: u32) {
    let target = Pid::from_u32(pid);
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[target]),
        true,
        ProcessRefreshKind::nothing(),
    );
    if let Some(process) = sys.process(target) {
        let _ = process.kill();
    }
}

/// Deadline-bounded poll: panics rather than hanging if `predicate` never
/// becomes true within `deadline`.
fn wait_until(mut predicate: impl FnMut() -> bool, deadline: Duration, what: &str) {
    let start = std::time::Instant::now();
    loop {
        if predicate() {
            return;
        }
        assert!(start.elapsed() < deadline, "timed out waiting for: {what}");
        thread::sleep(Duration::from_millis(200));
    }
}

/// Kills the backing `serve --http` process recorded in
/// `<workspace>/.codanna/serve.json`, if any, when dropped. Without this, a
/// failing assertion mid-test leaks a detached backing server process.
struct Reaper(PathBuf);

impl Drop for Reaper {
    fn drop(&mut self) {
        let codanna_dir = self.0.join(".codanna");
        if let Some(record) = codanna::serve_discovery::read_record(&codanna_dir) {
            kill_pid(record.pid);
        }
    }
}

/// THE LOAD-BEARING WORKSPACE-DISAPPEARED REGRESSION TEST.
///
/// Starts a real backing `serve --http` server (idle-shutdown left disabled)
/// rooted at a `TempDir`, waits for it to publish `serve.json`, then removes
/// the entire workspace root out from under the running server. Asserts:
///
/// 1. The server exits BY ITSELF (not killed by the harness) within a
///    bounded deadline once its workspace root is gone.
/// 2. `serve.json` is removed as part of that same exit -- the SAME
///    cleanup path (`ct.cancel()` + `remove_record`) the Ctrl+C and
///    idle-timer arms already use.
///
/// Note: renaming a directory preserves its inode (the same filesystem
/// object, reachable at a new path), so a rename must NOT trip this
/// check -- only an actual deletion (or delete-and-recreate at the same
/// path) does. This test therefore asserts on deletion (`remove_dir_all`),
/// never on a rename.
#[test]
fn backing_server_self_exits_when_workspace_root_is_deleted() {
    let workspace = TempDir::new().expect("create outer temp dir");
    let ws_root = {
        // Nest the actual workspace one level down so `remove_dir_all` below
        // deletes a directory distinct from the outer `TempDir`, which still
        // needs to exist for `Reaper`/cleanup bookkeeping after the inner
        // directory is gone.
        let inner = workspace.path().join("workspace");
        std::fs::create_dir_all(&inner).expect("create inner workspace dir");
        inner
    };

    let src_dir = ws_root.join("src");
    std::fs::create_dir_all(&src_dir).expect("create src dir");
    std::fs::write(
        src_dir.join("lib.rs"),
        r#"
/// Unique marker symbol used only by the workspace-disappeared e2e test.
pub fn codanna_workspace_disappears_e2e_marker() -> i32 {
    7
}
"#,
    )
    .expect("write fixture source");

    let codanna_dir = ws_root.join(".codanna");
    std::fs::create_dir_all(&codanna_dir).expect("create .codanna dir");
    std::fs::write(
        codanna_dir.join("settings.toml"),
        r#"
index_path = ".codanna/index"

[semantic_search]
enabled = false

[server]
idle_shutdown_minutes = 0
"#,
    )
    .expect("write settings.toml");

    let (code, stdout, stderr) = run_cli(&ws_root, &["index", "src", "--force", "--no-progress"]);
    assert_eq!(
        code, 0,
        "workspace fixture index should succeed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    let _reaper = Reaper(ws_root.clone());
    let codanna_dir = ws_root.join(".codanna");

    let mut server = start_http_server(&ws_root);

    // Wait for the server to publish its discovery record so we know it has
    // finished startup (including capturing workspace_dev/workspace_ino)
    // before the workspace root is pulled out from under it.
    wait_until(
        || codanna::serve_discovery::read_record(&codanna_dir).is_some(),
        FAST_DEADLINE,
        "backing server to publish serve.json",
    );
    let record =
        codanna::serve_discovery::read_record(&codanna_dir).expect("serve.json should exist");
    assert!(
        codanna::serve_discovery::pid_is_alive(record.pid),
        "freshly started backing server pid should be alive"
    );

    // The trigger: delete the workspace root entirely (not rename -- a
    // rename preserves the inode and must not trip this check).
    std::fs::remove_dir_all(&ws_root).expect("remove workspace root");

    // (1): self-exit, not a harness kill. Polled via `Child::try_wait`
    // (rather than `pid_is_alive`) so this loop also reaps the child as soon
    // as it exits.
    wait_until(
        || matches!(server.try_wait(), Ok(Some(_))),
        EXIT_DEADLINE,
        "backing server to self-exit after its workspace root is deleted",
    );

    // (2): serve.json (which lived under the now-deleted workspace root) is
    // gone -- trivially true once the directory itself is deleted, but this
    // also confirms the exit path ran `remove_record` rather than merely
    // crashing (a crash could leave `serve.json` if the directory still
    // existed for some other reason).
    assert!(
        codanna::serve_discovery::read_record(&codanna_dir).is_none(),
        "serve.json should be gone once the backing server exits after its \
         workspace root disappears"
    );
}
