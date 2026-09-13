//! Minimal filesystem path arithmetic for index generations.
//!
//! This is the subset of `IndexLayout` needed by [`super::markers`]: the
//! `gen/<id>/` directory and the two marker file paths inside it. The full
//! API described in the generations design (`current_file`, `tantivy_dir`,
//! `semantic_dir`, `meta_path`, `damaged_marker`, `gc_lock`, ...) belongs to
//! a separate work item and is intentionally not implemented here.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::error::{IndexError, IndexResult};
use crate::semantic::SemanticMetadata;
use crate::storage::{EMISSION_SEMANTICS_VERSION, IndexMetadata};

use super::GenerationId;
use super::markers::{Building, Complete, CompleteFileEntry};

/// Root-relative path arithmetic for a generation-based index layout.
///
/// `root` is the index directory (e.g. `.codanna/index`); every generation
/// lives under `root/gen/<id>/`.
#[derive(Debug, Clone)]
pub struct IndexLayout {
    root: PathBuf,
}

impl IndexLayout {
    /// Build a layout rooted at `root`. Does not touch the filesystem.
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// The index root directory this layout was constructed with.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The directory holding all files for generation `id`: `root/gen/<id>/`.
    pub fn gen_dir(&self, id: &GenerationId) -> PathBuf {
        self.root.join("gen").join(id.as_str())
    }

    /// Path to the `BUILDING` marker inside `id`'s generation directory.
    pub fn building_marker(&self, id: &GenerationId) -> PathBuf {
        self.gen_dir(id).join("BUILDING")
    }

    /// Path to the `COMPLETE` manifest inside `id`'s generation directory.
    pub fn complete_marker(&self, id: &GenerationId) -> PathBuf {
        self.gen_dir(id).join("COMPLETE")
    }

    /// Path to the `current` pointer file: `root/current`.
    pub fn current_file(&self) -> PathBuf {
        self.root.join("current")
    }

    /// Path to the `DAMAGED` marker inside `id`'s generation directory,
    /// written by [`resolve_current`] when a generation named by `current`
    /// or discovered during recovery fails [`validate_generation`].
    pub fn damaged_marker(&self, id: &GenerationId) -> PathBuf {
        self.gen_dir(id).join("DAMAGED")
    }

    /// Path to the temp file used to atomically replace `current`.
    fn current_tmp_file(&self) -> PathBuf {
        self.root.join("current.tmp")
    }

    /// Read the generation id pointed to by `current`.
    ///
    /// `current` is advisory state: callers reconcile it against the
    /// `gen/` directory contents themselves, so a malformed pointer is
    /// never treated as fatal here. A missing file, empty/whitespace
    /// content, and torn/invalid content (e.g. a crash mid-write before a
    /// [`Self::write_current`] rename lands) all return `Ok(None)`; only
    /// the last case additionally logs a `WARN` so an operator can notice
    /// a pointer that should have been valid.
    pub fn read_current(&self) -> IndexResult<Option<GenerationId>> {
        let path = self.current_file();

        let contents = match fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(IndexError::FileRead { path, source: e }),
        };

        let trimmed = contents.trim();
        if trimmed.is_empty() {
            return Ok(None);
        }

        match GenerationId::new(trimmed) {
            Some(id) => Ok(Some(id)),
            None => {
                tracing::warn!(
                    "[generation] ignoring malformed current pointer at {}: {trimmed:?}",
                    path.display()
                );
                Ok(None)
            }
        }
    }

    /// Atomically point `current` at `id`.
    ///
    /// Writes `<id>\n` to a `current.tmp` sibling in the same directory,
    /// renames it over `current` (an atomic replace on the same
    /// filesystem), then fsyncs the parent directory on unix so the
    /// rename's directory-entry update is itself durable across a crash,
    /// not just the file content.
    pub fn write_current(&self, id: &GenerationId) -> IndexResult<()> {
        fs::create_dir_all(&self.root).map_err(|e| IndexError::FileWrite {
            path: self.root.clone(),
            source: e,
        })?;

        let tmp_path = self.current_tmp_file();
        let final_path = self.current_file();

        let mut file = fs::File::create(&tmp_path).map_err(|e| IndexError::FileWrite {
            path: tmp_path.clone(),
            source: e,
        })?;
        writeln!(file, "{id}").map_err(|e| IndexError::FileWrite {
            path: tmp_path.clone(),
            source: e,
        })?;
        file.sync_all().map_err(|e| IndexError::FileWrite {
            path: tmp_path.clone(),
            source: e,
        })?;
        drop(file);

        fs::rename(&tmp_path, &final_path).map_err(|e| IndexError::FileWrite {
            path: final_path,
            source: e,
        })?;

        fsync_dir(&self.root)
    }
}

/// The lifecycle state of a single generation, per the state table in
/// `internal/index-generations.md` section 2.10.
///
/// `Incompatible` in this phase is judged only against
/// [`EMISSION_SEMANTICS_VERSION`] -- a compile-time constant already baked
/// into the running binary -- not against a live/negotiated binary-version
/// check; that wiring is a later work item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GenerationState {
    /// Named by `current` and validates.
    Current,
    /// Most recent `COMPLETE` generation older than `current` that
    /// validates.
    Previous,
    /// Has a `BUILDING` marker whose builder is alive.
    Building,
    /// `BUILDING` marker present but its builder is dead, or no `COMPLETE`
    /// manifest and not named by `current`.
    Orphan,
    /// `COMPLETE` present but the generation fails [`validate_generation`].
    Damaged,
    /// Validates structurally but was built with an emission-semantics
    /// version this binary does not read.
    Incompatible,
}

/// Read-only structural validation of a generation's on-disk contents.
///
/// Never opens the index: this is file/JSON inspection only, cheap enough to
/// run before every load. Checks, in order:
///
/// 1. `index.meta` exists and parses via [`IndexMetadata::load`].
/// 2. `tantivy/meta.json` exists and parses as JSON, and every segment file
///    it references (cross-checked against the `COMPLETE` manifest's file
///    list, when present) exists on disk with the manifest-recorded size.
/// 3. `semantic/metadata.json`, if present, loads via
///    [`SemanticMetadata::load`] with readable `dimension`/`model_name`.
///
/// Any failure maps to [`IndexError::GenerationDamaged`] naming `id` and a
/// human-readable reason.
pub fn validate_generation(layout: &IndexLayout, id: &GenerationId) -> IndexResult<()> {
    let gen_dir = layout.gen_dir(id);
    let damaged = |reason: String| IndexError::GenerationDamaged {
        id: id.to_string(),
        reason,
    };

    let meta_path = gen_dir.join("index.meta");
    if !meta_path.is_file() {
        return Err(damaged(format!(
            "index.meta missing at {}",
            meta_path.display()
        )));
    }
    IndexMetadata::load(&gen_dir)
        .map_err(|e| damaged(format!("index.meta failed to parse: {e}")))?;

    let tantivy_meta_path = gen_dir.join("tantivy").join("meta.json");
    let tantivy_meta_contents = fs::read_to_string(&tantivy_meta_path)
        .map_err(|e| damaged(format!("tantivy/meta.json unreadable: {e}")))?;
    let tantivy_meta: serde_json::Value = serde_json::from_str(&tantivy_meta_contents)
        .map_err(|e| damaged(format!("tantivy/meta.json failed to parse: {e}")))?;
    let segment_ids = extract_segment_ids(&tantivy_meta)
        .map_err(|reason| damaged(format!("tantivy/meta.json has unexpected shape: {reason}")))?;

    let complete_path = layout.complete_marker(id);
    if complete_path.is_file() {
        let manifest = Complete::read(&complete_path)
            .map_err(|e| damaged(format!("COMPLETE manifest failed to parse: {e}")))?;

        for seg_id in &segment_ids {
            for entry in manifest
                .files
                .iter()
                .filter(|entry| segment_file_belongs_to(&entry.path, seg_id))
            {
                let file_path = gen_dir.join(&entry.path);
                let actual_size = fs::metadata(&file_path)
                    .map_err(|_| {
                        damaged(format!(
                            "segment file '{}' referenced by tantivy/meta.json is missing",
                            entry.path
                        ))
                    })?
                    .len();
                if actual_size != entry.size {
                    return Err(damaged(format!(
                        "segment file '{}' size mismatch: manifest records {} bytes, found {}",
                        entry.path, entry.size, actual_size
                    )));
                }
            }
        }
    }

    let semantic_dir = gen_dir.join("semantic");
    if SemanticMetadata::exists(&semantic_dir) {
        let semantic_meta = SemanticMetadata::load(&semantic_dir)
            .map_err(|e| damaged(format!("semantic/metadata.json failed to load: {e}")))?;
        if semantic_meta.model_name.is_empty() {
            return Err(damaged(
                "semantic/metadata.json has an empty model_name".to_string(),
            ));
        }
        if semantic_meta.dimension == 0 {
            return Err(damaged(
                "semantic/metadata.json has a zero dimension".to_string(),
            ));
        }
    }

    Ok(())
}

