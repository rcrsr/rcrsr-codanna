//! Real-subprocess coverage for early spawn-failure detection in
//! `serve_discovery::discover_or_spawn` / `wait_until_healthy` (see the
//! `SpawnedProcessExited` doc comments on both).
//!
//! Mirrors the pattern established in `test_spawn_timeout_dedup.rs` (a real
//! `codanna serve --proxy` process racing `discover_or_spawn`, with a
//! per-test `HOME`/`XDG_CONFIG_HOME` and a deadline-bounded poll for the
//! child's exit) and `test_serve_registry.rs` (registry isolation via
//! `HOME`), but drives the OTHER test-only env hook in
//! `serve_discovery::build_spawn_command`:
//! `CODANNA_TEST_SPAWN_FAIL_CODE`. When set, the spawned backing server never
//! execs `codanna` at all -- it is a fast `sh -c 'echo <marker> >&2; exit
//! <code>'` that writes a known stderr marker and exits immediately.
//!
//! The load-bearing assertion this file exists to make: `discover_or_spawn`
//! must surface `DiscoveryError::SpawnedProcessExited` (carrying the exit
//! code and stderr marker) the moment the reaper thread observes the child
//! exit, NOT after waiting out the full `spawn_timeout_ms` deadline. The
//! proxy is started with a generous `spawn_timeout_ms` (5000ms) precisely so
//! that a passing "well under 2000ms" wall-clock assertion actually proves
//! early detection rather than merely a short timeout coinciding with a slow
//! failure.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
use tempfile::TempDir;

use codanna::serve_registry::{RegistryEntry, ServerRole};

use crate::support::codanna_binary;

/// Upper bound for every blocking wait in this file so a stuck process fails
/// the test instead of hanging CI.
const DEADLINE: Duration = Duration::from_secs(30);

/// `discover_or_spawn`'s own wait budget for this test's proxy call.
/// Deliberately generous (well above the sub-2s bound this test asserts on)
/// so an early-return result can only be explained by
/// `wait_until_healthy`'s exit-slot check firing before the deadline, never
/// by the deadline itself being short.
const SPAWN_TIMEOUT_MS: u64 = 5000;

/// The `sh -c` fail-fast script's exit code, arbitrary but distinctive
/// enough (not 0, 1, or another code a genuine crash commonly uses) to be
/// unambiguous in the surfaced error message.
const FAIL_CODE: i32 = 42;

/// Wall-clock ceiling this test's core assertion enforces -- comfortably
/// under [`SPAWN_TIMEOUT_MS`], proving the proxy did not wait out the
/// deadline before reporting the failure.
const EARLY_DETECTION_CEILING: Duration = Duration::from_millis(2000);

