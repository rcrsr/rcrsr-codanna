//! Real-process end-to-end coverage for the idle-shutdown timer added to
//! `serve --http` (`src/mcp/http_server.rs`).
//!
//! `idle_shutdown_minutes` is whole-minute granularity (see
//! `src/config/mod.rs`'s `ServerConfig`), so the smallest nonzero value that
//! *enables* the idle-timer `select!` arm in the real production path is one
//! minute -- `idle_shutdown_minutes = 1` is kept here purely to satisfy that
//! `idle_minutes > 0` gate. The actual wait duration this test observes is
//! driven by the `CODANNA_TEST_IDLE_THRESHOLD_MS`/`CODANNA_TEST_IDLE_POLL_MS`
//! env-var overrides (`idle_threshold_override`/`idle_poll_interval_override`
//! in `src/mcp/http_server.rs`), so this spawns a real subprocess and
//! exercises the real `select!` arm end-to-end without waiting on a real
//! ~60s timeout. The fast, sub-second exercise of the elapsed/threshold
//! arithmetic and poll loop in isolation lives as unit tests inside
//! `src/mcp/http_server.rs` (`idle_timeout_exceeded`, `wait_for_idle`).
//!
//! This file drives the real `codanna` binary as a subprocess, mirroring the
//! pattern established in `test_serve_proxy_discovery.rs`.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::Duration;

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
use tempfile::TempDir;

use crate::support::{codanna_binary, run_cli};

/// Test-only idle threshold, injected into the backing server via
/// `CODANNA_TEST_IDLE_THRESHOLD_MS` (see `idle_threshold_override` in
/// `src/mcp/http_server.rs`) so this test observes the real idle-exit
/// `select!` arm on a compressed timescale rather than waiting on
/// `idle_shutdown_minutes`' whole-minute production granularity.
///
/// This is 3s, not sub-second, on purpose: `idle_timeout_exceeded` compares
/// **whole-second** unix timestamps (`Duration::from_secs(now - last)`), so any
/// threshold under ~1s collapses to "fire at the next 1-second boundary" --
/// leaving the published `serve.json` alive for only 0-1s before the idle arm
/// removes it. That window is too short for this test's discovery poll to
/// observe reliably under load, which manifests as a flaky timeout on step (1)
/// ("first backing server to publish serve.json"). A 3s threshold keeps
/// `serve.json` observable for ~2-3s while still exiting fast enough to keep
/// the test brief.
const TEST_IDLE_THRESHOLD_MS: u64 = 3000;

/// Test-only idle poll interval, injected via `CODANNA_TEST_IDLE_POLL_MS`
/// (see `idle_poll_interval_override`), kept well under `TEST_IDLE_THRESHOLD_MS`
/// so the poll cadence doesn't pad the observed shutdown time relative to the
/// threshold.
const TEST_IDLE_POLL_MS: u64 = 100;

/// Upper bound for the wait on the backing server's self-initiated idle
/// exit: the injected millisecond-scale threshold plus generous headroom for
/// process startup and CI scheduling jitter (this test spawns a real
/// subprocess, so under a heavily loaded machine -- e.g. the full workspace
/// test suite running with many parallel test binaries -- scheduling alone
/// can eat multiple seconds).
const IDLE_EXIT_DEADLINE: Duration = Duration::from_secs(60);

/// Upper bound for every other (fast) wait in this file.
const FAST_DEADLINE: Duration = Duration::from_secs(30);