/// Extract the `segment_id` string of every entry in `meta["segments"]`.
///
/// A missing `segments` key is treated as "no segments" (an empty index is
/// structurally valid), but a `segments` array whose entries are not the
/// expected shape is reported so the caller can surface it as damage.
fn extract_segment_ids(meta: &serde_json::Value) -> Result<Vec<String>, String> {
    let Some(segments) = meta.get("segments") else {
        return Ok(Vec::new());
    };
    let segments = segments
        .as_array()
        .ok_or_else(|| "\"segments\" is not an array".to_string())?;

    segments
        .iter()
        .map(|segment| {
            segment
                .get("segment_id")
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .ok_or_else(|| "segment entry missing a string \"segment_id\"".to_string())
        })
        .collect()
}

/// True when `path`'s file stem (the file name with its final extension
/// stripped, e.g. `"abcd1234.store"` -> `"abcd1234"`) matches `segment_id`
/// -- the naming convention Tantivy uses for a segment's on-disk files.
fn segment_file_belongs_to(path: &str, segment_id: &str) -> bool {
    Path::new(path)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .is_some_and(|stem| stem == segment_id)
}

/// Tantivy's writer/meta lock files: flock-on-inode files Tantivy opens and
/// holds for the life of a writer/reader but never unlinks. Hardlinking
/// these into a clone would make the clone share the *live* lock with the
/// source generation, which is never correct -- a clone always starts with
/// no lock held.
const TANTIVY_LOCK_FILE_NAMES: [&str; 2] = [".tantivy-writer.lock", ".tantivy-meta.lock"];

/// True when `name` is one of [`TANTIVY_LOCK_FILE_NAMES`].
fn is_tantivy_lock_file(name: &std::ffi::OsStr) -> bool {
    TANTIVY_LOCK_FILE_NAMES
        .iter()
        .any(|lock_name| name == std::ffi::OsStr::new(lock_name))
}

/// True when `name` starts with `.staging-` -- the prefix
/// `SimpleSemanticSearch` uses for transient staging files/dirs under
/// `semantic/` that must never be copied into a clone.
fn is_staging_name(name: &std::ffi::OsStr) -> bool {
    name.to_str().is_some_and(|s| s.starts_with(".staging-"))
}

/// Create `path`'s parent directory (and any missing ancestors) if it does
/// not already exist.
fn ensure_parent_dir(path: &Path) -> IndexResult<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| IndexError::FileWrite {
            path: parent.to_path_buf(),
            source: e,
        })?;
    }
    Ok(())
}

/// Hardlink `from` to `to`, falling back to a byte copy if the hardlink
/// fails (cross-device, FAT, permission, or the destination already
/// existing). The fallback is per-file: a hardlink failure for one file
/// never aborts the clone of the rest.
fn hardlink_or_copy(from: &Path, to: &Path) -> IndexResult<()> {
    ensure_parent_dir(to)?;
    if fs::hard_link(from, to).is_ok() {
        return Ok(());
    }
    fs::copy(from, to)
        .map(|_| ())
        .map_err(|e| IndexError::FileWrite {
            path: to.to_path_buf(),
            source: e,
        })
}

/// Byte-copy `from` to `to`, creating `to`'s parent directory if needed.
fn copy_file(from: &Path, to: &Path) -> IndexResult<()> {
    ensure_parent_dir(to)?;
    fs::copy(from, to)
        .map(|_| ())
        .map_err(|e| IndexError::FileWrite {
            path: to.to_path_buf(),
            source: e,
        })
}

