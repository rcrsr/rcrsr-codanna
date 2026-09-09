//! `codanna ls` -- a read-only, top-level listing of every `codanna serve`
//! process visible to the invoking user, merged from three sources keyed by
//! pid into one table:
//!
//! 1. Live, non-stale entries from the per-user server registry
//!    (`crate::serve_registry::list_entries`), split by `role` into
//!    `Server`/`Proxy` rows.
//! 2. "Rogue" `codanna serve` pids -- discovered via a full-process-table
//!    scan (`crate::io::process::scan_codanna_serve_pids`) -- that have no
//!    live registry entry, enriched best-effort via [`resolve_rogue`].
//! 3. Each registered `Proxy` entry, attributed to the registered `Server`
//!    entry sharing its `workspace_root` (via `serve_registry::paths_match`),
//!    displayed as an attached proxy row under its backing server.
//!
//! This module is the SOLE owner of the merge/table-building logic for
//! `codanna ls`: no other module builds this table. It is read-only -- it
//! never calls `write_entry`, `remove_entry`, or any reap logic as a side
//! effect of listing, unlike `codanna serve --reap`/`--stop`.

use std::collections::HashSet;
use std::path::Path;

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

use crate::serve_discovery::{self, ServeScheme};
use crate::serve_registry::{RegistryEntry, ServerRole, ServerStatus, paths_match};

/// Best-effort enrichment for "rogue" `codanna serve` pids -- processes
/// that look like a codanna server (see
/// [`crate::io::process::looks_like_codanna_serve`]) but have no
/// corresponding entry in the per-user server registry
/// (`crate::serve_registry`), e.g. because they were started before the
/// registry existed, or their registry entry was pruned/removed out from
/// under a still-running process.
///
/// Best-effort resolve a rogue pid's workspace root, port, scheme, and
/// server/proxy kind, in a single `sysinfo` refresh -- rather than the two
/// separate per-pid scans `resolve_rogue`/`guess_rogue_kind` used to perform
/// independently (each constructing its own `System::new()` and refreshing
/// the same pid a second time).
///
/// Resolution order for workspace/port/scheme:
/// 1. Read the process's current working directory (best-effort guess at
///    its workspace root).
/// 2. If `<cwd>/.codanna/serve.json` exists and its recorded pid matches
///    `pid`, prefer its `port`/`scheme` -- this is authoritative discovery
///    data written by the server itself, not a guess.
/// 3. Otherwise, fall back to best-effort parsing a `--bind HOST:PORT` (or
///    `--bind=HOST:PORT`) argument out of the process's argv for a port
///    guess. No scheme can be inferred this way, so `scheme` stays `None`.
///
/// `kind` is a best-effort guess at whether the pid is a stdio-facing proxy
/// or a backing server, by checking its argv for a literal `--proxy` token
/// (see [`kind_from_cmd`]); it defaults to [`RowKind::Server`] when the
/// process cannot be inspected.
///
/// Any field that cannot be discovered -- because the process has already
/// exited, its cwd/argv are unreadable (e.g. permission-restricted
/// `/proc/<pid>` on Linux), or no `--bind` argument is present -- renders as
/// `None`. This function never returns an error.
fn resolve_rogue(pid: u32) -> (Option<String>, Option<u16>, Option<ServeScheme>, RowKind) {
    let target = Pid::from_u32(pid);
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[target]),
        true,
        ProcessRefreshKind::nothing()
            .with_cwd(UpdateKind::Always)
            .with_cmd(UpdateKind::Always),
    );
    let Some(process) = sys.process(target) else {
        return (None, None, None, RowKind::Server);
    };
    let kind = kind_from_cmd(process.cmd());

    let Some(cwd) = process.cwd().map(Path::to_path_buf) else {
        return (None, None, None, kind);
    };
    let workspace = cwd.to_string_lossy().into_owned();

    if let Some((port, scheme)) = read_record_if_pid_matches(&cwd, pid) {
        return (Some(workspace), Some(port), Some(scheme), kind);
    }

    let port = parse_bind_port(process.cmd()).filter(|port| *port != 0);
    (Some(workspace), port, None, kind)
}

/// Read `<cwd>/.codanna/serve.json` (via the existing
/// `serve_discovery::read_record`) and return its `port`/`scheme` only if
/// the record's own pid matches `pid` -- a mismatched pid means the record
/// names an unrelated server launch (e.g. a newer server that has since
/// overwritten the file), so it must not be attributed to this pid.
fn read_record_if_pid_matches(cwd: &Path, pid: u32) -> Option<(u16, ServeScheme)> {
    let codanna_dir = cwd.join(crate::init::local_dir_name());
    let record = serve_discovery::read_record(&codanna_dir)?;
    if record.pid != pid {
        return None;
    }
    Some((record.port, record.scheme))
}

