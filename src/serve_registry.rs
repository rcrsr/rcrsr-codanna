//! Per-user MCP server registry.
//!
//! `src/serve_discovery.rs` tracks, per workspace, the single backing server
//! that tree's `.codanna/serve.json` names. This module tracks something
//! different: every `codanna serve` process currently running for the
//! invoking user, across every workspace, so `codanna serve --list/--stop/
//! --reap` has something to enumerate without walking the filesystem for
//! `.codanna` directories.
//!
//! Each server owns exactly one file, `<registry_dir>/<pid>.json`, written
//! atomically (temp file + rename, mode 0600 on Unix) and removed on that
//! server's own graceful shutdown -- mirroring the write discipline of
//! `serve_discovery::write_record` for the same reasons: a partial write
//! (e.g. a crash mid-write) can never be observed as a corrupt entry, because
//! the rename is the only operation that makes the file visible under its
//! final name. Because each process only ever touches the one file keyed by
//! its own pid, no shared-file lock is needed the way `serve_discovery`'s
//! `http.lock` single-flight lock is for its spawn race.
//!
//! The registry directory lives under the user's per-user state directory
//! (`dirs::state_dir()`), falling back to the data directory
//! (`dirs::data_dir()`) on platforms where `state_dir()` returns `None`
//! (macOS, Windows) -- `data_dir()` is `Some` on all three platforms this
//! crate supports, so the fallback keeps the registry alive everywhere
//! rather than being silently dead on two of three platforms.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub use crate::io::process::{looks_like_codanna_serve, pid_is_alive};
pub use crate::serve_discovery::ServeScheme;

/// Whether a registered server has finished starting up.
///
/// `Spawning` names a server that a spawn attempt has just launched but
/// whose readiness has not yet been confirmed (e.g. `/health` has not yet
/// answered); `Healthy` names one that has. `find_spawning_for` uses this
/// distinction to detect an in-flight spawn for a workspace before starting
/// a second, redundant one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServerStatus {
    Spawning,
    Healthy,
}

/// Whether a registry entry names a backing MCP server or a stdio proxy.
///
/// Defaults to `Server` so that legacy registry entries written before this
/// field existed (no `role` key at all) deserialize as `Server` via
/// `#[serde(default)]` on `RegistryEntry::role`, rather than failing to
/// parse -- every entry the registry has ever written prior to this field's
/// introduction named a backing server, never a proxy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServerRole {
    #[default]
    Server,
    Proxy,
}

/// One entry in the per-user server registry, written to
/// `<registry_dir>/<pid>.json` by the server it describes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryEntry {
    pub pid: u32,
    pub port: u16,
    pub scheme: ServeScheme,
    pub workspace_root: PathBuf,
    /// Unix seconds at which the server started.
    pub start_time: u64,
    pub status: ServerStatus,
    #[serde(default)]
    pub role: ServerRole,
    /// The `codanna` version that wrote this entry, from
    /// `env!("CARGO_PKG_VERSION")`.
    ///
    /// Defaults to an empty string so that legacy registry entries written
    /// before this field existed (no `version` key at all) deserialize
    /// successfully via `#[serde(default)]` on `RegistryEntry::version`,
    /// rather than failing to parse -- mirroring the `role` back-compat
    /// precedent above.
    #[serde(default)]
    pub version: String,
}

/// Errors from reading/writing the per-user server registry.
#[derive(Error, Debug)]
pub enum RegistryError {
    #[error(
        "no per-user state or data directory is available on this platform (both dirs::state_dir() and dirs::data_dir() returned None); the server registry cannot be maintained"
    )]
    NoRegistryDir,

    #[error("failed to create server registry directory '{path}': {source}")]
    CreateDir { path: PathBuf, source: io::Error },

    #[error("failed to write server registry entry '{path}': {source}")]
    Write { path: PathBuf, source: io::Error },

    #[error("failed to set permissions on '{path}': {source}")]
    Permissions { path: PathBuf, source: io::Error },

    #[error("failed to rename '{from}' to '{to}': {source}")]
    Rename {
        from: PathBuf,
        to: PathBuf,
        source: io::Error,
    },

    #[error("failed to serialize server registry entry: {0}")]
    Serialize(#[from] serde_json::Error),
}

pub type RegistryResult<T> = Result<T, RegistryError>;

/// Resolve the per-user server registry directory:
/// `dirs::state_dir().or_else(dirs::data_dir)/codanna/servers`.
///
/// `state_dir()` returns `None` on macOS and Windows (there is no XDG-state
/// equivalent there); falling back to `data_dir()` -- `Some` on all three
/// platforms this crate supports -- keeps the registry functional everywhere
/// instead of silently doing nothing on two of three platforms.
pub fn registry_dir() -> Option<PathBuf> {
    dirs::state_dir()
        .or_else(dirs::data_dir)
        .map(|base| base.join("codanna").join("servers"))
}

