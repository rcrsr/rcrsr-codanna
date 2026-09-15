//! Serve command - MCP server modes (stdio, HTTP, HTTPS).

use std::path::PathBuf;
use std::sync::Arc;

use crate::config::Settings;
use crate::indexing::facade::IndexFacade;
use crate::mcp::probe_tolerant_stdio;
use crate::serve_discovery::{PidLockError, PidLockGuard};

/// Arguments for the serve command.
pub struct ServeArgs {
    pub watch: bool,
    pub watch_interval: u64,
    pub http: bool,
    pub https: bool,
    pub proxy: bool,
    pub bind: String,
    /// List servers from the per-user server registry instead of starting one.
    pub list: bool,
    /// Stop a registered server (pid or workspace-root path) instead of starting one.
    pub stop: Option<String>,
    /// Prune stale registry entries instead of starting one.
    pub reap: bool,
    /// Stop every registered server instead of starting one.
    pub stop_all: bool,
    /// Deprecated spelling of `stop_all`, merged into it in `run` (with a
    /// deprecation notice) since clap-derive can't tell which spelling was
    /// used at the point `stop_all` is read.
    pub kill_all: bool,
    /// With `stop_all`, also stop registered proxies (not just backing
    /// servers).
    pub include_proxies: bool,
    /// With `stop` or `stop_all`, send SIGKILL instead of the default
    /// SIGTERM.
    pub force: bool,
    /// With `stop_all`, also stop unregistered pids that still
    /// independently look like `codanna serve` processes (identity-checked
    /// via `scan_codanna_serve_pids` immediately before each is signaled). A
    /// numeric `--stop <pid>` selector no longer needs this flag: it always
    /// accepts any pid passing the identity check, registered or not.
    pub include_unknown: bool,
    /// Deprecated spelling of `include_unknown`, merged into it in `run`
    /// (with a deprecation notice) since clap-derive can't tell which
    /// spelling was used at the point `include_unknown` is read.
    pub include_rogue: bool,
    /// With `stop`, seconds to wait for the delivered signal to take effect
    /// before escalating to SIGKILL (default: 5).
    pub timeout: u64,
    /// With `stop`, opt out of escalating to SIGKILL when the target has
    /// not exited within `timeout`.
    pub no_force: bool,
}

/// Resolve which server transport `codanna serve` should start.
///
/// Precedence mirrors `main.rs`'s `is_proxy_serve` so the pre-dispatch
/// resource predicates and the actual server startup never disagree about
/// which mode is in effect:
/// 1. CLI `--https` (highest precedence)
/// 2. CLI `--http`
/// 3. CLI `--proxy`, OR `config.server.mode == "proxy"` when no CLI transport
///    flag was given (bare `codanna serve` can default to proxy via
///    `settings.toml`)
/// 4. `config.server.mode == "http"`
/// 5. stdio (default)
fn resolve_server_mode(https: bool, http: bool, proxy: bool, config_mode: &str) -> &'static str {
    if https {
        "https"
    } else if http {
        "http"
    } else if proxy || config_mode == "proxy" {
        "proxy"
    } else if config_mode == "http" {
        "http"
    } else {
        "stdio"
    }
}

/// Run the serve command.
///
/// `facade` is `None` exactly when proxy mode is selected: `main.rs` computes
/// the effective mode ahead of index loading (§4.5) and skips constructing an
/// `IndexFacade` entirely for proxy, since the proxy process holds no index
/// state of its own -- it only relays to a backing HTTP server's facade.
pub async fn run(
    args: ServeArgs,
    config: Settings,
    settings: Arc<Settings>,
    facade: Option<IndexFacade>,
    index_path: PathBuf,
    config_path: Option<PathBuf>,
) {
    let ServeArgs {
        watch,
        watch_interval,
        http,
        https,
        proxy,
        bind,
        list,
        stop,
        reap,
        stop_all,
        kill_all,
        include_proxies,
        force,
        include_unknown,
        include_rogue,
        timeout,
        no_force,
    } = args;

    // `--kill-all` is a deprecated spelling of `--stop-all`, merged here
    // (rather than in clap) because clap-derive cannot report which
    // spelling was actually used, and the deprecation notice needs that.
    let stop_all = stop_all || kill_all;
    if kill_all {
        eprintln!(
            "DEPRECATION: `codanna serve --kill-all` is deprecated in favor of `--stop-all`; it will be removed in a future release."
        );
    }

    // `--include-rogue` is a deprecated spelling of `--include-unknown`,
    // merged here for the same clap-derive-can't-tell-which-spelling reason
    // as `--kill-all` above.
    let include_unknown = include_unknown || include_rogue;
    if include_rogue {
        eprintln!(
            "DEPRECATION: `codanna serve --include-rogue` is deprecated in favor of `--include-unknown`; it will be removed in a future release."
        );
    }

    // Registry lifecycle operations (--list/--stop/--reap/--stop-all) are
    // never start-a-server operations: handle them here, before any
    // transport mode is resolved, and return without ever reaching the
    // server-startup match below. Clap's `conflicts_with_all` on these flags
    // (see `cli::args::Commands::Serve`) already guarantees `http`/`https`/
    // `proxy`/`bind` are all still at their defaults whenever one of these is
    // set.
    if list || stop.is_some() || reap || stop_all {
        run_registry_management(
            list,
            stop,
            reap,
            stop_all,
            include_proxies,
            force,
            include_unknown,
            timeout,
            no_force,
        )
        .await;
        return;
    }

    let server_mode = resolve_server_mode(https, http, proxy, &config.server.mode);

    // Use bind address from CLI if provided, otherwise from config
    // For HTTPS, default to port 8443 if using default bind
    let bind_address = if bind != "127.0.0.1:8080" {
        // CLI flag was explicitly set (not default)
        bind
    } else if https {
        // For HTTPS, use port 8443 by default
        "127.0.0.1:8443".to_string()
    } else {
        // Use config value
        config.server.bind.clone()
    };

    // Use watch interval from CLI if provided, otherwise from config
    let actual_watch_interval = if watch_interval != 5 {
        // CLI flag was explicitly set (not default)
        watch_interval
    } else {
        config.server.watch_interval
    };

    match server_mode {
        "https" => {
            run_https_server(&config, watch, bind_address).await;
        }
        "http" => {
            run_http_server(config, watch, bind_address).await;
        }
        "proxy" => {
            run_proxy_server(config, config_path).await;
        }
        _ => {
            run_stdio_server(
                config,
                settings,
                facade.expect("stdio serve requires an already-loaded IndexFacade"),
                index_path,
                watch,
                actual_watch_interval,
            )
            .await;
        }
    }
}

