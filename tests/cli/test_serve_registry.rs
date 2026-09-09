//! Real-process end-to-end coverage for the per-user server registry
//! (`src/serve_registry.rs`) and its CLI surface (`codanna serve
//! --list/--stop/--reap[/--force]`, `src/cli/commands/serve.rs`).
//!
//! Mirrors the subprocess-driven pattern established in
//! `test_idle_shutdown.rs`: a real `codanna serve --http` child is started
//! and observed/managed through the real CLI, not through direct calls into
//! `codanna::serve_registry`.
//!
//! Registry isolation between tests relies on the same mechanism already
//! used by every other subprocess test in this crate: `HOME`/
//! `XDG_CONFIG_HOME` are pointed at a per-test temp directory, and
//! `dirs::state_dir()`/`dirs::data_dir()` on Linux both derive from `$HOME`
//! (via `XDG_STATE_HOME`/`XDG_DATA_HOME` falling back to `$HOME/.local/...`),
//! so each test's registry directory is private to that test.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::Duration;

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, Signal, System};
use tempfile::TempDir;

use crate::support::codanna_binary;

const FAST_DEADLINE: Duration = Duration::from_secs(30);

/// Build a minimal indexed workspace, mirroring
/// `test_idle_shutdown.rs::prepare_idle_workspace` but without the idle
/// timer override (this suite manages servers via signals, not idle exit).
fn prepare_workspace() -> TempDir {
    let workspace = TempDir::new().expect("create temp workspace");

    let src_dir = workspace.path().join("src");
    std::fs::create_dir_all(&src_dir).expect("create src dir");
    std::fs::write(
        src_dir.join("lib.rs"),
        r#"
/// Unique marker symbol used only by the server-registry e2e test.
pub fn codanna_registry_e2e_marker() -> i32 {
    11
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

    let test_home = workspace.path().join(".home");
    std::fs::create_dir_all(&test_home).expect("create test home");

    let output = Command::new(codanna_binary())
        .args(["index", "src", "--force", "--no-progress"])
        .current_dir(workspace.path())
        .env("HOME", &test_home)
        .env("XDG_CONFIG_HOME", &test_home)
        .output()
        .expect("run codanna index");
    assert!(
        output.status.success(),
        "workspace fixture index should succeed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    workspace
}

/// Start `codanna serve --http --bind 127.0.0.1:0` rooted at `ws`, with
/// `HOME`/`XDG_CONFIG_HOME` pointed at `home` -- the shared per-test registry
/// root every helper in this file uses so all subprocesses (the server, and
/// every `codanna serve --list/--stop/--reap` invocation) see the same
/// registry directory.
fn start_http_server(ws: &Path, home: &Path) -> Child {
    Command::new(codanna_binary())
        .args(["serve", "--http", "--bind", "127.0.0.1:0"])
        .current_dir(ws)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn codanna serve --http")
}

/// Run `codanna serve <args>` to completion (a management op: `--list`,
/// `--stop`, or `--reap`, all of which return promptly rather than blocking
/// on a server loop), with the same `home` registry root as `start_http_server`.
fn run_serve_management(home: &Path, args: &[&str]) -> (i32, String, String) {
    let mut full_args = vec!["serve"];
    full_args.extend_from_slice(args);

    let output = Command::new(codanna_binary())
        .args(&full_args)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home)
        .output()
        .expect("run codanna serve management op");

    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}

/// Registry directory this test's `home` resolves to, mirroring
/// `serve_registry::registry_dir()`'s own resolution
/// (`dirs::state_dir().or_else(dirs::data_dir)/codanna/servers`) so the test
/// can inspect registry files directly without depending on the `codanna`
/// crate's internals being importable from an integration test binary.
fn registry_dir_for(home: &Path) -> PathBuf {
    // On Linux (the CI platform), `dirs::state_dir()` resolves to
    // `$XDG_STATE_HOME` or `$HOME/.local/state`; neither is set by these
    // tests, so it falls back to `$HOME/.local/state`.
    home.join(".local")
        .join("state")
        .join("codanna")
        .join("servers")
}

fn registry_file_exists(home: &Path, pid: u32) -> bool {
    registry_dir_for(home).join(format!("{pid}.json")).exists()
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
        thread::sleep(Duration::from_millis(100));
    }
}

/// Best-effort external termination of `pid`, used both for teardown and to
/// simulate an "unclean kill" (SIGKILL from outside codanna's own
/// self-deregistering shutdown paths) in the reap test below.
fn kill_pid_externally(pid: u32) {
    let target = Pid::from_u32(pid);
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[target]),
        true,
        ProcessRefreshKind::nothing(),
    );
    if let Some(process) = sys.process(target) {
        let _ = process.kill_with(Signal::Kill);
    }
}

fn pid_alive(pid: u32) -> bool {
    let target = Pid::from_u32(pid);
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[target]),
        true,
        ProcessRefreshKind::nothing(),
    );
    sys.process(target).is_some()
}

/// Kills the given pid (best-effort) when dropped, so a failing assertion
/// mid-test never leaks a detached backing server process.
struct Reaper(u32);

impl Drop for Reaper {
    fn drop(&mut self) {
        kill_pid_externally(self.0);
    }
}