/// Recursively hardlink every file under `from_dir` into the matching path
/// under `to_dir`, skipping Tantivy's lock files entirely and falling back
/// to a copy per-file when hardlinking a particular file fails.
///
/// A missing `from_dir` is not an error: it means the source generation has
/// no `tantivy/` contents to clone (defensive; a valid generation always
/// has one).
fn clone_tantivy_dir(from_dir: &Path, to_dir: &Path) -> IndexResult<()> {
    if !from_dir.is_dir() {
        return Ok(());
    }
    for entry in walkdir::WalkDir::new(from_dir) {
        let entry = entry.map_err(|e| IndexError::FileRead {
            path: from_dir.to_path_buf(),
            source: std::io::Error::other(e),
        })?;
        if !entry.file_type().is_file() {
            continue;
        }
        if is_tantivy_lock_file(entry.file_name()) {
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(from_dir)
            .expect("walkdir always yields entries nested under the dir it walks");
        hardlink_or_copy(entry.path(), &to_dir.join(rel))?;
    }
    Ok(())
}

/// Recursively copy every file under `from_dir` into the matching path
/// under `to_dir`, never hardlinking (semantic files get rewritten wholesale
/// so a shared inode would be unsafe) and skipping any `.staging-*`
/// dir/file entirely.
///
/// A missing `from_dir` is not an error: some generations have no
/// `semantic/` contents.
fn clone_semantic_dir(from_dir: &Path, to_dir: &Path) -> IndexResult<()> {
    if !from_dir.is_dir() {
        return Ok(());
    }
    let walker = walkdir::WalkDir::new(from_dir)
        .into_iter()
        .filter_entry(|entry| !is_staging_name(entry.file_name()));
    for entry in walker {
        let entry = entry.map_err(|e| IndexError::FileRead {
            path: from_dir.to_path_buf(),
            source: std::io::Error::other(e),
        })?;
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(from_dir)
            .expect("walkdir always yields entries nested under the dir it walks");
        copy_file(entry.path(), &to_dir.join(rel))?;
    }
    Ok(())
}

/// Clone generation `from` into generation `to` inside `layout`.
///
/// Every file under `from`'s `tantivy/` directory is hardlinked into the
/// matching path under `to`'s `tantivy/` directory, except
/// `.tantivy-writer.lock` and `.tantivy-meta.lock` (flock-on-inode files
/// Tantivy never unlinks; hardlinking them would share the source
/// generation's live writer lock with the clone). If a hardlink fails for a
/// particular file (cross-device, FAT, permission), that one file falls
/// back to a byte copy -- the rest of the clone still hardlinks.
///
/// Every file under `from`'s `semantic/` directory, and `from`'s
/// `index.meta`, are always byte-copied, never hardlinked, since semantic
/// files get rewritten wholesale. Any `.staging-*` dir/file under
/// `semantic/` (transient `SimpleSemanticSearch` staging output) is skipped
/// entirely.
///
/// Precondition (not enforced here): the caller must ensure `from` is
/// quiescent (no writer/builder active) for the duration of the clone; this
/// function acquires no locks of its own.
pub fn clone_generation(
    layout: &IndexLayout,
    from: &GenerationId,
    to: &GenerationId,
) -> IndexResult<()> {
    let from_dir = layout.gen_dir(from);
    let to_dir = layout.gen_dir(to);

    clone_tantivy_dir(&from_dir.join("tantivy"), &to_dir.join("tantivy"))?;
    clone_semantic_dir(&from_dir.join("semantic"), &to_dir.join("semantic"))?;
    copy_file(&from_dir.join("index.meta"), &to_dir.join("index.meta"))?;

    Ok(())
}

/// Migrate a flat (pre-generations) index layout at `layout.root()` into a
/// single generation under `gen/<id>/`.
///
/// A "flat" layout is what every index built before generations existed
/// left on disk: `tantivy/`, an optional `semantic/`, and `index.meta`
/// directly under `root`, with no `gen/` directory and no `current`
/// pointer. This detects that shape (`tantivy/meta.json` present, `current`
/// absent), moves -- never copies, since these can be large indexes -- those
/// three paths into a freshly allocated generation directory via
/// [`std::fs::rename`], synthesizes a `COMPLETE` manifest for it from a walk
/// of what was just moved, and points `current` at it. Returns `Ok(None)`
/// when there is nothing to migrate.
///
/// The synthesized manifest's `symbol_count`/`file_count` are read back from
/// the migrated `index.meta` (a flat-layout index always carries
/// already-computed counts there) rather than recomputed from the directory
/// walk, which sees file bytes, not parsed symbol/file semantics.
/// `started_at` is approximated from `id`'s embedded creation timestamp --
/// the closest available proxy for "when this migration began", since a
/// flat layout has no build-start timestamp of its own to carry forward --
/// and `completed_at` is the wall-clock time the manifest is written.
///
/// Idempotent and safe to re-run after a crash at any point in the process
/// below:
/// - If `current` already exists, migration already completed (by this call
///   or a previous one); returns `Ok(None)` untouched.
/// - Otherwise, if `gen/` already contains a generation directory (with or
///   without a `COMPLETE` manifest -- covering both "crashed mid-move" and
///   "crashed after the manifest was written but before `write_current`
///   landed"), that is a torn migration from an interrupted previous call.
///   [`migrate_flat_layout`] is the only writer of `gen/` entries reachable
///   in this phase (no production caller builds generations any other way
///   yet), so any such directory is unambiguously the migration to resume,
///   not another builder's unrelated in-progress build. Resumption reuses
///   that directory's id rather than allocating a new one, and each step is
///   individually idempotent: a move whose `from` path no longer exists
///   (already renamed by a previous call) is a no-op, and the manifest is
///   only rebuilt if `COMPLETE` is not already present.
/// - Only when no `gen/` directory exists at all is a completely fresh flat
///   layout considered, and only when `tantivy/meta.json` exists directly
///   under `root`; a bare/empty `root` returns `Ok(None)` untouched.
pub fn migrate_flat_layout(layout: &IndexLayout) -> IndexResult<Option<GenerationId>> {
    if layout.current_file().is_file() {
        return Ok(None);
    }

    let root = layout.root();
    let id = match find_torn_migration(layout)? {
        Some(id) => id,
        None => {
            if !root.join("tantivy").join("meta.json").is_file() {
                return Ok(None);
            }
            GenerationId::generate()
        }
    };

    let gen_dir = layout.gen_dir(&id);
    fs::create_dir_all(&gen_dir).map_err(|e| IndexError::FileWrite {
        path: gen_dir.clone(),
        source: e,
    })?;

    move_if_present(&root.join("tantivy"), &gen_dir.join("tantivy"))?;
    move_if_present(&root.join("semantic"), &gen_dir.join("semantic"))?;
    move_if_present(&root.join("index.meta"), &gen_dir.join("index.meta"))?;

    if !layout.complete_marker(&id).is_file() {
        let manifest = build_migration_manifest(&id, &gen_dir)?;
        manifest.write(layout)?;
    }

    layout.write_current(&id)?;

    Ok(Some(id))
}

/// Find the generation directory under `layout`'s `gen/` left behind by an
/// interrupted previous [`migrate_flat_layout`] call, if any. Called only
/// once [`migrate_flat_layout`] has already confirmed `current` does not
/// exist, so any generation directory found here -- whether or not it
/// already has a `COMPLETE` manifest -- is unambiguously a torn migration
/// to resume: this function is the only writer of `gen/` entries reachable
/// in this phase (no production caller builds generations any other way
/// yet), and a directory with `COMPLETE` but no `current` is exactly the
/// "crashed after writing the manifest but before `write_current` landed"
/// case. When more than one candidate exists (unexpected in practice), the
/// newest by [`GenerationId`] `Ord` is chosen, mirroring [`resolve_current`]'s
/// recovery scan.
fn find_torn_migration(layout: &IndexLayout) -> IndexResult<Option<GenerationId>> {
    let gen_root = layout.root().join("gen");
    let entries = match fs::read_dir(&gen_root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(IndexError::FileRead {
                path: gen_root,
                source: e,
            });
        }
    };

    let mut candidates: Vec<GenerationId> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| IndexError::FileRead {
            path: gen_root.clone(),
            source: e,
        })?;
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let Some(id) = entry.file_name().to_str().and_then(GenerationId::new) else {
            continue;
        };
        candidates.push(id);
    }

    candidates.sort();
    Ok(candidates.pop())
}

/// Rename `from` into `to` if `from` still exists, treating a missing
/// `from` as already-migrated (a previous call's rename already landed)
/// rather than an error -- the idempotency primitive [`migrate_flat_layout`]
/// relies on for each of its three moved paths independently.
fn move_if_present(from: &Path, to: &Path) -> IndexResult<()> {
    if !from.exists() {
        return Ok(());
    }
    ensure_parent_dir(to)?;
    fs::rename(from, to).map_err(|e| IndexError::FileWrite {
        path: to.to_path_buf(),
        source: e,
    })
}

