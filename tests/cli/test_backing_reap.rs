//! Regression coverage for `spawn_detached`'s reaper (`src/serve_discovery.rs`).
//!
//! `spawn_detached` launches a detached backing `serve --http` process as a
//! child of the spawning process (here, a `serve --proxy` process started by
//! this test). If the returned `Child` handle is ever dropped without a
//! `.wait()` on it, an exited backing server becomes a `<defunct>` zombie
//! under the spawning process for as long as that process stays alive --
//! which, for `serve --proxy`, is the lifetime of the whole session.
//!
//! DISCRIMINATION NOTE: a bare "kill the backing server, then poll its
//! status" test is NOT sufficient here. Without an active MCP session, this
//! test's `serve --proxy` process has nothing to keep it running once its
//! only upstream connection dies, so it exits almost immediately after the
//! kill; the backing server's zombie is then reparented to init/a subreaper
//! and reaped by the OS regardless of whether `spawn_detached`'s own reaper
//! thread exists. That made an earlier version of this test pass identically
//! whether or not the `src/serve_discovery.rs` fix was applied. To close that
//! gap, this test drives a REAL, connected `rmcp` stdio client through the
//! proxy (mirroring `proxy_revives_dead_upstream_mid_session` in
//! `test_serve_proxy_discovery.rs`) and, after killing the backing server,
//! makes the proxy's demonstrated survival AND successful revive-and-retry a
//! HARD PRECONDITION -- via a real tool call that must succeed -- before
//! ever inspecting the killed pid's process status. That closes the loophole:
//! the proxy (the backing server's actual OS parent) is proven alive for the
//! whole assertion window, so only `spawn_detached`'s own reaper thread can
//! explain the killed pid not lingering as a zombie under it.
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use rmcp::service::{RoleClient, RunningService, ServiceExt};
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use sysinfo::{Pid, ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, System};
use tempfile::TempDir;

use crate::support::{codanna_binary, run_cli};

/// Upper bound for the wait on the backing server pid being reaped rather
/// than left as a zombie: generous enough for `spawn_detached`'s background
/// reaper thread to run and for CI scheduling jitter, mirroring
/// `test_idle_shutdown.rs`'s `IDLE_EXIT_DEADLINE`.
const REAP_DEADLINE: Duration = Duration::from_secs(30);

/// Upper bound for every other (fast) wait in this file, including the
/// hard-precondition revive-and-retry tool call.
const FAST_DEADLINE: Duration = Duration::from_secs(30);

/// Build a minimal indexed workspace, mirroring
/// `test_idle_shutdown.rs`'s `prepare_idle_workspace` (semantic search
/// disabled so the spawned backing server starts quickly). No idle-shutdown
/// config is needed here: this test triggers the backing server's exit
/// explicitly via a kill signal rather than waiting on an idle timer.
fn prepare_workspace() -> TempDir {
    let workspace = TempDir::new().expect("create temp workspace");

    let src_dir = workspace.path().join("src");
    std::fs::create_dir_all(&src_dir).expect("create src dir");
    std::fs::write(
        src_dir.join("lib.rs"),
        r#"
/// Unique marker symbol used only by the backing-reap e2e test.
pub fn codanna_backing_reap_e2e_marker() -> i32 {
    7
}
"#,
    )
    .expect("write fixture source");

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

    let (code, stdout, stderr) = run_cli(
        workspace.path(),
        &["index", "src", "--force", "--no-progress"],
    );
    assert_eq!(
        code, 0,
        "workspace fixture index should succeed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    workspace
}

/// Connect a real `rmcp` stdio client to `codanna serve --proxy` rooted at
/// `ws`. This exercises the real `discover_or_spawn` -> `spawn_detached`
/// path (`src/serve_discovery.rs`), making the proxy process the parent of
/// the detached backing server -- exactly the process whose zombie-reaping
/// behavior this test targets -- while also giving this test an active
/// session it can use later to prove the proxy is still alive and
/// functioning, rather than merely inferring liveness from a race.
///
/// Mirrors `connect_proxy_client` in `test_serve_proxy_discovery.rs`.
async fn connect_proxy_client(ws: &Path) -> RunningService<RoleClient, ()> {
    let test_home = ws.join(".home");
    std::fs::create_dir_all(&test_home).expect("create test home");

    let ws = ws.to_path_buf();
    ().serve(
        TokioChildProcess::new(
            tokio::process::Command::new(codanna_binary()).configure(|cmd| {
                cmd.args(["serve", "--proxy"])
                    .current_dir(&ws)
                    .env("HOME", &test_home)
                    .env("XDG_CONFIG_HOME", &test_home);
            }),
        )
        .expect("spawn codanna serve --proxy as an rmcp child transport"),
    )
    .await
    .expect("rmcp client should complete the stdio initialize handshake with the proxy")
}

/// Best-effort termination of `pid` via sysinfo, used both to trigger the
/// backing server's exit and to reap any process left running at test
/// teardown. `Process::kill()` sends a generic terminate signal (not
/// necessarily `SIGKILL`); that's sufficient here since this is just
/// teardown cleanup / exit-triggering, not a correctness assertion about
/// signal handling.
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

/// Look up `pid`'s current `ProcessStatus` via sysinfo, or `None` if the pid
/// is no longer present in the process table at all (i.e. it has been fully
/// reaped and removed, not merely transitioned out of `Zombie`).
fn process_status(pid: u32) -> Option<ProcessStatus> {
    let target = Pid::from_u32(pid);
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[target]),
        true,
        ProcessRefreshKind::nothing(),
    );
    sys.process(target).map(|process| process.status())
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
/// failing assertion mid-test leaks a detached background process.
struct Reaper(PathBuf);