/// `codanna serve --list` must show a freshly started backing server as
/// `healthy` once it has published its registry entry.
#[test]
fn list_shows_healthy_running_server() {
    let workspace = prepare_workspace();
    let home = workspace.path().join(".home");
    std::fs::create_dir_all(&home).expect("create test home");

    let mut server = start_http_server(workspace.path(), &home);
    let pid = server.id();
    let _reaper = Reaper(pid);

    wait_until(
        || registry_file_exists(&home, pid),
        FAST_DEADLINE,
        "backing server to publish its registry entry",
    );

    let (code, stdout, stderr) = run_serve_management(&home, &["--list"]);
    assert_eq!(
        code, 0,
        "serve --list should succeed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains(&pid.to_string()),
        "serve --list output should mention pid {pid}:\n{stdout}"
    );
    assert!(
        stdout.to_lowercase().contains("healthy"),
        "serve --list output should mark the server healthy:\n{stdout}"
    );

    let _ = server.kill();
    let _ = server.wait();
}

/// `codanna serve --stop <pid>` sends SIGTERM; the server observes it,
/// exits, and removes its own registry entry (the same self-deregistration
/// path Ctrl+C and idle-shutdown already use for `serve.json`).
#[test]
fn stop_sends_sigterm_and_server_self_deregisters() {
    let workspace = prepare_workspace();
    let home = workspace.path().join(".home");
    std::fs::create_dir_all(&home).expect("create test home");

    let mut server = start_http_server(workspace.path(), &home);
    let pid = server.id();
    let _reaper = Reaper(pid);

    wait_until(
        || registry_file_exists(&home, pid),
        FAST_DEADLINE,
        "backing server to publish its registry entry",
    );

    let (code, stdout, stderr) = run_serve_management(&home, &["--stop", &pid.to_string()]);
    assert_eq!(
        code, 0,
        "serve --stop should report success\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    wait_until(
        || matches!(server.try_wait(), Ok(Some(_))),
        FAST_DEADLINE,
        "server to self-exit after SIGTERM",
    );

    assert!(
        !registry_file_exists(&home, pid),
        "registry file should be removed once the server self-deregisters after SIGTERM"
    );
}

/// `codanna serve --stop <pid> --force` sends SIGKILL. The server dies, but
/// -- unlike the plain SIGTERM case -- it never gets a chance to run its own
/// shutdown path, so its registry entry may remain on disk.
#[test]
fn stop_with_force_sends_sigkill_and_may_leave_registry_entry() {
    let workspace = prepare_workspace();
    let home = workspace.path().join(".home");
    std::fs::create_dir_all(&home).expect("create test home");

    let mut server = start_http_server(workspace.path(), &home);
    let pid = server.id();
    let _reaper = Reaper(pid);

    wait_until(
        || registry_file_exists(&home, pid),
        FAST_DEADLINE,
        "backing server to publish its registry entry",
    );

    let (code, stdout, stderr) =
        run_serve_management(&home, &["--stop", &pid.to_string(), "--force"]);
    assert_eq!(
        code, 0,
        "serve --stop --force should report success\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    wait_until(
        || matches!(server.try_wait(), Ok(Some(_))),
        FAST_DEADLINE,
        "server to die after SIGKILL",
    );

    // The load-bearing assertion for this test: SIGKILL gives the process no
    // opportunity to run its shutdown select! arms (which is what removes
    // the registry entry on a graceful stop), so the file is very likely
    // still present. This is documented, expected behavior -- `--reap` is
    // the tool for pruning it -- so this test asserts the fact rather than
    // treating it as a bug.
    assert!(
        registry_file_exists(&home, pid),
        "a SIGKILLed server cannot self-deregister; its registry entry should remain until --reap"
    );
}

/// A server killed uncleanly from outside codanna's own shutdown paths (e.g.
/// an operator or supervisor sending SIGKILL directly, bypassing `codanna
/// serve --stop` entirely) leaves a stale registry file. `--list` must skip
/// it (without deleting it), and `--reap` must then remove it.
#[test]
fn reap_prunes_stale_entry_that_list_already_skipped() {
    let workspace = prepare_workspace();
    let home = workspace.path().join(".home");
    std::fs::create_dir_all(&home).expect("create test home");

    let mut server = start_http_server(workspace.path(), &home);
    let pid = server.id();

    wait_until(
        || registry_file_exists(&home, pid),
        FAST_DEADLINE,
        "backing server to publish its registry entry",
    );

    // Simulate an unclean external kill (matching the best-effort,
    // test-only `kill_pid` pattern used by `test_idle_shutdown.rs`), rather
    // than going through `codanna serve --stop`.
    kill_pid_externally(pid);
    wait_until(
        || matches!(server.try_wait(), Ok(Some(_))),
        FAST_DEADLINE,
        "externally killed server to be reaped by try_wait",
    );
    wait_until(
        || !pid_alive(pid),
        FAST_DEADLINE,
        "externally killed server's pid to fully disappear",
    );

    assert!(
        registry_file_exists(&home, pid),
        "an uncleanly killed server must leave its registry file behind"
    );

    // --list must skip the stale entry without deleting it.
    let (list_code, list_stdout, list_stderr) = run_serve_management(&home, &["--list"]);
    assert_eq!(
        list_code, 0,
        "serve --list should succeed\nstdout:\n{list_stdout}\nstderr:\n{list_stderr}"
    );
    assert!(
        !list_stdout.contains(&pid.to_string()),
        "serve --list must not print a dead-pid entry:\n{list_stdout}"
    );
    assert!(
        registry_file_exists(&home, pid),
        "serve --list must not delete a stale entry -- that is --reap's job"
    );

    // --reap must then remove it.
    let (reap_code, reap_stdout, reap_stderr) = run_serve_management(&home, &["--reap"]);
    assert_eq!(
        reap_code, 0,
        "serve --reap should succeed\nstdout:\n{reap_stdout}\nstderr:\n{reap_stderr}"
    );
    assert!(
        !registry_file_exists(&home, pid),
        "serve --reap should remove the stale registry entry"
    );
}