/// Dispatch `codanna serve --list`/`--stop`/`--reap`/`--stop-all`. Runs each
/// requested operation in turn (reap, then stop, then stop-all, then list) so
/// `--stop --list` reflects the post-stop state, `--reap --list` reflects the
/// post-reap state, and `--stop-all --list` reflects the post-stop-all state.
///
/// `stop_all_servers` reports its outcome instead of exiting the process
/// directly, so `--list` still runs (and reflects the post-sweep state) even
/// when a `--stop-all` target failed to stop; the failure is only turned
/// into a nonzero exit code after every requested step has run.
async fn run_registry_management(
    list: bool,
    stop: Option<String>,
    reap: bool,
    stop_all: bool,
    include_proxies: bool,
    force: bool,
    include_unknown: bool,
    timeout: u64,
    no_force: bool,
) {
    if reap {
        reap_stale_entries();
    }
    if let Some(selector) = stop {
        stop_server(&selector, force, timeout, no_force).await;
    }
    let stop_all_succeeded = if stop_all {
        Some(stop_all_servers(force, include_proxies, include_unknown).await)
    } else {
        None
    };
    if list {
        print_registry_list();
    }
    if stop_all_succeeded == Some(false) {
        std::process::exit(1);
    }
}

/// Remove every registry entry whose process is no longer alive (via the
/// zombie-safe `pid_is_alive`). This never signals a process -- it only
/// prunes stale registry files left behind by a server that did not
/// self-deregister (e.g. it was killed with SIGKILL).
fn reap_stale_entries() {
    let mut reaped = 0u32;
    for entry in crate::serve_registry::list_entries() {
        if crate::serve_registry::entry_is_stale(&entry) {
            crate::serve_registry::remove_entry(entry.pid);
            reaped += 1;
        }
    }
    eprintln!(
        "Reaped {reaped} stale server registry {}.",
        if reaped == 1 { "entry" } else { "entries" }
    );
}

/// Print a table of every registered server whose process is currently
/// alive.
///
/// `codanna serve --list` is deprecated in favor of `codanna ls`, which owns
/// the sole merge/table-building logic for this listing (see the module doc
/// on `crate::cli::commands::ls`). This prints a one-line deprecation notice
/// to stderr, then delegates entirely to `ls::run` so there is exactly one
/// source of listing logic -- this function must never rebuild the table
/// itself.
fn print_registry_list() {
    eprintln!(
        "DEPRECATION: `codanna serve --list` is deprecated in favor of `codanna ls`; it will be removed in a future release."
    );
    crate::cli::commands::ls::run();
}

/// Resolve a `--stop` selector to a pid: either a literal pid, or a
/// workspace-root path matched (exactly, or after canonicalization) against
/// every registered entry's `workspace_root`.
///
/// A numeric selector must name a pid that still looks like a `codanna
/// serve` process -- otherwise `codanna serve --stop <pid>` would
/// SIGTERM/SIGKILL any process the invoking user can signal, not just a
/// codanna server, contradicting the "Stop a registered server..." help
/// text -- but it need NOT be present in the per-user server registry: the
/// identity check (`looks_like_codanna_serve`/`scan_codanna_serve_pids`) is
/// the sole safety net for a numeric selector, registered or not, so this
/// can never become a generic kill-any-pid primitive without also becoming a
/// no-op for stopping an unregistered-but-genuine codanna server (e.g. one
/// started before the registry existed, or from another user's session this
/// one can still signal).
///
/// A PATH selector, by contrast, stays registry-gated: a workspace-root path
/// only has meaning by way of a registered entry's `workspace_root` field,
/// so there is no way to resolve one to a pid without consulting the
/// registry.
///
/// The identity check itself is stricter for an unregistered pid than a
/// registered one. A registered pid is already vouched for by the registry
/// (this process itself wrote that entry), so the lenient
/// `looks_like_codanna_serve` (`cmd` contains "codanna" as a substring
/// anywhere) suffices to confirm the same process is still there. An
/// unregistered pid has no such vouching, so it is checked against the
/// stricter, basename-anchored `scan_codanna_serve_pids` predicate instead,
/// to minimize the chance of signaling an unrelated process that merely
/// mentions "codanna" on its command line.
///
/// Returns `(pid, was_registered)` so the caller can print a one-line note
/// when a numeric selector resolved to a pid outside the registry.
fn resolve_selector_to_pid(selector: &str) -> Option<(u32, bool)> {
    if let Ok(pid) = selector.parse::<u32>() {
        let is_registered = crate::serve_registry::list_entries()
            .iter()
            .any(|entry| entry.pid == pid);

        let looks_like_codanna_serve = if is_registered {
            crate::serve_registry::looks_like_codanna_serve(pid)
        } else {
            crate::io::process::scan_codanna_serve_pids().contains(&pid)
        };
        if !looks_like_codanna_serve {
            return None;
        }

        return Some((pid, is_registered));
    }

    let path = PathBuf::from(selector);
    let canonical = std::fs::canonicalize(&path).ok();

    crate::serve_registry::list_entries()
        .into_iter()
        .find_map(|entry| {
            let matches = entry.workspace_root == path
                || canonical.as_deref() == Some(entry.workspace_root.as_path());
            matches.then_some((entry.pid, true))
        })
}

