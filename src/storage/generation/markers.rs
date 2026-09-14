//! Build-ownership and completion markers for an index generation.
//!
//! A generation directory (`gen/<id>/`) carries at most one of two marker
//! files describing its lifecycle: `BUILDING` while a builder owns it, and
//! `COMPLETE` once that builder finished successfully. [`Building`] guards
//! the former; [`Complete`] reads and writes the latter.
//!
//! ## Liveness rationale (flock + PID)
//!
//! A builder's ownership of `BUILDING` is enforced the same way
//! [`crate::serve_discovery`]'s `PidLockGuard` enforces the stdio serve and
//! spawn locks: an OS advisory lock (`File::try_lock`) held for the whole
//! lifetime of the guard, backed by a PID recorded in the marker's content
//! for out-of-process inspection. The lock is authoritative for "is a
//! process holding this right now" -- it is released by the kernel the
//! instant the holding process exits or crashes, even if the process never
//! runs its own cleanup code, which a purely PID-based check cannot
//! guarantee (a PID can be reused by an unrelated process before anyone
//! notices the original exited). The recorded PID (paired with a
//! `launch_token` unique to this specific process launch) is the fallback
//! used when probing a marker from a separate process that does not want to
//! hold the file open just to ask "is anyone building this generation right
//! now" -- `Building::is_alive` answers that without needing its own
//! long-lived lock attempt to block.

use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rand::RngExt;
use serde::{Deserialize, Serialize};

use crate::error::{IndexError, IndexResult};
use crate::io::process::pid_is_alive;

use super::{GenerationId, IndexLayout};

/// On-disk content of a generation's `BUILDING` marker.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct BuildingMarker {
    pid: u32,
    started_at: u64,
    launch_token: u64,
    builder_version: String,
    parent: Option<GenerationId>,
}

/// Current unix-millis timestamp, saturating to 0 on a clock error rather
/// than panicking (a pre-epoch system clock is not this guard's problem to
/// solve).
fn unix_millis_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Ownership guard for an in-progress generation build.
///
/// [`Building::start`] creates `gen/<id>/`, writes the `BUILDING` marker,
/// and holds an OS advisory lock on it for the guard's lifetime. Dropping
/// the guard releases the lock (the kernel does this automatically when the
/// last file descriptor referencing the lock is closed) but deliberately
/// does **not** delete `gen/<id>/` -- reclaiming an abandoned or completed
/// build directory is a garbage-collection decision, not this guard's.
pub struct Building {
    // Held only to keep the advisory lock alive for the guard's lifetime;
    // never read after `start` returns except via `_lock`'s drop, and via
    // `finish`, which consumes it to release the lock explicitly.
    _lock: std::fs::File,
    started_at: u64,
}

impl Building {
    /// Claim ownership of generation `id`'s build: create `gen/<id>/`, write
    /// the `BUILDING` marker recording this process's pid, a fresh random
    /// launch token, the running binary's version, and `parent`, then
    /// acquire an exclusive advisory lock on that marker file.
    ///
    /// The marker file is opened without truncation *before* the lock is
    /// requested, and is only overwritten with this process's marker
    /// content *after* the lock is confirmed held -- so a marker belonging
    /// to a still-live concurrent builder for this same id is never
    /// clobbered before its liveness can be observed by a caller that reads
    /// the file directly.
    pub fn start(
        layout: &IndexLayout,
        id: &GenerationId,
        parent: Option<GenerationId>,
    ) -> IndexResult<Self> {
        let dir = layout.gen_dir(id);
        std::fs::create_dir_all(&dir).map_err(|e| IndexError::FileWrite {
            path: dir.clone(),
            source: e,
        })?;

        let marker_path = layout.building_marker(id);
        // truncate(false): a still-live concurrent builder's existing
        // marker content must survive until the lock attempt below proves
        // this call actually won the race; only a confirmed lock holder
        // may truncate and overwrite it (see the doc comment above).
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&marker_path)
            .map_err(|e| IndexError::FileWrite {
                path: marker_path.clone(),
                source: e,
            })?;

