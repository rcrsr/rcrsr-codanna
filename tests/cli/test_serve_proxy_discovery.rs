//! Real-process end-to-end coverage for `codanna serve --proxy` discovery.
//!
//! Unlike the hermetic two-thread race test in `serve_discovery.rs`'s own
//! `#[cfg(test)]` module, these tests drive the actual `codanna` binary as a
//! subprocess: `discover_or_spawn` resolves its child via
//! `std::env::current_exe()`, which inside a test *binary* resolves to the
//! test harness rather than `codanna` -- so the real spawn path can only be
//! exercised by making `codanna` itself the process that calls it. Running
//! two `codanna serve --proxy` children against one temp workspace exercises
//! two concurrent, cross-process `discover_or_spawn` calls, racing on the
//! `.codanna/http.lock` `O_EXCL` file -- a strictly stronger check than an
//! in-process, two-thread race.

use std::ffi::OsStr;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
use tempfile::TempDir;

use crate::support::{codanna_binary, run_cli};

/// Upper bound for every blocking wait in this file so a stuck proxy or
/// backing server FAILS the test instead of hanging CI.
const DEADLINE: Duration = Duration::from_secs(30);

/// Build a workspace with a uniquely-named fixture symbol, semantic search
/// disabled (see module docs on the KNOWN RISK below), and an already-built
/// index, ready for `codanna serve --proxy` to discover/spawn against.
///
/// KNOWN RISK verified before writing these tests: `Commands::Serve { .. }`
/// in non-proxy mode forces `needs_semantic_search = true` in `main.rs`, so
/// the spawned `serve --http` child takes the `load_facade` (not
/// `load_facade_lite`) path. However, `enable_semantic_search` and the
/// eager-load-if-present path in `IndexPersistence::load_facade_impl` are
/// additionally gated on `config.semantic_search.enabled` and on persisted
/// semantic data existing on disk, respectively. With `enabled = false` at
/// index time, no semantic data is ever persisted, so the spawned child does
/// not load a model on either indexing or serve.
fn prepare_workspace() -> TempDir {
    let workspace = TempDir::new().expect("create temp workspace");

    let src_dir = workspace.path().join("src");
    std::fs::create_dir_all(&src_dir).expect("create src dir");
    std::fs::write(
        src_dir.join("lib.rs"),
        r#"
/// Unique marker symbol used only by the serve --proxy discovery e2e tests.
pub fn codanna_proxy_e2e_marker() -> i32 {
    42
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

/// Build a workspace configured with a NON-DEFAULT, absolute `index_path`
/// pointing OUTSIDE `<workspace>/.codanna/`, in a second, unrelated tempdir.
///
/// Every other fixture in this file uses the default `index_path`, where
/// `index_path.parent()` and `<workspace_root>/.codanna` happen to be the
/// same directory -- exactly why the bug this file's tests guard against
/// shipped unnoticed. `init::resolve_index_path` returns an absolute
/// `index_path` as-is (see `src/init.rs`), so this is a genuinely supported
/// configuration a real user could write, not a synthetic corner case.
///
/// Returns `(workspace, index_container)`: the workspace tempdir, and the
/// second tempdir holding the custom index, so callers can assert nothing
/// under the latter ever receives a discovery record.
fn prepare_custom_index_path_workspace() -> (TempDir, TempDir) {
    let workspace = TempDir::new().expect("create temp workspace");
    let index_container = TempDir::new().expect("create temp index container");

    let src_dir = workspace.path().join("src");
    std::fs::create_dir_all(&src_dir).expect("create src dir");
    std::fs::write(
        src_dir.join("lib.rs"),
        r#"
/// Unique marker symbol used only by the custom index_path proxy discovery
/// e2e test.
pub fn codanna_proxy_e2e_custom_index_marker() -> i32 {
    99
}
"#,
    )
    .expect("write fixture source");

    let codanna_dir = workspace.path().join(".codanna");
    std::fs::create_dir_all(&codanna_dir).expect("create .codanna dir");

    let custom_index_path = index_container.path().join("custom-index");
    std::fs::write(
        codanna_dir.join("settings.toml"),
        format!(
            r#"
index_path = {custom_index_path:?}

[semantic_search]
enabled = false
"#,
        ),
    )
    .expect("write settings.toml");

    let (code, stdout, stderr) = run_cli(
        workspace.path(),
        &["index", "src", "--force", "--no-progress"],
    );
    assert_eq!(
        code, 0,
        "workspace fixture index (custom index_path) should succeed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        custom_index_path.exists(),
        "index should have been written to the custom index_path, not the default location"
    );

    (workspace, index_container)
}

/// Start `codanna serve --proxy` rooted at `ws`. stdin is left piped and
/// open: `.output()` would close stdin immediately, and the stdio proxy
/// transport exits as soon as it observes stdin EOF.
fn start_proxy(ws: &Path) -> Child {
    let test_home = ws.join(".home");
    std::fs::create_dir_all(&test_home).expect("create test home");

    Command::new(codanna_binary())
        .args(["serve", "--proxy"])
        .current_dir(ws)
        // `XDG_CONFIG_HOME` is set alongside `HOME` (to the SAME per-test
        // dir) because `dirs::config_dir()` -- used by both
        // `serve_tls::pinned_client` (here, to pin the backing HTTPS
        // server's cert) and `get_or_create_certificate` (there, to persist
        // it) -- prefers an ambient `XDG_CONFIG_HOME` over `HOME` on Linux.
        // Leaving it unset would let a real ambient `XDG_CONFIG_HOME` on the
        // test runner point this process at a different (real user)
        // certs directory than the one the test's own `--https` child wrote
        // to, silently breaking the HTTPS discovery tests below.
        .env("HOME", &test_home)
        .env("XDG_CONFIG_HOME", &test_home)
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

/// Like [`await_upstream`], but also returns the raw delegation line so
/// callers can assert on its dial-scheme prefix (e.g. `https://`).
#[cfg(feature = "https-server")]
fn await_upstream_with_line(child: &mut Child) -> (u32, u16, String) {
    let stderr = child
        .stderr
        .take()
        .expect("proxy child stderr should be piped");

    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let reader = BufReader::new(stderr);
        for line in reader.lines() {
            let Ok(line) = line else { break };
            if let Some((pid, port)) = parse_delegating_line(&line) {
                let _ = tx.send((pid, port, line));
                return;
            }
        }
    });

    rx.recv_timeout(DEADLINE)
        .expect("proxy should report the backing server within the deadline")
}

/// Start `codanna serve --https --bind 127.0.0.1:0` rooted at `ws`, with
/// `HOME`/`XDG_CONFIG_HOME` set to the SAME per-test dir `start_proxy` uses,
/// so the proxy's `serve_tls::pinned_client` pins the exact cert this
/// process persists.
#[cfg(feature = "https-server")]
fn start_https_server(ws: &Path) -> Child {
    let test_home = ws.join(".home");
    std::fs::create_dir_all(&test_home).expect("create test home");

    Command::new(codanna_binary())
        .args(["serve", "--https", "--bind", "127.0.0.1:0"])
        .current_dir(ws)
        .env("HOME", &test_home)
        .env("XDG_CONFIG_HOME", &test_home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn codanna serve --https")
}

/// Deadline-bounded wait for `<ws>/.codanna/serve.json` to exist and name an
/// `Https` backing server, returning the converged record.
#[cfg(feature = "https-server")]
fn wait_for_https_record(ws: &Path) -> codanna::serve_discovery::ServeRecord {
    let codanna_dir = ws.join(".codanna");
    wait_until(
        || {
            codanna::serve_discovery::read_record(&codanna_dir)
                .map(|record| record.scheme == codanna::serve_discovery::ServeScheme::Https)
                .unwrap_or(false)
        },
        DEADLINE,
        "serve.json to record an Https backing server",
    );
    codanna::serve_discovery::read_record(&codanna_dir)
        .expect("serve.json should exist once the Https record has converged")
}

/// Absolute path to the persisted server cert under the per-test config dir
/// (`XDG_CONFIG_HOME` = `HOME` = `<ws>/.home`, matching [`start_https_server`]
/// and [`start_proxy`]).
#[cfg(feature = "https-server")]
fn test_server_cert_path(ws: &Path) -> PathBuf {
    ws.join(".home")
        .join("codanna")
        .join("certs")
        .join("server.pem")
}

/// Count live processes whose cwd is `ws` (canonicalized) and whose command
/// line contains both `serve` and `--http`. cwd is the only discriminator
/// available: `spawn_detached` (`serve_discovery.rs`) sets `current_dir` on
/// the child and passes no workspace path argument.
fn count_http_children(ws: &Path) -> usize {
    let canonical_root = ws.canonicalize().expect("canonicalize workspace root");

    let mut sys = System::new();
    let refresh_kind = ProcessRefreshKind::nothing()
        .with_cwd(UpdateKind::Always)
        .with_cmd(UpdateKind::Always);
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, refresh_kind);

    sys.processes()
        .values()
        // On Linux, sysinfo also enumerates each thread of a multi-threaded
        // process (e.g. every tokio worker thread of `serve --http`) as its
        // own `Process` entry sharing the parent's cwd and cmd line.
        // `thread_kind()` is `Some` only for those thread entries, so
        // filtering them out is required to count actual OS processes
        // rather than (cwd, cmd)-matching threads.
        .filter(|process| process.thread_kind().is_none())
        .filter(|process| {
            process.cwd() == Some(canonical_root.as_path())
                && process.cmd().iter().any(|arg| arg == OsStr::new("serve"))
                && process.cmd().iter().any(|arg| arg == OsStr::new("--http"))
        })
        .count()
}

/// Best-effort SIGKILL of `pid` via sysinfo, used both to simulate a crashed
/// backing server and to reap it at test teardown.
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

/// Send SIGINT (not SIGKILL) to `pid`, used to trigger the backing server's
/// `shutdown_signal` future (`ctrl_c()` in `src/mcp/http_server.rs`) so its
/// graceful-shutdown `remove_record` cleanup path actually runs, instead of
/// being reaped abruptly.
fn interrupt_pid(pid: u32) {
    let target = Pid::from_u32(pid);
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[target]),
        true,
        ProcessRefreshKind::nothing(),
    );
    if let Some(process) = sys.process(target) {
        let _ = process.kill_with(sysinfo::Signal::Interrupt);
    }
}