/// Outcome of attempting to deliver a signal to a process, as classified by
/// `sysinfo::Process::kill_with`'s `Option<bool>` result.
enum SignalOutcome {
    /// The signal was delivered successfully.
    Sent,
    /// The OS rejected or failed to deliver the signal.
    SendFailed,
    /// Sending signals is not supported on this platform.
    UnsupportedPlatform,
}

/// Send `signal` to an already-resolved `process`, classifying the result.
///
/// Shared by `stop_server` and `stop_all_servers` so both agree on how
/// `sysinfo`'s `Option<bool>` outcome maps to a typed result (§BASIC.2 DRY).
fn send_signal(process: &sysinfo::Process, signal: sysinfo::Signal) -> SignalOutcome {
    match process.kill_with(signal) {
        Some(true) => SignalOutcome::Sent,
        Some(false) => SignalOutcome::SendFailed,
        None => SignalOutcome::UnsupportedPlatform,
    }
}

/// Poll `is_alive(pid)` every `poll_interval` until either `pid` is no
/// longer alive or `deadline` elapses, returning whether the pid exited
/// within the deadline.
///
/// `is_alive` is an injectable predicate (rather than a hardcoded call to
/// `crate::serve_registry::pid_is_alive`) so this can be unit-tested without
/// a real process, mirroring the split-for-testability pattern used
/// elsewhere (e.g. `serve_registry::find_spawning_for_in`, `ls::render`).
///
/// `pub` (not `pub(crate)`): exposed so `tests/serve_stop_bounded.rs`, an
/// external integration test, can drive `stop_server`'s escalation sequence
/// hermetically, mirroring `cli::commands::index::run_prune_indexed_paths`'s
/// exposure for the same reason.
pub async fn wait_for_exit(
    pid: u32,
    deadline: std::time::Duration,
    poll_interval: std::time::Duration,
    is_alive: impl Fn(u32) -> bool,
) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < deadline && is_alive(pid) {
        tokio::time::sleep(poll_interval).await;
    }
    !is_alive(pid)
}

/// The tail of `stop_server`'s SIGTERM -> timeout -> SIGKILL -> reap
/// escalation: re-poll `is_alive(pid)` for up to `repoll_deadline` after
/// SIGKILL has already been sent, then reap the target's registry entry via
/// `reap` unconditionally -- regardless of whether the re-poll observed it
/// exit. SIGKILL cannot be caught or ignored by a well-behaved process, so
/// this does not block the CLI's exit code on re-confirming death within a
/// short bounded window; `reap` runs either way so a killed process's
/// registry entry (which it can no longer remove itself) never lingers.
///
/// `is_alive` and `reap` are injectable (mirroring `wait_for_exit`'s
/// `is_alive` parameter) so this can be unit-tested with a stub that never
/// reports the target as dead, without a real process or a real registry
/// file. `pub` for the same external-test-seam reason as `wait_for_exit`.
///
/// Returns whether the re-poll observed the target exit (used only for the
/// message `stop_server` prints).
pub async fn repoll_after_sigkill_and_reap(
    pid: u32,
    repoll_deadline: std::time::Duration,
    poll_interval: std::time::Duration,
    is_alive: impl Fn(u32) -> bool,
    reap: impl FnOnce(u32),
) -> bool {
    let exited = wait_for_exit(pid, repoll_deadline, poll_interval, is_alive).await;
    reap(pid);
    exited
}

