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

use std::io::BufRead;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
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

/// Run `codanna ls` to completion, with the same `home` registry root as
/// `start_http_server`/`run_serve_management`, so it observes the same
/// registered/unknown state.
fn run_ls(home: &Path) -> (i32, String, String) {
    let output = Command::new(codanna_binary())
        .arg("ls")
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home)
        .output()
        .expect("run codanna ls");

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

/// Suspend `pid` with `SIGSTOP` (best-effort): the process stays alive (and
/// keeps looking like a `codanna serve` process, so `entry_is_stale` still
/// reports it as live/not-stale) but stops responding to `SIGTERM` until
/// resumed, letting a test force a genuine in-loop signal-then-wait timeout
/// in `stop_all_servers` without pre-filtering the target out via
/// `entry_is_stale`. `SIGKILL` (used by `Reaper`'s teardown) still terminates
/// a stopped process immediately, since `SIGKILL` cannot be blocked or
/// ignored even while stopped.
///
/// `SIGSTOP` is Unix-specific, so this helper (and the test that relies on
/// it to force a genuine in-loop timeout) is gated to Unix targets.
#[cfg(unix)]
fn stop_pid_externally(pid: u32) {
    let target = Pid::from_u32(pid);
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[target]),
        true,
        ProcessRefreshKind::nothing(),
    );
    if let Some(process) = sys.process(target) {
        let _ = process.kill_with(Signal::Stop);
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

/// A real, running `codanna serve` process whose registry entry lives under
/// a *different* per-user registry root ("unknown" from the perspective of the
/// `home` used for the `--stop` invocation below): the pid genuinely
/// `looks_like_codanna_serve`, but no entry for it exists in the registry
/// `--stop` is about to consult.
struct UnknownServer {
    _workspace: TempDir,
    _home: TempDir,
    server: Child,
}

fn start_unknown_server() -> UnknownServer {
    let workspace = prepare_workspace();
    let home_dir = TempDir::new().expect("create unknown-server registry home");
    let server = start_http_server(workspace.path(), home_dir.path());
    UnknownServer {
        _workspace: workspace,
        _home: home_dir,
        server,
    }
}

/// `codanna serve --stop <pid>` no longer gates a numeric selector on
/// registry membership: a pid that is not registered under the consulted
/// registry root still stops, as long as it independently looks like a
/// `codanna serve` process, and the CLI prints a one-line note that it was
/// unregistered.
#[test]
fn stop_of_unregistered_codanna_pid_succeeds_with_note() {
    let home = TempDir::new().expect("create test home");

    let mut unknown = start_unknown_server();
    let pid = unknown.server.id();
    let _reaper = Reaper(pid);

    wait_until(
        || pid_alive(pid),
        FAST_DEADLINE,
        "unregistered server to be running",
    );
    assert!(
        !registry_file_exists(home.path(), pid),
        "unregistered server's registry entry must not be visible under the --stop registry root"
    );

    let (code, stdout, stderr) = run_serve_management(home.path(), &["--stop", &pid.to_string()]);
    assert_eq!(
        code, 0,
        "serve --stop should accept an unregistered codanna-serve pid\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("was not registered; stopping anyway")
            || stderr.contains("was not registered; stopping anyway"),
        "serve --stop should note that the resolved pid was unregistered\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    // `unknown.server` is this test's own child handle: the OS keeps a
    // terminated child as a zombie -- which `pid_alive` deliberately still
    // treats as "alive" -- until its parent reaps it via `wait`/`try_wait`,
    // so exit must be observed through the owning `Child`, exactly like
    // `stop_sends_sigterm_and_server_self_deregisters` above.
    wait_until(
        || matches!(unknown.server.try_wait(), Ok(Some(_))),
        FAST_DEADLINE,
        "unregistered server to exit after --stop",
    );
}

/// The identity check remains the sole safety net for a numeric `--stop`
/// selector: a pid that is not a codanna process at all must still be
/// refused, registered or not.
#[test]
fn stop_still_refuses_non_codanna_pid() {
    let home = TempDir::new().expect("create test home");

    let mut non_codanna = Command::new("sleep")
        .arg("60")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn non-codanna process");
    let pid = non_codanna.id();
    let _reaper = Reaper(pid);

    wait_until(
        || pid_alive(pid),
        FAST_DEADLINE,
        "non-codanna process to be running",
    );

    let (code, stdout, stderr) = run_serve_management(home.path(), &["--stop", &pid.to_string()]);
    assert_eq!(
        code, 1,
        "serve --stop must still refuse a non-codanna pid\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        pid_alive(pid),
        "a refused --stop must never signal the target process"
    );

    let _ = non_codanna.kill();
    let _ = non_codanna.wait();
}

/// `codanna serve --list` is deprecated in favor of `codanna ls`: it must
/// print a one-line deprecation notice to stderr, and its stdout must carry
/// the exact same registered-server row content `codanna ls` produces for an
/// identical registry state, because it now delegates entirely into `ls.rs`'s
/// listing logic rather than duplicating a table-building loop of its own.
#[test]
fn list_prints_deprecation_notice_and_delegates_to_ls() {
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

    let (list_code, list_stdout, list_stderr) = run_serve_management(&home, &["--list"]);
    assert_eq!(
        list_code, 0,
        "serve --list should succeed\nstdout:\n{list_stdout}\nstderr:\n{list_stderr}"
    );
    assert!(
        list_stderr.to_lowercase().contains("deprecat")
            && list_stderr.contains("codanna ls")
            && list_stderr.contains("serve --list"),
        "serve --list must print a deprecation notice mentioning both itself and `codanna ls` to stderr:\n{list_stderr}"
    );

    let (ls_code, ls_stdout, ls_stderr) = run_ls(&home);
    assert_eq!(
        ls_code, 0,
        "codanna ls should succeed\nstdout:\n{ls_stdout}\nstderr:\n{ls_stderr}"
    );

    // Same registered-server row content: pid, "server" kind, "registered"
    // source, and "healthy" status must appear identically in both outputs,
    // proving `serve --list` delegated into the same listing logic rather
    // than rebuilding its own (now-divergent) table.
    for needle in [pid.to_string().as_str(), "server", "registered", "healthy"] {
        assert!(
            list_stdout.contains(needle),
            "serve --list output missing {needle:?}:\n{list_stdout}"
        );
        assert!(
            ls_stdout.contains(needle),
            "codanna ls output missing {needle:?}:\n{ls_stdout}"
        );
    }
    // Compare only the registered rows (source == "registered"), not the
    // full output: unknown rows come from a live, machine-wide process-table
    // scan taken independently by each invocation, so on a host with other
    // unknown `codanna serve` processes running concurrently that set can
    // legitimately differ between the two separate subprocess calls above.
    // The registered rows, backed by this test's own isolated registry
    // root, must match exactly regardless.
    fn registered_rows(stdout: &str) -> Vec<&str> {
        stdout
            .lines()
            .filter(|line| line.contains("registered"))
            .collect()
    }
    assert_eq!(
        registered_rows(&list_stdout),
        registered_rows(&ls_stdout),
        "serve --list's registered-server rows must match codanna ls's exactly\nserve --list:\n{list_stdout}\ncodanna ls:\n{ls_stdout}"
    );

    let _ = server.kill();
    let _ = server.wait();
}

/// Start `codanna serve --proxy` rooted at `ws`, registered under the
/// per-user registry `home` resolves to. Mirrors `test_ls.rs::start_proxy`:
/// stdin is left piped and open, since a closed stdin makes the stdio proxy
/// transport exit immediately.
fn start_proxy(ws: &Path, home: &Path) -> Child {
    Command::new(codanna_binary())
        .args(["serve", "--proxy"])
        .current_dir(ws)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn codanna serve --proxy")
}

/// Parse `Proxy: delegating to backing HTTP server at 127.0.0.1:{port} (pid
/// {pid})` (see `src/mcp/proxy.rs`'s `serve_proxy`) out of one stderr line.
/// Mirrors `test_ls.rs::parse_delegating_line`.
fn parse_delegating_line(line: &str) -> Option<(u32, u16)> {
    let after_addr = line.split("127.0.0.1:").nth(1)?;
    let port: u16 = after_addr
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .ok()?;

    let after_pid = line.split("(pid ").nth(1)?;
    let pid: u32 = after_pid
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .ok()?;

    Some((pid, port))
}

/// Block (deadline-bounded) until `child`'s stderr prints the delegation
/// line, returning the backing server's `(pid, port)`. Mirrors
/// `test_ls.rs::await_upstream`.
fn await_upstream(child: &mut Child) -> (u32, u16) {
    let stderr = child
        .stderr
        .take()
        .expect("proxy child stderr should be piped");

    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let reader = BufReader::new(stderr);
        for line in reader.lines() {
            let Ok(line) = line else { break };
            if let Some(found) = parse_delegating_line(&line) {
                let _ = tx.send(found);
                return;
            }
        }
    });

    rx.recv_timeout(FAST_DEADLINE)
        .expect("proxy should report the backing HTTP server within the deadline")
}