/// Recursively check whether any file named `filename` exists anywhere under
/// `root` (including nested directories). Used to confirm no shadow
/// discovery record leaks under a custom `index_path`'s directory tree.
fn any_file_named(root: &Path, filename: &str) -> bool {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.file_name() == Some(OsStr::new(filename)) {
                return true;
            }
        }
    }
    false
}

/// Deadline-bounded wait that distinguishes a child which terminated *on its
/// own* from one the harness had to SIGKILL: returns `None` when the deadline
/// elapsed with the child still running (it is killed and reaped either way,
/// so no process leaks).
///
/// This distinction is load-bearing for fail-closed assertions. A SIGKILLed
/// child also reports a non-success exit status, so `!status.success()` alone
/// cannot tell "the process refused to proceed" apart from "the process was
/// working fine and we killed it".
///
/// Called by the https-server-gated pinned-cert test and by Arm A of the
/// emission-gate test below, which is compiled unconditionally -- so this
/// helper carries no `#[cfg]` gate.
fn wait_for_self_exit(child: &mut Child, deadline: Duration) -> Option<std::process::ExitStatus> {
    let start = std::time::Instant::now();
    loop {
        if let Some(status) = child.try_wait().expect("poll proxy child exit status") {
            return Some(status);
        }
        if start.elapsed() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        thread::sleep(Duration::from_millis(100));
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
        thread::sleep(Duration::from_millis(100));
    }
}

/// Kills the backing `serve --http` process recorded in
/// `<workspace>/.codanna/serve.json`, if any, when dropped. Mandatory on
/// every test in this file: without it, a failing test leaks a detached
/// `serve --http` process that outlives the test run.
struct Reaper(PathBuf);

impl Drop for Reaper {
    fn drop(&mut self) {
        let codanna_dir = self.0.join(".codanna");
        if let Some(record) = codanna::serve_discovery::read_record(&codanna_dir) {
            kill_pid(record.pid);
        }
    }
}

#[test]
fn two_concurrent_proxies_share_one_backing_http_server() {
    let workspace = prepare_workspace();
    // Declared before any proxies are started so it drops (and reaps the
    // backing server) after every other local in this test, regardless of
    // which assertion fails first.
    let _reaper = Reaper(workspace.path().to_path_buf());

    // Start both proxies back-to-back, before either can converge: no
    // serve.json record exists yet, so both take the lock-acquisition path.
    let mut proxy_a = start_proxy(workspace.path());
    let mut proxy_b = start_proxy(workspace.path());

    let (pid_a, port_a) = await_upstream(&mut proxy_a);
    let (pid_b, port_b) = await_upstream(&mut proxy_b);

    let _ = proxy_a.kill();
    let _ = proxy_a.wait();
    let _ = proxy_b.kill();
    let _ = proxy_b.wait();

    // A1: both proxies must report the same backing server -- kills "loser
    // waits on the wrong record" (mismatched pid/port, or a loser
    // SpawnTimeout because it never observed a healthy record).
    assert_eq!(
        pid_a, pid_b,
        "both proxies must delegate to the same backing server pid"
    );
    assert_eq!(
        port_a, port_b,
        "both proxies must delegate to the same backing server port"
    );

    // A2: exactly one backing `serve --http` process for the workspace --
    // the assertion the current suite otherwise cannot make. Kills "both
    // branches spawn".
    assert_eq!(
        count_http_children(workspace.path()),
        1,
        "exactly one backing `serve --http` process should exist for the workspace"
    );

    // A3: the discovery record names the same live pid both proxies
    // reported -- kills a fabricated/stale record.
    let codanna_dir = workspace.path().join(".codanna");
    let record = codanna::serve_discovery::read_record(&codanna_dir)
        .expect("serve.json should exist once both proxies have converged");
    assert_eq!(
        record.pid, pid_a,
        "serve.json pid should match both proxies' reported pid"
    );
    assert!(
        codanna::serve_discovery::pid_is_alive(record.pid),
        "serve.json pid should still be alive"
    );

    // A4: the single-flight spawn lock does not outlive the race -- kills
    // the guard `Drop` never firing.
    assert!(
        !codanna_dir.join("http.lock").exists(),
        "http.lock should not exist once both proxies have settled"
    );
}

#[test]
fn killed_server_is_respawned_and_record_is_updated() {
    let workspace = prepare_workspace();
    let _reaper = Reaper(workspace.path().to_path_buf());
    let codanna_dir = workspace.path().join(".codanna");

    let mut proxy1 = start_proxy(workspace.path());
    let (pid1, _port1) = await_upstream(&mut proxy1);
    let _ = proxy1.kill();
    let _ = proxy1.wait();

    // SIGKILL the backing server directly: this skips its graceful-shutdown
    // `remove_record` path, so serve.json is left naming a now-dead pid.
    // That staleness is the scenario under test.
    kill_pid(pid1);
    wait_until(
        || !codanna::serve_discovery::pid_is_alive(pid1),
        DEADLINE,
        "backing server pid1 to die after being killed",
    );

    let stale_record = codanna::serve_discovery::read_record(&codanna_dir)
        .expect("serve.json should still exist (stale) right after the kill");
    assert_eq!(
        stale_record.pid, pid1,
        "serve.json must still name the killed pid -- confirms the record is genuinely stale, \
         not already reclaimed"
    );

    let mut proxy2 = start_proxy(workspace.path());
    let (pid2, port2) = await_upstream(&mut proxy2);
    let _ = proxy2.kill();
    let _ = proxy2.wait();

    // B1: a fresh pid, not the stale one -- kills an impl that returns the
    // stale record without a liveness check.
    assert_ne!(
        pid2, pid1,
        "respawned backing server must have a different pid than the killed one"
    );

    // B2 + B3: the record on disk is fully and correctly rewritten to the
    // new pid/port -- kills an impl that spawns but never rewrites the
    // record, and kills a partial/torn write.
    let updated_record = codanna::serve_discovery::read_record(&codanna_dir)
        .expect("serve.json should exist after respawn");
    assert_eq!(
        updated_record.pid, pid2,
        "serve.json should be updated to the respawned server's pid"
    );
    assert_eq!(
        updated_record.port, port2,
        "serve.json should be updated to the respawned server's port"
    );
    assert!(
        codanna::serve_discovery::pid_is_alive(pid2),
        "respawned server pid should be alive"
    );
}

