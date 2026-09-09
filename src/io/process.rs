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

/// Returns the pids of all currently running processes that look like a
/// `codanna serve` invocation, using a strict identity predicate.
///
/// This performs a full-system process scan (`ProcessesToUpdate::All`) --
/// a capability [`looks_like_codanna_serve`] does not have, since that
/// function only ever refreshes and inspects one already-known target pid.
/// Callers that need to *discover* candidate pids (rather than validate one
/// they already hold, e.g. from a registry entry) use this function instead.
///
/// A process qualifies iff:
/// 1. its executable basename -- from [`sysinfo::Process::exe`], falling
///    back to the basename of `cmd[0]` when `exe()` is unavailable (e.g.
///    permission-restricted `/proc/<pid>/exe` or a platform that doesn't
///    populate it) -- equals `codanna` case-insensitively, tolerating a
///    trailing `.exe` suffix (Windows); AND
/// 2. its argument list (`cmd[1..]`, joined and split on whitespace)
///    contains an exact `serve` token -- not merely a substring match.
///
/// This is deliberately STRICTER than [`looks_like_codanna_serve`], which
/// accepts a bare `codanna` substring anywhere in the joined command line.
/// That leniency is safe there because the caller already holds a specific
/// pid from a trusted source (a registry record or a known spawn) and is
/// only asking "does this still look like the same process", so a wrapper
/// like `sh -c "... exec codanna serve ..."` must still be recognized. Here,
/// by contrast, the input is the *entire process table* of the host
/// machine: a lenient substring-anywhere-in-argv match would flag any
/// unrelated process whose command line happens to mention "codanna" (a log
/// path, a config file argument, a grep invocation, etc.) as if it were a
/// live server. Requiring the executable's own basename to be `codanna`
/// anchors the match to the binary identity rather than incidental argv
/// text, which matters far more when scanning system-wide than when
/// re-checking a single already-trusted pid.
///
/// Read-only: performs no signal, kill, or write side effects.
pub fn scan_codanna_serve_pids() -> Vec<u32> {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing()
            .with_cmd(UpdateKind::Always)
            .with_exe(UpdateKind::Always),
    );
    sys.processes()
        .iter()
        .filter(|(_, process)| process_is_codanna_serve(process))
        .map(|(pid, _)| pid.as_u32())
        .collect()
}

/// The strict `codanna serve` identity predicate used by
/// [`scan_codanna_serve_pids`], factored out so it can be exercised directly
/// against known pids in tests without depending on a nondeterministic
/// full-system scan.
fn process_is_codanna_serve(process: &sysinfo::Process) -> bool {
    // On Linux sysinfo lists every thread as its own entry sharing the
    // parent's exe/argv; only the thread-group leader is a real process.
    if process.thread_kind().is_some() {
        return false;
    }
    let cmd = process.cmd();

    let basename = process
        .exe()
        .and_then(|path| path.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .or_else(|| {
            cmd.first().map(|arg0| {
                std::path::Path::new(arg0)
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| arg0.to_string_lossy().into_owned())
            })
        });
    let names_codanna = matches!(
        basename.as_deref().map(str::to_lowercase),
        Some(name) if name == "codanna" || name == "codanna.exe"
    );
    if !names_codanna {
        return false;
    }

    let joined_args = cmd
        .iter()
        .skip(1)
        .map(|arg| arg.to_string_lossy().to_lowercase())
        .collect::<Vec<_>>()
        .join(" ");
    joined_args.split_whitespace().any(|tok| tok == "serve")
}

#[cfg(test)]
mod tests {
    use super::{pid_is_alive, process_is_codanna_serve, scan_codanna_serve_pids};
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

