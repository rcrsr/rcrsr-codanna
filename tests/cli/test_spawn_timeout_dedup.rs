//! Real-subprocess coverage for spawn-timeout duplicate prevention in
//! `serve_discovery::discover_or_spawn` (see the doc comments on its "Guard
//! 3" section and `wait_on_spawning_pid`).
//!
//! Mirrors the pattern established in `test_serve_proxy_discovery.rs` (real
//! `codanna serve --proxy` child processes racing `discover_or_spawn`) and
//! `test_serve_registry.rs` (reading the per-user server registry directly
//! off disk under a per-test `HOME`/`XDG_CONFIG_HOME`), combined to exercise
//! the specific regression this file targets:
//!
//! 1. Proxy #1 is started with a deliberately tiny `spawn_timeout_ms` (via
//!    `--config`, so only ITS OWN `discover_or_spawn` call is affected -- the
//!    spawned backing server's own runtime is unaffected by this value) AND
//!    `CODANNA_TEST_SPAWN_DELAY_MS` set, a test-only hook in
//!    `serve_discovery::build_spawn_command` that makes the spawned backing
//!    server sleep for a controlled duration before it starts up --
//!    deterministically simulating the "cold index build / embedding-model
//!    download that legitimately exceeds `spawn_timeout_ms`" scenario this
//!    fix targets, without needing an actually-slow fixture (measured on this
//!    machine: a real cold start with a tiny fixture converges in ~10-30ms,
//!    far too fast and too close to noise to race deterministically against a
//!    "tens of milliseconds" timeout). Proxy #1's wait for the backing server
//!    to become healthy times out well before the delay elapses, so it exits
//!    with a `SpawnTimeout` error -- but the backing server it spawned is
//!    deliberately left running (see `discover_or_spawn`'s winner-branch doc
//!    comment).
//! 2. Proxy #2 is spawned THE INSTANT proxy #1 exits, using the workspace's
//!    DEFAULT (generous) `spawn_timeout_ms` and no delay hook, while the
//!    backing server is still asleep (well before the delay elapses). Before
//!    the fix, proxy #2 would find `http.lock` free (proxy #1's guard dropped
//!    on timeout) and spawn a SECOND backing server. After the fix, proxy #2
//!    finds proxy #1's `Spawning` registry entry
//!    (`serve_registry::find_spawning_for`) and waits on that same pid
//!    instead, observing it converge to `Healthy` once the delay elapses.
//!
//! The registry (not just "no error") is the load-bearing assertion surface:
//! reading `<home>/.local/state/codanna/servers/*.json` directly, mirroring
//! `test_serve_registry.rs::registry_dir_for`, so this test observes the
//! exact mechanism the fix relies on rather than merely inferring success
//! from the two processes' exit codes.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

use codanna::serve_registry::{RegistryEntry, ServerRole, ServerStatus};
use tempfile::TempDir;

use crate::support::codanna_binary;

/// Upper bound for every blocking wait in this file so a stuck process fails
/// the test instead of hanging CI.
const DEADLINE: Duration = Duration::from_secs(30);

/// Deliberately tiny relative to [`SPAWN_DELAY_MS`]: proxy #1's own
/// `discover_or_spawn` call is guaranteed to time out long before the
/// artificially delayed backing server can become healthy.
const SHORT_SPAWN_TIMEOUT_MS: u64 = 100;

/// How long `CODANNA_TEST_SPAWN_DELAY_MS` makes the backing server sleep
/// before starting up -- comfortably longer than [`SHORT_SPAWN_TIMEOUT_MS`]
/// (so proxy #1 reliably times out while it is still asleep) and comfortably
/// longer than the time it takes this test's harness to detect proxy #1's
/// exit and spawn proxy #2 (so the race window this test drives is wide, not
/// a hair's-breadth timing coincidence), while still well under proxy #2's
/// default `spawn_timeout_ms` (8000ms) and this file's [`DEADLINE`].
const SPAWN_DELAY_MS: u64 = 1500;