/// Build a workspace with `[server] idle_shutdown_minutes = 1` (the smallest
/// nonzero -- i.e. enabled -- value) and semantic search disabled so the
/// spawned backing server starts quickly, mirroring
/// `test_serve_proxy_discovery.rs`'s `prepare_workspace`.
fn prepare_idle_workspace() -> TempDir {
    let workspace = TempDir::new().expect("create temp workspace");

    let src_dir = workspace.path().join("src");
    std::fs::create_dir_all(&src_dir).expect("create src dir");
    std::fs::write(
        src_dir.join("lib.rs"),
        r#"
/// Unique marker symbol used only by the idle-shutdown e2e test.
pub fn codanna_idle_shutdown_e2e_marker() -> i32 {
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

[server]
idle_shutdown_minutes = 1
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

/// Start `codanna serve --http --bind 127.0.0.1:0` rooted at `ws` directly
/// (not via `discover_or_spawn`), so this test observes the backing server's
/// own idle-timer exit rather than a proxy's.
fn start_http_server(ws: &Path) -> Child {
    let test_home = ws.join(".home");
    std::fs::create_dir_all(&test_home).expect("create test home");

    Command::new(codanna_binary())
        .args(["serve", "--http", "--bind", "127.0.0.1:0"])
        .current_dir(ws)
        .env("HOME", &test_home)
        .env("XDG_CONFIG_HOME", &test_home)
        .env(
            "CODANNA_TEST_IDLE_THRESHOLD_MS",
            TEST_IDLE_THRESHOLD_MS.to_string(),
        )
        .env("CODANNA_TEST_IDLE_POLL_MS", TEST_IDLE_POLL_MS.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn codanna serve --http")
}

/// Start `codanna serve --proxy` rooted at `ws`, used here purely to trigger
/// `discover_or_spawn` and observe the pid it converges on -- mirroring
/// `test_serve_proxy_discovery.rs`'s `start_proxy`.
fn start_proxy(ws: &Path) -> Child {
    let test_home = ws.join(".home");
    std::fs::create_dir_all(&test_home).expect("create test home");

    Command::new(codanna_binary())
        .args(["serve", "--proxy"])
        .current_dir(ws)
        .env("HOME", &test_home)
        .env("XDG_CONFIG_HOME", &test_home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn codanna serve --proxy")
}

/// Parse `Proxy: delegating to backing HTTP server at 127.0.0.1:{port} (pid
/// {pid})` out of one stderr line (see `src/mcp/proxy.rs`'s `serve_proxy`).
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
/// line, returning the backing server's `(pid, port)`.
fn await_upstream(child: &mut Child) -> (u32, u16) {
    use std::io::{BufRead, BufReader};
    use std::sync::mpsc;

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

/// Best-effort SIGKILL of `pid` via sysinfo, used to reap any process left
/// running at test teardown.
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

/// THE LOAD-BEARING IDLE-SHUTDOWN REGRESSION TEST.
///
/// Starts a real backing `serve --http` server with `idle_shutdown_minutes =
/// 1` and sends it no traffic. Asserts:
///
/// 1. The server exits BY ITSELF (not killed by the harness) once the idle
///    threshold elapses.
/// 2. `serve.json` is removed as part of that exit -- the SAME cleanup path
///    the Ctrl+C arm uses (`crate::serve_discovery::remove_record` +
///    `ct.cancel()`).
/// 3. `discover_or_spawn` (driven here via `codanna serve --proxy`, exactly
///    as in `test_serve_proxy_discovery.rs`) subsequently spawns a NEW
///    backing server with a DIFFERENT pid, since the workspace is once again
///    recordless.
#[test]
fn idle_backing_server_self_exits_and_is_respawned_with_a_new_pid() {
    let workspace = prepare_idle_workspace();
    let _reaper = Reaper(workspace.path().to_path_buf());
    let codanna_dir = workspace.path().join(".codanna");

    let mut server = start_http_server(workspace.path());

    // Wait for the first server to publish its discovery record so we know
    // its pid before it (eventually) exits on its own.
    wait_until(
        || codanna::serve_discovery::read_record(&codanna_dir).is_some(),
        FAST_DEADLINE,
        "first backing server to publish serve.json",
    );
    let record1 =
        codanna::serve_discovery::read_record(&codanna_dir).expect("serve.json should exist");
    let pid1 = record1.pid;
    assert!(
        codanna::serve_discovery::pid_is_alive(pid1),
        "freshly started backing server pid should be alive"
    );

    // No requests are sent to the server at all: it must exit purely from
    // inactivity, exercising the idle-timer `select!` arm rather than any
    // other shutdown path.
    //
    // (1): self-exit, not a harness kill. Polled via `Child::try_wait`
    // (rather than `pid_is_alive`) so this loop also reaps the child as soon
    // as it exits -- an unreaped exited child is a zombie, which
    // `pid_is_alive`'s `/proc` scan (via `sysinfo`) still reports as
    // present, which would make this loop spin for the full
    // `IDLE_EXIT_DEADLINE` even though the process already self-exited.
    wait_until(
        || matches!(server.try_wait(), Ok(Some(_))),
        IDLE_EXIT_DEADLINE,
        "idle backing server to self-exit after the configured idle_shutdown_minutes elapses",
    );
    // (2): serve.json is removed as part of that same self-exit -- the SAME
    // cleanup path the Ctrl+C arm uses.
    wait_until(
        || codanna::serve_discovery::read_record(&codanna_dir).is_none(),
        FAST_DEADLINE,
        "serve.json to be removed once the idle backing server exits",
    );

    // (3): discover_or_spawn (via a real `serve --proxy` child, exactly like
    // test_serve_proxy_discovery.rs) spawns a fresh backing server now that
    // serve.json is gone, with a NEW pid.
    let mut proxy = start_proxy(workspace.path());
    let (pid2, _port2) = await_upstream(&mut proxy);
    let _ = proxy.kill();
    let _ = proxy.wait();

    assert_ne!(
        pid2, pid1,
        "respawned backing server after idle self-exit must have a different pid"
    );
    assert!(
        codanna::serve_discovery::pid_is_alive(pid2),
        "respawned backing server pid should be alive"
    );

    let record2 = codanna::serve_discovery::read_record(&codanna_dir)
        .expect("serve.json should exist after respawn");
    assert_eq!(
        record2.pid, pid2,
        "serve.json should name the respawned server's pid"
    );
}