/// Stop a registered server: resolve `selector` to a pid, send SIGTERM (or
/// SIGKILL with `force`), then poll -- bounded by `timeout` seconds -- for
/// the target to exit. SIGTERM is the default; SIGKILL is only ever sent as
/// the *initial* signal when `force` is set explicitly.
///
/// If the initial signal was SIGTERM and the target has not exited within
/// `timeout`, this escalates to SIGKILL by default (unless `no_force` opts
/// out), re-polls briefly, and -- since a SIGKILL'd process cannot
/// self-deregister -- reaps its registry entry directly via `remove_entry`.
async fn stop_server(selector: &str, force: bool, timeout: u64, no_force: bool) {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, Signal, System, UpdateKind};

    let Some((pid, was_registered)) = resolve_selector_to_pid(selector) else {
        if let Ok(pid) = selector.parse::<u32>() {
            // A numeric selector's only failure mode now is the identity
            // check (`resolve_selector_to_pid` no longer gates numeric
            // selectors on registry membership).
            eprintln!("pid {pid} does not look like a codanna serve process.");
        } else {
            eprintln!(
                "No registered server matches '{selector}' (expected a pid or a workspace-root path)."
            );
        }
        std::process::exit(1);
    };

    if !was_registered {
        eprintln!("pid {pid} was not registered; stopping anyway.");
    }

    let mut sys = System::new();
    let target = Pid::from_u32(pid);
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[target]),
        true,
        ProcessRefreshKind::nothing()
            .with_cmd(UpdateKind::Always)
            .with_exe(UpdateKind::Always),
    );

    let Some(process) = sys.process(target) else {
        eprintln!(
            "No running process with pid {pid} (its registry entry may be stale; try --reap)."
        );
        std::process::exit(1);
    };

    // Re-verify identity immediately before signaling: a registered pid may
    // have been recycled by the OS for an unrelated process since it was
    // selected, and an unregistered pid was never vouched for by the
    // registry at all -- so the unregistered case uses the stricter,
    // `scan_codanna_serve_pids`-style predicate (mirrors
    // `stop_one_registered_target`/`stop_one_unknown_target`).
    let identity_ok = if was_registered {
        crate::io::process::process_looks_like_codanna_serve(process)
    } else {
        crate::io::process::process_is_codanna_serve(process)
    };
    if !identity_ok {
        eprintln!(
            "pid {pid} no longer looks like a codanna serve process; refusing to signal it \
             (its registry entry may be stale; try --reap)."
        );
        std::process::exit(1);
    }

    // Liveness is determinable here: `sys.process(target)` just resolved to
    // a live process above, so report that before signaling it.
    eprintln!("pid {pid} is alive; sending signal.");

    let signal = if force { Signal::Kill } else { Signal::Term };
    match send_signal(process, signal) {
        SignalOutcome::Sent => {}
        SignalOutcome::SendFailed => {
            let name = if force { "SIGKILL" } else { "SIGTERM" };
            eprintln!("Failed to send {name} to pid {pid}.");
            std::process::exit(1);
        }
        SignalOutcome::UnsupportedPlatform => {
            eprintln!("Sending signals is not supported on this platform (pid {pid}).");
            std::process::exit(1);
        }
    }

    let deadline = std::time::Duration::from_secs(timeout);
    let poll_interval = std::time::Duration::from_millis(100);
    let exited = wait_for_exit(
        pid,
        deadline,
        poll_interval,
        crate::serve_registry::pid_is_alive,
    )
    .await;

    if !exited {
        // `--force` already sent SIGKILL as the initial signal, so there is
        // nothing left to escalate to; `--no-force` explicitly opts out of
        // escalation. Either way, report the still-alive state and give up.
        if force || no_force {
            eprintln!(
                "Server pid {pid} did not exit within {}s of receiving the signal (still alive).",
                deadline.as_secs()
            );
            std::process::exit(1);
        }

        eprintln!(
            "Server pid {pid} did not exit within {}s of SIGTERM (still alive); escalating to SIGKILL.",
            deadline.as_secs()
        );

        let mut sys = System::new();
        sys.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[target]),
            true,
            ProcessRefreshKind::nothing()
                .with_cmd(UpdateKind::Always)
                .with_exe(UpdateKind::Always),
        );
        // The target may have exited between `wait_for_exit`'s last poll and
        // this refresh -- nothing to escalate to in that case; fall through
        // to the re-poll below, which will observe it as no longer alive.
        if let Some(process) = sys.process(target) {
            // Re-verify identity before the SIGKILL escalation too: the
            // SIGTERM timeout window is long enough for the pid to have
            // been recycled by the OS for an unrelated process.
            let identity_ok = if was_registered {
                crate::io::process::process_looks_like_codanna_serve(process)
            } else {
                crate::io::process::process_is_codanna_serve(process)
            };
            if !identity_ok {
                eprintln!(
                    "pid {pid} no longer looks like a codanna serve process; skipping SIGKILL \
                     escalation (it may have exited and the pid been reused)."
                );
            } else {
                match send_signal(process, Signal::Kill) {
                    SignalOutcome::Sent => {}
                    SignalOutcome::SendFailed => {
                        eprintln!("Failed to send SIGKILL to pid {pid}.");
                        std::process::exit(1);
                    }
                    SignalOutcome::UnsupportedPlatform => {
                        eprintln!("Sending signals is not supported on this platform (pid {pid}).");
                        std::process::exit(1);
                    }
                }
            }
        }

        let repoll_deadline = std::time::Duration::from_secs(2);
        let exited_after_kill = repoll_after_sigkill_and_reap(
            pid,
            repoll_deadline,
            poll_interval,
            crate::serve_registry::pid_is_alive,
            crate::serve_registry::remove_entry,
        )
        .await;

        if exited_after_kill {
            eprintln!(
                "Killed server pid {pid} (SIGKILL after SIGTERM timeout, now exited); reaped its registry entry."
            );
            return;
        }

        eprintln!(
            "Sent SIGKILL to pid {pid} after SIGTERM timeout; it had not yet exited by the end \
             of the re-poll window (still alive), but its registry entry was reaped anyway \
             since a SIGKILL'd process cannot self-deregister."
        );
        std::process::exit(1);
    }

    if force {
        eprintln!(
            "Killed server pid {pid} (SIGKILL); its registry entry may remain since a killed \
             process cannot self-deregister -- run `codanna serve --reap` to prune it."
        );
    } else {
        eprintln!("Stopped server pid {pid} (SIGTERM).");
    }
}