impl Drop for Reaper {
    fn drop(&mut self) {
        let codanna_dir = self.0.join(".codanna");
        if let Some(record) = codanna::serve_discovery::read_record(&codanna_dir) {
            kill_pid(record.pid);
        }
    }
}

/// THE LOAD-BEARING ZOMBIE-REAP REGRESSION TEST.
///
/// Starts a real, connected `rmcp` client against `codanna serve --proxy`,
/// which spawns a real detached backing `serve --http` server via
/// `spawn_detached` (`src/serve_discovery.rs`). Kills the backing server
/// directly, then -- as a HARD PRECONDITION, not an inference -- makes a
/// second tool call on the SAME client connection that must succeed via the
/// proxy's revive-and-retry path (`DelegatingProxyHandler::delegate` in
/// `src/mcp/proxy.rs`), proving the proxy (the backing server's real OS
/// parent) is still alive and functioning for the whole assertion window.
/// Only then does it assert the killed pid is reaped by that demonstrably
/// live parent rather than left as a `<defunct>` zombie -- the exact failure
/// mode of dropping the `Child` handle from `spawn_detached` without ever
/// calling `.wait()` on it.
#[tokio::test]
async fn killed_backing_server_is_reaped_not_left_as_zombie() {
    let workspace = prepare_workspace();
    let _reaper = Reaper(workspace.path().to_path_buf());
    let codanna_dir = workspace.path().join(".codanna");

    let client = tokio::time::timeout(FAST_DEADLINE, connect_proxy_client(workspace.path()))
        .await
        .expect("proxy client should connect within the deadline");

    // First call: proves the initial connection is live and lets the
    // discovery record converge before anything is killed.
    let tools_before = tokio::time::timeout(FAST_DEADLINE, client.list_tools(Default::default()))
        .await
        .expect("first list_tools should complete within the deadline")
        .expect("first list_tools should succeed through the freshly-dialed upstream");
    assert!(
        !tools_before.tools.is_empty(),
        "first list_tools should relay a non-empty tool list from the live upstream"
    );

    let record_before = codanna::serve_discovery::read_record(&codanna_dir)
        .expect("serve.json should exist once the proxy has converged on a backing server");
    let backing_pid = record_before.pid;
    assert!(
        codanna::serve_discovery::pid_is_alive(backing_pid),
        "spawned backing server pid should be alive before it is killed"
    );

    kill_pid(backing_pid);

    // HARD PRECONDITION: a tool CALL (not `list_tools`, which rmcp 3.x serves
    // from the client-side cache and would never reach the dead backing) on
    // the SAME client connection must succeed via the proxy's
    // revive-and-retry path. If the proxy itself died (e.g. because this
    // test lacked an active session, as an earlier version of it did), this
    // call fails/times out and the test fails loudly here -- it can no
    // longer silently fall through to a trivially-passing zombie check.
    let revived_call = tokio::time::timeout(
        FAST_DEADLINE,
        client.call_tool(rmcp::model::CallToolRequestParams::new("get_index_info")),
    )
    .await
    .expect(
        "a tool call through the proxy must complete within the deadline after the backing \
         server is killed -- a timeout here means the proxy itself died instead of reviving, \
         which would make any later zombie-reap check meaningless",
    )
    .expect(
        "a tool call through the proxy must succeed via revive-and-retry after the backing \
         server is killed -- an error here means the proxy did not survive/revive, which would \
         make any later zombie-reap check meaningless",
    );
    assert_ne!(
        revived_call.is_error,
        Some(true),
        "the post-kill tool call must not be an application-level error result, got: \
         {revived_call:?}"
    );

    let record_after = codanna::serve_discovery::read_record(&codanna_dir)
        .expect("serve.json should exist again after the proxy revived its upstream");
    assert_ne!(
        record_after.pid, backing_pid,
        "the revived backing server must be a genuinely new process, not the killed one -- \
         confirms the proxy actually respawned rather than this call somehow reaching a still-live \
         old process"
    );

    // The proxy (backing_pid's real OS parent) has just been proven alive
    // and functioning by the successful revive-and-retry call above. Any
    // zombie state observed for `backing_pid` from here on can only be
    // explained by `spawn_detached`'s reaper thread, not by the proxy itself
    // having exited and orphaned it to init.
    wait_until(
        || !matches!(process_status(backing_pid), Some(ProcessStatus::Zombie)),
        REAP_DEADLINE,
        "killed backing server pid to be reaped by its still-live parent rather than left as a \
         zombie",
    );

    // Give the reaper thread a further short grace window, then assert the
    // settled state is still not a zombie (transient re-appearance as
    // Zombie immediately after exit, before the reaper thread's `wait()`
    // call runs, is expected and not itself a failure -- what matters is
    // that it does not persist).
    thread::sleep(Duration::from_millis(500));
    assert!(
        !matches!(process_status(backing_pid), Some(ProcessStatus::Zombie)),
        "backing server pid must not remain a zombie once spawn_detached's reaper thread \
         has had time to run"
    );

    client
        .cancel()
        .await
        .expect("proxy client should shut down cleanly");
}