/// `codanna serve --stop-all` must stop every registered server, attempting
/// (and reporting on) every target rather than stopping after the first.
#[test]
fn stop_all_stops_all_registered_servers() {
    let workspace = prepare_workspace();
    let home = workspace.path().join(".home");
    std::fs::create_dir_all(&home).expect("create test home");

    let mut server_a = start_http_server(workspace.path(), &home);
    let pid_a = server_a.id();
    let _reaper_a = Reaper(pid_a);

    let mut server_b = start_http_server(workspace.path(), &home);
    let pid_b = server_b.id();
    let _reaper_b = Reaper(pid_b);

    wait_until(
        || registry_file_exists(&home, pid_a) && registry_file_exists(&home, pid_b),
        FAST_DEADLINE,
        "both backing servers to publish their registry entries",
    );

    let (code, stdout, stderr) = run_serve_management(&home, &["--stop-all"]);
    assert_eq!(
        code, 0,
        "serve --stop-all should report success when every target stopped\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    for pid in [pid_a, pid_b] {
        assert!(
            stdout.contains(&pid.to_string()) || stderr.contains(&pid.to_string()),
            "serve --stop-all output should mention pid {pid}\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
    }

    wait_until(
        || matches!(server_a.try_wait(), Ok(Some(_))),
        FAST_DEADLINE,
        "server A to self-exit after --stop-all",
    );
    wait_until(
        || matches!(server_b.try_wait(), Ok(Some(_))),
        FAST_DEADLINE,
        "server B to self-exit after --stop-all",
    );
    assert!(
        !registry_file_exists(&home, pid_a),
        "server A should self-deregister once stopped by --stop-all"
    );
    assert!(
        !registry_file_exists(&home, pid_b),
        "server B should self-deregister once stopped by --stop-all"
    );
}

/// The deprecated `--kill-all` spelling must produce identical stop
/// behavior to `--stop-all` (it is merged into `stop_all` in
/// `cli::commands::serve::run`), and must also emit a one-line deprecation
/// notice.
#[test]
fn kill_all_alias_stops_registered_servers_and_warns() {
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

    let (code, stdout, stderr) = run_serve_management(&home, &["--kill-all"]);
    assert_eq!(
        code, 0,
        "serve --kill-all should report success when the target stopped\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("DEPRECATION") && stderr.contains("--stop-all"),
        "serve --kill-all should emit a deprecation notice pointing at --stop-all\nstderr:\n{stderr}"
    );

    wait_until(
        || matches!(server.try_wait(), Ok(Some(_))),
        FAST_DEADLINE,
        "server to self-exit after --kill-all",
    );
    assert!(
        !registry_file_exists(&home, pid),
        "server should self-deregister once stopped by --kill-all, just like --stop-all"
    );
}

/// A bare `codanna serve --stop-all` targets only `ServerRole::Server`
/// entries: a registered proxy must survive it. `--stop-all
/// --include-proxies` must additionally stop the proxy.
#[test]
fn stop_all_excludes_proxies_by_default_and_includes_with_flag() {
    let workspace = prepare_workspace();
    let home = workspace.path().join(".home");
    std::fs::create_dir_all(&home).expect("create test home");

    let mut proxy = start_proxy(workspace.path(), &home);
    let proxy_pid = proxy.id();
    let (backing_pid, _backing_port) = await_upstream(&mut proxy);
    let _reaper_proxy = Reaper(proxy_pid);
    let _reaper_backing = Reaper(backing_pid);

    wait_until(
        || registry_file_exists(&home, proxy_pid) && registry_file_exists(&home, backing_pid),
        FAST_DEADLINE,
        "both the proxy and its backing server to publish registry entries",
    );

    let (code, stdout, stderr) = run_serve_management(&home, &["--stop-all"]);
    assert_eq!(
        code, 0,
        "bare serve --stop-all should report success\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    wait_until(
        || !pid_alive(backing_pid),
        FAST_DEADLINE,
        "backing server to be stopped by a bare --stop-all",
    );
    assert!(
        pid_alive(proxy_pid),
        "a bare --stop-all must never stop a registered proxy"
    );
    assert!(
        registry_file_exists(&home, proxy_pid),
        "a bare --stop-all must not touch the proxy's registry entry"
    );

    let (code, stdout, stderr) = run_serve_management(&home, &["--stop-all", "--include-proxies"]);
    assert_eq!(
        code, 0,
        "serve --stop-all --include-proxies should report success\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    // `proxy` is this test's own child handle: like
    // `stop_sends_sigterm_and_server_self_deregisters`, the OS keeps a
    // terminated child as a zombie -- which `pid_alive` deliberately still
    // treats as "alive" -- until its parent reaps it via `wait`/`try_wait`,
    // so exit must be observed through the owning `Child`.
    wait_until(
        || matches!(proxy.try_wait(), Ok(Some(_))),
        FAST_DEADLINE,
        "proxy to be stopped once --include-proxies is passed",
    );
}

/// A stale registry entry (process already dead, unable to self-deregister)
/// must not stop `--stop-all` from attempting and reporting on the other,
/// still-live registered target.
#[test]
fn stop_all_continues_past_a_dead_target() {
    let workspace = prepare_workspace();
    let home = workspace.path().join(".home");
    std::fs::create_dir_all(&home).expect("create test home");

    let mut dead = start_http_server(workspace.path(), &home);
    let dead_pid = dead.id();

    wait_until(
        || registry_file_exists(&home, dead_pid),
        FAST_DEADLINE,
        "soon-to-be-dead server to publish its registry entry",
    );

    // Kill it out-of-band (SIGKILL, not `codanna serve --stop`), matching
    // `reap_prunes_stale_entry_that_list_already_skipped`'s idiom: it cannot
    // self-deregister, so its registry entry becomes stale but remains on
    // disk until pruned.
    kill_pid_externally(dead_pid);
    wait_until(
        || matches!(dead.try_wait(), Ok(Some(_))),
        FAST_DEADLINE,
        "externally killed server to be reaped by try_wait",
    );
    wait_until(
        || !pid_alive(dead_pid),
        FAST_DEADLINE,
        "externally killed server's pid to fully disappear",
    );
    assert!(
        registry_file_exists(&home, dead_pid),
        "an uncleanly killed server must leave its registry file behind"
    );

    let mut live = start_http_server(workspace.path(), &home);
    let live_pid = live.id();
    let _reaper_live = Reaper(live_pid);

    wait_until(
        || registry_file_exists(&home, live_pid),
        FAST_DEADLINE,
        "live server to publish its registry entry",
    );

    let (code, stdout, stderr) = run_serve_management(&home, &["--stop-all"]);
    assert_eq!(
        code, 0,
        "serve --stop-all should still report success for the live target despite a stale \
         registered entry\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains(&live_pid.to_string()) || stderr.contains(&live_pid.to_string()),
        "serve --stop-all output should mention the live target pid {live_pid}\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    wait_until(
        || matches!(live.try_wait(), Ok(Some(_))),
        FAST_DEADLINE,
        "live server to self-exit after --stop-all",
    );
}

/// Unlike `stop_all_continues_past_a_dead_target` (which pre-filters a dead
/// target out via `entry_is_stale` before `stop_all_servers`'s per-target
/// loop ever runs), this exercises a genuine in-loop failure: a target that
/// is live and not stale (so it reaches `stop_one_registered_target`) but
/// never actually exits within the signal-then-wait deadline, alongside a
/// second target that does stop cleanly. `--stop-all` must still attempt and
/// report on the second target rather than aborting the sweep on the first
/// target's failure.
#[cfg(unix)]
#[test]
fn stop_all_continues_past_an_unresponsive_live_target() {
    let workspace = prepare_workspace();
    let home = workspace.path().join(".home");
    std::fs::create_dir_all(&home).expect("create test home");

    let mut stuck = start_http_server(workspace.path(), &home);
    let stuck_pid = stuck.id();

    let mut live = start_http_server(workspace.path(), &home);
    let live_pid = live.id();
    let _reaper_live = Reaper(live_pid);

    wait_until(
        || registry_file_exists(&home, stuck_pid) && registry_file_exists(&home, live_pid),
        FAST_DEADLINE,
        "both servers to publish their registry entries",
    );

    // Suspend `stuck` so it stays alive (and still looks like `codanna
    // serve`, so `entry_is_stale` does not filter it out) but cannot act on
    // the SIGTERM `stop_all_servers` sends it, forcing a genuine in-loop
    // wait-for-exit timeout rather than a pre-loop filter exclusion.
    stop_pid_externally(stuck_pid);
    // `Reaper` normally handles teardown via SIGKILL, but it is only
    // registered for `live`; `stuck` is killed explicitly at the end of this
    // test since its pid is also asserted on mid-test.
    let _cleanup_stuck = Reaper(stuck_pid);

    let (code, stdout, stderr) = run_serve_management(&home, &["--stop-all"]);
    assert_eq!(
        code, 1,
        "serve --stop-all should report failure when a live target never exits\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains(&live_pid.to_string()) || stderr.contains(&live_pid.to_string()),
        "serve --stop-all must still attempt and report the second, healthy target \
         {live_pid} despite the first target's failure\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    wait_until(
        || matches!(live.try_wait(), Ok(Some(_))),
        FAST_DEADLINE,
        "live server to self-exit after --stop-all despite the stuck target's failure",
    );
    assert!(
        registry_file_exists(&home, stuck_pid),
        "the stuck target must remain registered since it never actually stopped"
    );

    // Reap `stuck` through the `Child` handle that owns it, rather than
    // relying solely on `_cleanup_stuck`'s external SIGKILL, so this test
    // never leaves a zombie process behind.
    drop(_cleanup_stuck);
    wait_until(
        || matches!(stuck.try_wait(), Ok(Some(_))),
        FAST_DEADLINE,
        "stuck server to exit after explicit teardown kill",
    );
}

/// `codanna serve --stop-all --include-unknown` additionally sweeps an
/// unregistered `codanna serve` pid that a process-table scan finds, not just
/// registered targets, and reports the two counts separately in its summary
/// line.
#[test]
#[ignore = "machine-wide sweep: --stop-all --include-unknown signals EVERY same-user `codanna serve` process on this host, including real ones and those spawned by parallel tests; run explicitly with `cargo test --test cli_tests -- --ignored --test-threads=1 stop_all_include`"]
fn stop_all_include_unknown_includes_unregistered_target_and_reports_separately() {
    let workspace = prepare_workspace();
    let home = workspace.path().join(".home");
    std::fs::create_dir_all(&home).expect("create test home");

    let mut registered = start_http_server(workspace.path(), &home);
    let registered_pid = registered.id();
    let _reaper_registered = Reaper(registered_pid);

    let mut unknown = start_unknown_server();
    let unknown_pid = unknown.server.id();
    let _reaper_rogue = Reaper(unknown_pid);

    wait_until(
        || registry_file_exists(&home, registered_pid) && pid_alive(unknown_pid),
        FAST_DEADLINE,
        "registered server's registry entry to publish and the unregistered server to be running",
    );
    assert!(
        !registry_file_exists(&home, unknown_pid),
        "unregistered server must not be registered under this test's registry root"
    );

    let (code, stdout, stderr) = run_serve_management(&home, &["--stop-all", "--include-unknown"]);
    assert_eq!(
        code, 0,
        "serve --stop-all --include-unknown should report success\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let combined = format!("{stdout}\n{stderr}").to_lowercase();
    assert!(
        combined.contains("registered") && combined.contains("unknown"),
        "summary line should report registered and unknown counts separately\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    wait_until(
        || matches!(registered.try_wait(), Ok(Some(_))),
        FAST_DEADLINE,
        "registered server to self-exit after --stop-all --include-unknown",
    );
    wait_until(
        || matches!(unknown.server.try_wait(), Ok(Some(_))),
        FAST_DEADLINE,
        "unregistered server to exit after --stop-all --include-unknown",
    );
}

/// The deprecated `--include-rogue` spelling must still work as
/// `--stop-all`'s unregistered-target opt-in (merged into `include_unknown`
/// in `cli::commands::serve::run`), and must emit a one-line deprecation
/// notice pointing at `--include-unknown`.
#[test]
#[ignore = "machine-wide sweep: --stop-all --include-unknown signals EVERY same-user `codanna serve` process on this host, including real ones and those spawned by parallel tests; run explicitly with `cargo test --test cli_tests -- --ignored --test-threads=1 stop_all_include`"]
fn stop_all_include_rogue_alias_still_works_and_warns() {
    let home = TempDir::new().expect("create test home");

    let mut unknown = start_unknown_server();
    let unknown_pid = unknown.server.id();
    let _reaper_rogue = Reaper(unknown_pid);

    wait_until(
        || pid_alive(unknown_pid),
        FAST_DEADLINE,
        "unregistered server to be running",
    );

    let (code, stdout, stderr) =
        run_serve_management(home.path(), &["--stop-all", "--include-rogue"]);
    assert_eq!(
        code, 0,
        "serve --stop-all --include-rogue should report success\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("DEPRECATION") && stderr.contains("--include-unknown"),
        "serve --stop-all --include-rogue should emit a deprecation notice pointing at --include-unknown\nstderr:\n{stderr}"
    );

    wait_until(
        || matches!(unknown.server.try_wait(), Ok(Some(_))),
        FAST_DEADLINE,
        "unregistered server to exit after --stop-all --include-rogue",
    );
}

/// clap must reject `--stop` and `--stop-all` together: they are mutually
/// exclusive registry-lifecycle operations.
#[test]
fn stop_and_stop_all_together_is_rejected_by_clap() {
    let home = TempDir::new().expect("create test home");

    let (code, stdout, stderr) =
        run_serve_management(home.path(), &["--stop", "12345", "--stop-all"]);
    assert_ne!(
        code, 0,
        "clap should reject --stop and --stop-all together\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("cannot be used with"),
        "clap's conflict error should explain --stop/--stop-all cannot be combined:\n{stderr}"
    );
}

/// clap must reject `--stop` and `--include-proxies` together even though
/// `--include-proxies` only declares `requires = "stop_all"`: when `--stop`
/// is present, clap treats `stop_all` as blocked by its own `conflicts_with
/// = "stop"` and silently skips validating `requires` against it, which
/// would otherwise let `--include-proxies` parse as a silent no-op under
/// `--stop` instead of being rejected.
#[test]
fn stop_and_include_proxies_together_is_rejected_by_clap() {
    let home = TempDir::new().expect("create test home");

    let (code, stdout, stderr) =
        run_serve_management(home.path(), &["--stop", "12345", "--include-proxies"]);
    assert_ne!(
        code, 0,
        "clap should reject --stop and --include-proxies together\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("cannot be used with"),
        "clap's conflict error should explain --stop/--include-proxies cannot be combined:\n{stderr}"
    );
}