/// Stop every registered server (and, with `include_proxies`, every
/// registered proxy too), and, with `include_unknown`, every unregistered
/// pid that still independently looks like a `codanna serve` process --
/// instead of starting one.
///
/// Registered targets come from `serve_registry::list_entries()` -- the same
/// source `codanna ls`/`codanna serve --list` read -- filtered to entries
/// that are not already stale (`entry_is_stale`, mirroring `--reap`'s
/// definition of "still alive"). By default only `ServerRole::Server`
/// entries are targeted; `include_proxies` additionally targets
/// `ServerRole::Proxy` entries. `include_unknown` additionally targets every
/// pid `io::process::scan_codanna_serve_pids` finds that is NOT already a
/// registered pid and is not this process's own pid -- these have no
/// registry entry to filter by role or staleness, since they were never
/// registered, and need no registry cleanup once stopped. Every target
/// (registered or unknown) is attempted even if an earlier one fails or
/// times out (§BASIC.12.1: bounded fail-fast -- report every failure, but
/// never abort the sweep on the first one).
///
/// Every target is signaled first (re-validating its identity immediately
/// before its own signal, since an earlier target's signal may have taken
/// time), then every signaled target is polled for exit *concurrently*, so
/// one unresponsive target cannot push the sweep's wall-clock time past a
/// single `wait_for_exit` deadline.
///
/// Returns whether every target stopped (`true`) or at least one did not
/// (`false`), so the caller can run any remaining requested steps (e.g.
/// `--list`) before turning a failure into a nonzero exit code.
async fn stop_all_servers(force: bool, include_proxies: bool, include_unknown: bool) -> bool {
    let registered_entries = crate::serve_registry::list_entries();

    let registered_targets: Vec<u32> = registered_entries
        .iter()
        .filter(|entry| !crate::serve_registry::entry_is_stale(entry))
        .filter(|entry| include_proxies || entry.role == crate::serve_registry::ServerRole::Server)
        .map(|entry| entry.pid)
        .collect();

    let unknown_targets: Vec<u32> = if include_unknown {
        let registered_pids: std::collections::HashSet<u32> =
            registered_entries.iter().map(|entry| entry.pid).collect();
        let own_pid = std::process::id();
        crate::io::process::scan_codanna_serve_pids()
            .into_iter()
            .filter(|pid| !registered_pids.contains(pid) && *pid != own_pid)
            .collect()
    } else {
        Vec::new()
    };

    if registered_targets.is_empty() && unknown_targets.is_empty() {
        let hint = if include_proxies {
            "run `codanna serve --reap` to prune stale entries"
        } else {
            "run `codanna serve --reap` to prune stale entries, or pass --include-proxies to \
             also target registered proxies"
        };
        eprintln!("No live registered servers to stop ({hint}).");
        return true;
    }

    let mut any_failed = false;

    let mut signaled_registered = Vec::with_capacity(registered_targets.len());
    for pid in registered_targets {
        if stop_one_registered_target(pid, force) {
            signaled_registered.push(pid);
        } else {
            any_failed = true;
        }
    }

    let mut signaled_unknown = Vec::with_capacity(unknown_targets.len());
    for pid in unknown_targets {
        if stop_one_unknown_target(pid, force) {
            signaled_unknown.push(pid);
        } else {
            any_failed = true;
        }
    }

    let mut registered_waits = tokio::task::JoinSet::new();
    for pid in signaled_registered {
        registered_waits.spawn(wait_for_registered_target(pid, force));
    }
    let mut unknown_waits = tokio::task::JoinSet::new();
    for pid in signaled_unknown {
        unknown_waits.spawn(wait_for_unknown_target(pid, force));
    }

    let mut registered_stopped = 0u32;
    while let Some(result) = registered_waits.join_next().await {
        match result {
            Ok(true) => registered_stopped += 1,
            Ok(false) => any_failed = true,
            Err(_) => any_failed = true,
        }
    }
    let mut unknown_stopped = 0u32;
    while let Some(result) = unknown_waits.join_next().await {
        match result {
            Ok(true) => unknown_stopped += 1,
            Ok(false) => any_failed = true,
            Err(_) => any_failed = true,
        }
    }

    eprintln!(
        "Stop-all summary: stopped {registered_stopped} registered, {unknown_stopped} unknown."
    );

    !any_failed
}

/// Re-validate one already-selected registered pid's identity, then signal
/// it (SIGTERM, or SIGKILL with `force`) -- the same signal step
/// `stop_server` uses -- sharing a single process-table refresh across the
/// identity check and the signal send rather than scanning twice.
///
/// The identity re-check happens immediately before signaling (mirroring
/// `stop_server`'s tight check-then-use adjacency) because, in
/// `stop_all_servers`'s sequential signal pass, a pid selected by the
/// up-front filter may no longer be a live codanna server by the time its
/// turn comes.
fn stop_one_registered_target(pid: u32, force: bool) -> bool {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, Signal, System, UpdateKind};

    let mut sys = System::new();
    let target = Pid::from_u32(pid);
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[target]),
        true,
        ProcessRefreshKind::nothing().with_cmd(UpdateKind::Always),
    );

    let Some(process) = sys.process(target) else {
        eprintln!(
            "No running process with pid {pid} (its registry entry may be stale; try --reap)."
        );
        return false;
    };

    if !crate::io::process::process_looks_like_codanna_serve(process) {
        eprintln!(
            "pid {pid} no longer looks like a codanna serve process; skipping (its registry \
             entry may be stale; try --reap)."
        );
        return false;
    }

    let signal = if force { Signal::Kill } else { Signal::Term };
    match send_signal(process, signal) {
        SignalOutcome::Sent => true,
        SignalOutcome::SendFailed => {
            let name = if force { "SIGKILL" } else { "SIGTERM" };
            eprintln!("Failed to send {name} to pid {pid}.");
            false
        }
        SignalOutcome::UnsupportedPlatform => {
            eprintln!("Sending signals is not supported on this platform (pid {pid}).");
            false
        }
    }
}

/// Poll one already-signaled registered pid for exit and report the outcome
/// in the same message shape `stop_server` uses, returning whether this
/// target ended up stopped -- so `stop_all_servers` can poll every signaled
/// target concurrently and still learn each one's individual outcome.
async fn wait_for_registered_target(pid: u32, force: bool) -> bool {
    let deadline = std::time::Duration::from_secs(5);
    let poll_interval = std::time::Duration::from_millis(100);
    let exited = wait_for_exit(
        pid,
        deadline,
        poll_interval,
        crate::serve_registry::pid_is_alive,
    )
    .await;

    if !exited {
        eprintln!(
            "Server pid {pid} did not exit within {}s of receiving the signal -- run `codanna \
             serve --reap` to prune it if it has in fact died.",
            deadline.as_secs()
        );
        return false;
    }

    if force {
        eprintln!(
            "Killed server pid {pid} (SIGKILL); its registry entry may remain since a killed \
             process cannot self-deregister -- run `codanna serve --reap` to prune it."
        );
    } else {
        eprintln!("Stopped server pid {pid} (SIGTERM).");
    }
    true
}