fn entry_path(dir: &Path, pid: u32) -> PathBuf {
    dir.join(format!("{pid}.json"))
}

/// Write `entry` to `<registry_dir>/<entry.pid>.json` atomically: a sibling
/// temp file is written first, tightened to mode 0600 on Unix, then renamed
/// into place. See the module doc comment for why no shared-file lock is
/// needed here.
pub fn write_entry(entry: &RegistryEntry) -> RegistryResult<()> {
    let dir = registry_dir().ok_or(RegistryError::NoRegistryDir)?;
    write_entry_in(&dir, entry)
}

/// Directory-parameterized implementation of [`write_entry`], split out so
/// unit tests can exercise the real write/rename/permission discipline
/// against a `TempDir` without mutating process-global environment
/// variables (which `registry_dir()` reads via `dirs::state_dir`/
/// `dirs::data_dir`) -- a change that would race with other tests running in
/// the same process.
fn write_entry_in(dir: &Path, entry: &RegistryEntry) -> RegistryResult<()> {
    fs::create_dir_all(dir).map_err(|source| RegistryError::CreateDir {
        path: dir.to_path_buf(),
        source,
    })?;

    // Harden the registry directory itself to owner-only, mirroring the
    // 0600 hardening applied to each entry file below: on a misconfigured or
    // non-default `$XDG_STATE_HOME` (e.g. pointed at a shared/world-readable
    // location), an unhardened directory could otherwise leak pids via
    // `<pid>.json` filenames to other local users even though the file
    // contents themselves are protected.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(|source| {
            RegistryError::Permissions {
                path: dir.to_path_buf(),
                source,
            }
        })?;
    }

    let final_path = entry_path(dir, entry.pid);
    let tmp_path = dir.join(format!("{}.json.tmp.{}", entry.pid, std::process::id()));

    let json = serde_json::to_string(entry)?;

    fs::write(&tmp_path, json.as_bytes()).map_err(|source| RegistryError::Write {
        path: tmp_path.clone(),
        source,
    })?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp_path, fs::Permissions::from_mode(0o600)).map_err(|source| {
            RegistryError::Permissions {
                path: tmp_path.clone(),
                source,
            }
        })?;
    }

    fs::rename(&tmp_path, &final_path).map_err(|source| RegistryError::Rename {
        from: tmp_path.clone(),
        to: final_path.clone(),
        source,
    })?;

    Ok(())
}

/// Remove `<registry_dir>/<pid>.json`, if present. Best-effort: errors are
/// swallowed since this runs on shutdown paths where there is nothing
/// meaningful to do about a failed removal.
pub fn remove_entry(pid: u32) {
    if let Some(dir) = registry_dir() {
        let _ = fs::remove_file(entry_path(&dir, pid));
    }
}

/// Read every parseable entry out of the registry directory. Files that are
/// missing, unreadable, or fail to parse (a stale in-progress temp file, or a
/// corrupt leftover from a crashed write) are skipped rather than causing an
/// error, matching `serve_discovery::read_record`'s best-effort-discovery
/// precedent.
pub fn list_entries() -> Vec<RegistryEntry> {
    match registry_dir() {
        Some(dir) => list_entries_in(&dir),
        None => Vec::new(),
    }
}

/// Directory-parameterized implementation of [`list_entries`]; see
/// [`write_entry_in`] for why this split exists.
fn list_entries_in(dir: &Path) -> Vec<RegistryEntry> {
    let Ok(read_dir) = fs::read_dir(dir) else {
        return Vec::new();
    };

    read_dir
        .filter_map(Result::ok)
        .filter(|dir_entry| {
            dir_entry
                .path()
                .extension()
                .is_some_and(|ext| ext == "json")
        })
        .filter_map(|dir_entry| {
            let contents = fs::read_to_string(dir_entry.path()).ok()?;
            serde_json::from_str::<RegistryEntry>(&contents).ok()
        })
        .collect()
}

/// Find a live, still-`Spawning` registry entry for `workspace_root`.
///
/// Used to detect an in-flight spawn for a workspace before starting a
/// redundant second one: an entry only counts if its pid is alive (via the
/// zombie-safe `pid_is_alive`) AND its status is still `Spawning` -- a dead
/// pid or one that has already transitioned to `Healthy` does not match.
pub fn find_spawning_for(workspace_root: &Path) -> Option<RegistryEntry> {
    find_spawning_for_in(&list_entries(), workspace_root, is_live_codanna_serve)
}