/// Build a workspace with a unique fixture symbol, semantic search disabled,
/// and an already-built index, ready for `codanna serve --proxy` to
/// discover/spawn against. Mirrors `test_serve_proxy_discovery.rs::prepare_workspace`.
fn prepare_workspace() -> TempDir {
    let workspace = TempDir::new().expect("create temp workspace");

    let src_dir = workspace.path().join("src");
    std::fs::create_dir_all(&src_dir).expect("create src dir");
    std::fs::write(
        src_dir.join("lib.rs"),
        r#"
/// Unique marker symbol used only by the spawn-timeout-dedup e2e test.
pub fn codanna_spawn_timeout_dedup_marker() -> i32 {
    7
}
"#,
    )
    .expect("write fixture source");

    let codanna_dir = workspace.path().join(".codanna");
    std::fs::create_dir_all(&codanna_dir).expect("create .codanna dir");
    // The workspace's OWN settings.toml (required by discover_or_spawn's
    // NoConfiguration guard) deliberately does NOT override spawn_timeout_ms
    // -- proxy #2 below relies on the generous default (8000ms) so its wait
    // on proxy #1's Spawning registry entry has ample room to observe the
    // backing server converge to healthy.
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

/// Write a SEPARATE config file (not `<ws>/.codanna/settings.toml`) carrying
/// the same index configuration plus a deliberately tiny
/// `[server] spawn_timeout_ms`, for proxy #1 to load via `--config`. Kept
/// separate from the workspace's own `settings.toml` so proxy #2 (started
/// without `--config`) uses the generous default instead.
fn write_short_timeout_config(ws: &Path) -> PathBuf {
    let path = ws.join("short-timeout-config.toml");
    std::fs::write(
        &path,
        format!(
            r#"
index_path = ".codanna/index"

[semantic_search]
enabled = false

[server]
spawn_timeout_ms = {SHORT_SPAWN_TIMEOUT_MS}
"#
        ),
    )
    .expect("write short-timeout config");
    path
}

/// Start `codanna serve --proxy` rooted at `ws`, optionally with `--config
/// <config_path>`, sharing `home` as `HOME`/`XDG_CONFIG_HOME` so every
/// process in this test sees the same per-user server registry. When
/// `spawn_delay_ms` is `Some`, sets `CODANNA_TEST_SPAWN_DELAY_MS` in this
/// process's OWN environment (not the backing server's): the hook is read by
/// `serve_discovery::build_spawn_command` inside THIS process's own
/// `discover_or_spawn` call, which bakes the delay into the backing server it
/// spawns -- so only a proxy call that actually spawns (the winner) needs it
/// set.
fn start_proxy(
    ws: &Path,
    home: &Path,
    config_path: Option<&Path>,
    spawn_delay_ms: Option<u64>,
) -> Child {
    let mut args: Vec<String> = Vec::new();
    if let Some(config_path) = config_path {
        args.push("--config".to_string());
        args.push(config_path.to_string_lossy().into_owned());
    }
    args.push("serve".to_string());
    args.push("--proxy".to_string());

    let mut cmd = Command::new(codanna_binary());
    cmd.args(&args)
        .current_dir(ws)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(delay_ms) = spawn_delay_ms {
        cmd.env("CODANNA_TEST_SPAWN_DELAY_MS", delay_ms.to_string());
    }
    cmd.spawn().expect("spawn codanna serve --proxy")
}

/// Deadline-bounded, TIGHT-poll wait for `child` to exit on its own, without
/// touching its stdout/stderr pipes. Split out from output-draining
/// (`drain_output` below) so the caller can spawn proxy #2 the INSTANT
/// proxy #1 exits, rather than after the extra latency of reading its pipes
/// first -- that gap is exactly the window the race this test drives depends
/// on being as small as possible (the backing server's real cold-start time
/// is only ~tens of milliseconds longer than the deliberately tiny
/// `spawn_timeout_ms` proxy #1 uses). Panics (rather than hanging CI) if the
/// child is still running at `deadline`.
fn wait_for_exit_status(child: &mut Child, deadline: Duration) -> std::process::ExitStatus {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().expect("poll child exit status") {
            return status;
        }
        assert!(
            start.elapsed() < deadline,
            "process did not exit on its own within {deadline:?}"
        );
        thread::sleep(Duration::from_micros(200));
    }
}

