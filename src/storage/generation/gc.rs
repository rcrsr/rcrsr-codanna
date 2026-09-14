//! Garbage collection for stale index generations.
//!
//! [`gc`] deletes generation directories that are no longer useful: dead
//! builds ([`GenerationState::Orphan`]), superseded valid builds
//! ([`GenerationState::Previous`], subject to `keep_previous`), and damaged
//! builds ([`GenerationState::Damaged`]) once the current generation has
//! demonstrably been served since the damage occurred. [`GenerationState::Current`]
//! and [`GenerationState::Building`] are never deleted;
//! [`GenerationState::Incompatible`] is left alone in this phase (deciding
//! its fate belongs to whichever work item wires binary-version
//! negotiation).
//!
//! [`gc`] runs only at explicit points -- before and after every publish,
//! once at server startup, and on `codanna index --gc` -- never on a timer
//! or background task. [`gc_logged`] is the logging wrapper those call
//! sites share.

use std::fs;
use std::fs::OpenOptions;
use std::io::ErrorKind;
use std::time::{Duration, UNIX_EPOCH};

use crate::error::{IndexError, IndexResult};

use super::layout::{GenerationState, list_generations};
use super::markers::Complete;
use super::{GenerationId, IndexLayout};

/// Outcome of a single [`gc`] run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GcSummary {
    /// Generation directories deleted this run (including ones that were
    /// already gone -- `ENOENT` on delete counts as success).
    pub removed: Vec<GenerationId>,
    /// Generation directories this run wanted to delete but could not
    /// because the underlying files were transiently in use (Windows
    /// sharing-violation/`PermissionDenied` only); a later run may succeed.
    pub retried_later: Vec<GenerationId>,
    /// `true` when another process already held the GC lock, so this run
    /// did nothing and returned immediately.
    pub skipped_locked: bool,
}

/// Reclaim disk space used by stale generations under `layout`.
///
/// Acquires an exclusive, non-blocking advisory lock on `root/gc.lock` for
/// the duration of the run so concurrent `gc` invocations (from separate
/// processes) never race each other's deletes. When the lock is already
/// held elsewhere, this returns immediately with
/// `GcSummary { skipped_locked: true, .. }` -- that is not an error, just a
/// no-op this time.
///
/// `keep_previous` controls how many [`GenerationState::Previous`]
/// generations survive: the single newest one when `true`, none when
/// `false`.
pub fn gc(layout: &IndexLayout, keep_previous: bool) -> IndexResult<GcSummary> {
    fs::create_dir_all(layout.root()).map_err(|e| IndexError::FileWrite {
        path: layout.root().to_path_buf(),
        source: e,
    })?;

    let lock_path = layout.gc_lock();
    let lock_file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(|e| IndexError::FileWrite {
            path: lock_path.clone(),
            source: e,
        })?;

    // See the msrv note in `markers::Building::start` for why `try_lock`
    // (stabilized Rust 1.89) is used here despite being above this crate's
    // declared clippy msrv floor.
    #[allow(clippy::incompatible_msrv)]
    let lock_result = lock_file.try_lock();
    match lock_result {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => {
            return Ok(GcSummary {
                skipped_locked: true,
                ..Default::default()
            });
        }
        Err(std::fs::TryLockError::Error(source)) => {
            return Err(IndexError::FileWrite {
                path: lock_path,
                source,
            });
        }
    }

    let result = run(layout, keep_previous);

    // The lock is released when `lock_file` drops at the end of this
    // function; an explicit unlock here just makes that visible to the
    // reader rather than relying solely on the drop.
    #[allow(clippy::incompatible_msrv)]
    let _ = lock_file.unlock();

    result
}

/// Run [`gc`] and log its outcome, so every caller shares one logging
/// policy instead of duplicating the gating logic at each call site.
///
/// Logs at INFO only when `summary.removed` is non-empty (a no-op run stays
/// silent); the INFO line also reports `retried_later`/`skipped_locked` so a
/// reader can distinguish "nothing to collect" from "collection was
/// deferred". `context` tags the line with the caller's situation (e.g.
/// `"startup"`, `"post-publish"`) so log output disambiguates the two
/// callers. A `gc` error is always logged at WARN, regardless of
/// `context` -- GC is best-effort and a failure here must never propagate
/// as a hard error to either caller.
pub fn gc_logged(
    layout: &IndexLayout,
    keep_previous: bool,
    context: &str,
) -> IndexResult<GcSummary> {
    let result = gc(layout, keep_previous);
    match &result {
        Ok(summary) if !summary.removed.is_empty() => tracing::info!(
            "[gc:{context}] removed={} retried_later={} skipped_locked={}",
            summary.removed.len(),
            summary.retried_later.len(),
            summary.skipped_locked
        ),
        Ok(_) => {}
        Err(e) => tracing::warn!("[gc:{context}] failed: {e}"),
    }
    result
}