/// Re-validate one already-selected unregistered pid's identity, then signal
/// it (SIGTERM, or SIGKILL with `force`) -- the `--stop-all --include-unknown`
/// counterpart of `stop_one_registered_target`, using the same
/// single-pid-refresh-then-signal shape but the STRICTER,
/// `scan_codanna_serve_pids`-style predicate (`process_is_codanna_serve`),
/// since an unknown target has no registry entry vouching for it.
///
/// The identity re-check happens immediately before signaling for the same
/// reason `stop_one_registered_target`'s does: a pid selected by
/// `stop_all_servers`'s up-front scan may no longer be a live codanna server
/// by the time its turn in the sequential signal pass comes.
fn stop_one_unknown_target(pid: u32, force: bool) -> bool {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, Signal, System, UpdateKind};

    let mut sys = System::new();
    let target = Pid::from_u32(pid);
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[target]),
        true,
        ProcessRefreshKind::nothing()
            .with_cmd(UpdateKind::Always)
            .with_exe(UpdateKind::Always),
    );

    let Some(process) = sys.process(target) else {
        eprintln!("No running process with pid {pid} (it may have exited already); skipping.");
        return false;
    };

    if !crate::io::process::process_is_codanna_serve(process) {
        eprintln!("pid {pid} no longer looks like a codanna serve process; skipping.");
        return false;
    }

    let signal = if force { Signal::Kill } else { Signal::Term };
    match send_signal(process, signal) {
        SignalOutcome::Sent => true,
        SignalOutcome::SendFailed => {
            let name = if force { "SIGKILL" } else { "SIGTERM" };
            eprintln!("Failed to send {name} to pid {pid}.");
            false
        }
        SignalOutcome::UnsupportedPlatform => {
            eprintln!("Sending signals is not supported on this platform (pid {pid}).");
            false
        }
    }
}

/// Poll one already-signaled unknown (unregistered) pid for exit, mirroring
/// `wait_for_registered_target` but without any registry-entry wording,
/// since an unknown target was never registered and needs no registry
/// cleanup once stopped.
async fn wait_for_unknown_target(pid: u32, force: bool) -> bool {
    let deadline = std::time::Duration::from_secs(5);
    let poll_interval = std::time::Duration::from_millis(100);
    let exited = wait_for_exit(
        pid,
        deadline,
        poll_interval,
        crate::io::process::pid_is_alive,
    )
    .await;

    if !exited {
        eprintln!(
            "Unregistered pid {pid} did not exit within {}s of receiving the signal.",
            deadline.as_secs()
        );
        return false;
    }

    if force {
        eprintln!("Killed unregistered pid {pid} (SIGKILL).");
    } else {
        eprintln!("Stopped unregistered pid {pid} (SIGTERM).");
    }
    true
}

async fn run_https_server(config: &Settings, watch: bool, bind_address: String) {
    // HTTPS mode - secure server with TLS
    tracing::info!(target: "mcp", "starting HTTPS server on {bind_address}");
    if watch || config.file_watch.enabled {
        tracing::debug!(
            target: "mcp",
            "file watching enabled with {}ms debounce",
            config.file_watch.debounce_ms
        );
    }

    // Use the HTTPS server implementation
    #[cfg(feature = "https-server")]
    {
        use crate::mcp::https_server::serve_https;
        if let Err(e) = serve_https(config.clone(), watch, bind_address).await {
            eprintln!("HTTPS server error: {e}");
            std::process::exit(1);
        }
        // `serve_https` has already joined any watcher tasks; exit directly
        // here (CLI-only call site, before the Tokio runtime is dropped)
        // rather than letting a detached watcher's leftover state block
        // `main()`'s exit.
        std::process::exit(0);
    }

    #[cfg(not(feature = "https-server"))]
    {
        eprintln!("HTTPS server support is not compiled in.");
        eprintln!("Please rebuild with: cargo build --features https-server");
        std::process::exit(1);
    }
}

async fn run_proxy_server(config: Settings, config_path: Option<PathBuf>) {
    // Proxy mode - stdio-facing delegate that discovers/spawns a backing
    // `codanna serve --http` and relays MCP traffic to it. No IndexFacade is
    // constructed in this process (§4.5).
    eprintln!("Starting MCP server in proxy mode (stdio <-> HTTP delegate)");

    if let Err(e) = crate::mcp::proxy::serve_proxy(config, config_path).await {
        eprintln!("Proxy server error: {e}");
        std::process::exit(1);
    }
}

async fn run_http_server(config: Settings, watch: bool, bind_address: String) {
    // HTTP mode - persistent server with event-driven file watching
    eprintln!("Starting MCP server in HTTP mode");
    eprintln!("Bind address: {bind_address}");
    if watch || config.file_watch.enabled {
        eprintln!(
            "File watching: ENABLED (event-driven with {}ms debounce)",
            config.file_watch.debounce_ms
        );
    }

    // Use the HTTP server implementation
    use crate::mcp::http_server::serve_http;
    if let Err(e) = serve_http(config, watch, bind_address).await {
        eprintln!("HTTP server error: {e}");
        std::process::exit(1);
    }
    // `serve_http` has already joined any watcher tasks; exit directly here
    // (CLI-only call site, before the Tokio runtime is dropped) rather than
    // letting a detached watcher's leftover state block `main()`'s exit.
    std::process::exit(0);
}