/// Build a `COMPLETE` manifest for a just-migrated flat layout by walking
/// every file under `gen_dir`. File paths and sizes are exactly derivable
/// this way; `symbol_count`/`file_count` are not (a directory walk sees
/// bytes, not parsed symbol/file semantics), so they are read back from the
/// `index.meta` that was just moved into `gen_dir` instead. `IndexMetadata::load`
/// treats a missing file as a fresh-empty metadata (zero counts) rather than
/// an error, so the zero fallback here only triggers on a genuinely
/// corrupt/unparseable `index.meta`.
fn build_migration_manifest(id: &GenerationId, gen_dir: &Path) -> IndexResult<Complete> {
    let (symbol_count, file_count) = match IndexMetadata::load(gen_dir) {
        Ok(meta) => (u64::from(meta.symbol_count), u64::from(meta.file_count)),
        Err(_) => (0, 0),
    };

    let mut files = Vec::new();
    for entry in walkdir::WalkDir::new(gen_dir) {
        let entry = entry.map_err(|e| IndexError::FileRead {
            path: gen_dir.to_path_buf(),
            source: std::io::Error::other(e),
        })?;
        if !entry.file_type().is_file() {
            continue;
        }
        // Defensive: neither marker exists yet at this point in the normal
        // flow (this manifest is built before it is written, and no
        // `BUILDING`/`DAMAGED` marker is ever written for a migration), but
        // excluding them keeps a resumed/racing call's manifest honest if
        // that ever changes.
        if matches!(
            entry.file_name().to_str(),
            Some("COMPLETE") | Some("BUILDING") | Some("DAMAGED")
        ) {
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(gen_dir)
            .expect("walkdir always yields entries nested under the dir it walks");
        let size = entry
            .metadata()
            .map_err(|e| IndexError::FileRead {
                path: entry.path().to_path_buf(),
                source: std::io::Error::other(e),
            })?
            .len();
        files.push(CompleteFileEntry {
            path: rel.to_string_lossy().replace('\\', "/"),
            size,
        });
    }

    Ok(Complete {
        id: id.clone(),
        parent: None,
        started_at: migration_started_at(id),
        completed_at: unix_millis_now(),
        builder_version: env!("CARGO_PKG_VERSION").to_string(),
        symbol_count,
        file_count,
        files,
    })
}

/// Best-effort `started_at` for a migrated generation: the millis timestamp
/// embedded in the first 13 hex characters of `id` -- the closest available
/// proxy for "when this migration began", since a flat legacy layout
/// carries no build-start timestamp of its own. Falls back to 0 if `id`
/// somehow yields fewer than 13 characters (unreachable in practice: every
/// `GenerationId` is exactly 19 hex characters by construction).
fn migration_started_at(id: &GenerationId) -> u64 {
    id.as_str()
        .get(..13)
        .and_then(|hex| u64::from_str_radix(hex, 16).ok())
        .unwrap_or(0)
}

/// Write a `DAMAGED` marker for `id`, recording `reason` (typically a
/// [`validate_generation`] error's `Display` output) for forensic inspection.
/// Best-effort content only -- no temp-file-then-rename atomicity, since a
/// torn `DAMAGED` marker is merely a diagnostic degraded to "damage noted,
/// exact reason lost", never a correctness hazard the way a torn `current`
/// pointer would be.
fn write_damaged_marker(layout: &IndexLayout, id: &GenerationId, reason: &str) -> IndexResult<()> {
    let path = layout.damaged_marker(id);
    ensure_parent_dir(&path)?;
    fs::write(&path, reason).map_err(|e| IndexError::FileWrite { path, source: e })
}

/// Resolve which generation `current` should point at, recovering from a
/// missing, torn, or invalid pointer.
///
/// Fast path: [`IndexLayout::read_current`] returns `Some(id)` and `id`
/// passes [`validate_generation`] -- returns `Ok(Some(id))` with no other
/// side effects.
///
/// Recovery path (current missing, torn, or its named generation fails
/// validation): logs a `WARN` naming the specific failure, then scans every
/// generation directory under `gen/` for the newest (by [`GenerationId`]
/// `Ord`, i.e. most recently constructed) one that has a `COMPLETE` marker
/// and passes `validate_generation`. If one is found, [`IndexLayout::write_current`]
/// is called to repoint `current` at it, and `Ok(Some(that_id))` is
/// returned. A `DAMAGED` marker is written on the generation that *actually
/// failed validation* -- only when `current` named a specific generation
/// that turned out invalid, never when `current` was simply absent (there is
/// no specific generation to blame in that case).
///
/// If no generation on disk validates at all, returns `Ok(None)` and leaves
/// every directory untouched -- nothing is deleted or rewritten, preserving
/// the evidence for forensics.
pub fn resolve_current(layout: &IndexLayout) -> IndexResult<Option<GenerationId>> {
    let current_id = layout.read_current()?;

    if let Some(id) = &current_id {
        match validate_generation(layout, id) {
            Ok(()) => return Ok(Some(id.clone())),
            Err(e) => {
                tracing::warn!(
                    "[generation] current pointer names {id} but it failed validation ({e}); scanning gen/ for a valid fallback"
                );
                write_damaged_marker(layout, id, &e.to_string())?;
            }
        }
    } else {
        tracing::warn!(
            "[generation] current pointer is missing or torn; scanning gen/ for a valid fallback"
        );
    }

    let gen_root = layout.root().join("gen");
    let entries = match fs::read_dir(&gen_root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(IndexError::FileRead {
                path: gen_root,
                source: e,
            });
        }
    };

    let mut candidates: Vec<GenerationId> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| IndexError::FileRead {
            path: gen_root.clone(),
            source: e,
        })?;
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let name = entry.file_name();
        let Some(id) = name.to_str().and_then(GenerationId::new) else {
            continue;
        };
        if !layout.complete_marker(&id).is_file() {
            continue;
        }
        if validate_generation(layout, &id).is_err() {
            continue;
        }
        candidates.push(id);
    }

    // `GenerationId` sorts ascending by construction time; newest last.
    candidates.sort();
    let Some(newest) = candidates.pop() else {
        return Ok(None);
    };

    layout.write_current(&newest)?;
    Ok(Some(newest))
}

/// Classify a single generation's lifecycle state per the table in
/// `internal/index-generations.md` section 2.10.
pub fn classify(layout: &IndexLayout, id: &GenerationId) -> GenerationState {
    let is_current = layout.read_current().ok().flatten().as_ref() == Some(id);

    let building_marker_path = layout.building_marker(id);
    let building_marker_exists = building_marker_path.is_file();
    if building_marker_exists && Building::is_alive(&building_marker_path) {
        return GenerationState::Building;
    }
    if building_marker_exists {
        // Marker present but its builder is dead: an abandoned build.
        return GenerationState::Orphan;
    }

    let has_complete = layout.complete_marker(id).is_file();
    if !has_complete && !is_current {
        return GenerationState::Orphan;
    }

    match validate_generation(layout, id) {
        Err(_) => GenerationState::Damaged,
        Ok(()) => {
            if !emission_version_compatible(layout, id) {
                GenerationState::Incompatible
            } else if is_current {
                GenerationState::Current
            } else {
                GenerationState::Previous
            }
        }
    }
}

/// Whether `id`'s `index.meta.emission_version` matches the running
/// binary's [`EMISSION_SEMANTICS_VERSION`]. Only called after
/// [`validate_generation`] has already confirmed `index.meta` parses, so a
/// load failure here is unreachable in practice; treated as incompatible
/// rather than propagated, since this helper's contract is a bool.
fn emission_version_compatible(layout: &IndexLayout, id: &GenerationId) -> bool {
    match IndexMetadata::load(&layout.gen_dir(id)) {
        Ok(meta) => meta.emission_version == Some(EMISSION_SEMANTICS_VERSION),
        Err(_) => false,
    }
}

/// List every generation under `layout`'s `gen/` directory with its
/// classified state, on-disk size in bytes, and age derived from the
/// generation id's embedded millis timestamp.
///
/// Returns an empty list (not an error) when `gen/` does not exist yet.
pub fn list_generations(
    layout: &IndexLayout,
) -> IndexResult<Vec<(GenerationId, GenerationState, u64, Duration)>> {
    let gen_root = layout.root().join("gen");

    let entries = match fs::read_dir(&gen_root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(IndexError::FileRead {
                path: gen_root,
                source: e,
            });
        }
    };

    let now_millis = unix_millis_now();
    let mut results = Vec::new();

    for entry in entries {
        let entry = entry.map_err(|e| IndexError::FileRead {
            path: gen_root.clone(),
            source: e,
        })?;
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let name = entry.file_name();
        let Some(id) = name.to_str().and_then(GenerationId::new) else {
            continue;
        };

        let state = classify(layout, &id);
        let size = dir_size(&entry.path())?;
        let age = generation_age(&id, now_millis);
        results.push((id, state, size, age));
    }

    Ok(results)
}