/// Build a workspace with a unique fixture symbol, semantic search disabled,
/// and an already-built index, ready for `codanna serve --proxy` to
/// discover/spawn against. Mirrors `test_spawn_timeout_dedup.rs::prepare_workspace`.
fn prepare_workspace() -> TempDir {
    let workspace = TempDir::new().expect("create temp workspace");

    let src_dir = workspace.path().join("src");
    std::fs::create_dir_all(&src_dir).expect("create src dir");
    std::fs::write(
        src_dir.join("lib.rs"),
        r#"
/// Unique marker symbol used only by the spawn-exit-detection e2e test.
pub fn codanna_spawn_exit_detection_marker() -> i32 {
    13
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

/// Write a SEPARATE config file (not `<ws>/.codanna/settings.toml`) carrying
/// the same index configuration plus the generous [`SPAWN_TIMEOUT_MS`], for
/// the proxy to load via `--config`. Kept separate from the workspace's own
/// `settings.toml` -- mirrors `test_spawn_timeout_dedup.rs::write_short_timeout_config`.
fn write_config(ws: &Path) -> PathBuf {
    let path = ws.join("spawn-exit-detection-config.toml");
    std::fs::write(
        &path,
        format!(
            r#"
index_path = ".codanna/index"

[semantic_search]
enabled = false

[server]
spawn_timeout_ms = {SPAWN_TIMEOUT_MS}
"#
        ),
    )
    .expect("write config");
    path
}

/// Start `codanna --config <config_path> serve --proxy` rooted at `ws`, with
/// `CODANNA_TEST_SPAWN_FAIL_CODE` set in THIS process's own environment (not
/// the never-execed backing server's): the hook is read by
/// `serve_discovery::build_spawn_command` inside the proxy's own
/// `discover_or_spawn` call, which substitutes the fast-failing `sh -c`
/// script for the real spawn.
fn start_proxy(ws: &Path, home: &Path, config_path: &Path, fail_code: i32) -> Child {
    Command::new(codanna_binary())
        .args([
            "--config",
            config_path.to_string_lossy().as_ref(),
            "serve",
            "--proxy",
        ])
        .current_dir(ws)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home)
        .env("CODANNA_TEST_SPAWN_FAIL_CODE", fail_code.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn codanna serve --proxy")
}

/// Deadline-bounded, tight-poll wait for `child` to exit on its own, without
/// touching its stdout/stderr pipes. Panics (rather than hanging CI) if the
/// child is still running at `deadline`. Mirrors
/// `test_spawn_timeout_dedup.rs::wait_for_exit_status`.
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
/// meaningful after `child` has already exited.
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
/// `test_spawn_timeout_dedup.rs::registry_dir_for`.
fn registry_dir_for(home: &Path) -> PathBuf {
    home.join(".local")
        .join("state")
        .join("codanna")
        .join("servers")
}

/// Read every parseable registry entry under `home`'s registry directory
/// whose `workspace_root` matches `ws`.
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

/// THE LOAD-BEARING REGRESSION TEST.
///
/// With `CODANNA_TEST_SPAWN_FAIL_CODE` set, the "backing server" the proxy
/// spawns is a fast-failing `sh -c` script, not a real `codanna serve`
/// process. `discover_or_spawn` must surface
/// `DiscoveryError::SpawnedProcessExited` (naming the marker and exit code)
/// well before its 5000ms `spawn_timeout_ms` budget elapses -- proving
/// `wait_until_healthy`'s exit-slot check preempts the timeout instead of a
/// fail-loud outcome merely happening to arrive before the deadline anyway.
#[test]
fn spawn_exit_is_detected_before_timeout_elapses() {
    let workspace = prepare_workspace();
    let home = workspace.path().join(".home");
    std::fs::create_dir_all(&home).expect("create test home");
    let config = write_config(workspace.path());

    let start = Instant::now();
    let mut proxy = start_proxy(workspace.path(), &home, &config, FAIL_CODE);
    let status = wait_for_exit_status(&mut proxy, DEADLINE);
    let elapsed = start.elapsed();
    let (_stdout, stderr) = drain_output(&mut proxy);

    assert!(
        !status.success(),
        "proxy should exit non-zero when discover_or_spawn observes the spawned \
         process exit before becoming healthy; stderr:\n{stderr}"
    );

    assert!(
        stderr.contains("codanna-test-spawn-fail-marker"),
        "surfaced error should contain the fixed stderr marker written by the fast-failing \
         sh -c script; stderr:\n{stderr}"
    );
    assert!(
        stderr.contains(&FAIL_CODE.to_string()),
        "surfaced error should contain the fast-failing script's exit code ({FAIL_CODE}); \
         stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("exited"),
        "surfaced error should be the SpawnedProcessExited case, not e.g. a bare timeout; \
         stderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("did not become healthy"),
        "surfaced error must be the early SpawnedProcessExited detection, not a \
         SpawnTimeout that waited out the deadline; stderr:\n{stderr}"
    );

    assert!(
        elapsed < EARLY_DETECTION_CEILING,
        "discover_or_spawn took {elapsed:?} to report the spawn failure, which should be \
         detected almost immediately (well under the {EARLY_DETECTION_CEILING:?} ceiling and \
         far short of the {SPAWN_TIMEOUT_MS}ms spawn_timeout_ms budget) -- a slower result \
         suggests the exit-slot check regressed back to waiting out the timeout"
    );

    // No lingering registry entry should still name a live pid: the
    // fast-failing sh -c script has already exited by the time this
    // assertion runs, so nothing here should require external cleanup.
    for entry in registry_entries_for_workspace(&home, workspace.path()) {
        if entry.role == ServerRole::Server {
            assert!(
                !pid_alive(entry.pid),
                "the fast-failing spawn's pid ({}) must not still be alive",
                entry.pid
            );
        }
    }
}