async fn run_stdio_server(
    config: Settings,
    settings: Arc<Settings>,
    facade: IndexFacade,
    index_path: PathBuf,
    watch: bool,
    actual_watch_interval: u64,
) {
    // Acquire the stdio serve lock before doing anything else. Bound at
    // function scope so the guard removes the lockfile on return / unwind.
    // The process::exit arms below must drop it explicitly: exit skips
    // destructors and would leave the lockfile behind.
    let serve_lock = match PidLockGuard::acquire(&index_path.join("serve.lock")) {
        Ok(guard) => guard,
        Err(PidLockError::Held { pid, lock_path }) => {
            eprintln!(
                "Another codanna serve is already running for this index (PID {pid}, lock at {}).",
                crate::parsing::paths::render_absolute_path(&lock_path).display()
            );
            eprintln!();
            eprintln!("Subagents and other AI tools may have spawned a duplicate. To run multiple");
            eprintln!("clients against one index, use HTTP mode:");
            eprintln!("  codanna serve --http --watch");
            eprintln!("HTTP mode supports concurrent clients without lock conflicts.");
            eprintln!();
            eprintln!(
                "If you are sure no other codanna serve is running, remove {} and retry.",
                crate::parsing::paths::render_absolute_path(&lock_path).display()
            );
            std::process::exit(1);
        }
        Err(PidLockError::Io(e)) => {
            eprintln!(
                "Failed to acquire serve lock under {}: {e}",
                crate::parsing::paths::render_absolute_path(&index_path).display()
            );
            std::process::exit(1);
        }
    };

    // stdio mode - current implementation
    eprintln!("Starting MCP server on stdio transport");
    if watch {
        eprintln!("Index watching enabled (interval: {actual_watch_interval}s)");
    }
    eprintln!("To test: npx @modelcontextprotocol/inspector cargo run -- serve");

    // Create MCP server using the already-loaded facade
    tracing::debug!(
        target: "mcp",
        "creating server with facade - symbols: {}, semantic: {}",
        facade.symbol_count(),
        facade.has_semantic_search()
    );

    // Startup GC: reclaim stale generations left behind by a prior run, once
    // per process start, independent of whether the watcher below ends up
    // starting.
    let _ = crate::storage::generation::gc_logged(
        facade.index_layout(),
        facade.settings().indexing.previous_generation_max_age(),
        "startup",
    );

    let broadcaster = Arc::new(crate::mcp::notifications::NotificationBroadcaster::new(100));
    let server =
        crate::mcp::CodeIntelligenceServer::new(facade).with_broadcaster(broadcaster.clone());

    // Load document store and attach to server (shared with watcher later)
    let document_store_arc = crate::documents::load_from_settings(&config);
    let server = if let Some(ref store_arc) = document_store_arc {
        tracing::debug!(target: "mcp", "attaching document store to server");
        server.with_document_store_arc(store_arc.clone())
    } else {
        server
    };

    // If watch mode is enabled, start the hot-reload watcher
    if watch {
        use crate::watcher::HotReloadWatcher;
        use std::time::Duration;

        let facade_arc = server.get_facade_arc();
        let watcher = HotReloadWatcher::new(
            facade_arc,
            settings.clone(),
            Duration::from_secs(actual_watch_interval),
        )
        .with_broadcaster(broadcaster.clone());

        // Spawn watcher in background
        tokio::spawn(async move {
            watcher.watch().await;
        });

        eprintln!("Hot-reload watcher started");
    }

    // Start unified file watcher if enabled
    if watch || config.file_watch.enabled {
        use crate::watcher::UnifiedWatcher;
        use crate::watcher::handlers::{CodeFileHandler, ConfigFileHandler, DocumentFileHandler};

        let workspace_root = config
            .workspace_root
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

        let settings_path = workspace_root.join(".codanna/settings.toml");
        let debounce_ms = config.file_watch.debounce_ms;
        let facade_arc = server.get_facade_arc();

        // Build unified watcher with handlers
        let mut builder = UnifiedWatcher::builder()
            .broadcaster(broadcaster.clone())
            .indexer(facade_arc.clone())
            .workspace_root(workspace_root.clone())
            .debounce_ms(debounce_ms)
            .refresh_on_overflow(config.file_watch.refresh_on_overflow)
            .startup_catch_up(config.file_watch.startup_catch_up)
            .cancellation_token(tokio_util::sync::CancellationToken::new());

        // Add code file handler
        builder = builder.handler(CodeFileHandler::new(
            facade_arc.clone(),
            workspace_root.clone(),
        ));

        // Add config file handler
        match ConfigFileHandler::new(settings_path.clone()) {
            Ok(config_handler) => {
                builder = builder.handler(config_handler);
            }
            Err(e) => {
                eprintln!("Failed to create config handler: {e}");
            }
        }

        // Add document handler using shared document store
        if let Some(store_arc) = document_store_arc {
            tracing::debug!(target: "mcp", "adding document handler to watcher");
            builder = builder
                .document_store(store_arc.clone())
                .chunking_config(config.documents.defaults.clone())
                .handler(DocumentFileHandler::new(store_arc, workspace_root.clone()));
        }

        // Subscribe to broadcaster for MCP notifications
        let notification_receiver = broadcaster.subscribe();
        let notification_server = server.clone();

        // Build and start the unified watcher
        match builder.build() {
            Ok(unified_watcher) => {
                tokio::spawn(async move {
                    if let Err(e) = unified_watcher.watch().await {
                        eprintln!("Unified watcher error: {e}");
                    }
                });
                eprintln!(
                    "Unified watcher started (debounce: {debounce_ms}ms, config: {})",
                    crate::parsing::paths::render_absolute_path(&settings_path).display()
                );

                // Start notification listener to forward events to MCP client
                tokio::spawn(async move {
                    notification_server
                        .start_notification_listener(notification_receiver)
                        .await;
                });
            }
            Err(e) => {
                eprintln!("Failed to start unified watcher: {e}");
            }
        }
    }

    // Start server with stdio transport
    use rmcp::{ServerHandler, ServiceExt};
    let discover_result = serde_json::to_value(rmcp::model::DiscoverResult::from_server_info(
        server.supported_protocol_versions().into_owned(),
        server.get_info(),
    ))
    .expect("DiscoverResult serializes: closed struct of strings and maps");
    let service = match server.serve(probe_tolerant_stdio(discover_result)).await {
        Ok(service) => service,
        Err(e) => {
            eprintln!("Failed to start MCP server: {e}");
            drop(serve_lock);
            std::process::exit(1);
        }
    };

    // Wait for server to complete, racing it against SIGTERM/Ctrl+C. The
    // default disposition of SIGTERM is immediate termination, which would
    // skip destructors entirely and leave `serve_lock`'s lockfile behind
    // (mirrors the same rationale as `http_server::serve_http`'s
    // `shutdown_signal`, which additionally deregisters a server-registry
    // entry -- stdio mode never registers one, so only the lock needs
    // dropping here).
    tokio::select! {
        result = service.waiting() => {
            if let Err(e) = result {
                eprintln!("MCP server error: {e}");
                drop(serve_lock);
                std::process::exit(1);
            }
        }
        _ = stdio_shutdown_signal() => {
            eprintln!("Shutting down stdio MCP server...");
            drop(serve_lock);
            crate::serve_registry::remove_entry(std::process::id());
            std::process::exit(0);
        }
    }
}