        // `File::try_lock`/`unlock` stabilized in Rust 1.89, above this
        // crate's declared `clippy.toml` msrv floor (1.85) but within the
        // `stable` toolchain CI actually installs; see this module's doc
        // comment for why the fork-generations design requires flock-based
        // ownership rather than a `create_new`-only marker file.
        #[allow(clippy::incompatible_msrv)]
        let lock_result = file.try_lock();
        match lock_result {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                let holder_pid = read_marker(&marker_path).map(|m| m.pid).unwrap_or(0);
                return Err(IndexError::GenerationBuildInProgress {
                    id: id.to_string(),
                    pid: holder_pid,
                });
            }
            Err(std::fs::TryLockError::Error(source)) => {
                return Err(IndexError::FileWrite {
                    path: marker_path,
                    source,
                });
            }
        }

        let mut rng = rand::rng();
        let started_at = unix_millis_now();
        let marker = BuildingMarker {
            pid: std::process::id(),
            started_at,
            launch_token: rng.random(),
            builder_version: env!("CARGO_PKG_VERSION").to_string(),
            parent,
        };
        let json = serde_json::to_string_pretty(&marker).map_err(|e| IndexError::FileWrite {
            path: marker_path.clone(),
            source: std::io::Error::other(e),
        })?;

        file.set_len(0).map_err(|e| IndexError::FileWrite {
            path: marker_path.clone(),
            source: e,
        })?;
        file.seek(SeekFrom::Start(0))
            .map_err(|e| IndexError::FileWrite {
                path: marker_path.clone(),
                source: e,
            })?;
        file.write_all(json.as_bytes())
            .map_err(|e| IndexError::FileWrite {
                path: marker_path.clone(),
                source: e,
            })?;
        file.sync_all().map_err(|e| IndexError::FileWrite {
            path: marker_path,
            source: e,
        })?;

        Ok(Self {
            _lock: file,
            started_at,
        })
    }

    /// The unix-millis timestamp this build claimed ownership, as recorded
    /// in the `BUILDING` marker by [`Building::start`].
    pub fn started_at(&self) -> u64 {
        self.started_at
    }

    /// Release this guard's advisory lock and remove the `BUILDING` marker
    /// for `id`, signaling the build is no longer in progress. The marker
    /// having already been removed (e.g. by a previous call, or externally)
    /// is not an error.
    pub fn finish(self, layout: &IndexLayout, id: &GenerationId) -> IndexResult<()> {
        let marker_path = layout.building_marker(id);
        drop(self);
        match std::fs::remove_file(&marker_path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(IndexError::FileWrite {
                path: marker_path,
                source: e,
            }),
        }
    }

    /// True when the `BUILDING` marker at `marker_path` describes a build
    /// that is still in progress: either the advisory lock on the marker is
    /// currently held by another open handle (the strong, kernel-enforced
    /// signal), or -- when the lock is free but the marker's content still
    /// names a currently-live process by pid -- that recorded pid is alive
    /// via [`pid_is_alive`] (the fallback signal for content read
    /// out-of-band from the lock probe itself).
    ///
    /// False when the marker is missing, unreadable, or names a pid that is
    /// no longer running.
    pub fn is_alive(marker_path: &Path) -> bool {
        let probe = match OpenOptions::new().read(true).open(marker_path) {
            Ok(f) => f,
            Err(_) => return false,
        };

        // See the msrv note in `Building::start`.
        #[allow(clippy::incompatible_msrv)]
        let lock_result = probe.try_lock();
        match lock_result {
            Err(std::fs::TryLockError::WouldBlock) => return true,
            Ok(()) => {
                // We only opened this handle to probe; release immediately
                // so we don't mask this instant with our own lock.
                #[allow(clippy::incompatible_msrv)]
                let _ = probe.unlock();
            }
            Err(std::fs::TryLockError::Error(_)) => {
                // Inconclusive (e.g. platform/filesystem lock failure);
                // fall through to the pid-based fallback below.
            }
        }

        match read_marker(marker_path) {
            Some(marker) => pid_is_alive(marker.pid),
            None => false,
        }
    }
}

/// Best-effort read of a `BUILDING` marker's content, tolerating a missing
/// or malformed file by returning `None` rather than propagating an error --
/// callers here only use this for liveness probes, where "can't tell" and
/// "not alive" are treated the same way.
fn read_marker(path: &Path) -> Option<BuildingMarker> {
    let mut contents = String::new();
    std::fs::File::open(path)
        .ok()?
        .read_to_string(&mut contents)
        .ok()?;
    serde_json::from_str(&contents).ok()
}

/// One file recorded in a [`Complete`] manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompleteFileEntry {
    pub path: String,
    pub size: u64,
}

/// The `COMPLETE` manifest written once a generation build finishes
/// successfully.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Complete {
    pub id: GenerationId,
    pub parent: Option<GenerationId>,
    pub started_at: u64,
    pub completed_at: u64,
    pub builder_version: String,
    pub symbol_count: u64,
    pub file_count: u64,
    pub files: Vec<CompleteFileEntry>,
}

impl Complete {
    /// Write this manifest to `layout`'s `COMPLETE` marker for `self.id`,
    /// via a temp-file-then-rename swap so a crash mid-write cannot leave a
    /// truncated manifest in place (mirrors
    /// `SemanticMetadata::save_staged`'s write-then-rename idiom).
    pub fn write(&self, layout: &IndexLayout) -> IndexResult<()> {
        let final_path = layout.complete_marker(&self.id);
        let tmp_path = final_path.with_extension("tmp");

        let json = serde_json::to_string_pretty(self).map_err(|e| IndexError::FileWrite {
            path: tmp_path.clone(),
            source: std::io::Error::other(e),
        })?;
        std::fs::write(&tmp_path, json).map_err(|e| IndexError::FileWrite {
            path: tmp_path.clone(),
            source: e,
        })?;
        std::fs::rename(&tmp_path, &final_path).map_err(|e| IndexError::FileWrite {
            path: final_path,
            source: e,
        })?;

        Ok(())
    }