/// Current unix-millis timestamp, saturating to 0 on a clock error rather
/// than panicking (mirrors `markers::unix_millis_now`; not reused directly
/// since it is private to that module).
fn unix_millis_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Age of `id`, derived from the 13 hex digits of millis timestamp embedded
/// in its first [`super::id`]-defined characters. `GenerationId::new`
/// already validated every character as lowercase hex, so the parse below
/// is infallible in practice; `unwrap_or` is a defensive fallback (age 0)
/// rather than a panic.
fn generation_age(id: &GenerationId, now_millis: u64) -> Duration {
    let timestamp_hex = &id.as_str()[..13];
    let created_millis = u64::from_str_radix(timestamp_hex, 16).unwrap_or(now_millis);
    Duration::from_millis(now_millis.saturating_sub(created_millis))
}

/// Sum the size in bytes of every regular file under `dir`, recursively.
fn dir_size(dir: &Path) -> IndexResult<u64> {
    let mut total = 0u64;
    for entry in walkdir::WalkDir::new(dir) {
        let entry = entry.map_err(|e| IndexError::FileRead {
            path: dir.to_path_buf(),
            source: std::io::Error::other(e),
        })?;
        if entry.file_type().is_file() {
            let metadata = entry.metadata().map_err(|e| IndexError::FileRead {
                path: entry.path().to_path_buf(),
                source: std::io::Error::other(e),
            })?;
            total += metadata.len();
        }
    }
    Ok(total)
}

/// Fsync `dir` so a preceding rename into it is durable across a crash.
#[cfg(unix)]
fn fsync_dir(dir: &Path) -> IndexResult<()> {
    let dir_handle = fs::File::open(dir).map_err(|e| IndexError::FileWrite {
        path: dir.to_path_buf(),
        source: e,
    })?;
    dir_handle.sync_all().map_err(|e| IndexError::FileWrite {
        path: dir.to_path_buf(),
        source: e,
    })
}

/// No-op on non-unix platforms: opening a directory as a file for fsync is
/// not portable there.
#[cfg(not(unix))]
fn fsync_dir(_dir: &Path) -> IndexResult<()> {
    Ok(())
}

/// Check that the filesystem backing `layout.root()` has at least
/// `needed_bytes` of free space, before starting a new generation build.
///
/// Finds the mounted disk whose mount point is the longest matching path
/// prefix of `layout.root()` (canonicalized, walking up to the nearest
/// existing ancestor if `root` itself does not exist yet), then compares
/// its reported available space against `needed_bytes`.
///
/// This preflight is best-effort: if no mounted disk's reported mount
/// point is a prefix of `root` at all (nothing to compare against), the
/// check is skipped with a `WARN` log rather than treated as fatal --
/// `sysinfo::Disks` mount-point enumeration is a heuristic over
/// `/proc/mounts` (or the platform equivalent) and case has been observed
/// where every reported mount point could plausibly fail to prefix-match a
/// legitimate root (e.g. an unusual bind-mount or overlay setup). When it
/// *does* find a match, the match is trusted and a real shortfall is a
/// hard [`IndexError::IndexNotSpaceForBuild`].
pub fn free_space_preflight(layout: &IndexLayout, needed_bytes: u64) -> IndexResult<()> {
    let disks = sysinfo::Disks::new_with_refreshed_list();
    let candidates = disks
        .list()
        .iter()
        .map(|disk| (disk.mount_point(), disk.available_space()));
    free_space_preflight_against(layout.root(), needed_bytes, candidates)
}

/// Testable core of [`free_space_preflight`]: takes the candidate
/// `(mount_point, available_bytes)` pairs as a plain iterator so tests can
/// inject fake disks instead of depending on the real machine's disk
/// layout being in any particular state.
fn free_space_preflight_against<'a>(
    root: &Path,
    needed_bytes: u64,
    disks: impl Iterator<Item = (&'a Path, u64)>,
) -> IndexResult<()> {
    let target = canonicalize_existing_ancestor(root);

    let mut best_match: Option<(usize, u64)> = None;
    for (mount_point, available) in disks {
        if !target.starts_with(mount_point) {
            continue;
        }
        let specificity = mount_point.components().count();
        if best_match.is_none_or(|(best_specificity, _)| specificity > best_specificity) {
            best_match = Some((specificity, available));
        }
    }

    let Some((_, available)) = best_match else {
        tracing::warn!(
            "[generation] free_space_preflight: no mounted disk's reported mount point \
             prefixes {}; skipping the free-space check for this build",
            target.display()
        );
        return Ok(());
    };

    if needed_bytes > available {
        return Err(IndexError::IndexNotSpaceForBuild {
            needed: needed_bytes,
            available,
        });
    }

    Ok(())
}