/// Search-only half of [`find_spawning_for`], split out so unit tests can
/// supply a fixed `Vec<RegistryEntry>` (including dead-pid or
/// already-`Healthy` entries) without depending on `registry_dir()`.
///
/// An entry only counts as an in-flight spawn if, beyond the `Spawning`
/// status and workspace match, `is_live_serve(entry.pid)` holds. In
/// production that means both pid-alive AND cmdline-still-looks-like-codanna
/// (see [`is_live_codanna_serve`]): a stale entry whose pid has been reused
/// by an unrelated live process must not be mistaken for a genuine in-flight
/// spawn, or the caller would wait unnecessarily (or time out) for a spawn
/// that will never report healthy. Taking the check as a parameter (rather
/// than calling `is_live_codanna_serve` directly) lets unit tests simulate
/// "alive and looks like codanna" / "dead or reused" without spawning a real
/// process.
fn find_spawning_for_in(
    entries: &[RegistryEntry],
    workspace_root: &Path,
    is_live_serve: impl Fn(u32) -> bool,
) -> Option<RegistryEntry> {
    entries
        .iter()
        .find(|entry| {
            entry.status == ServerStatus::Spawning
                && paths_match(&entry.workspace_root, workspace_root)
                && is_live_serve(entry.pid)
        })
        .cloned()
}

/// Whether `pid` is both alive and still looks like a `codanna serve`
/// process (see [`looks_like_codanna_serve`]). The production liveness
/// check used by [`find_spawning_for`] and [`entry_is_stale`]: combining
/// both conditions here means a registry entry whose pid has been reused by
/// an unrelated live process is treated the same as a dead pid everywhere
/// staleness matters.
fn is_live_codanna_serve(pid: u32) -> bool {
    pid_is_alive(pid) && looks_like_codanna_serve(pid)
}

/// Whether `entry` is stale: its pid is dead, OR the pid is alive but no
/// longer looks like a `codanna serve` process (i.e. it was reused by an
/// unrelated live process after the original server exited uncleanly).
/// `codanna serve --reap`/`--list` use this instead of a bare
/// `!pid_is_alive(entry.pid)` check so a reused pid does not leave an
/// unprunable stale entry behind.
pub fn entry_is_stale(entry: &RegistryEntry) -> bool {
    !is_live_codanna_serve(entry.pid)
}