/// The actual collection pass, run while the GC lock is held.
fn run(layout: &IndexLayout, keep_previous: bool) -> IndexResult<GcSummary> {
    let mut summary = GcSummary::default();

    let current_completed_at = current_completed_at_millis(layout);
    let generations = list_generations(layout)?;

    let mut previous_ids: Vec<&GenerationId> = generations
        .iter()
        .filter(|(_, state, _, _)| *state == GenerationState::Previous)
        .map(|(id, _, _, _)| id)
        .collect();
    // `GenerationId` sorts ascending by construction time; newest last.
    previous_ids.sort();
    let keep_count = if keep_previous { 1 } else { 0 };
    let previous_len = previous_ids.len();
    let previous_to_delete: Vec<GenerationId> = previous_ids
        .into_iter()
        .take(previous_len.saturating_sub(keep_count))
        .cloned()
        .collect();

    for (id, state, _size, _age) in &generations {
        let should_delete = match state {
            GenerationState::Orphan => true,
            GenerationState::Previous => previous_to_delete.contains(id),
            GenerationState::Damaged => {
                is_damaged_stale_enough_to_delete(layout, id, current_completed_at)
            }
            GenerationState::Current
            | GenerationState::Building
            | GenerationState::Incompatible => false,
        };

        if !should_delete {
            continue;
        }

        match remove_generation_dir(layout, id)? {
            RemovalOutcome::Removed => summary.removed.push(id.clone()),
            RemovalOutcome::RetryLater => summary.retried_later.push(id.clone()),
        }
    }

    Ok(summary)
}

/// The current generation's `COMPLETE.completed_at`, or `None` when there is
/// no `current` pointer or its manifest cannot be read -- either way, with
/// no trustworthy "served since" timestamp to compare against, every
/// [`GenerationState::Damaged`] generation is kept for forensics rather than
/// guessed at.
fn current_completed_at_millis(layout: &IndexLayout) -> Option<u64> {
    let current_id = layout.read_current().ok().flatten()?;
    let manifest = Complete::read(&layout.complete_marker(&current_id)).ok()?;
    Some(manifest.completed_at)
}

/// True when the current generation has demonstrably been served since
/// `id`'s damage occurred: its `COMPLETE.completed_at` is later than the
/// wall-clock time `id`'s `COMPLETE` marker (the file whose presence made it
/// classify as [`GenerationState::Damaged`] rather than
/// [`GenerationState::Orphan`]) was last written.
fn is_damaged_stale_enough_to_delete(
    layout: &IndexLayout,
    id: &GenerationId,
    current_completed_at: Option<u64>,
) -> bool {
    let Some(current_completed_at) = current_completed_at else {
        return false;
    };
    let Ok(metadata) = fs::metadata(layout.complete_marker(id)) else {
        return false;
    };
    let Ok(marker_mtime) = metadata.modified() else {
        return false;
    };

    UNIX_EPOCH + Duration::from_millis(current_completed_at) > marker_mtime
}

/// Result of attempting to remove a single generation directory.
enum RemovalOutcome {
    /// Deleted (or already absent, which counts as success).
    Removed,
    /// Deletion failed with a transient, platform-specific "in use" error;
    /// a later run should try again.
    RetryLater,
}

/// Delete `layout.gen_dir(id)` recursively, tolerating `ENOENT` (already
/// gone is success) and, on Windows only, a sharing-violation/
/// `PermissionDenied` error (the file is transiently open elsewhere; retry
/// on a later run rather than failing this whole GC pass).
fn remove_generation_dir(layout: &IndexLayout, id: &GenerationId) -> IndexResult<RemovalOutcome> {
    let dir = layout.gen_dir(id);
    match fs::remove_dir_all(&dir) {
        Ok(()) => Ok(RemovalOutcome::Removed),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(RemovalOutcome::Removed),
        Err(e) if is_windows_retry_later_error(&e) => Ok(RemovalOutcome::RetryLater),
        Err(e) => Err(IndexError::FileWrite {
            path: dir,
            source: e,
        }),
    }
}