/// Canonicalize `path`, walking up to the nearest existing ancestor first
/// if `path` itself does not exist yet (e.g. a generation root that has not
/// been created). Falls back to `path` unmodified if no ancestor exists or
/// canonicalization fails, since this is only used to pick the best
/// matching disk mount point, not for correctness-critical path identity.
fn canonicalize_existing_ancestor(path: &Path) -> PathBuf {
    for ancestor in path.ancestors() {
        if let Ok(canonical) = ancestor.canonicalize() {
            return canonical;
        }
    }
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout_in(dir: &tempfile::TempDir) -> IndexLayout {
        IndexLayout::new(dir.path().to_path_buf())
    }

    /// A fixture segment: its id and the bytes written to disk for it.
    struct SegmentFixture {
        id: String,
        bytes: &'static [u8],
    }

    /// Build a structurally valid generation under `layout`'s `gen/<id>/`:
    /// `index.meta` (compatible emission version), a `tantivy/meta.json`
    /// referencing one segment whose file matches the `COMPLETE` manifest's
    /// recorded size, and a `semantic/metadata.json`.
    ///
    /// Returns the generation id and the segment fixture so callers can
    /// tamper with the segment file to exercise damage paths.
    fn write_valid_generation(layout: &IndexLayout, id: &GenerationId) -> SegmentFixture {
        let gen_dir = layout.gen_dir(id);
        fs::create_dir_all(gen_dir.join("tantivy")).expect("create tantivy dir");
        fs::create_dir_all(gen_dir.join("semantic")).expect("create semantic dir");

        let mut meta = IndexMetadata::new();
        meta.emission_version = Some(EMISSION_SEMANTICS_VERSION);
        meta.save(&gen_dir).expect("save index.meta");

        let segment = SegmentFixture {
            id: "01977c94df1e1a1e8000000000000000".to_string(),
            bytes: b"segment-data",
        };
        let segment_file_name = format!("{}.store", segment.id);
        fs::write(
            gen_dir.join("tantivy").join(&segment_file_name),
            segment.bytes,
        )
        .expect("write segment file");

        let tantivy_meta = serde_json::json!({
            "segments": [{"segment_id": segment.id, "max_doc": 1, "deletes": null}],
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
            completed_at: 0,
            builder_version: "0.0.0".to_string(),
            symbol_count: 0,
            file_count: 0,
            files: vec![super::super::CompleteFileEntry {
                path: format!("tantivy/{segment_file_name}"),
                size: segment.bytes.len() as u64,
            }],
        };
        complete.write(layout).expect("write COMPLETE manifest");

        segment
    }

    #[test]
    fn validate_generation_accepts_a_valid_fixture() {
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = layout_in(&dir);
        let id = GenerationId::generate();
        write_valid_generation(&layout, &id);

        assert!(validate_generation(&layout, &id).is_ok());
    }

    #[test]
    fn validate_generation_reports_a_missing_referenced_segment_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = layout_in(&dir);
        let id = GenerationId::generate();
        let segment = write_valid_generation(&layout, &id);

        fs::remove_file(
            layout
                .gen_dir(&id)
                .join("tantivy")
                .join(format!("{}.store", segment.id)),
        )
        .expect("remove segment file");

        let err = validate_generation(&layout, &id).expect_err("missing segment must be damaged");
        match err {
            IndexError::GenerationDamaged { reason, .. } => {
                assert!(
                    reason.contains("is missing"),
                    "reason should name the missing segment file: {reason}"
                );
            }
            other => panic!("expected GenerationDamaged, got {other:?}"),
        }
    }

    #[test]
    fn validate_generation_reports_a_segment_size_mismatch() {
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = layout_in(&dir);
        let id = GenerationId::generate();
        let segment = write_valid_generation(&layout, &id);

        fs::write(
            layout
                .gen_dir(&id)
                .join("tantivy")
                .join(format!("{}.store", segment.id)),
            b"different-length-content",
        )
        .expect("rewrite segment file");

        let err = validate_generation(&layout, &id).expect_err("size mismatch must be damaged");
        assert!(matches!(err, IndexError::GenerationDamaged { .. }));
    }

    #[test]
    fn validate_generation_reports_a_truncated_index_meta() {
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = layout_in(&dir);
        let id = GenerationId::generate();
        write_valid_generation(&layout, &id);

        fs::write(layout.gen_dir(&id).join("index.meta"), b"{ not json")
            .expect("truncate index.meta");

        let err =
            validate_generation(&layout, &id).expect_err("truncated index.meta must be damaged");
        match err {
            IndexError::GenerationDamaged { reason, .. } => {
                assert!(
                    reason.contains("index.meta"),
                    "reason should name index.meta: {reason}"
                );
            }
            other => panic!("expected GenerationDamaged, got {other:?}"),
        }
    }

    #[test]
    fn classify_reports_current_for_the_current_valid_generation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = layout_in(&dir);
        let id = GenerationId::generate();
        write_valid_generation(&layout, &id);
        layout.write_current(&id).expect("write current");

        assert_eq!(classify(&layout, &id), GenerationState::Current);
    }

    #[test]
    fn classify_reports_previous_for_an_older_valid_generation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = layout_in(&dir);
        let previous = GenerationId::generate();
        write_valid_generation(&layout, &previous);
        let current = GenerationId::generate();
        write_valid_generation(&layout, &current);
        layout.write_current(&current).expect("write current");

        assert_eq!(classify(&layout, &previous), GenerationState::Previous);
    }

    #[test]
    fn classify_reports_building_while_the_builder_is_alive() {
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = layout_in(&dir);
        let id = GenerationId::generate();
        let _guard = Building::start(&layout, &id, None).expect("start building");

        assert_eq!(classify(&layout, &id), GenerationState::Building);
    }

    #[test]
    fn classify_reports_orphan_for_a_dead_builder() {
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = layout_in(&dir);
        let id = GenerationId::generate();
        {
            let _guard = Building::start(&layout, &id, None).expect("start building");
            // Guard dropped here releases the flock; the marker's recorded
            // pid is this test process's own live pid, so overwrite it with
            // a pid that can never be alive to simulate a crashed builder.
        }
        let marker_path = layout.building_marker(&id);
        let stale = serde_json::json!({
            "pid": i32::MAX as u32,
            "started_at": 0,
            "launch_token": 0,
            "builder_version": "0.0.0",
            "parent": null,
        });
        fs::write(
            &marker_path,
            serde_json::to_string_pretty(&stale).expect("serialize stale marker"),
        )
        .expect("write stale marker");

        assert_eq!(classify(&layout, &id), GenerationState::Orphan);
    }

    #[test]
    fn classify_reports_orphan_for_no_complete_and_not_current() {
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = layout_in(&dir);
        let id = GenerationId::generate();
        fs::create_dir_all(layout.gen_dir(&id)).expect("create gen dir");

        assert_eq!(classify(&layout, &id), GenerationState::Orphan);
    }

    #[test]
    fn classify_reports_damaged_for_a_complete_generation_that_fails_validation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = layout_in(&dir);
        let id = GenerationId::generate();
        write_valid_generation(&layout, &id);
        fs::write(layout.gen_dir(&id).join("index.meta"), b"{ not json")
            .expect("truncate index.meta");

        assert_eq!(classify(&layout, &id), GenerationState::Damaged);
    }

    #[test]
    fn classify_reports_incompatible_for_a_mismatched_emission_version() {
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = layout_in(&dir);
        let id = GenerationId::generate();
        write_valid_generation(&layout, &id);

        let mut meta = IndexMetadata::new();
        meta.emission_version = Some(EMISSION_SEMANTICS_VERSION + 1);
        meta.save(&layout.gen_dir(&id))
            .expect("overwrite index.meta with mismatched version");

        assert_eq!(classify(&layout, &id), GenerationState::Incompatible);
    }

    #[test]
    fn list_generations_is_empty_when_gen_dir_is_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = layout_in(&dir);

        let listed = list_generations(&layout).expect("list generations");

        assert!(listed.is_empty());
    }

    #[test]
    fn list_generations_reports_state_size_and_age() {
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = layout_in(&dir);
        let id = GenerationId::generate();
        write_valid_generation(&layout, &id);
        layout.write_current(&id).expect("write current");

        let listed = list_generations(&layout).expect("list generations");

        assert_eq!(listed.len(), 1);
        let (listed_id, state, size, age) = &listed[0];
        assert_eq!(listed_id, &id);
        assert_eq!(*state, GenerationState::Current);
        assert!(*size > 0, "on-disk size must include the written fixture");
        // The fixture was just written, so its age must be small.
        assert!(*age < Duration::from_secs(60));
    }

    #[test]
    fn write_then_read_current_round_trips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = layout_in(&dir);
        let id = GenerationId::generate();

        layout.write_current(&id).expect("write current");
        let read_back = layout.read_current().expect("read current");

        assert_eq!(read_back, Some(id));
    }

    #[test]
    fn read_current_missing_file_is_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = layout_in(&dir);

        let read_back = layout.read_current().expect("read current");

        assert_eq!(read_back, None);
    }

    #[test]
    fn read_current_ignores_a_stray_tmp_file_next_to_a_valid_current() {
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = layout_in(&dir);
        let id = GenerationId::generate();

        layout.write_current(&id).expect("write current");
        // Simulate a crash right after a subsequent write_current created
        // its tmp file but before the rename landed.
        fs::write(dir.path().join("current.tmp"), "0123456789abcdef012\n")
            .expect("write stray tmp file");

        let read_back = layout.read_current().expect("read current");

        assert_eq!(read_back, Some(id));
    }

    #[test]
    fn read_current_truncated_content_is_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = layout_in(&dir);
        fs::write(layout.current_file(), "not-a-valid-generation-id\n")
            .expect("write garbage current");

        let read_back = layout.read_current().expect("read current");

        assert_eq!(read_back, None);
    }

    #[test]
    fn read_current_empty_file_is_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = layout_in(&dir);
        fs::write(layout.current_file(), "   \n").expect("write whitespace current");

        let read_back = layout.read_current().expect("read current");

        assert_eq!(read_back, None);
    }

    #[test]
    #[cfg(unix)]
    fn clone_generation_hardlinks_tantivy_segment_files() {
        use std::os::unix::fs::MetadataExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let layout = layout_in(&dir);
        let from = GenerationId::generate();
        write_valid_generation(&layout, &from);
        let to = GenerationId::generate();

        clone_generation(&layout, &from, &to).expect("clone generation");

        let from_meta_json = layout.gen_dir(&from).join("tantivy").join("meta.json");
        let to_meta_json = layout.gen_dir(&to).join("tantivy").join("meta.json");
        let from_inode = fs::metadata(&from_meta_json)
            .expect("stat source meta.json")
            .ino();
        let to_inode = fs::metadata(&to_meta_json)
            .expect("stat cloned meta.json")
            .ino();
        assert_eq!(
            from_inode, to_inode,
            "clone must hardlink tantivy files, sharing the source inode"
        );
        assert_eq!(
            fs::metadata(&to_meta_json)
                .expect("stat cloned meta.json")
                .nlink(),
            2,
            "hardlinked file must have nlink == 2 (source + clone)"
        );
    }

    #[test]
    fn clone_generation_excludes_tantivy_lock_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = layout_in(&dir);
        let from = GenerationId::generate();
        write_valid_generation(&layout, &from);
        fs::write(
            layout
                .gen_dir(&from)
                .join("tantivy")
                .join(".tantivy-writer.lock"),
            b"",
        )
        .expect("write writer lock fixture");
        fs::write(
            layout
                .gen_dir(&from)
                .join("tantivy")
                .join(".tantivy-meta.lock"),
            b"",
        )
        .expect("write meta lock fixture");
        let to = GenerationId::generate();

        clone_generation(&layout, &from, &to).expect("clone generation");

        assert!(
            !layout
                .gen_dir(&to)
                .join("tantivy")
                .join(".tantivy-writer.lock")
                .exists(),
            ".tantivy-writer.lock must never be present in a clone"
        );
        assert!(
            !layout
                .gen_dir(&to)
                .join("tantivy")
                .join(".tantivy-meta.lock")
                .exists(),
            ".tantivy-meta.lock must never be present in a clone"
        );
    }

    #[test]
    fn clone_generation_excludes_staging_files_under_semantic() {
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = layout_in(&dir);
        let from = GenerationId::generate();
        write_valid_generation(&layout, &from);
        fs::write(
            layout
                .gen_dir(&from)
                .join("semantic")
                .join(".staging-abc123.tmp"),
            b"transient",
        )
        .expect("write staging fixture");
        let to = GenerationId::generate();

        clone_generation(&layout, &from, &to).expect("clone generation");

        assert!(
            !layout
                .gen_dir(&to)
                .join("semantic")
                .join(".staging-abc123.tmp")
                .exists(),
            ".staging-* files under semantic/ must not be copied into a clone"
        );
        assert!(
            layout
                .gen_dir(&to)
                .join("semantic")
                .join("metadata.json")
                .exists(),
            "non-staging semantic files must still be copied"
        );
    }

    #[test]
    fn clone_generation_copies_index_meta_and_semantic_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = layout_in(&dir);
        let from = GenerationId::generate();
        write_valid_generation(&layout, &from);
        let to = GenerationId::generate();

        clone_generation(&layout, &from, &to).expect("clone generation");

        assert!(layout.gen_dir(&to).join("index.meta").is_file());
        assert!(
            layout
                .gen_dir(&to)
                .join("semantic")
                .join("metadata.json")
                .is_file()
        );
        assert!(validate_generation(&layout, &to).is_ok());
    }

    #[test]
    fn resolve_current_recovers_from_a_torn_pointer_by_picking_the_newest_valid_generation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = layout_in(&dir);

        let older = GenerationId::generate();
        write_valid_generation(&layout, &older);
        std::thread::sleep(std::time::Duration::from_millis(5));
        let newer = GenerationId::generate();
        write_valid_generation(&layout, &newer);

        // A torn `current` pointer: malformed content, not naming any
        // generation.
        fs::write(layout.current_file(), "not-a-valid-generation-id\n")
            .expect("write torn current pointer");

        let resolved = resolve_current(&layout).expect("resolve current");

        assert_eq!(resolved, Some(newer.clone()));
        assert_eq!(
            layout.read_current().expect("read current"),
            Some(newer.clone()),
            "current pointer must be rewritten to the newly resolved generation"
        );
        assert!(
            !layout.damaged_marker(&older).is_file(),
            "no specific generation failed validation from a torn pointer, so nothing is marked DAMAGED"
        );
        assert!(!layout.damaged_marker(&newer).is_file());
    }

    #[test]
    fn resolve_current_falls_back_past_a_damaged_current_and_marks_it_damaged() {
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = layout_in(&dir);

        let fallback = GenerationId::generate();
        write_valid_generation(&layout, &fallback);
        std::thread::sleep(std::time::Duration::from_millis(5));

        let damaged = GenerationId::generate();
        write_valid_generation(&layout, &damaged);
        fs::write(layout.gen_dir(&damaged).join("index.meta"), b"{ not json")
            .expect("corrupt index.meta to force validation failure");
        layout.write_current(&damaged).expect("write current");

        let resolved = resolve_current(&layout).expect("resolve current");

        assert_eq!(resolved, Some(fallback.clone()));
        assert_eq!(
            layout.read_current().expect("read current"),
            Some(fallback.clone()),
            "current pointer must be rewritten to the fallback generation"
        );
        assert!(
            layout.damaged_marker(&damaged).is_file(),
            "the generation that actually failed validation must be marked DAMAGED"
        );
        assert!(
            !layout.damaged_marker(&fallback).is_file(),
            "the healthy fallback generation must not be marked DAMAGED"
        );
    }

    #[test]
    fn resolve_current_returns_none_and_touches_nothing_when_no_generation_validates() {
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = layout_in(&dir);

        let id = GenerationId::generate();
        write_valid_generation(&layout, &id);
        fs::write(layout.gen_dir(&id).join("index.meta"), b"{ not json")
            .expect("corrupt index.meta so nothing validates");

        let resolved = resolve_current(&layout).expect("resolve current");

        assert_eq!(resolved, None);
        assert!(
            layout.gen_dir(&id).is_dir(),
            "the only generation's directory must survive untouched for forensics"
        );
        assert!(
            !layout.current_file().is_file(),
            "no current pointer should be written when nothing validates"
        );
    }

    #[test]
    fn hardlink_or_copy_falls_back_to_copy_when_hardlink_fails() {
        let dir = tempfile::tempdir().expect("tempdir");
        let from = dir.path().join("source.txt");
        fs::write(&from, b"source-bytes").expect("write source file");

        // Pre-create a regular file at the destination: `fs::hard_link`
        // fails with `AlreadyExists` here (exercising the fallback branch
        // in isolation, standing in for a cross-device/FAT/permission
        // hardlink failure), while `fs::copy` succeeds by overwriting it.
        let to = dir.path().join("dest.txt");
        fs::write(&to, b"stale-bytes").expect("pre-create destination file");

        hardlink_or_copy(&from, &to).expect("fallback copy must succeed");

        let contents = fs::read(&to).expect("read destination file");
        assert_eq!(contents, b"source-bytes");
    }

    /// Write a minimal flat (pre-generations) legacy layout directly under
    /// `layout.root()`: `tantivy/meta.json` plus one segment file, and
    /// `index.meta` with non-zero symbol/file counts so the migrated
    /// manifest's best-effort counts are observably non-default. Returns
    /// the exact `index.meta` bytes written, so a test can assert the
    /// migrated copy is byte-identical.
    fn write_flat_layout(layout: &IndexLayout) -> Vec<u8> {
        let root = layout.root();
        fs::create_dir_all(root.join("tantivy")).expect("create flat tantivy dir");

        let segment_id = "01977c94df1e1a1e8000000000000001";
        fs::write(
            root.join("tantivy").join(format!("{segment_id}.store")),
            b"flat-segment-data",
        )
        .expect("write flat segment file");
        let tantivy_meta = serde_json::json!({
            "segments": [{"segment_id": segment_id, "max_doc": 1, "deletes": null}],
            "schema": [],
            "opstamp": 1,
            "payload": null,
        });
        fs::write(
            root.join("tantivy").join("meta.json"),
            serde_json::to_string(&tantivy_meta).expect("serialize flat tantivy meta"),
        )
        .expect("write flat tantivy meta.json");

        let mut meta = IndexMetadata::new();
        meta.symbol_count = 7;
        meta.file_count = 3;
        meta.save(root).expect("save flat index.meta");

        fs::read(root.join("index.meta")).expect("read back flat index.meta")
    }

    #[test]
    fn migrate_flat_layout_migrates_a_fresh_flat_layout() {
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = layout_in(&dir);
        let original_index_meta = write_flat_layout(&layout);

        let migrated = migrate_flat_layout(&layout).expect("migrate flat layout");

        let id = migrated.expect("a flat layout must migrate to Some(id)");
        assert_eq!(
            layout.read_current().expect("read current"),
            Some(id.clone()),
            "migration must point current at the new generation"
        );
        assert!(
            !layout.root().join("tantivy").is_dir(),
            "flat tantivy/ must be moved, not left behind"
        );
        assert!(
            !layout.root().join("index.meta").is_file(),
            "flat index.meta must be moved, not left behind"
        );

        let gen_dir = layout.gen_dir(&id);
        assert!(gen_dir.join("tantivy").join("meta.json").is_file());
        let migrated_index_meta =
            fs::read(gen_dir.join("index.meta")).expect("read migrated index.meta");
        assert_eq!(
            migrated_index_meta, original_index_meta,
            "moved index.meta must be byte-identical to the original"
        );

        assert!(validate_generation(&layout, &id).is_ok());
        let manifest = Complete::read(&layout.complete_marker(&id)).expect("read COMPLETE");
        assert_eq!(manifest.symbol_count, 7);
        assert_eq!(manifest.file_count, 3);
        assert!(
            manifest.files.iter().any(|f| f.path
                == format!(
                    "tantivy/{segment_id}.store",
                    segment_id = "01977c94df1e1a1e8000000000000001"
                )),
            "manifest must record the migrated segment file"
        );
    }

    #[test]
    fn migrate_flat_layout_is_a_no_op_once_current_is_written() {
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = layout_in(&dir);
        write_flat_layout(&layout);

        let first = migrate_flat_layout(&layout).expect("first migration");
        assert!(first.is_some());

        let second = migrate_flat_layout(&layout).expect("second migration is a no-op");
        assert_eq!(
            second, None,
            "calling again after success must return Ok(None)"
        );
    }

    #[test]
    fn migrate_flat_layout_returns_none_when_there_is_nothing_to_migrate() {
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = layout_in(&dir);

        let migrated = migrate_flat_layout(&layout).expect("migrate empty root");

        assert_eq!(migrated, None);
    }

    #[test]
    fn migrate_flat_layout_resumes_a_torn_migration_with_tantivy_still_flat() {
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = layout_in(&dir);
        let original_index_meta = write_flat_layout(&layout);

        // Simulate a crash partway through a previous migration: the
        // semantic move already landed under gen/<id>/semantic (empty here,
        // standing in for real semantic files) while tantivy/ and
        // index.meta are still at the flat root and `current` was never
        // written.
        let torn_id = GenerationId::generate();
        fs::create_dir_all(layout.gen_dir(&torn_id).join("semantic"))
            .expect("create torn semantic dir");

        let migrated = migrate_flat_layout(&layout).expect("resume torn migration");

        assert_eq!(
            migrated,
            Some(torn_id.clone()),
            "resumption must reuse the torn migration's existing id, not allocate a new one"
        );
        assert_eq!(
            layout.read_current().expect("read current"),
            Some(torn_id.clone())
        );
        assert!(!layout.root().join("tantivy").is_dir());
        assert!(!layout.root().join("index.meta").is_file());

        let gen_dir = layout.gen_dir(&torn_id);
        assert!(gen_dir.join("tantivy").join("meta.json").is_file());
        assert!(gen_dir.join("semantic").is_dir());
        assert_eq!(
            fs::read(gen_dir.join("index.meta")).expect("read migrated index.meta"),
            original_index_meta
        );
        assert!(validate_generation(&layout, &torn_id).is_ok());
    }

    #[test]
    fn migrate_flat_layout_resumes_a_torn_migration_missing_only_current() {
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = layout_in(&dir);
        write_flat_layout(&layout);

        // First call performs the full move + manifest write but we erase
        // `current` afterward to simulate a crash between the COMPLETE
        // write and the write_current call landing.
        let id = migrate_flat_layout(&layout)
            .expect("first migration")
            .expect("must migrate");
        fs::remove_file(layout.current_file()).expect("simulate crash before write_current");

        let resumed = migrate_flat_layout(&layout).expect("resume torn migration");

        assert_eq!(resumed, Some(id.clone()));
        assert_eq!(layout.read_current().expect("read current"), Some(id));
    }

    #[test]
    fn free_space_preflight_against_errs_when_needed_exceeds_available() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("index-root");
        fs::create_dir_all(&root).expect("create root");
        let mount = dir.path().to_path_buf();
        let fake_disks = vec![(mount.as_path(), 100u64)];

        let err = free_space_preflight_against(&root, 200, fake_disks.into_iter())
            .expect_err("200 needed > 100 available must error");

        match err {
            IndexError::IndexNotSpaceForBuild { needed, available } => {
                assert_eq!(needed, 200);
                assert_eq!(available, 100);
            }
            other => panic!("expected IndexNotSpaceForBuild, got {other:?}"),
        }
    }

    #[test]
    fn free_space_preflight_against_ok_when_needed_fits_available() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("index-root");
        fs::create_dir_all(&root).expect("create root");
        let mount = dir.path().to_path_buf();
        let fake_disks = vec![(mount.as_path(), 100u64)];

        let result = free_space_preflight_against(&root, 100, fake_disks.into_iter());

        assert!(result.is_ok());
    }

    #[test]
    fn free_space_preflight_against_picks_longest_matching_mount_prefix() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("nested").join("index-root");
        fs::create_dir_all(&root).expect("create root");

        // A shorter, unrelated mount with plenty of space, and a longer,
        // more specific mount (the tempdir itself) that is nearly full.
        // The specific mount must win the match, so the tight budget on it
        // is what gets enforced.
        let unrelated_root = std::path::Path::new("/");
        let fake_disks = vec![(unrelated_root, u64::MAX), (dir.path(), 10u64)];

        let err = free_space_preflight_against(&root, 20, fake_disks.into_iter())
            .expect_err("must use the longer/more specific mount match");

        match err {
            IndexError::IndexNotSpaceForBuild { needed, available } => {
                assert_eq!(needed, 20);
                assert_eq!(available, 10);
            }
            other => panic!("expected IndexNotSpaceForBuild, got {other:?}"),
        }
    }

    #[test]
    fn free_space_preflight_against_is_advisory_when_no_mount_matches() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("index-root");
        fs::create_dir_all(&root).expect("create root");

        // No candidate disk's mount point prefixes `root` at all.
        let fake_disks: Vec<(&Path, u64)> = vec![];

        let result = free_space_preflight_against(&root, u64::MAX, fake_disks.into_iter());

        assert!(result.is_ok());
    }
}
