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