    /// Read a `COMPLETE` manifest from `path`.
    pub fn read(path: &Path) -> IndexResult<Self> {
        let contents = std::fs::read_to_string(path).map_err(|e| IndexError::FileRead {
            path: path.to_path_buf(),
            source: e,
        })?;
        serde_json::from_str(&contents).map_err(|e| IndexError::ParseError {
            path: path.to_path_buf(),
            language: "json".to_string(),
            reason: e.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout_in(dir: &tempfile::TempDir) -> IndexLayout {
        IndexLayout::new(dir.path().to_path_buf())
    }

    #[test]
    fn building_guard_marker_is_alive_while_held() {
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = layout_in(&dir);
        let id = GenerationId::generate();

        let guard = Building::start(&layout, &id, None).expect("start building");
        let marker_path = layout.building_marker(&id);

        assert!(
            Building::is_alive(&marker_path),
            "marker must report alive while the guard holds the lock"
        );

        drop(guard);
    }

    #[test]
    fn building_guard_marker_is_dead_after_drop_with_dead_pid_content() {
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = layout_in(&dir);
        let id = GenerationId::generate();

        let guard = Building::start(&layout, &id, None).expect("start building");
        let marker_path = layout.building_marker(&id);
        drop(guard);

        // Hand-write a marker recording a PID that cannot be alive, with no
        // active lock held on the file, and confirm is_alive reports false.
        let bogus = BuildingMarker {
            pid: dead_pid(),
            started_at: 0,
            launch_token: 0,
            builder_version: "0.0.0".to_string(),
            parent: None,
        };
        std::fs::write(
            &marker_path,
            serde_json::to_string_pretty(&bogus).expect("serialize bogus marker"),
        )
        .expect("write bogus marker");

        assert!(
            !Building::is_alive(&marker_path),
            "marker with a dead pid and no active lock must report not alive"
        );
    }

    #[test]
    fn building_start_creates_generation_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = layout_in(&dir);
        let id = GenerationId::generate();

        let _guard = Building::start(&layout, &id, None).expect("start building");

        assert!(layout.gen_dir(&id).is_dir());
        assert!(layout.building_marker(&id).is_file());
    }

    #[test]
    fn building_start_records_parent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = layout_in(&dir);
        let parent = GenerationId::generate();
        let id = GenerationId::generate();

        let _guard = Building::start(&layout, &id, Some(parent.clone())).expect("start building");

        let marker = read_marker(&layout.building_marker(&id)).expect("read marker back");
        assert_eq!(marker.parent, Some(parent));
        assert_eq!(marker.pid, std::process::id());
    }

    #[test]
    fn finish_removes_the_building_marker_and_releases_the_lock() {
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = layout_in(&dir);
        let id = GenerationId::generate();

        let guard = Building::start(&layout, &id, None).expect("start building");
        let marker_path = layout.building_marker(&id);

        guard.finish(&layout, &id).expect("finish building");

        assert!(
            !marker_path.is_file(),
            "BUILDING marker must be removed after finish"
        );
        assert!(
            !Building::is_alive(&marker_path),
            "finish must release the advisory lock"
        );
    }

    #[test]
    fn complete_write_read_round_trip_preserves_all_fields() {
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = layout_in(&dir);
        let id = GenerationId::generate();
        let parent = GenerationId::generate();
        std::fs::create_dir_all(layout.gen_dir(&id)).expect("create gen dir");

        let manifest = Complete {
            id: id.clone(),
            parent: Some(parent.clone()),
            started_at: 1_000,
            completed_at: 2_000,
            builder_version: "1.2.3".to_string(),
            symbol_count: 42,
            file_count: 7,
            files: vec![
                CompleteFileEntry {
                    path: "src/lib.rs".to_string(),
                    size: 123,
                },
                CompleteFileEntry {
                    path: "src/main.rs".to_string(),
                    size: 456,
                },
            ],
        };

        manifest.write(&layout).expect("write complete manifest");
        let read_back =
            Complete::read(&layout.complete_marker(&id)).expect("read complete manifest");

        assert_eq!(read_back, manifest);
    }

    #[test]
    fn complete_read_missing_file_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("does-not-exist").join("COMPLETE");

        let result = Complete::read(&missing);

        assert!(matches!(result, Err(IndexError::FileRead { .. })));
    }

    /// A pid that is astronomically unlikely to be alive: the max valid
    /// value on Linux (`/proc/sys/kernel/pid_max` is far below `i32::MAX`
    /// by default), reused here purely as "a pid nothing will ever hold".
    fn dead_pid() -> u32 {
        i32::MAX as u32
    }
}