    /// Spawns a short-lived process whose executable is literally named
    /// `codanna` (a copy of `sh` placed at `<tmpdir>/codanna`, so that
    /// `Process::exe()` resolves to a path with basename `codanna`) and
    /// whose argument list contains an exact `serve` token. Reuses the
    /// `sh -c "<script>"` no-op pattern from
    /// `serve_discovery::spawn_fake_http_serve_process` so the marker token
    /// is never actually executed as a command. The caller is responsible
    /// for killing the returned child.
    #[cfg(unix)]
    fn spawn_fake_codanna_serve_process() -> (std::process::Child, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("failed to create temp dir for fake codanna binary");
        let fake_exe = dir.path().join("codanna");
        std::fs::copy("/bin/sh", &fake_exe).expect("failed to copy /bin/sh to fake codanna path");
        // A concurrent test's fork can briefly inherit the copy's write fd,
        // so exec may transiently fail with ETXTBSY; retry rather than flake.
        let mut attempts = 0;
        let child = loop {
            match Command::new(&fake_exe)
                .arg("-c")
                .arg(": serve ; sleep 30 & wait")
                .spawn()
            {
                Ok(child) => break child,
                Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy && attempts < 40 => {
                    attempts += 1;
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(e) => panic!("failed to spawn fake codanna serve process for test: {e}"),
            }
        };
        (child, dir)
    }

    /// `process_is_codanna_serve` must require BOTH an executable basename
    /// of `codanna` AND an exact `serve` argument token: a plain `sleep 30`
    /// process satisfies neither and must be rejected, while a process
    /// spawned from a binary literally named `codanna` with a `serve`
    /// argument must be accepted. Tested directly against the two known
    /// pids' `Process` records (not via `scan_codanna_serve_pids`, whose
    /// full-system-scan result is nondeterministic across CI hosts).
    /// The full-table scan must return real processes only, never their
    /// threads: sysinfo on Linux lists each thread as a separate entry that
    /// shares the leader's exe/argv, so a multi-threaded server would
    /// otherwise appear once per worker thread. Every returned pid must be
    /// its own thread-group leader (`Tgid == Pid` in `/proc/<pid>/status`).
    #[test]
    #[cfg(target_os = "linux")]
    fn scan_codanna_serve_pids_returns_only_thread_group_leaders() {
        for pid in scan_codanna_serve_pids() {
            let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) else {
                continue; // exited between scan and check
            };
            let tgid = status
                .lines()
                .find_map(|line| line.strip_prefix("Tgid:"))
                .and_then(|v| v.trim().parse::<u32>().ok());
            assert_eq!(
                tgid,
                Some(pid),
                "scan returned pid {pid}, which is a thread of process {tgid:?}, not a process"
            );
        }
    }

    #[test]
    #[cfg(unix)]
    fn process_is_codanna_serve_requires_exact_basename_and_serve_token() {
        use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

        let (mut codanna_like, _dir) = spawn_fake_codanna_serve_process();
        let mut sleeper = Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("failed to spawn unrelated sleep process for test");

        let codanna_pid = Pid::from_u32(codanna_like.id());
        let sleeper_pid = Pid::from_u32(sleeper.id());

        // On a loaded CI runner there can be a brief window right after
        // `Command::spawn()` returns before `exe()`/`cmd()` are populated
        // for the new pid, so poll briefly rather than refreshing once.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut sys = System::new();
        let codanna_matches = loop {
            sys.refresh_processes_specifics(
                ProcessesToUpdate::Some(&[codanna_pid, sleeper_pid]),
                true,
                ProcessRefreshKind::nothing()
                    .with_cmd(UpdateKind::Always)
                    .with_exe(UpdateKind::Always),
            );
            let codanna_process = sys
                .process(codanna_pid)
                .expect("fake codanna process must be visible to sysinfo");
            if process_is_codanna_serve(codanna_process) || Instant::now() >= deadline {
                break process_is_codanna_serve(codanna_process);
            }
            std::thread::sleep(Duration::from_millis(10));
        };

        let sleeper_process = sys
            .process(sleeper_pid)
            .expect("sleeper process must be visible to sysinfo");
        let sleeper_matches = process_is_codanna_serve(sleeper_process);

        let _ = codanna_like.kill();
        let _ = codanna_like.wait();
        let _ = sleeper.kill();
        let _ = sleeper.wait();

        assert!(
            codanna_matches,
            "process with basename `codanna` and a `serve` arg token must match"
        );
        assert!(
            !sleeper_matches,
            "unrelated `sleep 30` process must not match"
        );
    }
}
