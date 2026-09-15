//! Bounded, interrupting `serve --stop`/SIGTERM coverage.
//!
//! (a) drives a real `codanna serve --http --watch` subprocess against a
//! `[file_watch] startup_catch_up = true` fixture large enough to keep a
//! full clear-and-rebuild catch-up reindex in flight for a measurable
//! window, then SIGTERMs it mid-flight. Asserts the process exits within
//! `http_server::SHUTDOWN_GRACE` plus generous scheduling margin, its
//! registry and discovery records are gone, and the *live* index generation
//! (the one published before the server ever started) is untouched -- which
//! proves the unified watcher's cancellation arm aborted the in-flight
//! catch-up build rather than corrupting or blocking shutdown on it.
//!
//! (b) is a hermetic, no-subprocess test of `stop_server`'s SIGTERM ->
//! timeout -> SIGKILL -> reap escalation, driven entirely through the
//! injectable `is_alive` predicate (`wait_for_exit` /
//! `repoll_after_sigkill_and_reap` in `src/cli/commands/serve.rs`) with a
//! stub target that never reports dead, asserting the registry entry is
//! still reaped.

#![cfg(unix)]

use std::env;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use tempfile::TempDir;

fn codanna_binary() -> PathBuf {
    if let Some(path) = option_env!("CARGO_BIN_EXE_codanna") {
        let bin = PathBuf::from(path);
        if bin.exists() {
            return bin;
        }
    }

    let manifest_dir = env::var("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| env::current_dir().expect("current dir"));

    let debug_bin = manifest_dir.join("target/debug/codanna");
    if debug_bin.exists() {
        return debug_bin;
    }

    let status = Command::new("cargo")
        .args(["build", "--bin", "codanna"])
        .current_dir(&manifest_dir)
        .status()
        .expect("build codanna binary");
    assert!(status.success(), "cargo build failed");
    debug_bin
}

/// Number of generated fixture files: large enough that a full
/// clear-and-rebuild catch-up reindex takes measurably longer than the
/// short debounce/poll cadence configured below, giving this test a real
/// window to observe the "reindexing" log line and SIGTERM the process
/// while the catch-up reindex is still in flight.
const FIXTURE_FILE_COUNT: usize = 500;

fn write_fixture(workspace: &Path) {
    let src = workspace.join("src");
    std::fs::create_dir_all(&src).expect("create src dir");
    for i in 0..FIXTURE_FILE_COUNT {
        let mut content = String::new();
        for j in 0..12 {
            content.push_str(&format!(
                "pub fn generated_{i}_{j}(x: i32, y: i32) -> i32 {{\n    let mut acc = x + y;\n    for k in 0..8 {{\n        acc = acc.wrapping_mul(k + 1).wrapping_add(x);\n    }}\n    acc\n}}\n\n"
            ));
        }
        std::fs::write(src.join(format!("generated_{i}.rs")), content).expect("write fixture file");
    }
}

fn write_settings(workspace: &Path) {
    let codanna_dir = workspace.join(".codanna");
    std::fs::create_dir_all(&codanna_dir).expect("create .codanna");
    std::fs::write(
        codanna_dir.join("settings.toml"),
        r#"
index_path = ".codanna/index"

[semantic_search]
enabled = false

[file_watch]
debounce_ms = 20
startup_catch_up = true
"#,
    )
    .expect("write settings");
}

/// Isolates HOME/XDG_CONFIG_HOME/XDG_STATE_HOME to `workspace/.home` for
/// both the spawned subprocess AND this test process itself, so
/// `codanna::serve_registry::list_entries()` (called from this process)
/// resolves to the same per-user registry directory the spawned server
/// wrote its entry to. Mutating process-global env vars from a test is only
/// safe because this file has a single test that touches them; restored via
/// `Drop`, mirroring `tests/integration/test_init_module.rs`.
struct IsolatedHome {
    original_home: Option<String>,
    original_xdg_config: Option<String>,
    original_xdg_state: Option<String>,
}

impl IsolatedHome {
    fn install(workspace: &Path) -> Self {
        let test_home = workspace.join(".home");
        std::fs::create_dir_all(&test_home).expect("create test home");
        let original_home = env::var("HOME").ok();
        let original_xdg_config = env::var("XDG_CONFIG_HOME").ok();
        let original_xdg_state = env::var("XDG_STATE_HOME").ok();
        // SAFETY: see struct doc -- no other test in this binary mutates
        // these vars concurrently.
        unsafe {
            env::set_var("HOME", &test_home);
            env::set_var("XDG_CONFIG_HOME", &test_home);
            env::set_var("XDG_STATE_HOME", test_home.join("state"));
        }
        Self {
            original_home,
            original_xdg_config,
            original_xdg_state,
        }
    }
}

impl Drop for IsolatedHome {
    fn drop(&mut self) {
        // SAFETY: see `install`.
        unsafe {
            match self.original_home.take() {
                Some(v) => env::set_var("HOME", v),
                None => env::remove_var("HOME"),
            }
            match self.original_xdg_config.take() {
                Some(v) => env::set_var("XDG_CONFIG_HOME", v),
                None => env::remove_var("XDG_CONFIG_HOME"),
            }
            match self.original_xdg_state.take() {
                Some(v) => env::set_var("XDG_STATE_HOME", v),
                None => env::remove_var("XDG_STATE_HOME"),
            }
        }
    }
}

fn seed_workspace() -> TempDir {
    let workspace = TempDir::new().expect("temp dir");
    write_fixture(workspace.path());
    write_settings(workspace.path());
    let test_home = workspace.path().join(".home");
    std::fs::create_dir_all(&test_home).expect("create test home");
    let status = Command::new(codanna_binary())
        .args(["index", "src", "--no-progress"])
        .current_dir(workspace.path())
        .env("HOME", &test_home)
        .env("XDG_CONFIG_HOME", &test_home)
        .env("XDG_STATE_HOME", test_home.join("state"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("run seed index");
    assert!(status.success(), "seed index should succeed");
    workspace
}

/// A spawned `codanna serve` child plus a background-drained copy of its
/// stderr lines.
///
/// The draining thread runs for the *entire* life of the child (until its
/// stderr closes, i.e. the child exits), not just until [`wait_for_stderr_line`]
/// finds what it is looking for: stopping early and dropping the read end of
/// the pipe would leave the child writing into a closed pipe once it logs
/// its next line (e.g. the shutdown-sequence messages this test wants to
/// keep observing after sending SIGTERM), which is a `BrokenPipe` write
/// error that can abort the child entirely -- turning this test's own
/// instrumentation into the thing that kills the process under test.
struct HttpServe {
    child: Child,
    stderr_lines: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

impl Drop for HttpServe {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Spawn `codanna serve --http --watch` with stderr piped and continuously
/// drained (see [`HttpServe`]) so the caller can scan for the catch-up
/// reindex log line. `RUST_LOG` is tuned to `info` so that (normally
/// `warn`-suppressed) line is actually emitted.
fn spawn_http_serve(workspace: &Path) -> HttpServe {
    use std::io::{BufRead, BufReader};

    let test_home = workspace.join(".home");
    let mut child = Command::new(codanna_binary())
        .args(["serve", "--http", "--bind", "127.0.0.1:0", "--watch"])
        .current_dir(workspace)
        .env("HOME", &test_home)
        .env("XDG_CONFIG_HOME", &test_home)
        .env("XDG_STATE_HOME", test_home.join("state"))
        .env("RUST_LOG", "codanna=info")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn serve --http");

    let stderr = child.stderr.take().expect("child stderr should be piped");
    let stderr_lines = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = stderr_lines.clone();
    thread::spawn(move || {
        let reader = BufReader::new(stderr);
        for line in reader.lines() {
            let Ok(line) = line else { break };
            sink.lock().expect("stderr line buffer lock").push(line);
        }
    });

    HttpServe {
        child,
        stderr_lines,
    }
}

/// Block (bounded by `deadline`) until a line containing `needle` has
/// appeared in `serve`'s drained stderr, returning that line. Panics with
/// every line observed so far if the deadline elapses first.
fn wait_for_stderr_line(serve: &HttpServe, needle: &str, deadline: Duration) -> String {
    let start = Instant::now();
    let mut seen_up_to = 0usize;
    loop {
        {
            let lines = serve.stderr_lines.lock().expect("stderr line buffer lock");
            while seen_up_to < lines.len() {
                let line = lines[seen_up_to].clone();
                seen_up_to += 1;
                if line.contains(needle) {
                    return line;
                }
            }
        }
        if start.elapsed() >= deadline {
            let lines = serve.stderr_lines.lock().expect("stderr line buffer lock");
            panic!(
                "did not observe a stderr line containing {needle:?} within {deadline:?}; \
                 lines seen:\n{}",
                lines.join("\n")
            );
        }
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn sigterm_during_catchup_reindex_exits_bounded_and_preserves_live_generation() {
    let workspace = seed_workspace();
    let _home = IsolatedHome::install(workspace.path());

    let layout = codanna::storage::IndexLayout::new(workspace.path().join(".codanna/index"));
    let live_generation = codanna::storage::generation::resolve_current(&layout)
        .expect("resolve_current should succeed against the freshly seeded index")
        .expect("seeded index should have a current generation");

    let mut serve = spawn_http_serve(workspace.path());

    // Wait for the watcher's catch-up reindex to actually be executing on
    // its `spawn_blocking` worker thread (armed by `startup_catch_up =
    // true`) before signaling -- this is what makes the SIGTERM land
    // mid-flight rather than before/after the build. The needle is the
    // "phase 2 walk started" log line emitted from inside
    // `ReindexHandles::run` once it is genuinely running, not the earlier
    // "quiet window elapsed" line, which is emitted before `tokio::spawn`
    // in `maybe_start_catch_up` and so can fire before the catch-up task
    // has even been polled -- a SIGTERM sent right after that earlier line
    // could race ahead of the task ever starting, so `handle.abort()` in
    // the watcher's cancellation arm would never actually cancel an
    // in-flight reindex.
    wait_for_stderr_line(&serve, "phase 2 walk started", Duration::from_secs(30));

    let pid = serve.child.id();
    let sigterm_sent_at = Instant::now();
    let status = Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .expect("send SIGTERM via `kill`");
    assert!(status.success(), "kill -TERM should succeed for pid {pid}");

    // `http_server::serve_http`'s `SHUTDOWN_GRACE` bounds each post-signal
    // watcher-handle join to ~3s; allow generous scheduling margin on top
    // for CI.
    let deadline = Duration::from_secs(3) + Duration::from_secs(12);
    let exit_status = loop {
        if let Some(status) = serve.child.try_wait().expect("poll child status") {
            break status;
        }
        assert!(
            sigterm_sent_at.elapsed() < deadline,
            "serve --http --watch did not exit within {deadline:?} of SIGTERM sent mid-catch-up"
        );
        thread::sleep(Duration::from_millis(50));
    };
    let elapsed = sigterm_sent_at.elapsed();
    assert!(
        elapsed < deadline,
        "serve --http --watch took {elapsed:?} to exit after SIGTERM, exceeding the \
         {deadline:?} bound"
    );
    assert!(
        exit_status.success(),
        "serve --http --watch should exit 0 on a clean SIGTERM shutdown, got {exit_status:?}"
    );

    let discovery_record =
        codanna::serve_discovery::read_record(&workspace.path().join(".codanna"));
    assert!(
        discovery_record.is_none(),
        "discovery record (serve.json) must be removed on SIGTERM shutdown, found: \
         {discovery_record:?}"
    );

    let registry_entries = codanna::serve_registry::list_entries();
    assert!(
        registry_entries.iter().all(|e| e.pid != pid),
        "registry entry for pid {pid} must be reaped on SIGTERM shutdown, entries: \
         {registry_entries:?}"
    );

    // The live generation published before `serve` ever started must be
    // untouched: the in-flight catch-up build was aborted, not allowed to
    // publish, and never corrupted the generation this process was serving.
    let post_generation = codanna::storage::generation::resolve_current(&layout)
        .expect("resolve_current should still succeed after the SIGTERM'd catch-up")
        .expect("index must still have a current generation after the SIGTERM'd catch-up");
    assert_eq!(
        post_generation, live_generation,
        "the live generation must be unchanged: an aborted catch-up build must never publish"
    );

    let settings = codanna::Settings {
        index_path: workspace.path().join(".codanna/index"),
        workspace_root: Some(workspace.path().to_path_buf()),
        ..Default::default()
    };
    let reloaded = codanna::indexing::facade::IndexFacade::new(std::sync::Arc::new(settings))
        .expect("the live generation must still open/load after the SIGTERM'd catch-up");
    assert!(
        reloaded.symbol_count() > 0,
        "the reloaded live generation should still carry the seeded symbols"
    );
}

#[tokio::test]
async fn stop_server_sigkill_escalation_reaps_registry_entry_even_when_target_never_reports_dead() {
    use codanna::cli::commands::serve::{repoll_after_sigkill_and_reap, wait_for_exit};

    // A "never-dying" stub: `is_alive` always reports the target alive, so
    // neither the initial SIGTERM-timeout wait nor the post-SIGKILL re-poll
    // ever observes an exit -- exercising the "reap regardless of confirmed
    // death" invariant `repoll_after_sigkill_and_reap` documents.
    let is_alive = |_pid: u32| true;

    let short_deadline = Duration::from_millis(150);
    let poll_interval = Duration::from_millis(20);

    // Phase 1: mirrors `stop_server`'s post-SIGTERM wait -- times out,
    // matching the "escalate to SIGKILL" branch condition.
    let exited_after_term = wait_for_exit(4242, short_deadline, poll_interval, is_alive).await;
    assert!(
        !exited_after_term,
        "never-dying stub must not report exit after the SIGTERM-timeout wait"
    );

    // Phase 2: SIGKILL -> short re-poll -> reap, unconditionally.
    let reaped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reaped_write = reaped.clone();
    let exited_after_kill =
        repoll_after_sigkill_and_reap(4242, short_deadline, poll_interval, is_alive, move |pid| {
            assert_eq!(pid, 4242, "reap must be called with the target pid");
            reaped_write.store(true, std::sync::atomic::Ordering::SeqCst);
        })
        .await;

    assert!(
        !exited_after_kill,
        "never-dying stub must not report exit even after the post-SIGKILL re-poll"
    );
    assert!(
        reaped.load(std::sync::atomic::Ordering::SeqCst),
        "the registry entry must be reaped even though the target never reported dead"
    );
}