/// Text a delegated tool call carries while the backing server is still being
/// dialed (pinned copy from `src/mcp/proxy.rs`).
const NOT_READY_TEXT: &str =
    "codanna index not available yet \u{2014} backend starting, check back shortly";

/// Stable prefix of the text a delegated tool call carries after a failed dial
/// round (pinned copy from `src/mcp/proxy.rs`).
const FAILED_PREFIX: &str = "codanna backend unavailable:";

/// Every tool the proxy must advertise locally, whatever the backend state.
const EXPECTED_TOOLS: [&str; 13] = [
    "find_symbol",
    "find_symbols",
    "get_calls",
    "find_callers",
    "analyze_impact",
    "get_index_info",
    "search_symbols",
    "semantic_search_docs",
    "semantic_search_with_context",
    "search_documents",
    "reindex",
    "get_file_outline",
    "read_symbol",
];

type ProxyClient = rmcp::service::RunningService<rmcp::service::RoleClient, ()>;

/// Kills a spawned child and reaps it when dropped, so a failing assertion
/// never leaks a backing server.
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Registry directory `home` resolves to (matching `connect_proxy_client_with_env`,
/// which pins `XDG_STATE_HOME` to `<home>/.local/state`).
fn registry_dir_for(home: &Path) -> PathBuf {
    home.join(".local")
        .join("state")
        .join("codanna")
        .join("servers")
}

/// Every parseable registry entry under `home` whose workspace root is `ws`.
fn registry_entries_for_workspace(
    home: &Path,
    ws: &Path,
) -> Vec<codanna::serve_registry::RegistryEntry> {
    let canonical_ws = ws.canonicalize().expect("canonicalize workspace root");
    let Ok(read_dir) = std::fs::read_dir(registry_dir_for(home)) else {
        return Vec::new();
    };
    read_dir
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let contents = std::fs::read_to_string(entry.path()).ok()?;
            let parsed: codanna::serve_registry::RegistryEntry =
                serde_json::from_str(&contents).ok()?;
            let root = parsed.workspace_root.canonicalize().ok()?;
            (root == canonical_ws).then_some(parsed)
        })
        .collect()
}

/// Kills every process registered for `ws` under `home` when dropped
/// (backing servers spawned by a proxy that may never publish `serve.json`).
struct RegistryReaper {
    home: PathBuf,
    ws: PathBuf,
}

impl Drop for RegistryReaper {
    fn drop(&mut self) {
        for entry in registry_entries_for_workspace(&self.home, &self.ws) {
            kill_pid(entry.pid);
        }
    }
}