/// Whether `a` and `b` name the same workspace root, tolerating a
/// symlink-vs-real-path spelling difference between two proxy invocations
/// that otherwise agree on the workspace: both sides are canonicalized
/// before comparing, falling back to the original (uncanonicalized) path on
/// either side when canonicalization fails (e.g. the path no longer exists),
/// so a failed canonicalize degrades to the previous raw-equality behavior
/// rather than making every comparison fail.
pub(crate) fn paths_match(a: &Path, b: &Path) -> bool {
    let canon_a = std::fs::canonicalize(a).unwrap_or_else(|_| a.to_path_buf());
    let canon_b = std::fs::canonicalize(b).unwrap_or_else(|_| b.to_path_buf());
    canon_a == canon_b
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn sample_entry(pid: u32, workspace_root: &Path, status: ServerStatus) -> RegistryEntry {
        RegistryEntry {
            pid,
            port: 8080,
            scheme: ServeScheme::Http,
            workspace_root: workspace_root.to_path_buf(),
            start_time: 1_700_000_000,
            status,
            role: ServerRole::Server,
            version: "0.0.0-test".to_string(),
        }
    }

    #[test]
    fn legacy_entry_without_role_key_deserializes_as_server() {
        let legacy_json = r#"{
            "pid": 4242,
            "port": 8080,
            "scheme": "http",
            "workspace_root": "/tmp/legacy-workspace",
            "start_time": 1700000000,
            "status": "healthy"
        }"#;

        let entry: RegistryEntry =
            serde_json::from_str(legacy_json).expect("legacy entry without role must deserialize");

        assert_eq!(entry.role, ServerRole::Server);
    }

    #[test]
    fn legacy_entry_without_version_key_deserializes_as_empty() {
        let legacy_json = r#"{
            "pid": 4242,
            "port": 8080,
            "scheme": "http",
            "workspace_root": "/tmp/legacy-workspace",
            "start_time": 1700000000,
            "status": "healthy"
        }"#;

        let entry: RegistryEntry = serde_json::from_str(legacy_json)
            .expect("legacy entry without role/version must deserialize");

        assert!(entry.version.is_empty());
    }

    #[test]
    fn write_then_list_round_trips() {
        let dir = TempDir::new().unwrap();
        let entry = sample_entry(
            std::process::id(),
            Path::new("/tmp/some-workspace"),
            ServerStatus::Healthy,
        );

        write_entry_in(dir.path(), &entry).expect("write_entry_in should succeed");
        let entries = list_entries_in(dir.path());

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].pid, entry.pid);
        assert_eq!(entries[0].port, entry.port);
        assert_eq!(entries[0].workspace_root, entry.workspace_root);
        assert_eq!(entries[0].status, ServerStatus::Healthy);
        assert_eq!(entries[0].version, entry.version);
    }

    #[test]
    #[cfg(unix)]
    fn written_entry_has_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new().unwrap();
        let entry = sample_entry(
            std::process::id(),
            Path::new("/tmp/some-workspace"),
            ServerStatus::Healthy,
        );

        write_entry_in(dir.path(), &entry).expect("write_entry_in should succeed");

        let path = dir.path().join(format!("{}.json", entry.pid));
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "registry entry file must be mode 0600");
    }

    #[test]
    #[cfg(unix)]
    fn registry_directory_has_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new().unwrap();
        let entry = sample_entry(
            std::process::id(),
            Path::new("/tmp/some-workspace"),
            ServerStatus::Healthy,
        );

        write_entry_in(dir.path(), &entry).expect("write_entry_in should succeed");

        let mode = fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "registry directory must be mode 0700");
    }

    #[test]
    fn list_entries_skips_corrupt_files_without_panicking() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("1234.json"), b"not valid json{{{").unwrap();

        let good = sample_entry(
            std::process::id(),
            Path::new("/tmp/another-workspace"),
            ServerStatus::Healthy,
        );
        write_entry_in(dir.path(), &good).expect("write_entry_in should succeed");

        let entries = list_entries_in(dir.path());
        assert_eq!(entries.len(), 1, "corrupt file must be skipped, not panic");
        assert_eq!(entries[0].pid, good.pid);
    }

    #[test]
    fn list_entries_returns_empty_for_missing_directory() {
        let dir = TempDir::new().unwrap();
        let missing = dir.path().join("does-not-exist");
        assert!(list_entries_in(&missing).is_empty());
    }

    #[test]
    fn find_spawning_for_matches_live_spawning_entry_for_workspace() {
        let workspace = Path::new("/tmp/matching-workspace");
        let entries = vec![sample_entry(
            std::process::id(),
            workspace,
            ServerStatus::Spawning,
        )];

        let found = find_spawning_for_in(&entries, workspace, |_| true);
        assert!(found.is_some());
        assert_eq!(found.unwrap().pid, std::process::id());
    }

    #[test]
    fn find_spawning_for_ignores_healthy_entry() {
        let workspace = Path::new("/tmp/healthy-workspace");
        let entries = vec![sample_entry(
            std::process::id(),
            workspace,
            ServerStatus::Healthy,
        )];

        assert!(find_spawning_for_in(&entries, workspace, |_| true).is_none());
    }

    #[test]
    fn find_spawning_for_ignores_dead_pid() {
        let workspace = Path::new("/tmp/dead-workspace");
        // pid 0 reads as dead via pid_is_alive.
        let entries = vec![sample_entry(0, workspace, ServerStatus::Spawning)];

        assert!(find_spawning_for_in(&entries, workspace, |_| false).is_none());
    }

    #[test]
    fn find_spawning_for_ignores_different_workspace() {
        let entries = vec![sample_entry(
            std::process::id(),
            Path::new("/tmp/workspace-a"),
            ServerStatus::Spawning,
        )];

        assert!(find_spawning_for_in(&entries, Path::new("/tmp/workspace-b"), |_| true).is_none());
    }

    #[test]
    fn find_spawning_for_ignores_pid_reused_by_non_codanna_process() {
        // Simulates a stale `Spawning` entry whose pid is alive but has been
        // reused by an unrelated process (the process is alive, but
        // `is_live_serve` reports it does not look like codanna serve).
        let workspace = Path::new("/tmp/reused-pid-workspace");
        let entries = vec![sample_entry(
            std::process::id(),
            workspace,
            ServerStatus::Spawning,
        )];

        assert!(find_spawning_for_in(&entries, workspace, |_| false).is_none());
    }

    #[test]
    fn find_spawning_for_matches_across_symlinked_workspace_path() {
        // Simulates two proxy invocations reaching the same workspace via
        // different path spellings (symlink vs. real path): the registry
        // entry's path and the lookup path canonicalize to the same real
        // path even though they are spelled differently.
        let real_dir = TempDir::new().unwrap();
        let entries = vec![sample_entry(
            std::process::id(),
            real_dir.path(),
            ServerStatus::Spawning,
        )];

        let symlink_dir = TempDir::new().unwrap();
        let symlink_path = symlink_dir.path().join("workspace-link");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(real_dir.path(), &symlink_path).unwrap();
            assert!(find_spawning_for_in(&entries, &symlink_path, |_| true).is_some());
        }
    }

    #[test]
    fn registry_dir_ends_in_codanna_servers() {
        if let Some(dir) = registry_dir() {
            assert!(dir.ends_with(Path::new("codanna").join("servers")));
        }
    }
}