/// Read the remainder of `child`'s stdout/stderr pipes to completion. Only
/// meaningful after `child` has already exited (see `wait_for_exit_status`).
fn drain_output(child: &mut Child) -> (String, String) {
    use std::io::Read;
    let mut stdout = String::new();
    let mut stderr = String::new();
    if let Some(mut out) = child.stdout.take() {
        let _ = out.read_to_string(&mut stdout);
    }
    if let Some(mut err) = child.stderr.take() {
        let _ = err.read_to_string(&mut stderr);
    }
    (stdout, stderr)
}

/// Registry directory this test's `home` resolves to, mirroring
/// `test_serve_registry.rs::registry_dir_for` (`serve_registry::registry_dir()`'s
/// own resolution: `dirs::state_dir().or_else(dirs::data_dir)/codanna/servers`).
/// On Linux (the CI platform) with no `XDG_STATE_HOME` set, `dirs::state_dir()`
/// falls back to `$HOME/.local/state`.
fn registry_dir_for(home: &Path) -> PathBuf {
    home.join(".local")
        .join("state")
        .join("codanna")
        .join("servers")
}

/// Read every parseable registry entry under `home`'s registry directory
/// whose `workspace_root` matches `ws` (canonicalized on both sides so a
/// tempdir's possible symlink normalization does not spuriously mismatch).
fn registry_entries_for_workspace(home: &Path, ws: &Path) -> Vec<RegistryEntry> {
    let canonical_ws = ws.canonicalize().expect("canonicalize workspace root");
    let dir = registry_dir_for(home);
    let Ok(read_dir) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    read_dir
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let contents = std::fs::read_to_string(entry.path()).ok()?;
            let parsed: RegistryEntry = serde_json::from_str(&contents).ok()?;
            let entry_root = parsed.workspace_root.canonicalize().ok()?;
            (entry_root == canonical_ws).then_some(parsed)
        })
        .collect()
}