/// Async deadline-bounded poll (does not block the runtime's other tasks).
async fn wait_until_async(mut predicate: impl FnMut() -> bool, what: &str) {
    let start = std::time::Instant::now();
    loop {
        if predicate() {
            return;
        }
        assert!(start.elapsed() < DEADLINE, "timed out waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Connect a real `rmcp` stdio client to `codanna serve --proxy` rooted at
/// `ws` WITHOUT waiting for the backing server: returns as soon as the
/// initialize handshake (answered locally by the proxy) completes.
async fn connect_proxy_client_nowait(ws: &Path) -> ProxyClient {
    connect_proxy_client_with_env(ws, &[]).await
}

/// Like [`connect_proxy_client_nowait`], with extra environment variables for
/// the proxy process (inherited by the backing server it spawns).
async fn connect_proxy_client_with_env(ws: &Path, extra_env: &[(&str, &str)]) -> ProxyClient {
    use rmcp::service::ServiceExt;
    use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};

    let test_home = ws.join(".home");
    std::fs::create_dir_all(&test_home).expect("create test home");
    let state_home = test_home.join(".local").join("state");

    let ws = ws.to_path_buf();
    let extra_env: Vec<(String, String)> = extra_env
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    tokio::time::timeout(
        DEADLINE,
        ().serve(
            TokioChildProcess::new(tokio::process::Command::new(codanna_binary()).configure(
                |cmd| {
                    cmd.args(["serve", "--proxy"])
                        .current_dir(&ws)
                        // Same `HOME`/`XDG_CONFIG_HOME` pairing as
                        // `start_proxy`; `XDG_STATE_HOME` pins the per-user
                        // server registry under the per-test home.
                        .env("HOME", &test_home)
                        .env("XDG_CONFIG_HOME", &test_home)
                        .env("XDG_STATE_HOME", &state_home);
                    for (key, value) in &extra_env {
                        cmd.env(key, value);
                    }
                },
            ))
            .expect("spawn codanna serve --proxy as an rmcp child transport"),
        ),
    )
    .await
    .expect("proxy handshake should complete within the deadline")
    .expect("rmcp client should complete the stdio initialize handshake with the proxy")
}

/// Concatenated text blocks of a tool result.
fn result_text(result: &rmcp::model::CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|block| match block {
            rmcp::model::ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// True for the proxy's "backend still starting" tool result.
fn is_not_ready(result: &rmcp::model::CallToolResult) -> bool {
    result.is_error == Some(true) && result_text(result).contains(NOT_READY_TEXT)
}

/// One `get_index_info` call through the proxy (a protocol-level failure
/// panics: unavailability is reported as an `isError` result, never an `Err`).
async fn call_get_index_info(client: &ProxyClient) -> rmcp::model::CallToolResult {
    tokio::time::timeout(
        DEADLINE,
        client.call_tool(rmcp::model::CallToolRequestParams::new("get_index_info")),
    )
    .await
    .expect("get_index_info should return promptly, even while the backend is unavailable")
    .expect("proxy should answer tool calls with a result, not a protocol error")
}

/// Poll `get_index_info` until the result is not the NOT_READY one, bounded
/// by [`DEADLINE`], and return that result (which may still be an error such
/// as the FAILED text).
async fn wait_until_not_not_ready(client: &ProxyClient) -> rmcp::model::CallToolResult {
    let start = std::time::Instant::now();
    loop {
        let result = call_get_index_info(client).await;
        if !is_not_ready(&result) {
            return result;
        }
        assert!(
            start.elapsed() < DEADLINE,
            "proxy still reported NOT_READY after {DEADLINE:?}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Connect a real `rmcp` stdio client to `codanna serve --proxy` rooted at
/// `ws`, mirroring the exact `().serve(TokioChildProcess::new(..))` pattern
/// `CodeIntelligenceClient::test_server` uses in `src/mcp/client.rs`, then
/// wait until the backing server is ready: the handshake and `list_tools` are
/// answered locally, so readiness is observed by polling `get_index_info`
/// until it stops returning NOT_READY. Panics if the backend failed instead.
async fn connect_proxy_client(ws: &Path) -> ProxyClient {
    let client = connect_proxy_client_nowait(ws).await;
    let result = wait_until_not_not_ready(&client).await;
    assert_ne!(
        result.is_error,
        Some(true),
        "backing server should become ready, got: {}",
        result_text(&result)
    );
    client
}

#[tokio::test]
async fn proxy_serves_real_mcp_traffic_from_shared_upstream() {
    let workspace = prepare_workspace();
    let _reaper = Reaper(workspace.path().to_path_buf());

    let client = tokio::time::timeout(DEADLINE, connect_proxy_client(workspace.path()))
        .await
        .expect("proxy client should connect within the deadline");

    // C1: the proxy answers `initialize` LOCALLY, from the same in-binary
    // server info the backend uses, so the negotiated identity must match the
    // backend's ("codanna"), not a distinct proxy-only identity. This proves
    // local identity parity with the backend; it does NOT prove relay (relay
    // provenance is C3).
    let server_info = client
        .peer_info()
        .expect("proxy should have negotiated peer info during initialize");
    let server_name = server_info
        .server_info
        .as_ref()
        .map(|impl_| impl_.name.as_str())
        .expect("negotiated peer info should carry a server implementation identity");
    assert_eq!(
        server_name, "codanna",
        "proxy's locally-answered identity should match the backend's"
    );
    assert_ne!(
        server_name, "codanna-proxy",
        "proxy must not advertise a distinct proxy-only identity"
    );

    // C2: `tools/list` is also answered locally from the same in-binary
    // routers as the backend, so a non-empty list proves local parity with
    // the backend's tool set -- not that anything was relayed. The Bearer
    // handshake to the backing HTTP server is proven only by C3 below.
    let tools = tokio::time::timeout(DEADLINE, client.list_tools(Default::default()))
        .await
        .expect("list_tools should complete within the deadline")
        .expect("list_tools should succeed through the proxy");
    let tool_names: Vec<&str> = tools.tools.iter().map(|t| t.name.as_ref()).collect();
    assert!(
        tool_names.contains(&"find_symbol"),
        "locally-served tool list should contain find_symbol, got: {tool_names:?}"
    );
    assert!(
        tool_names.contains(&"search_symbols"),
        "locally-served tool list should contain search_symbols, got: {tool_names:?}"
    );

    // C3: a real find_symbol call against the fixture symbol. The proxy
    // holds no `IndexFacade`, so a match can only come from the upstream
    // index: this is the relay provenance check (and, since the backing HTTP
    // server rejects a bad Bearer token, the authenticated-handshake check).
    let call_result = tokio::time::timeout(
        DEADLINE,
        client.call_tool(
            rmcp::model::CallToolRequestParams::new("find_symbol").with_arguments(
                serde_json::json!({ "name": "codanna_proxy_e2e_marker" })
                    .as_object()
                    .cloned()
                    .expect("json object literal"),
            ),
        ),
    )
    .await
    .expect("call_tool should complete within the deadline")
    .expect("find_symbol call should succeed through the proxy");

    let found_marker = call_result.content.iter().any(|block| match block {
        rmcp::model::ContentBlock::Text(text) => text.text.contains("codanna_proxy_e2e_marker"),
        _ => false,
    });
    assert!(
        found_marker,
        "find_symbol result relayed through the proxy should name the fixture symbol, got: {:?}",
        call_result.content
    );

    client
        .cancel()
        .await
        .expect("proxy client should shut down cleanly");
}

/// W-6(C): PROXY + HTTP MODE E2E for the `reindex` tool.
///
/// This drives the exact same real subprocess pattern as
/// `proxy_serves_real_mcp_traffic_from_shared_upstream` above (a real
/// `codanna serve --proxy` child, which `discover_or_spawn`s a real
/// `codanna serve --http` backing server, connected to over real MCP stdio
/// framing -- no mocks anywhere in the chain), but targets `reindex`
/// specifically:
///
/// - `list_tools()` through the proxy proves the HTTP backing server (the
///   `new_with_facade` constructor, see `src/cli/commands/serve.rs`) really
///   registers `reindex` -- the proxy itself registers no tools of its own
///   (`src/mcp/proxy.rs`), so a `reindex` entry can only have come from the
///   upstream HTTP server's tool router.
/// - `call_tool("reindex")` through the proxy proves BOTH that the upstream
///   HTTP server can actually execute the tool AND that the proxy correctly
///   forwards an admin-router tool call end-to-end, not just the read-only
///   tools already covered by the `find_symbol` assertions above. A
///   `METHOD_NOT_FOUND` error here would mean either the upstream never
///   wired `reindex` into its router, or the proxy's forwarding path
///   silently drops/misroutes it.
#[tokio::test]
async fn proxy_forwards_reindex_tool_from_http_upstream() {
    let workspace = prepare_workspace();
    let _reaper = Reaper(workspace.path().to_path_buf());

    let client = tokio::time::timeout(DEADLINE, connect_proxy_client(workspace.path()))
        .await
        .expect("proxy client should connect within the deadline");

    // (a) `reindex` is present in the tool list relayed through the proxy
    // from the real HTTP backing server.
    let tools = tokio::time::timeout(DEADLINE, client.list_tools(Default::default()))
        .await
        .expect("list_tools should complete within the deadline")
        .expect("list_tools should succeed through the proxy");
    let tool_names: Vec<&str> = tools.tools.iter().map(|t| t.name.as_ref()).collect();
    assert!(
        tool_names.contains(&"reindex"),
        "upstream HTTP server's tool list relayed through the proxy should contain reindex, got: \
         {tool_names:?}"
    );

    // (b) calling `reindex` through the proxy actually reaches the upstream
    // and executes -- not a METHOD_NOT_FOUND, and not an application-level
    // error result either.
    let call_result = tokio::time::timeout(
        DEADLINE,
        client.call_tool(
            rmcp::model::CallToolRequestParams::new("reindex").with_arguments(
                serde_json::json!({})
                    .as_object()
                    .cloned()
                    .expect("json object"),
            ),
        ),
    )
    .await
    .expect("call_tool should complete within the deadline");

    let call_result = match call_result {
        Ok(result) => result,
        Err(err) => panic!(
            "reindex call through the proxy should not fail at the protocol level (e.g. \
             METHOD_NOT_FOUND), got: {err:?}"
        ),
    };
    assert_ne!(
        call_result.is_error,
        Some(true),
        "reindex call through the proxy should not be an application-level error result, got: \
         {call_result:?}"
    );

    client
        .cancel()
        .await
        .expect("proxy client should shut down cleanly");
}

#[tokio::test]
async fn second_proxy_shares_one_upstream_one_record_one_pid() {
    let workspace = prepare_workspace();
    let _reaper = Reaper(workspace.path().to_path_buf());
    let codanna_dir = workspace.path().join(".codanna");

    // Proxy #1 spawns the backing server (cold path, WARM path is what
    // proxy #2 below exercises).
    let client1 = tokio::time::timeout(DEADLINE, connect_proxy_client(workspace.path()))
        .await
        .expect("proxy #1 client should connect within the deadline");
    let tools1 = tokio::time::timeout(DEADLINE, client1.list_tools(Default::default()))
        .await
        .expect("proxy #1 list_tools should complete within the deadline")
        .expect("proxy #1 list_tools should succeed");
    assert!(
        !tools1.tools.is_empty(),
        "proxy #1 should relay a non-empty tool list from the upstream"
    );

    let record_before = codanna::serve_discovery::read_record(&codanna_dir)
        .expect("serve.json should exist once proxy #1 has converged");

    // Proxy #2 starts while the record is already live: this takes the
    // `Decision::Discover` branch at serve_discovery.rs:406, not the
    // lock-acquisition path both proxy #1 here and TEST 1 exercise.
    let client2 = tokio::time::timeout(DEADLINE, connect_proxy_client(workspace.path()))
        .await
        .expect("proxy #2 client should connect within the deadline");

    // D1: both proxies' list_tools calls succeed.
    let tools2 = tokio::time::timeout(DEADLINE, client2.list_tools(Default::default()))
        .await
        .expect("proxy #2 list_tools should complete within the deadline")
        .expect("proxy #2 list_tools should succeed");
    assert!(
        !tools2.tools.is_empty(),
        "proxy #2 should relay a non-empty tool list from the same upstream"
    );

    let record_after = codanna::serve_discovery::read_record(&codanna_dir)
        .expect("serve.json should still exist after proxy #2 has converged");

    // D2 + D4: the discovery record is byte-identical before and after
    // proxy #2 starts -- one record, one pid, one port, not a second spawn.
    assert_eq!(
        record_before.pid, record_after.pid,
        "serve.json pid should be unchanged after the warm-path proxy connects"
    );
    assert_eq!(
        record_before.port, record_after.port,
        "serve.json port should be unchanged after the warm-path proxy connects"
    );

    // D3: exactly one backing `serve --http` process for the workspace with
    // both proxies connected.
    assert_eq!(
        count_http_children(workspace.path()),
        1,
        "exactly one backing `serve --http` process should exist with both proxies connected"
    );

    client1
        .cancel()
        .await
        .expect("proxy #1 client should shut down cleanly");
    client2
        .cancel()
        .await
        .expect("proxy #2 client should shut down cleanly");
}

/// THE LOAD-BEARING E2E REGRESSION TEST.
///
/// Every other fixture in this file uses the default `index_path`, where
/// `index_path.parent()` and `<workspace_root>/.codanna` happen to be the
/// same directory -- exactly why the bug this test targets shipped
/// unnoticed. Before the fix, `serve --http` derived the discovery-record
/// directory from `config.index_path.parent()` while `discover_or_spawn`
/// derived it from `workspace_root/.codanna`; under a custom `index_path`
/// (proven here via `prepare_custom_index_path_workspace`) the two diverge:
/// the proxy waits in `.codanna` for a record written elsewhere, burns the
/// full `spawn_timeout_ms`, and leaks an orphan `codanna serve` process.
#[test]
fn proxy_discovers_backing_server_with_custom_index_path() {
    let (workspace, index_container) = prepare_custom_index_path_workspace();
    // Declared immediately so a failing assertion below still reaps the
    // backing server via SIGKILL rather than leaking it -- this test is
    // about orphan processes, so it must not be able to leak one itself.
    let _reaper = Reaper(workspace.path().to_path_buf());

    let settings = codanna::Settings::default();
    let spawn_timeout = Duration::from_millis(settings.server.spawn_timeout_ms);

    let started_at = std::time::Instant::now();
    let mut proxy = start_proxy(workspace.path());
    let (proxy_pid, _proxy_port) = await_upstream(&mut proxy);
    let elapsed = started_at.elapsed();

    let _ = proxy.kill();
    let _ = proxy.wait();

    // (a) the fixed derivation reaches the backing server comfortably inside
    // `spawn_timeout_ms`. Against the pre-fix `index_path.parent()`
    // derivation, the proxy waits on the wrong directory for the *entire*
    // `spawn_timeout_ms` before giving up -- this assertion is what actually
    // trips on the bug, rather than merely relying on `await_upstream`'s much
    // longer 30s `DEADLINE` to eventually panic.
    assert!(
        elapsed < spawn_timeout,
        "proxy should reach the backing server well before spawn_timeout_ms ({}ms) elapses \
         (an index_path-derived discovery dir would burn the full timeout instead); took {elapsed:?}",
        settings.server.spawn_timeout_ms
    );

    // (b) the discovery record exists under the WORKSPACE's `.codanna/`,
    // not wherever the custom index_path happens to live.
    let codanna_dir = workspace.path().join(".codanna");
    let record = codanna::serve_discovery::read_record(&codanna_dir).expect(
        "serve.json should exist under <workspace>/.codanna while the backing server is live",
    );

    // (c) THE SHADOW-WRITE CHECK. No serve.json exists anywhere under the
    // custom index_path's directory tree while the server is live. This is
    // deliberately a separate assertion from (a): (a) alone would still pass
    // if the record were written to BOTH the correct and the old,
    // index_path-derived location (a shadow write), since the proxy would
    // still converge quickly by reading the correct one.
    assert!(
        !any_file_named(index_container.path(), "serve.json"),
        "no serve.json should exist anywhere under the custom index_path's directory while the \
         server is live -- finding one here means the old index_path-derived discovery dir has \
         been reintroduced, whether instead of or in addition to the correct one"
    );

    // (d) the pid the proxy delegates to is the pid recorded on disk.
    assert_eq!(
        proxy_pid, record.pid,
        "the pid the proxy reports delegating to must match the pid in \
         <workspace>/.codanna/serve.json"
    );

    // (e) graceful shutdown: SIGINT (not SIGKILL) so the backing server's
    // `shutdown_signal` future (`ctrl_c()`, src/mcp/http_server.rs) fires and
    // its `remove_record` cleanup path actually runs, proving serve.json is
    // removed and no orphan process remains -- not merely that this test's
    // `Reaper` cleans up after it.
    interrupt_pid(record.pid);

    wait_until(
        || codanna::serve_discovery::read_record(&codanna_dir).is_none(),
        DEADLINE,
        "serve.json to be removed after graceful shutdown",
    );
    wait_until(
        || !codanna::serve_discovery::pid_is_alive(record.pid),
        DEADLINE,
        "backing server process to exit after graceful shutdown",
    );
    assert_eq!(
        count_http_children(workspace.path()),
        0,
        "no orphan `serve --http` process should remain for the workspace after graceful shutdown"
    );
}

/// THE DIAL-SCHEME PROVENANCE TEST.
///
/// Starts a real `codanna serve --https` backing server directly (not via
/// `discover_or_spawn`/`spawn_detached`, which always spawns `--http` and
/// must not be touched by this change), waits for its `Https`-scheme
/// `serve.json` record, then starts a proxy against the SAME workspace and
/// asserts it discovers and dials that record over `https://` -- never
/// spawning its own `--http` child as a shortcut.
#[test]
#[cfg(feature = "https-server")]
fn proxy_discovers_and_dials_existing_https_server() {
    let workspace = prepare_workspace();
    let _reaper = Reaper(workspace.path().to_path_buf());

    let mut https_server = start_https_server(workspace.path());
    let record = wait_for_https_record(workspace.path());
    assert_eq!(
        record.scheme,
        codanna::serve_discovery::ServeScheme::Https,
        "serve.json should record the Https backing server this test started"
    );

    let mut proxy = start_proxy(workspace.path());
    let (_upstream_pid, upstream_port, delegating_line) = await_upstream_with_line(&mut proxy);

    let _ = proxy.kill();
    let _ = proxy.wait();

    // The proxy discovered (not spawned) the existing Https record and
    // reports delegating to that record's exact port.
    assert_eq!(
        upstream_port, record.port,
        "proxy should report delegating to the discovered Https record's port"
    );

    // Dial-scheme provenance: the delegation line must name `https://`, not
    // a hardcoded `http://`.
    assert!(
        delegating_line.contains("https://"),
        "delegation line should report the https:// dial scheme, got: {delegating_line:?}"
    );

    // THE anti-dead-code check: without this, the cheapest wrong
    // implementation -- one that ignores `record.scheme` entirely and always
    // spawns/dials an `--http` child, which would also "work" end-to-end --
    // passes every assertion above. Confirm no such `--http` child exists for
    // this workspace at all.
    assert_eq!(
        count_http_children(workspace.path()),
        0,
        "proxy must not spawn an --http child when an Https backing server is already discoverable"
    );

    // The Https child must be reaped by SIGKILL like any other backing
    // server this suite starts.
    let _ = https_server.kill();
    let _ = https_server.wait();
}

/// THE ONLY CHECK A VERIFICATION BYPASS FAILS.
///
/// After a real `--https` backing server is up and its cert is persisted,
/// overwrite the persisted cert with an UNRELATED self-signed PEM (same SANs,
/// different keypair) before starting the proxy. `serve_tls::pinned_client`
/// pins trust to whatever PEM is on disk at connect time, so the proxy now
/// pins the wrong certificate and its TLS handshake against the real backing
/// server must fail closed. The proxy no longer exits on a failed dial: it
/// stays up, answers the handshake locally, and reports the failure through
/// tool calls. So the load-bearing signal is that EVERY tool call, once the
/// proxy stops saying "starting", is an `isError` result naming the pinned
/// `https://` upstream and "failed to connect" -- a verification bypass
/// (`danger_accept_invalid_certs`, or dropping `tls_certs_only`) would make
/// a call succeed, which this test treats as a failure.
#[tokio::test]
#[cfg(feature = "https-server")]
async fn proxy_refuses_https_when_pinned_cert_does_not_match() {
    let workspace = prepare_workspace();
    let _reaper = Reaper(workspace.path().to_path_buf());

    let https_server = ChildGuard(start_https_server(workspace.path()));
    let record = wait_for_https_record(workspace.path());
    assert_eq!(record.scheme, codanna::serve_discovery::ServeScheme::Https);

    let cert_path = test_server_cert_path(workspace.path());
    assert!(
        cert_path.is_file(),
        "the https server should have persisted a cert at {cert_path:?} before this test \
         overwrites it"
    );

    // Generate an UNRELATED self-signed cert (same SANs as the real one, but
    // an entirely different keypair) and overwrite the pinned file with it.
    let unrelated = rcgen::generate_simple_self_signed(vec![
        "localhost".to_string(),
        "127.0.0.1".to_string(),
        "::1".to_string(),
    ])
    .expect("generate unrelated self-signed certificate");
    std::fs::write(&cert_path, unrelated.cert.pem())
        .expect("overwrite pinned cert with an unrelated certificate");

    // `connect_proxy_client_nowait` sets HOME + XDG_CONFIG_HOME to the same
    // per-test dir the https server used, so the proxy pins the (now
    // overwritten) cert at `cert_path`.
    let client = connect_proxy_client_nowait(workspace.path()).await;

    // Poll until the proxy stops reporting NOT_READY; the settled result must
    // be the dial failure. Any success means the mismatched pin was accepted.
    let result = wait_until_not_not_ready(&client).await;
    let text = result_text(&result);
    assert_eq!(
        result.is_error,
        Some(true),
        "a tool call succeeded through the proxy against a MISMATCHED pinned certificate -- it \
         must fail closed. This is what a TLS verification bypass looks like from the outside. \
         got: {text}"
    );
    // We deliberately do NOT assert on "certificate"/"handshake": rmcp
    // surfaces the failure as a transport/send error without rendering
    // rustls's cause, so the proxy reports only `failed to connect ... error
    // sending request for url (https://...)`. That text proves the proxy
    // dialed the pinned `https://` upstream and could not connect.
    let lowered = text.to_lowercase();
    assert!(
        lowered.contains("https://127.0.0.1") && lowered.contains("failed to connect"),
        "the error result does not show a failed connection to the pinned https:// upstream, so \
         this test is not observing the pinned-cert rejection. got: {text}"
    );

    // The retry call must fail the same way -- never succeed on a later round.
    let second = wait_until_not_not_ready(&client).await;
    assert_eq!(
        second.is_error,
        Some(true),
        "a retried call succeeded against a MISMATCHED pinned certificate; got: {}",
        result_text(&second)
    );

    client
        .cancel()
        .await
        .expect("proxy client should shut down cleanly after a failed dial");
    drop(https_server);
}

/// THE EMISSION-GATE x PROXY-MODE REGRESSION TEST.
///
/// `main.rs`'s pre-dispatch `needs_indexer` predicate (`src/main.rs:307-320`)
/// gates a full-process read of `index.meta`'s `emission_version` against the
/// binary's `EMISSION_SEMANTICS_VERSION`, refusing to proceed on a mismatch
/// (`src/main.rs:392-417`). `needs_indexer` is `&& !is_proxy_serve(..)`
/// (`src/main.rs:320`, `is_proxy_serve` at `src/main.rs:200-216`) because
/// `serve --proxy` never constructs its own `IndexFacade` in-process -- it
/// only discovers-or-spawns a backing `codanna serve --http` and relays
/// stdio traffic to it (`src/mcp/proxy.rs`'s `serve_proxy`).
///
/// A ONE-ARMED "proxy starts against a corrupted index" test would prove
/// nothing: a fresh workspace's `index.meta` always carries the current
/// `emission_version` (stamped at build time by `persistence.rs`), so the
/// gate never fires and the proxy would "pass" even with the exemption
/// deleted. This test is deliberately two-armed:
///
/// - Arm A is the CONTROL. It proves the gate is actually ARMED against the
///   on-disk state this test writes: a plain, non-proxy `codanna serve`
///   process reads that state at its own startup and is refused.
/// - Arm B is the ASSERTION. Against the exact SAME on-disk state, a NEW
///   `codanna serve --proxy` process is not refused at its own startup (the
///   `is_proxy_serve` exemption) and reaches the "Proxy: delegating to
///   backing MCP server at ..." line by discovering an already-live backing
///   server that finished loading its `IndexFacade` into memory BEFORE this
///   test corrupts `index.meta` on disk. That ordering is what makes the
///   scenario real rather than contrived: the backing server's in-memory
///   state is unaffected by the later on-disk corruption (it never re-reads
///   `index.meta` mid-flight), while any FRESH process -- proxy or not --
///   reading that same file at startup sees the mismatch.
///
/// Defect each arm discriminates if `src/main.rs:320`'s
/// `&& !is_proxy_serve(&cli.command, &config)` is deleted (or otherwise
/// broken, e.g. inverted or scoped to the wrong `Commands::Serve` variant):
///
/// - Arm A would be unaffected (it never touches `is_proxy_serve`) -- it is
///   the control that proves the corrupted `index.meta` this test writes is
///   real and load-bearing, not a no-op fixture.
/// - Arm B would FAIL: the proxy process's own `needs_indexer` would become
///   `true` again, so main.rs's emission gate would fire *in the proxy's own
///   process* against the corrupted on-disk `index.meta` before `serve_proxy`
///   (and therefore `discover_or_spawn`) is ever reached. The proxy would
///   print the same "Error: index emission semantics changed ..." refusal
///   Arm A asserts on and exit with `IndexCorrupted`, never printing the
///   delegating line -- `await_upstream` would time out and panic.
#[test]
fn proxy_mode_is_exempt_from_the_emission_gate_that_refuses_plain_serve() {
    let workspace = prepare_workspace();
    let _reaper = Reaper(workspace.path().to_path_buf());

    // Warm a healthy backing server FIRST, while `index.meta` still carries
    // the current `emission_version` `prepare_workspace`'s index build
    // stamped. `start_proxy` + `await_upstream` is the existing
    // discover-or-spawn harness reused verbatim; killing this first proxy
    // process only stops the stdio relay, not the detached backing
    // `serve --http` server it spawned (same pattern as
    // `killed_server_is_respawned_and_record_is_updated` and
    // `second_proxy_shares_one_upstream_one_record_one_pid` above).
    let mut warm_proxy = start_proxy(workspace.path());
    let (backing_pid, _backing_port) = await_upstream(&mut warm_proxy);
    let _ = warm_proxy.kill();
    let _ = warm_proxy.wait();
    assert!(
        codanna::serve_discovery::pid_is_alive(backing_pid),
        "the backing server warmed for this test should still be alive after only the stdio \
         proxy relay in front of it was killed"
    );

    // Deliberately write a mismatched `emission_version` into the on-disk
    // `index.meta` the fixture's index build produced -- the mismatch this
    // test's two arms observe is WRITTEN here, not assumed.
    let index_dir = crate::support::current_generation_dir(workspace.path());
    let mut meta = codanna::storage::IndexMetadata::load(&index_dir)
        .expect("load the index.meta written by prepare_workspace's index build");
    assert_eq!(
        meta.emission_version,
        Some(codanna::storage::EMISSION_SEMANTICS_VERSION),
        "prepare_workspace's index build should have stamped the binary's current \
         emission_version before this test corrupts it"
    );
    meta.emission_version = Some(codanna::storage::EMISSION_SEMANTICS_VERSION + 999);
    meta.save(&index_dir)
        .expect("write the deliberately mismatched emission_version to index.meta");

    // Arm A (control): a FRESH, non-proxy `codanna serve` process reads this
    // on-disk state at its own startup and is refused by the emission gate.
    //
    // Spawned and polled rather than routed through `run_cli`, because
    // upstream v0.10.1 made this path enter an rmcp stdio serve loop before
    // exiting: the gate now serves a degraded zero-tool handshake so
    // client-spawned servers receive the heal command over the protocol
    // instead of on a stderr nobody reads. Closed stdin ends that session at
    // once, but `run_cli` uses `.output()`, which would block FOREVER rather
    // than fail if a future rmcp ever stopped returning on stdin EOF. This
    // file's `DEADLINE` exists precisely so a stuck server fails the test
    // instead of hanging CI, and before v0.10.1 `serve` could not block here
    // at all -- bounding this wait restores that invariant.
    let test_home = workspace.path().join(".home");
    std::fs::create_dir_all(&test_home).expect("create test home");
    let mut serve = Command::new(codanna_binary())
        .args(["serve"])
        .current_dir(workspace.path())
        .env("HOME", &test_home)
        .env("XDG_CONFIG_HOME", &test_home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        // Left undrained while we poll below: the gate emits three short
        // lines, orders of magnitude under the pipe buffer, so it cannot
        // fill and deadlock the child on write. Anything that materially
        // increases this path's stderr output must drain concurrently.
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn non-proxy codanna serve");

    let status = wait_for_self_exit(&mut serve, DEADLINE).unwrap_or_else(|| {
        panic!(
            "non-proxy `codanna serve` did not exit on its own within {DEADLINE:?} against the \
             deliberately mismatched index.meta -- the emission gate's degraded stdio handshake \
             is not returning on stdin EOF, which would hang CI rather than fail this assertion"
        )
    });

    let mut stderr = String::new();
    {
        use std::io::Read;
        serve
            .stderr
            .take()
            .expect("piped serve stderr")
            .read_to_string(&mut stderr)
            .expect("read serve stderr");
    }

    assert_eq!(
        status.code(),
        Some(codanna::io::ExitCode::IndexCorrupted as i32),
        "non-proxy `codanna serve` should exit with the emission gate's IndexCorrupted code \
         against the deliberately mismatched index.meta; stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("index emission semantics changed"),
        "non-proxy `codanna serve` should print the emission gate's specific refusal message, \
         not merely exit non-zero for some unrelated reason; stderr:\n{stderr}"
    );

    // Arm B (the actual assertion): against the exact SAME on-disk state, a
    // NEW `codanna serve --proxy` process is not refused at its own startup
    // and reaches the delegating line -- because it discovers the
    // already-warmed backing server above rather than trying (and failing)
    // to spawn a new one.
    let mut proxy = start_proxy(workspace.path());
    let (delegated_pid, _delegated_port) = await_upstream(&mut proxy);
    let _ = proxy.kill();
    let _ = proxy.wait();

    assert_eq!(
        delegated_pid, backing_pid,
        "the proxy should delegate to the SAME already-warmed backing server rather than \
         attempting to spawn a new one -- a fresh spawn would hit its own emission-gate \
         refusal against the corrupted on-disk index.meta and this test would instead time \
         out in await_upstream"
    );
}

/// Deadline-bounded wait for `pid` to actually stop RUNNING -- either
/// exited outright or reduced to a zombie (`Z` state in `/proc/<pid>/stat`).
///
/// `serve_discovery::pid_is_alive` (mere `/proc/<pid>` existence) is
/// deliberately NOT used here: in the two tests below, the process doing the
/// killing is a still-live `codanna serve --proxy` process that is the
/// PARENT of the backing server being killed (via `spawn_detached`) and,
/// unlike the OTHER `kill_pid`-based tests in this file, is never itself
/// killed-and-`wait()`-ed first. A killed child whose parent never reaps it
/// stays a zombie -- present in the process table, so `pid_is_alive` would
/// wait past `DEADLINE` and never observe it as gone. `discover_or_spawn`'s
/// own `decide()` already treats a zombie correctly regardless (a zombie's
/// `/proc/<pid>/cmdline` reads back empty, so `pid_looks_like_codanna_serve`
/// returns `false` and `decide()` returns `Spawn`); this helper mirrors that
/// same "exited or zombie" liveness definition for the test's own polling.
fn wait_until_dead_or_zombie(pid: u32, deadline: Duration) {
    wait_until(
        || !process_is_running(pid),
        deadline,
        &format!("pid {pid} to stop running (exit or zombie)"),
    );
}

/// Linux-specific (this suite runs on Linux): reads `/proc/<pid>/stat`'s
/// state field. Returns `false` if the process is gone entirely OR is a
/// zombie (`Z`) -- i.e. "no longer doing anything", regardless of whether
/// its process-table entry has been reaped yet.
fn process_is_running(pid: u32) -> bool {
    let Ok(contents) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    // Format: "pid (comm) state ...". `comm` can itself contain spaces or
    // parentheses, so find the LAST ')' to skip past it before reading the
    // state field that immediately follows.
    let Some((_, after_comm)) = contents.rsplit_once(')') else {
        return false;
    };
    !matches!(after_comm.trim_start().chars().next(), Some('Z') | None)
}

/// Like [`prepare_workspace`], but with `[server] auto_spawn = false` so the
/// proxy may attach to an already-live backing server but must never spawn
/// one of its own.
fn prepare_workspace_with_auto_spawn_disabled() -> TempDir {
    let workspace = prepare_workspace();
    let codanna_dir = workspace.path().join(".codanna");
    std::fs::write(
        codanna_dir.join("settings.toml"),
        r#"
index_path = ".codanna/index"

[semantic_search]
enabled = false

[server]
auto_spawn = false
"#,
    )
    .expect("rewrite settings.toml with auto_spawn = false");
    workspace
}

/// Start `codanna serve --http --bind 127.0.0.1:0` rooted at `ws` directly
/// (bypassing `discover_or_spawn`), so a test can control exactly when a
/// backing server exists independent of a proxy's own spawn decision --
/// needed by [`proxy_revive_respects_auto_spawn_disabled`], where the proxy
/// itself must never be the one to spawn it.
fn start_http_server(ws: &Path) -> Child {
    let test_home = ws.join(".home");
    std::fs::create_dir_all(&test_home).expect("create test home");

    Command::new(codanna_binary())
        .args(["serve", "--http", "--bind", "127.0.0.1:0"])
        .current_dir(ws)
        .env("HOME", &test_home)
        .env("XDG_CONFIG_HOME", &test_home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn codanna serve --http")
}

/// Deadline-bounded wait for `<ws>/.codanna/serve.json` to exist at all,
/// returning the converged record.
fn wait_for_record(ws: &Path) -> codanna::serve_discovery::ServeRecord {
    let codanna_dir = ws.join(".codanna");
    wait_until(
        || codanna::serve_discovery::read_record(&codanna_dir).is_some(),
        DEADLINE,
        "serve.json to exist",
    );
    codanna::serve_discovery::read_record(&codanna_dir)
        .expect("serve.json should exist once wait_until above observed it")
}

/// THE PROXY-LEVEL REVIVE REGRESSION TEST.
///
/// Drives a single long-lived stdio client connection through a real
/// `codanna serve --proxy` process: a readiness wait proves the initial
/// connection is live, then the backing `serve --http` process is SIGKILLed
/// out from under it (skipping graceful shutdown, so `serve.json` is left
/// stale -- the scenario `UpstreamHandle::revive` exists to recover from). The
/// next tool call ON THE SAME CLIENT CONNECTION must NOT block on an inline
/// revive: the proxy flips the dead connection back to "connecting", starts a
/// background dial, and answers NOT_READY immediately. Polling the same
/// session then yields a successful result from a genuinely new backing
/// server.
///
/// Notification continuity after a revive is covered by the unit test
/// `revive_preserves_downstream_state` in `src/mcp/proxy.rs`.
#[tokio::test]
async fn proxy_revives_dead_upstream_mid_session() {
    let workspace = prepare_workspace();
    let _reaper = Reaper(workspace.path().to_path_buf());
    let codanna_dir = workspace.path().join(".codanna");

    // `connect_proxy_client` returns only once `get_index_info` stops
    // reporting NOT_READY, i.e. the initial connection is live.
    let client = connect_proxy_client(workspace.path()).await;

    let record_before = codanna::serve_discovery::read_record(&codanna_dir)
        .expect("serve.json should exist once the proxy has converged on a backing server");
    let pid_before = record_before.pid;

    // SIGKILL (not graceful shutdown) so the killed process cannot run its
    // own `remove_record` cleanup: serve.json is left naming a now-dead pid,
    // exactly the stale-record scenario `discover_or_spawn` (driven here via
    // `UpstreamHandle::revive`) must recover from.
    kill_pid(pid_before);
    wait_until_dead_or_zombie(pid_before, DEADLINE);

    // First call after the kill, ON THE SAME CLIENT CONNECTION: the dead
    // transport is detected, the slot flips to connecting, and the call
    // returns NOT_READY without waiting for the revive.
    let first_after_kill = call_get_index_info(&client).await;
    assert!(
        is_not_ready(&first_after_kill),
        "the first call after the backend dies must return NOT_READY immediately (non-blocking \
         flip), got: {first_after_kill:?}"
    );

    // Polling the same session must reach a successful result served by the
    // revived backing server.
    let revived = wait_until_not_not_ready(&client).await;
    assert_ne!(
        revived.is_error,
        Some(true),
        "the call should succeed once the proxy has revived its upstream, got: {}",
        result_text(&revived)
    );

    let record_after = codanna::serve_discovery::read_record(&codanna_dir)
        .expect("serve.json should exist again after the proxy revived its upstream");
    assert_ne!(
        record_after.pid, pid_before,
        "the revived backing server must be a genuinely new process, not the killed one"
    );
    assert!(
        codanna::serve_discovery::pid_is_alive(record_after.pid),
        "the revived backing server's recorded pid should actually be alive"
    );

    client
        .cancel()
        .await
        .expect("proxy client should shut down cleanly");
}

/// THE AUTO-SPAWN-DISABLED REVIVE REGRESSION TEST.
///
/// With `[server] auto_spawn = false`, the proxy must still become ready by
/// discovering an already-live backing server started manually (discovery of a
/// live record is not subject to the auto_spawn guard; only spawning a new
/// one is -- see `discover_or_spawn`'s "Guard 2" in `src/serve_discovery.rs`).
/// Once that backing server is killed, the first call returns NOT_READY (the
/// non-blocking mid-session flip), and polling then settles on an `isError`
/// result carrying the actionable FAILED text that names auto-spawn as the fix
/// and the workspace, rather than silently spawning a server anyway or hanging.
#[tokio::test]
async fn proxy_revive_respects_auto_spawn_disabled() {
    let workspace = prepare_workspace_with_auto_spawn_disabled();
    let _reaper = Reaper(workspace.path().to_path_buf());

    // Start the backing server manually: with `auto_spawn = false` the proxy
    // itself would refuse to create one, so readiness below can only come
    // from discovering this already-live process.
    let mut http_server = ChildGuard(start_http_server(workspace.path()));
    let record = wait_for_record(workspace.path());
    let pid_before = record.pid;

    // Readiness wait: succeeds against the manually-started backing server.
    let client = connect_proxy_client(workspace.path()).await;

    kill_pid(pid_before);
    wait_until_dead_or_zombie(pid_before, DEADLINE);
    let _ = http_server.0.wait();

    // First call after the kill: non-blocking flip, so NOT_READY.
    let first_after_kill = call_get_index_info(&client).await;
    assert!(
        is_not_ready(&first_after_kill),
        "the first call after the backend dies must return NOT_READY immediately, got: \
         {first_after_kill:?}"
    );

    // Poll until the failed dial round settles. It must surface as an error
    // RESULT (not a protocol error, not a hang) with the actionable text.
    let failed = wait_until_not_not_ready(&client).await;
    assert_eq!(
        failed.is_error,
        Some(true),
        "with the backend dead and auto_spawn = false the call must be an error result, got: {}",
        result_text(&failed)
    );
    let text = result_text(&failed);
    let message = text.to_lowercase();
    assert!(
        message.starts_with(FAILED_PREFIX),
        "the failure should carry the stable FAILED prefix, got: {text}"
    );
    assert!(
        message.contains("auto-spawn") || message.contains("auto_spawn"),
        "the revive failure should name auto-spawn as the actionable fix, got: {text}"
    );
    let workspace_name = workspace
        .path()
        .file_name()
        .and_then(|name| name.to_str())
        .expect("temp workspace path should have a directory name")
        .to_lowercase();
    assert!(
        message.contains(&workspace_name),
        "the revive failure should name the workspace root, got: {text}"
    );

    // The proxy process itself must still be responsive to a clean shutdown,
    // not have panicked or wedged while handling the failed revive.
    client
        .cancel()
        .await
        .expect("proxy client should shut down cleanly even after a failed delegated call");
}

/// While the backing server is still starting (artificially delayed), the
/// proxy answers the handshake and `tools/list` locally, reports NOT_READY on
/// tool calls, and has already registered itself as a healthy proxy; the same
/// session then becomes ready and relays a real `find_symbol`.
#[tokio::test]
async fn proxy_answers_locally_and_reports_not_ready_while_backend_spawns() {
    let workspace = prepare_workspace();
    let home = workspace.path().join(".home");
    let codanna_dir = workspace.path().join(".codanna");
    let _reaper = Reaper(workspace.path().to_path_buf());
    let _registry_reaper = RegistryReaper {
        home: home.clone(),
        ws: workspace.path().to_path_buf(),
    };

    let client = connect_proxy_client_with_env(
        workspace.path(),
        &[("CODANNA_TEST_SPAWN_DELAY_MS", "10000")],
    )
    .await;

    // Local list_tools: all 13 names, with the backend still asleep.
    let tools = tokio::time::timeout(DEADLINE, client.list_tools(Default::default()))
        .await
        .expect("list_tools should complete within the deadline")
        .expect("list_tools must succeed locally while the backend is starting");
    let mut names: Vec<&str> = tools.tools.iter().map(|t| t.name.as_ref()).collect();
    names.sort_unstable();
    let mut expected = EXPECTED_TOOLS.to_vec();
    expected.sort_unstable();
    assert_eq!(
        names, expected,
        "the proxy must advertise the backend's full tool set locally"
    );

    // Tool calls report NOT_READY as an error result.
    let early = call_get_index_info(&client).await;
    assert!(
        is_not_ready(&early),
        "a tool call while the backend is spawning must return NOT_READY, got: {early:?}"
    );

    // At that moment: no serve.json, backend entry Spawning, proxy entry
    // Healthy with role Proxy.
    let ws = workspace.path().to_path_buf();
    wait_until_async(
        || {
            registry_entries_for_workspace(&home, &ws).iter().any(|e| {
                e.role == codanna::serve_registry::ServerRole::Server
                    && e.status == codanna::serve_registry::ServerStatus::Spawning
            })
        },
        "backend registry entry to be Spawning",
    )
    .await;
    assert!(
        codanna::serve_discovery::read_record(&codanna_dir).is_none(),
        "serve.json must not exist while the backend is still spawning"
    );
    let entries = registry_entries_for_workspace(&home, workspace.path());
    let proxy_entries: Vec<_> = entries
        .iter()
        .filter(|e| e.role == codanna::serve_registry::ServerRole::Proxy)
        .collect();
    assert_eq!(
        proxy_entries.len(),
        1,
        "exactly one proxy registry entry expected, got: {entries:?}"
    );
    assert_eq!(
        proxy_entries[0].status,
        codanna::serve_registry::ServerStatus::Healthy,
        "the proxy registers itself Healthy (never Spawning) before the backend is ready"
    );

    // Same session: poll until ready, then a real relayed find_symbol.
    let ready = wait_until_not_not_ready(&client).await;
    assert_ne!(
        ready.is_error,
        Some(true),
        "the backend should become ready, got: {}",
        result_text(&ready)
    );
    let found = tokio::time::timeout(
        DEADLINE,
        client.call_tool(
            rmcp::model::CallToolRequestParams::new("find_symbol").with_arguments(
                serde_json::json!({ "name": "codanna_proxy_e2e_marker" })
                    .as_object()
                    .cloned()
                    .expect("json object literal"),
            ),
        ),
    )
    .await
    .expect("find_symbol should complete within the deadline")
    .expect("find_symbol should succeed once the backend is ready");
    assert!(
        result_text(&found).contains("codanna_proxy_e2e_marker"),
        "find_symbol on the same session should return the fixture marker, got: {found:?}"
    );

    client
        .cancel()
        .await
        .expect("proxy client should shut down cleanly");
}

/// Cancelling the client while the backend is still spawning makes the proxy
/// exit promptly instead of waiting out the dial: the detached backend keeps
/// running (its Spawning entry survives) and the proxy leaves no `http.lock`.
#[tokio::test]
async fn proxy_exits_promptly_when_client_cancels_during_backend_spawn() {
    const SPAWN_DELAY: Duration = Duration::from_secs(20);

    let workspace = prepare_workspace();
    let home = workspace.path().join(".home");
    let codanna_dir = workspace.path().join(".codanna");
    let _reaper = Reaper(workspace.path().to_path_buf());
    let _registry_reaper = RegistryReaper {
        home: home.clone(),
        ws: workspace.path().to_path_buf(),
    };

    let client = connect_proxy_client_with_env(
        workspace.path(),
        &[("CODANNA_TEST_SPAWN_DELAY_MS", "20000")],
    )
    .await;

    // Wait for both the detached backend (Spawning) and the proxy's own
    // entry, remembering their pids.
    let ws = workspace.path().to_path_buf();
    wait_until_async(
        || {
            let entries = registry_entries_for_workspace(&home, &ws);
            entries.iter().any(|e| {
                e.role == codanna::serve_registry::ServerRole::Server
                    && e.status == codanna::serve_registry::ServerStatus::Spawning
            }) && entries
                .iter()
                .any(|e| e.role == codanna::serve_registry::ServerRole::Proxy)
        },
        "backend Spawning entry and proxy entry to be registered",
    )
    .await;
    let entries = registry_entries_for_workspace(&home, workspace.path());
    let spawning_pid = entries
        .iter()
        .find(|e| e.role == codanna::serve_registry::ServerRole::Server)
        .expect("spawning backend entry")
        .pid;
    let proxy_pid = entries
        .iter()
        .find(|e| e.role == codanna::serve_registry::ServerRole::Proxy)
        .expect("proxy entry")
        .pid;

    let started = std::time::Instant::now();
    client
        .cancel()
        .await
        .expect("proxy client should shut down cleanly");
    wait_until_dead_or_zombie(proxy_pid, DEADLINE);
    let elapsed = started.elapsed();

    assert!(
        elapsed < SPAWN_DELAY / 2,
        "the proxy must self-exit well before the {SPAWN_DELAY:?} backend delay elapses, took \
         {elapsed:?}"
    );
    assert!(
        process_is_running(spawning_pid),
        "the detached Spawning backend (pid {spawning_pid}) must survive the proxy's exit"
    );
    assert!(
        !codanna_dir.join("http.lock").exists(),
        "dropping the in-flight dial must release http.lock"
    );
    assert!(
        registry_entries_for_workspace(&home, workspace.path())
            .iter()
            .all(|e| e.pid != proxy_pid),
        "the exited proxy must remove its own registry entry"
    );
}