/// Best-effort parse of a `--bind HOST:PORT` (space-separated) or
/// `--bind=HOST:PORT` (`=`-joined) argument out of a process's argv,
/// returning just the port. Returns `None` if no `--bind` argument is
/// present, or its value does not parse as `HOST:PORT` with a valid `u16`
/// port.
fn parse_bind_port(argv: &[std::ffi::OsString]) -> Option<u16> {
    let argv: Vec<String> = argv
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();

    for (i, arg) in argv.iter().enumerate() {
        if let Some(value) = arg.strip_prefix("--bind=") {
            if let Some(port) = parse_addr_port(value) {
                return Some(port);
            }
        } else if arg == "--bind" {
            if let Some(value) = argv.get(i + 1) {
                if let Some(port) = parse_addr_port(value) {
                    return Some(port);
                }
            }
        }
    }
    None
}

/// Parse the port out of a `HOST:PORT` bind address string.
fn parse_addr_port(addr: &str) -> Option<u16> {
    addr.rsplit(':').next()?.parse().ok()
}

/// Kind of `codanna serve` process a listed row names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowKind {
    Server,
    Proxy,
}

impl RowKind {
    fn as_str(self) -> &'static str {
        match self {
            RowKind::Server => "server",
            RowKind::Proxy => "proxy",
        }
    }
}

/// Where a listed row's data came from: a live per-user registry entry, or a
/// pid discovered only by scanning the process table (no matching registry
/// entry).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowSource {
    Registered,
    Rogue,
}

impl RowSource {
    fn as_str(self) -> &'static str {
        match self {
            RowSource::Registered => "registered",
            RowSource::Rogue => "rogue",
        }
    }
}

/// One row of the merged `codanna ls` table. See the module doc for how rows
/// are merged from the three sources.
struct Row {
    pid: u32,
    kind: RowKind,
    source: RowSource,
    port: Option<u16>,
    scheme: Option<ServeScheme>,
    status: String,
    workspace: Option<String>,
}

/// Best-effort guess at whether a rogue process is a stdio-facing proxy or a
/// backing server, by checking its argv for a literal `--proxy` token.
/// Defaults to `Server` when the token is absent, since a backing server is
/// the common case and every field of a rogue row is already best-effort
/// display data (see [`resolve_rogue`]). Pure function over an already-
/// fetched argv so [`resolve_rogue`] can derive kind from the single scan it
/// already performs, instead of a second per-pid `sysinfo` refresh.
fn kind_from_cmd(cmd: &[std::ffi::OsString]) -> RowKind {
    let is_proxy = cmd.iter().any(|arg| arg.to_string_lossy() == "--proxy");
    if is_proxy {
        RowKind::Proxy
    } else {
        RowKind::Server
    }
}

fn registry_status_str(status: ServerStatus) -> &'static str {
    match status {
        ServerStatus::Spawning => "spawning",
        ServerStatus::Healthy => "healthy",
    }
}

fn registered_row(entry: &RegistryEntry, kind: RowKind) -> Row {
    Row {
        pid: entry.pid,
        kind,
        source: RowSource::Registered,
        port: (entry.port != 0).then_some(entry.port),
        scheme: Some(entry.scheme),
        status: registry_status_str(entry.status).to_string(),
        workspace: Some(entry.workspace_root.display().to_string()),
    }
}

/// Build the merged `codanna ls` table.
///
/// Read-only: this only reads `serve_registry::list_entries` and performs a
/// process-table scan (`io::process::scan_codanna_serve_pids`); it never
/// calls `write_entry`, `remove_entry`, or any reap logic as a side effect of
/// building the listing.
///
/// Merge order:
/// 1. Live, non-stale registry entries (`role = Server`), one row each, pid-
///    ascending.
/// 2. Immediately under each server row, any live registry entry with
///    `role = Proxy` whose `workspace_root` matches that server's (via
///    `serve_registry::paths_match`) -- an "attached" proxy row.
/// 3. Any registered proxy left unattached (no live server shares its
///    workspace, e.g. the backing server is itself rogue or stale).
/// 4. Rogue pids from a full-process-table scan, minus any pid already
///    covered by step 1/2/3's live registry entries, enriched best-effort
///    via a single per-pid [`resolve_rogue`] call.
fn build_rows() -> Vec<Row> {
    let live_entries: Vec<RegistryEntry> = crate::serve_registry::list_entries()
        .into_iter()
        .filter(|entry| !crate::serve_registry::entry_is_stale(entry))
        .collect();

    let mut servers: Vec<&RegistryEntry> = live_entries
        .iter()
        .filter(|entry| entry.role == ServerRole::Server)
        .collect();
    servers.sort_by_key(|entry| entry.pid);

    let mut proxies: Vec<&RegistryEntry> = live_entries
        .iter()
        .filter(|entry| entry.role == ServerRole::Proxy)
        .collect();
    proxies.sort_by_key(|entry| entry.pid);

    let mut attached: HashSet<u32> = HashSet::new();
    let mut rows = Vec::new();

    for server in &servers {
        rows.push(registered_row(server, RowKind::Server));

        for proxy in &proxies {
            if paths_match(&proxy.workspace_root, &server.workspace_root) {
                attached.insert(proxy.pid);
                rows.push(registered_row(proxy, RowKind::Proxy));
            }
        }
    }

    for proxy in &proxies {
        if !attached.contains(&proxy.pid) {
            rows.push(registered_row(proxy, RowKind::Proxy));
        }
    }

    let registered_pids: HashSet<u32> = live_entries.iter().map(|entry| entry.pid).collect();

    let mut rogue_pids: Vec<u32> = crate::io::process::scan_codanna_serve_pids()
        .into_iter()
        .filter(|pid| !registered_pids.contains(pid))
        .collect();
    rogue_pids.sort_unstable();
    rogue_pids.dedup();

    for pid in rogue_pids {
        let (workspace, port, scheme, kind) = resolve_rogue(pid);
        rows.push(Row {
            pid,
            kind,
            source: RowSource::Rogue,
            port,
            scheme,
            status: "running".to_string(),
            workspace,
        });
    }

    rows
}