/// True when `e` is the Windows-specific "file is open elsewhere" class of
/// error (a sharing violation, or the `PermissionDenied` kind std maps some
/// of those to) that should be retried on a later GC run rather than
/// failing this one. Always `false` on non-Windows platforms, where a
/// `PermissionDenied` deleting a generation directory is a real permissions
/// problem worth surfacing as an error.
#[cfg(windows)]
fn is_windows_retry_later_error(e: &std::io::Error) -> bool {
    // ERROR_SHARING_VIOLATION == 32; std maps some but not all sharing
    // violations to `ErrorKind::PermissionDenied`, so both checks are kept.
    e.kind() == ErrorKind::PermissionDenied || e.raw_os_error() == Some(32)
}

#[cfg(not(windows))]
fn is_windows_retry_later_error(_e: &std::io::Error) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::semantic::SemanticMetadata;
    use crate::storage::{EMISSION_SEMANTICS_VERSION, IndexMetadata};
    use std::thread::sleep;

    use super::super::markers::{Building, CompleteFileEntry};

    fn layout_in(dir: &tempfile::TempDir) -> IndexLayout {
        IndexLayout::new(dir.path().to_path_buf())
    }

    /// Build a structurally valid generation under `layout`'s `gen/<id>/`
    /// (mirrors `layout::tests::write_valid_generation`, private to that
    /// module) with an explicit `completed_at`, so GC's damaged-staleness
    /// comparison can be exercised precisely.
    fn write_valid_generation(layout: &IndexLayout, id: &GenerationId, completed_at: u64) {
        let gen_dir = layout.gen_dir(id);
        fs::create_dir_all(gen_dir.join("tantivy")).expect("create tantivy dir");
        fs::create_dir_all(gen_dir.join("semantic")).expect("create semantic dir");

        let mut meta = IndexMetadata::new();
        meta.emission_version = Some(EMISSION_SEMANTICS_VERSION);
        meta.save(&gen_dir).expect("save index.meta");

        let segment_id = "01977c94df1e1a1e8000000000000000";
        let segment_bytes = b"segment-data";
        let segment_file_name = format!("{segment_id}.store");
        fs::write(
            gen_dir.join("tantivy").join(&segment_file_name),
            segment_bytes,
        )
        .expect("write segment file");

        let tantivy_meta = serde_json::json!({
            "segments": [{"segment_id": segment_id, "max_doc": 1, "deletes": null}],
            "schema": [],
            "opstamp": 1,
            "payload": null,
        });
        fs::write(
            gen_dir.join("tantivy").join("meta.json"),
            serde_json::to_string(&tantivy_meta).expect("serialize tantivy meta"),
        )
        .expect("write tantivy meta.json");

        SemanticMetadata::new("test-model".to_string(), 384, 0)
            .save(&gen_dir.join("semantic"))
            .expect("save semantic metadata");

        let complete = Complete {
            id: id.clone(),
            parent: None,
            started_at: 0,
            completed_at,
            builder_version: "0.0.0".to_string(),
            symbol_count: 0,
            file_count: 0,
            files: vec![CompleteFileEntry {
                path: format!("tantivy/{segment_file_name}"),
                size: segment_bytes.len() as u64,
            }],
        };
        complete.write(layout).expect("write COMPLETE manifest");
    }

    /// A generation dir with no `BUILDING` marker, no `COMPLETE` manifest,
    /// and not named by `current`: classifies as `Orphan`.
    fn write_orphan(layout: &IndexLayout) -> GenerationId {
        let id = GenerationId::generate();
        fs::create_dir_all(layout.gen_dir(&id)).expect("create orphan gen dir");
        id
    }

    #[test]
    fn gc_deletes_orphan_generations() {
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = layout_in(&dir);
        let current = GenerationId::generate();
        write_valid_generation(&layout, &current, 0);
        layout.write_current(&current).expect("write current");
        let orphan = write_orphan(&layout);

        let summary = gc(&layout, true).expect("gc run");

        assert!(summary.removed.contains(&orphan));
        assert!(!layout.gen_dir(&orphan).exists());
        assert!(!summary.skipped_locked);
    }

    #[test]
    fn gc_never_touches_a_building_generation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = layout_in(&dir);
        let current = GenerationId::generate();
        write_valid_generation(&layout, &current, 0);
        layout.write_current(&current).expect("write current");

        let building_id = GenerationId::generate();
        let guard = Building::start(&layout, &building_id, None).expect("start building");

        let summary = gc(&layout, true).expect("gc run");

        assert!(!summary.removed.contains(&building_id));
        assert!(layout.gen_dir(&building_id).is_dir());
        drop(guard);
    }

    #[test]
    fn gc_keeps_only_the_newest_previous_generation_when_keep_previous() {
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = layout_in(&dir);

        let mut previous_ids = Vec::new();
        for _ in 0..3 {
            let id = GenerationId::generate();
            write_valid_generation(&layout, &id, 0);
            previous_ids.push(id);
            sleep(std::time::Duration::from_millis(5));
        }
        let current = GenerationId::generate();
        write_valid_generation(&layout, &current, 0);
        layout.write_current(&current).expect("write current");

        let newest_previous = previous_ids.last().cloned().expect("at least one previous");

        let summary = gc(&layout, true).expect("gc run");

        for id in &previous_ids[..previous_ids.len() - 1] {
            assert!(
                summary.removed.contains(id),
                "older previous generation must be removed"
            );
            assert!(!layout.gen_dir(id).exists());
        }
        assert!(
            !summary.removed.contains(&newest_previous),
            "newest previous generation must survive"
        );
        assert!(layout.gen_dir(&newest_previous).is_dir());
    }

    #[test]
    fn gc_deletes_all_previous_generations_when_keep_previous_is_false() {
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = layout_in(&dir);

        let mut previous_ids = Vec::new();
        for _ in 0..3 {
            let id = GenerationId::generate();
            write_valid_generation(&layout, &id, 0);
            previous_ids.push(id);
            sleep(std::time::Duration::from_millis(5));
        }
        let current = GenerationId::generate();
        write_valid_generation(&layout, &current, 0);
        layout.write_current(&current).expect("write current");

        let summary = gc(&layout, false).expect("gc run");

        for id in &previous_ids {
            assert!(summary.removed.contains(id));
            assert!(!layout.gen_dir(id).exists());
        }
    }

    #[test]
    fn gc_keeps_a_damaged_generation_when_current_has_not_been_served_since_the_damage() {
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = layout_in(&dir);

        // completed_at far in the past: current finished long before the
        // damaged generation's marker was written just now, so current has
        // not "been served since" the damage.
        let current = GenerationId::generate();
        write_valid_generation(&layout, &current, 1);
        layout.write_current(&current).expect("write current");

        let damaged = GenerationId::generate();
        write_valid_generation(&layout, &damaged, 0);
        fs::write(layout.gen_dir(&damaged).join("index.meta"), b"{ not json")
            .expect("corrupt index.meta to force Damaged classification");

        let summary = gc(&layout, true).expect("gc run");

        assert!(!summary.removed.contains(&damaged));
        assert!(layout.gen_dir(&damaged).is_dir());
    }

    #[test]
    fn gc_deletes_a_damaged_generation_once_current_postdates_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = layout_in(&dir);

        let damaged = GenerationId::generate();
        write_valid_generation(&layout, &damaged, 0);
        fs::write(layout.gen_dir(&damaged).join("index.meta"), b"{ not json")
            .expect("corrupt index.meta to force Damaged classification");

        // Give the damaged marker's mtime a moment to be strictly before
        // `current`'s completed_at, which we set to well beyond "now".
        sleep(std::time::Duration::from_millis(5));

        let far_future_millis = unix_millis_far_future();
        let current = GenerationId::generate();
        write_valid_generation(&layout, &current, far_future_millis);
        layout.write_current(&current).expect("write current");

        let summary = gc(&layout, true).expect("gc run");

        assert!(summary.removed.contains(&damaged));
        assert!(!layout.gen_dir(&damaged).exists());
    }

    /// A `completed_at` timestamp far enough in the future that it postdates
    /// any generation-marker mtime a fast-running test could produce.
    fn unix_millis_far_future() -> u64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
            + Duration::from_secs(3600).as_millis() as u64
    }

    #[test]
    fn gc_tolerates_enoent_mid_run() {
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = layout_in(&dir);
        let orphan = write_orphan(&layout);

        // Simulate a concurrent process reclaiming this generation dir
        // between GC's classify/list pass and its own delete attempt: the
        // directory is gone by the time the delete syscall runs.
        fs::remove_dir_all(layout.gen_dir(&orphan)).expect("preemptively remove orphan dir");

        let outcome = remove_generation_dir(&layout, &orphan)
            .expect("a delete racing an already-gone directory must not error");

        assert!(
            matches!(outcome, RemovalOutcome::Removed),
            "ENOENT on delete must be treated as success"
        );

        // And a full `gc` run over a tree with no generation dirs left at
        // all must still complete cleanly.
        let summary = gc(&layout, true).expect("gc must tolerate an already-empty gen/ dir");
        assert!(summary.removed.is_empty());
        assert!(summary.retried_later.is_empty());
        assert!(!summary.skipped_locked);
    }

    #[test]
    fn gc_returns_skipped_locked_when_the_lock_is_already_held() {
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = layout_in(&dir);
        let current = GenerationId::generate();
        write_valid_generation(&layout, &current, 0);
        layout.write_current(&current).expect("write current");
        let orphan = write_orphan(&layout);

        fs::create_dir_all(layout.root()).expect("create root");
        let lock_path = layout.gc_lock();
        let holder = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .expect("open gc lock");
        #[allow(clippy::incompatible_msrv)]
        holder.try_lock().expect("acquire gc lock in test");

        let summary = gc(&layout, true).expect("gc must not error when locked");

        assert!(summary.skipped_locked);
        assert!(summary.removed.is_empty());
        assert!(summary.retried_later.is_empty());
        // The orphan must survive: this run did nothing.
        assert!(layout.gen_dir(&orphan).is_dir());

        drop(holder);
    }

    /// An in-memory `io::Write` sink so a test can assert on the exact text
    /// `tracing` emitted, without depending on a dedicated log-capture crate
    /// (none is a project dependency).
    #[derive(Clone, Default)]
    struct CapturedLogs(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for CapturedLogs {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .expect("captured-logs mutex poisoned")
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl CapturedLogs {
        fn as_string(&self) -> String {
            String::from_utf8(self.0.lock().expect("captured-logs mutex poisoned").clone())
                .expect("tracing output must be valid utf-8")
        }
    }

    /// Run `f` under a `tracing` subscriber that writes formatted events into
    /// the returned buffer, so a test can assert on log-line presence/
    /// absence rather than only on the returned [`GcSummary`].
    fn capture_tracing_output(f: impl FnOnce()) -> CapturedLogs {
        let captured = CapturedLogs::default();
        let make_writer = captured.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || make_writer.clone())
            .with_ansi(false)
            .finish();

        tracing::subscriber::with_default(subscriber, f);
        captured
    }

    #[test]
    fn gc_logged_stays_silent_at_info_when_nothing_was_removed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = layout_in(&dir);
        let current = GenerationId::generate();
        write_valid_generation(&layout, &current, 0);
        layout.write_current(&current).expect("write current");

        let mut summary = None;
        let output = capture_tracing_output(|| {
            summary = Some(gc_logged(&layout, true, "startup").expect("gc_logged run"));
        });

        let summary = summary.expect("gc_logged must have run");
        assert!(summary.removed.is_empty());
        assert!(
            !output.as_string().contains("[gc:startup]"),
            "an empty removed-set must not produce an INFO gc_logged line: {}",
            output.as_string()
        );
    }

    #[test]
    fn gc_logged_logs_at_info_when_something_was_removed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = layout_in(&dir);
        let current = GenerationId::generate();
        write_valid_generation(&layout, &current, 0);
        layout.write_current(&current).expect("write current");
        let orphan = write_orphan(&layout);

        let mut summary = None;
        let output = capture_tracing_output(|| {
            summary = Some(gc_logged(&layout, true, "startup").expect("gc_logged run"));
        });

        let summary = summary.expect("gc_logged must have run");
        assert!(summary.removed.contains(&orphan));
        let text = output.as_string();
        assert!(
            text.contains("[gc:startup]") && text.contains("removed=1"),
            "a non-empty removed-set must produce an INFO gc_logged line tagged with the \
             caller's context and including the removed count: {text}"
        );
    }
}
