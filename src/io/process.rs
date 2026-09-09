//! Process liveness for lock and staging ownership checks.

/// True when a process with this pid currently exists and is not a zombie.
///
/// A zombie process has exited but not yet been reaped by its parent; it
/// holds no resources and must be treated as dead by lock/staging-ownership
/// checks.
pub fn pid_is_alive(pid: u32) -> bool {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, System};
    let mut sys = System::new();
    let pid = Pid::from_u32(pid);
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing(),
    );
    match sys.process(pid) {
        Some(process) => !matches!(
            process.status(),
            ProcessStatus::Zombie | ProcessStatus::Dead
        ),
        None => false,
    }
}

/// True when the process at `pid` looks like a `codanna serve` process: its
/// command line, joined and lowercased, both names a `codanna` executable
/// and contains a `serve` token.
///
/// Deliberately checks the WHOLE joined command line rather than requiring
/// `cmd[0]` itself to name `codanna`: a spawned backing server's `cmd[0]` is
/// not always the `codanna` binary directly -- e.g. `serve_discovery`'s
/// test-only `CODANNA_TEST_SPAWN_DELAY_MS` hook wraps the real invocation in
/// `sh -c "sleep <n> && exec <codanna> serve ..."`, so `cmd[0]` is `sh` until
/// the `exec` replaces the process image, yet the pid is a genuine in-flight
/// codanna spawn throughout. Joining first tolerates that (and any other)
/// wrapper as long as the underlying invocation is visible somewhere on the
/// command line.
///
/// This is a lightweight heuristic, not a strong identity check: it exists
/// so registry consumers (`codanna serve --reap`/`--stop`, and the proxy's
/// spawn-timeout dedup in `serve_registry::find_spawning_for`) can tell a
/// stale registry entry -- whose pid has been reused by an unrelated live
/// process -- apart from a genuine, still-running codanna server, without
/// requiring a stronger (and platform-fragile) process-identity mechanism.
/// A dead pid is not "looks like codanna serve"; callers combine this with
/// [`pid_is_alive`] to decide staleness (dead OR doesn't-look-like-codanna).
pub fn looks_like_codanna_serve(pid: u32) -> bool {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
    let mut sys = System::new();
    let target = Pid::from_u32(pid);
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[target]),
        true,
        ProcessRefreshKind::nothing().with_cmd(UpdateKind::Always),
    );
    let Some(process) = sys.process(target) else {
        return false;
    };
    let joined_cmd = process
        .cmd()
        .iter()
        .map(|arg| arg.to_string_lossy().to_lowercase())
        .collect::<Vec<_>>()
        .join(" ");
    let names_codanna = joined_cmd.contains("codanna");
    let has_serve_token = joined_cmd.split_whitespace().any(|tok| tok == "serve");
    names_codanna && has_serve_token
}

#[cfg(test)]
mod tests {
    use super::pid_is_alive;
    use std::process::Command;
    use std::time::{Duration, Instant};

    /// A wrong existence-only implementation of `pid_is_alive` reports a
    /// zombie process as alive; this test asserts the opposite.
    /// Linux-only: it confirms the zombie transition via `/proc/<pid>/status`.
    #[test]
    #[cfg(target_os = "linux")]
    fn pid_is_alive_reports_zombie_as_dead() {
        let mut child = Command::new("true")
            .spawn()
            .expect("failed to spawn `true`");
        let pid = child.id();

        // Do not call child.wait() yet: without reaping, the child becomes
        // a zombie once it exits. Poll briefly for that transition.
        let deadline = Instant::now() + Duration::from_secs(5);
        let status_path = format!("/proc/{pid}/status");
        let became_zombie = loop {
            if let Ok(contents) = std::fs::read_to_string(&status_path) {
                if contents.lines().any(|line| line == "State:\tZ (zombie)") {
                    break true;
                }
            }
            if Instant::now() >= deadline {
                break false;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(became_zombie, "child process did not become a zombie");

        let alive = pid_is_alive(pid);

        // Reap the zombie so the test process does not leak it.
        let _ = child.wait();

        assert!(!alive, "zombie process must be reported as dead");
    }
}