/// Await a shutdown signal (SIGTERM on Unix, in addition to Ctrl+C/SIGINT;
/// Ctrl+C only elsewhere) so `run_stdio_server` can drop `PidLockGuard`
/// (removing the serve lockfile) instead of relying on the default
/// disposition of SIGTERM, which terminates the process immediately and
/// skips destructors.
#[cfg(unix)]
async fn stdio_shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut sigterm = signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            eprintln!("Received shutdown signal (Ctrl+C)");
        }
        _ = sigterm.recv() => {
            eprintln!("Received shutdown signal (SIGTERM)");
        }
    }
}

#[cfg(not(unix))]
async fn stdio_shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("failed to listen for ctrl+c");
    eprintln!("Received shutdown signal (Ctrl+C)");
}

/// Serve a degraded stdio MCP session for a gate-refused index.
/// Completes the handshake with zero tools and heal instructions;
/// never touches the index, so no serve lock is taken and no watcher
/// starts. The caller exits with the gate code when this returns.
pub async fn run_stale_stdio(stored: Option<u32>, current: u32) {
    use rmcp::{ServerHandler, ServiceExt};

    let server = crate::mcp::StaleIndexServer::new(stored, current);
    let discover_result = serde_json::to_value(rmcp::model::DiscoverResult::from_server_info(
        server.supported_protocol_versions().into_owned(),
        server.get_info(),
    ))
    .expect("DiscoverResult serializes: closed struct of strings and maps");
    match server.serve(probe_tolerant_stdio(discover_result)).await {
        Ok(service) => {
            if let Err(e) = service.waiting().await {
                eprintln!("Degraded MCP server error: {e}");
            }
        }
        Err(e) => {
            eprintln!("Failed to start degraded MCP server: {e}");
        }
    }
}

#[cfg(test)]
mod wait_for_exit_tests {
    use super::wait_for_exit;

    // `wait_for_exit` takes `is_alive` as an injectable predicate specifically
    // so the "still alive after deadline" branch (stop_server's "did not exit
    // within Ns" path) can be exercised hermetically -- no real stuck process,
    // no wall-clock dependency beyond the short deadline injected here.
    #[tokio::test]
    async fn wait_for_exit_reports_still_alive_after_deadline() {
        let deadline = std::time::Duration::from_millis(200);
        let poll_interval = std::time::Duration::from_millis(20);

        let exited = wait_for_exit(1, deadline, poll_interval, |_pid| true).await;

        assert!(
            !exited,
            "expected wait_for_exit to report the pid still alive once the deadline elapsed"
        );
    }
}

#[cfg(test)]
mod server_mode_selection_tests {
    use super::resolve_server_mode;

    // These tests exercise `resolve_server_mode` in isolation: it is a pure
    // function of (https, http, proxy, config_mode) -> &'static str with no
    // I/O, so precedence can be asserted hermetically without spawning any
    // process or binding any port.
    //
    // The stdio<->HTTP delegating pump in `mcp::proxy` is intentionally NOT
    // unit tested here: exercising it for real requires a live backing HTTP
    // MCP server (a spawned child process) and a connected stdio client on
    // the other end, which is an integration/manual validation concern, not
    // a hermetic unit test.

    #[test]
    fn cli_proxy_flag_selects_proxy() {
        assert_eq!(resolve_server_mode(false, false, true, "stdio"), "proxy");
    }

    #[test]
    fn config_mode_proxy_selects_proxy_with_bare_serve() {
        // Bare `codanna serve` (no CLI transport flags) must be able to
        // default to proxy via settings.toml `server.mode = "proxy"`.
        assert_eq!(resolve_server_mode(false, false, false, "proxy"), "proxy");
    }

    #[test]
    fn cli_http_flag_wins_over_config_mode_proxy() {
        assert_eq!(resolve_server_mode(false, true, false, "proxy"), "http");
    }

    #[test]
    fn cli_https_flag_wins_over_everything() {
        assert_eq!(resolve_server_mode(true, false, false, "proxy"), "https");
        assert_eq!(resolve_server_mode(true, false, true, "http"), "https");
    }

    #[test]
    fn config_mode_http_selects_http_without_cli_flags() {
        assert_eq!(resolve_server_mode(false, false, false, "http"), "http");
    }

    #[test]
    fn default_is_stdio() {
        assert_eq!(resolve_server_mode(false, false, false, "stdio"), "stdio");
    }
}