/// Deadline-bounded poll: panics rather than hanging if `predicate` never
/// becomes true within `deadline`.
fn wait_until(mut predicate: impl FnMut() -> bool, deadline: Duration, what: &str) {
    let start = Instant::now();
    loop {
        if predicate() {
            return;
        }
        assert!(start.elapsed() < deadline, "timed out waiting for: {what}");
        thread::sleep(Duration::from_millis(50));
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

/// Best-effort SIGKILL, used only for teardown -- this test's whole point is
/// that the backing server is NEVER killed by `discover_or_spawn` itself.
fn kill_pid_externally(pid: u32) {
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

/// Kills the backing server recorded for `ws` in `home`'s registry (if any)
/// when dropped, so a failing assertion never leaks a detached backing
/// server process.
struct Reaper {
    home: PathBuf,
    ws: PathBuf,
}

impl Drop for Reaper {
    fn drop(&mut self) {
        for entry in registry_entries_for_workspace(&self.home, &self.ws) {
            kill_pid_externally(entry.pid);
        }
    }
}

/// THE LOAD-BEARING REGRESSION TEST.
///
/// Before the fix: proxy #1's `SpawnTimeout` drops its `http.lock` guard
/// while the child it spawned keeps running; proxy #2 then re-acquires the
/// now-free lock and calls `spawn_detached` again, producing a SECOND backing
/// server. After the fix: proxy #2 discovers proxy #1's `Spawning` registry
/// entry via `find_spawning_for` and waits on that same pid instead.
#[test]
fn second_proxy_waits_on_spawning_entry_instead_of_duplicating() {
    let workspace = prepare_workspace();
    let home = workspace.path().join(".home");
    std::fs::create_dir_all(&home).expect("create test home");
    let short_config = write_short_timeout_config(workspace.path());

    let _reaper = Reaper {
        home: home.clone(),
        ws: workspace.path().to_path_buf(),
    };

    // Proxy #1: tiny spawn_timeout_ms, guaranteed to time out waiting for the
    // backing server it spawns to become healthy.
    let mut proxy1 = start_proxy(
        workspace.path(),
        &home,
        Some(&short_config),
        Some(SPAWN_DELAY_MS),
    );
    let status1 = wait_for_exit_status(&mut proxy1, DEADLINE);

    // Proxy #2 is spawned THE INSTANT proxy #1's exit is observed -- before
    // draining proxy #1's pipes or making any assertions on it -- to keep the
    // race window (the real backing server's cold-start time minus proxy
    // #1's own deliberately tiny `spawn_timeout_ms`) as tight as possible.
    // Uses the workspace's DEFAULT (generous) spawn_timeout_ms, no --config
    // override: if the backing server has not yet published its Healthy
    // registry entry, this exercises the `find_spawning_for` wait path
    // (Guard 3 in `discover_or_spawn`); either way, no second
    // `spawn_detached` must occur.
    let mut proxy2 = start_proxy(workspace.path(), &home, None, None);

    let (_stdout1, stderr1) = drain_output(&mut proxy1);
    assert!(
        !status1.success(),
        "proxy #1 should exit non-zero after its own discover_or_spawn call times out \
         waiting for the backing server to become healthy; stderr:\n{stderr1}"
    );
    assert!(
        stderr1.contains("did not become healthy"),
        "proxy #1 should report the SpawnTimeout error from discover_or_spawn, got stderr:\n{stderr1}"
    );

    // The Spawning (or, if the race lost, already-Healthy) registry entry
    // proxy #1 published before waiting must be on disk, naming a
    // still-alive pid -- NOT killed just because proxy #1's own wait timed
    // out.
    let entries_after_timeout = registry_entries_for_workspace(&home, workspace.path());
    assert_eq!(
        entries_after_timeout.len(),
        1,
        "exactly one registry entry should exist for this workspace right after proxy #1 times \
         out, got: {entries_after_timeout:?}"
    );
    let first_pid = entries_after_timeout[0].pid;
    assert!(
        pid_alive(first_pid),
        "the backing server proxy #1 spawned (pid {first_pid}) must still be running -- a \
         legitimately slow cold start is not a failure to correct by killing"
    );

    let status2 = wait_for_exit_status(&mut proxy2, DEADLINE);
    let (_stdout2, stderr2) = drain_output(&mut proxy2);

    // Proxy #2 must reach a live backing server without erroring -- proving
    // it actually waited on (and observed) the promotion to Healthy, rather
    // than also timing out or refusing to proceed.
    assert!(
        status2.success() || stderr2.contains("delegating to backing MCP server"),
        "proxy #2 should successfully discover the backing server proxy #1 spawned; \
         stderr:\n{stderr2}"
    );

    // The registry must still show exactly ONE entry for this workspace,
    // naming the SAME pid proxy #1's spawn produced -- the central
    // dedup assertion.
    wait_until(
        || {
            registry_entries_for_workspace(&home, workspace.path())
                .iter()
                .any(|e| e.status == ServerStatus::Healthy)
        },
        DEADLINE,
        "the backing server's registry entry to converge to Healthy",
    );
    // Proxy #2 now also publishes its own best-effort `Proxy`-role registry
    // entry once its `Dialer::connect` succeeds, so the dedup assertion must
    // be scoped to `Server`-role entries -- this test is about backing-server
    // duplication, not the (expected) proxy attribution row.
    let entries_final = registry_entries_for_workspace(&home, workspace.path());
    let server_entries_final: Vec<&RegistryEntry> = entries_final
        .iter()
        .filter(|e| e.role == ServerRole::Server)
        .collect();
    assert_eq!(
        server_entries_final.len(),
        1,
        "exactly one Server-role registry entry should exist for this workspace after both \
         proxy calls -- a second entry here means a duplicate backing server was spawned; \
         got: {entries_final:?}"
    );
    assert_eq!(
        server_entries_final[0].pid, first_pid,
        "the single Server-role registry entry after both proxy calls must still name proxy \
         #1's pid, not a second spawned server"
    );

    // The discovery record under <ws>/.codanna/serve.json must also name
    // that same pid -- a second, cross-checking view onto the same
    // invariant via a different on-disk artifact.
    let codanna_dir = workspace.path().join(".codanna");
    let record = codanna::serve_discovery::read_record(&codanna_dir)
        .expect("serve.json should exist once the backing server has converged");
    assert_eq!(
        record.pid, first_pid,
        "serve.json should name the same pid proxy #1's spawn produced, not a duplicate"
    );

    // The backing server must never have been killed -- still alive, same
    // pid, throughout the entire sequence.
    assert!(
        pid_alive(first_pid),
        "the backing server (pid {first_pid}) must still be alive after both proxy calls \
         completed"
    );

    // No orphaned `http.lock` left behind by either proxy call.
    assert!(
        !codanna_dir.join("http.lock").exists(),
        "http.lock should not exist once both proxy calls have settled"
    );
}
