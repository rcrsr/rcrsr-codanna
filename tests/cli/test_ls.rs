//! Real-process end-to-end coverage for the top-level `codanna ls` command
//! (`src/cli/commands/ls.rs`).
//!
//! Mirrors the subprocess-driven pattern already established in
//! `test_serve_registry.rs` (registered/rogue registry state) and
//! `test_serve_proxy_discovery.rs` (a real `codanna serve --proxy` delegating
//! to a real backing `codanna serve --http`): every scenario here drives the
//! real `codanna` binary, never calling into `codanna::serve_registry`
//! directly.
//!
//! Registry isolation between tests relies on the same `HOME`/
//! `XDG_CONFIG_HOME` redirection every other subprocess test in this crate
//! uses, since `dirs::state_dir()`/`dirs::data_dir()` on Linux both derive
//! from `$HOME`.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, Signal, System};
use tempfile::TempDir;

use crate::support::codanna_binary;

const DEADLINE: Duration = Duration::from_secs(30);

/// Build a minimal indexed workspace, mirroring
/// `test_serve_registry.rs::prepare_workspace`.
fn prepare_workspace() -> TempDir {
    let workspace = TempDir::new().expect("create temp workspace");

    let src_dir = workspace.path().join("src");
    std::fs::create_dir_all(&src_dir).expect("create src dir");
    std::fs::write(
        src_dir.join("lib.rs"),
        r#"
/// Unique marker symbol used only by the `codanna ls` e2e test.
pub fn codanna_ls_e2e_marker() -> i32 {
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

/// Start `codanna serve --http --bind 127.0.0.1:0` rooted at `ws`, registered
/// under the per-user registry that `home` resolves to.
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

/// Start `codanna serve --proxy` rooted at `ws`, registered under the
/// per-user registry `home` resolves to. stdin is left piped and open: a
/// closed stdin makes the stdio proxy transport exit immediately.
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

    rx.recv_timeout(DEADLINE)
        .expect("proxy should report the backing HTTP server within the deadline")
}

/// Run `codanna ls` under the per-user registry that `home` resolves to.
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

/// Registry directory `home` resolves to, mirroring
/// `serve_registry::registry_dir()`'s own resolution on Linux (the CI
/// platform): `dirs::state_dir()` falls back to `$HOME/.local/state` when
/// `$XDG_STATE_HOME` is unset, as it is in these tests.
fn registry_dir_for(home: &Path) -> PathBuf {
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

/// Best-effort external SIGKILL of `pid`, used for teardown.
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

/// Kills the given pid (best-effort) when dropped, so a failing assertion
/// mid-test never leaks a detached server/proxy process.
struct Reaper(u32);

impl Drop for Reaper {
    fn drop(&mut self) {
        kill_pid_externally(self.0);
    }
}

/// (1) An empty registry -- no registered entries under a freshly created,
/// isolated registry root -- must never surface a `registered` row: the
/// exact "no servers" wording for a truly empty result set (registry AND
/// process scan both empty) is covered deterministically by
/// `src/cli/commands/ls.rs`'s own `render_empty_rows_prints_no_servers_message`
/// unit test, which is not subject to this test's one unavoidable source of
/// nondeterminism: `codanna ls` also does a full host-wide process-table
/// scan (`io::process::scan_codanna_serve_pids`), so an unrelated, genuinely
/// running `codanna serve` process elsewhere on the same host (e.g. an
/// MCP-plugin-backed server for an unrelated tool) legitimately produces a
/// rogue row here, and this test must not treat that as a failure.
#[test]
fn empty_registry_never_reports_a_registered_row() {
    let home = TempDir::new().expect("create isolated test home");

    let (code, stdout, stderr) = run_ls(home.path());
    assert_eq!(
        code, 0,
        "codanna ls should succeed against an empty registry\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        !stdout.to_lowercase().contains("registered"),
        "codanna ls must not report any registered row when this test's own isolated registry \
         root is empty, got:\n{stdout}"
    );
}

/// (2) A real registered backing server appears as a `registered`/`server`
/// row, marked `healthy` once its registry entry has been published.
#[test]
fn registered_backing_server_appears_as_registered_server_healthy() {
    let workspace = prepare_workspace();
    let home = workspace.path().join(".home");
    std::fs::create_dir_all(&home).expect("create test home");

    let mut server = start_http_server(workspace.path(), &home);
    let pid = server.id();
    let _reaper = Reaper(pid);

    wait_until(
        || registry_file_exists(&home, pid),
        DEADLINE,
        "backing server to publish its registry entry",
    );

    let (code, stdout, stderr) = run_ls(&home);
    assert_eq!(
        code, 0,
        "codanna ls should succeed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    let row = stdout
        .lines()
        .find(|line| line.contains(&pid.to_string()))
        .unwrap_or_else(|| panic!("codanna ls output should mention pid {pid}:\n{stdout}"));
    assert!(
        row.to_lowercase().contains("server"),
        "row for the registered backing server should be marked kind=server: {row}"
    );
    assert!(
        row.to_lowercase().contains("registered"),
        "row for the registered backing server should be marked source=registered: {row}"
    );
    assert!(
        row.to_lowercase().contains("healthy"),
        "row for the registered backing server should be marked status=healthy: {row}"
    );

    let _ = server.kill();
    let _ = server.wait();
}

/// (3) A real `codanna serve --http` process registered under a DIFFERENT
/// per-user registry root ("rogue" from the perspective of the registry
/// `codanna ls` consults here) must still show up in the merged table,
/// marked as a rogue source rather than being silently dropped.
#[test]
fn rogue_server_under_different_home_is_marked_rogue() {
    let workspace = prepare_workspace();
    let server_home = workspace.path().join(".home");
    std::fs::create_dir_all(&server_home).expect("create server test home");

    let mut server = start_http_server(workspace.path(), &server_home);
    let pid = server.id();
    let _reaper = Reaper(pid);

    wait_until(
        || registry_file_exists(&server_home, pid),
        DEADLINE,
        "backing server to publish its registry entry under its own home",
    );

    // `codanna ls` is run under an entirely different, freshly created
    // registry root: the server's own registry entry is invisible here, but
    // a full-process-table scan (`io::process::scan_codanna_serve_pids`)
    // still finds the live pid, so it must be reported as a rogue row.
    let ls_home = TempDir::new().expect("create isolated ls home");
    assert!(
        !registry_file_exists(ls_home.path(), pid),
        "the server's registry entry must not be visible under the ls home's registry root"
    );

    let (code, stdout, stderr) = run_ls(ls_home.path());
    assert_eq!(
        code, 0,
        "codanna ls should succeed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    let row = stdout
        .lines()
        .find(|line| line.contains(&pid.to_string()))
        .unwrap_or_else(|| panic!("codanna ls output should mention rogue pid {pid}:\n{stdout}"));
    assert!(
        row.to_lowercase().contains("rogue"),
        "row for the unregistered-here server should be marked source=rogue: {row}"
    );

    let _ = server.kill();
    let _ = server.wait();
}

/// (4) A real `codanna serve --proxy`, connected to a real backing server it
/// discovers/spawns, produces a proxy row attributed to that backing server
/// -- both pids appear, the backing pid as `server`, the proxy's own pid as
/// `proxy`, both `registered` (both self-register under the same per-user
/// registry root).
#[test]
fn proxy_row_is_attributed_to_its_backing_server() {
    let workspace = prepare_workspace();
    let home = workspace.path().join(".home");
    std::fs::create_dir_all(&home).expect("create test home");

    let mut proxy = start_proxy(workspace.path(), &home);
    let proxy_pid = proxy.id();
    let (backing_pid, _backing_port) = await_upstream(&mut proxy);

    let _reap_proxy = Reaper(proxy_pid);
    let _reap_backing = Reaper(backing_pid);

    wait_until(
        || registry_file_exists(&home, proxy_pid) && registry_file_exists(&home, backing_pid),
        DEADLINE,
        "both the proxy and its backing server to publish registry entries",
    );

    let (code, stdout, stderr) = run_ls(&home);
    assert_eq!(
        code, 0,
        "codanna ls should succeed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    let server_row = stdout
        .lines()
        .find(|line| line.contains(&backing_pid.to_string()))
        .unwrap_or_else(|| {
            panic!("codanna ls output should mention backing server pid {backing_pid}:\n{stdout}")
        });
    assert!(
        server_row.to_lowercase().contains("server"),
        "backing server row should be marked kind=server: {server_row}"
    );

    let proxy_row = stdout
        .lines()
        .find(|line| line.contains(&proxy_pid.to_string()))
        .unwrap_or_else(|| {
            panic!("codanna ls output should mention proxy pid {proxy_pid}:\n{stdout}")
        });
    assert!(
        proxy_row.to_lowercase().contains("proxy"),
        "proxy row should be marked kind=proxy: {proxy_row}"
    );
    assert!(
        proxy_row.to_lowercase().contains("registered"),
        "proxy row should be marked source=registered: {proxy_row}"
    );

    // The proxy row must be attributed under its backing server: it should
    // appear on the line immediately after the backing server's own row.
    let server_index = stdout
        .lines()
        .position(|line| line.contains(&backing_pid.to_string()))
        .expect("backing server row index");
    let proxy_index = stdout
        .lines()
        .position(|line| line.contains(&proxy_pid.to_string()))
        .expect("proxy row index");
    assert_eq!(
        proxy_index,
        server_index + 1,
        "the attached proxy row should be displayed immediately under its backing server row:\n{stdout}"
    );

    drop(proxy.stdin.take());
    let _ = proxy.kill();
    let _ = proxy.wait();
    kill_pid_externally(backing_pid);
}