/// Render the merged `codanna ls` table as the exact text `run` prints.
///
/// Preserves the empty-result wording style of `codanna serve --list`'s
/// `print_registry_list` ("No running codanna servers registered.") for the
/// case where `rows` is empty. Split out from `run` (which owns nothing
/// beyond calling this and printing it) so the exact text can be asserted on
/// directly against a synthetic `rows` slice, without depending on the real
/// per-user registry or a live process-table scan.
fn render(rows: &[Row]) -> String {
    if rows.is_empty() {
        return "No running codanna servers registered.\n".to_string();
    }

    let mut out = format!(
        "{:<10} {:<7} {:<11} {:<7} {:<7} {:<10} WORKSPACE\n",
        "PID", "KIND", "SOURCE", "PORT", "SCHEME", "STATUS"
    );
    for row in rows {
        let port = row
            .port
            .map(|p| p.to_string())
            .unwrap_or_else(|| "-".to_string());
        let scheme = row
            .scheme
            .map(|s| s.as_str().to_string())
            .unwrap_or_else(|| "-".to_string());
        let workspace = row.workspace.as_deref().unwrap_or("-");
        out.push_str(&format!(
            "{:<10} {:<7} {:<11} {:<7} {:<7} {:<10} {}\n",
            row.pid,
            row.kind.as_str(),
            row.source.as_str(),
            port,
            scheme,
            row.status,
            workspace
        ));
    }
    out
}

/// Run `codanna ls`: print the merged table described in the module doc.
/// Read-only: see `build_rows`.
pub fn run() {
    print!("{}", render(&build_rows()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_roots_match_identical_paths() {
        let path = Path::new("/tmp/some-workspace");
        assert!(paths_match(path, path));
    }

    #[test]
    fn workspace_roots_match_differing_paths_that_do_not_canonicalize() {
        // Neither side exists on disk, so both fall back to the raw path;
        // two distinct raw paths must not match.
        assert!(!paths_match(
            Path::new("/tmp/does-not-exist-a"),
            Path::new("/tmp/does-not-exist-b"),
        ));
    }

    #[test]
    fn row_kind_and_source_render_expected_strings() {
        assert_eq!(RowKind::Server.as_str(), "server");
        assert_eq!(RowKind::Proxy.as_str(), "proxy");
        assert_eq!(RowSource::Registered.as_str(), "registered");
        assert_eq!(RowSource::Rogue.as_str(), "rogue");
    }

    /// (1) An empty result set -- no registered entries and no rogue pids --
    /// must render the same "no servers" wording `codanna serve --list` uses
    /// for its own empty case, not a bare header with no rows or a different
    /// message. Exercised directly against `render`, independent of the real
    /// per-user registry or a live process-table scan, so it is deterministic
    /// regardless of what else happens to be running on the host.
    #[test]
    fn render_empty_rows_prints_no_servers_message() {
        assert_eq!(render(&[]), "No running codanna servers registered.\n");
    }

    #[test]
    fn render_non_empty_rows_includes_header_and_every_row() {
        let rows = vec![
            Row {
                pid: 111,
                kind: RowKind::Server,
                source: RowSource::Registered,
                port: Some(8080),
                scheme: Some(ServeScheme::Http),
                status: "healthy".to_string(),
                workspace: Some("/tmp/ws".to_string()),
            },
            Row {
                pid: 222,
                kind: RowKind::Proxy,
                source: RowSource::Registered,
                port: Some(8081),
                scheme: Some(ServeScheme::Http),
                status: "healthy".to_string(),
                workspace: Some("/tmp/ws".to_string()),
            },
            Row {
                pid: 333,
                kind: RowKind::Server,
                source: RowSource::Rogue,
                port: None,
                scheme: None,
                status: "running".to_string(),
                workspace: None,
            },
        ];

        let text = render(&rows);
        assert!(
            text.starts_with("PID"),
            "output should start with a header row: {text}"
        );
        assert!(text.contains("111") && text.contains("server") && text.contains("registered"));
        assert!(text.contains("222") && text.contains("proxy") && text.contains("registered"));
        assert!(text.contains("333") && text.contains("rogue") && text.contains('-'));
    }
}
