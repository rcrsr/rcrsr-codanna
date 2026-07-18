//! Canonical `ignore::WalkBuilder` construction for every codanna file walk.
//!
//! Before this module existed, three independently-configured `WalkBuilder`
//! sites drifted out of sync: `FileWalker::walk` (used by `--dry-run`),
//! `DiscoverStage::run` (the parallel walk used by the real index), and
//! `DiscoverStage::collect_all_files` (the sequential walk used by
//! incremental discovery). Comments at the latter two claimed to match
//! `FileWalker` behavior, but nothing enforced it. This module is now the
//! single source of truth for ignore semantics across all three.

use crate::Settings;
use crate::error::{IndexError, IndexResult};
use crate::indexing::calculate_hash;
use ignore::WalkBuilder;
use ignore::gitignore::GitignoreBuilder;
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::Arc;

/// Build the single, canonical WalkBuilder for every codanna file walk.
///
/// The ONLY place in the crate permitted to call `WalkBuilder::new`.
/// All three walk sites (`FileWalker`, `DiscoverStage::run`,
/// `DiscoverStage::collect_all_files`) must obtain their builder here so
/// ignore semantics cannot drift between the dry-run and the real index.
///
/// Constraints:
/// - `hidden(false)`, `git_ignore`/`git_global`/`git_exclude(true)`, `require_git(false)`
/// - `add_custom_ignore_filename(".codannaignore")`
/// - Custom ignore patterns from `settings.indexing.ignore_patterns` are
///   compiled into a single in-memory `ignore::gitignore::Gitignore` matcher
///   (the same dialect as `.codannaignore`/`.gitignore`: `!` negation,
///   trailing `/` for directory-only, `**`, and anchoring all apply) and
///   installed via `WalkBuilder::filter_entry`. There is no
///   `WalkBuilder::add_gitignore(Gitignore)`, and `overrides()` uses a
///   different (whitelist-inverting) dialect that silently empties the
///   index the moment a user writes a `!` re-include pattern, so
///   `filter_entry` is the only correct injection point. The matcher is
///   compiled once and shared via `Arc` because this is the walk hot path
///   (10k+ files/s) and `filter_entry` runs per directory entry on both the
///   sequential and parallel (`build_parallel`) walk. Because
///   `filter_entry` composes *after* the crate's own gitignore/.codannaignore
///   matching, a `!` in `ignore_patterns` cannot re-include a file already
///   excluded by `.gitignore`/`.codannaignore` — it only negates other
///   `ignore_patterns` entries.
/// - Caller sets `.threads()` afterward (parallel site only)
///
/// # Errors
///
/// Returns [`IndexError::InvalidIgnorePattern`] when a pattern from
/// `settings.indexing.ignore_patterns` fails to parse as a gitignore line.
pub fn build_walker(settings: &Settings, root: &Path) -> IndexResult<WalkBuilder> {
    let mut builder = WalkBuilder::new(root);
    let follow_links = settings.indexing.follow_links;

    builder
        .hidden(false) // Don't auto-skip hidden directories
        .git_ignore(true) // Respect .gitignore
        .git_global(true) // Respect global gitignore
        .git_exclude(true) // Respect .git/info/exclude
        .follow_links(follow_links) // See `indexing.follow_links` setting
        .require_git(false); // Allow gitignore to work in non-git directories

    // Always support .codannaignore files for custom ignore patterns (follows .gitignore pattern)
    builder.add_custom_ignore_filename(".codannaignore");

    // Compiled once; shared (not rebuilt) across every entry on both the
    // sequential and parallel walk - filter_entry runs per directory entry
    // on the 10k+ files/s hot path (see module docs above).
    let ignore_matcher = compile_ignore_matcher(settings, root)?;

    // A followed symlink can point outside the workspace (e.g. a malicious
    // repo shipping `follow_links = true` plus a symlink to `~/.ssh`), so
    // when `follow_links` is enabled every symlink entry is required to
    // canonicalize to a descendant of this containment root. Only computed
    // when `follow_links` is set, since `follow_links(false)` never
    // descends into a symlinked directory (see `warn_if_skipped_symlink_dir`
    // below) and symlinked files are simply not followed either.
    let containment_root = if follow_links {
        let base = settings.workspace_root.as_deref().unwrap_or(root);
        base.canonicalize().ok()
    } else {
        None
    };

    if ignore_matcher.is_some() || containment_root.is_some() {
        builder.filter_entry(move |entry| {
            if let Some(ref matcher) = ignore_matcher {
                // `entry.file_type()` reports the *followed target's* type
                // once `follow_links` is on, so a symlinked directory
                // already reports `is_dir() == true` here with no special
                // casing needed for directory-only ignore patterns.
                let is_dir = entry.file_type().is_some_and(|ft| ft.is_dir());
                if matcher.matched(entry.path(), is_dir).is_ignore() {
                    return false;
                }
            }

            if let Some(ref containment_root) = containment_root {
                // `entry.file_type().is_symlink()` is unreliable here: once
                // `follow_links` is on, `ignore`/`walkdir` report the
                // *followed target's* file type, not the link's, so that
                // check is always false exactly when this branch runs.
                // `path_is_symlink()` reports whether the entry itself is a
                // symlink regardless of `follow_links`.
                if entry.path_is_symlink() {
                    match entry.path().canonicalize() {
                        Ok(canonical) if canonical.starts_with(containment_root) => {}
                        _ => {
                            tracing::warn!(
                                "skipping symlink '{}' that escapes the workspace root",
                                entry.path().display()
                            );
                            return false;
                        }
                    }
                }
            }

            true
        });
    }

    Ok(builder)
}

/// Compiles `settings.indexing.ignore_patterns` into a single in-memory
/// `Gitignore` matcher rooted at `root` (workspace root when known,
/// otherwise `root`), or `None` when no patterns are configured. Shared by
/// [`build_walker`] (installs the matcher via `filter_entry`) and
/// [`validate_ignore_patterns`] (checks the patterns compile without
/// constructing a full `WalkBuilder`).
///
/// # Errors
///
/// Returns [`IndexError::InvalidIgnorePattern`] when a pattern fails to
/// parse as a gitignore line.
fn compile_ignore_matcher(
    settings: &Settings,
    root: &Path,
) -> IndexResult<Option<Arc<ignore::gitignore::Gitignore>>> {
    if settings.indexing.ignore_patterns.is_empty() {
        return Ok(None);
    }

    // Root the matcher at the workspace root when known, so patterns behave
    // the same regardless of which subdirectory is being walked; fall back
    // to the given root otherwise.
    let ignore_root = settings.workspace_root.as_deref().unwrap_or(root);
    let mut ignore_builder = GitignoreBuilder::new(ignore_root);
    for pattern in &settings.indexing.ignore_patterns {
        ignore_builder
            .add_line(None, pattern)
            .map_err(|e| IndexError::InvalidIgnorePattern {
                pattern: pattern.clone(),
                reason: format!("indexing.ignore_patterns: {e}"),
            })?;
    }
    let matcher = ignore_builder
        .build()
        .map_err(|e| IndexError::InvalidIgnorePattern {
            pattern: settings.indexing.ignore_patterns.join(", "),
            reason: format!("indexing.ignore_patterns: {e}"),
        })?;

    Ok(Some(Arc::new(matcher)))
}

/// Validates that `settings.indexing.ignore_patterns` parse as gitignore
/// lines, without constructing a `WalkBuilder` or requiring a walk root to
/// exist.
///
/// Intended for call sites that iterate multiple paths (or none, e.g. a
/// reindex over `indexing.indexed_paths`) up front: validating once before
/// the loop turns a malformed pattern into a single hard failure instead of
/// a per-path `InvalidIgnorePattern` that gets caught, logged, and skipped,
/// silently producing "reindexed 0 files" on every run.
///
/// # Errors
///
/// Returns [`IndexError::InvalidIgnorePattern`] when a pattern fails to
/// parse as a gitignore line.
pub fn validate_ignore_patterns(settings: &Settings) -> IndexResult<()> {
    compile_ignore_matcher(settings, Path::new(".")).map(|_| ())
}

/// Hex-encoded SHA256 of raw file bytes, computed directly with `sha2`
/// rather than [`calculate_hash`] because ignore files (`.gitignore`,
/// `.codannaignore`, `.git/info/exclude`) are not guaranteed to be valid
/// UTF-8, and `calculate_hash` takes `&str`.
fn hash_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

/// Appends a deterministic, presence-distinguishing marker for `path` to
/// `input`: `"<label>=absent\n"` when the file does not exist, or
/// `"<label>=present:<sha256-of-bytes>\n"` when it does (including when it
/// exists but is empty — its content hash differs from the "absent" marker).
///
/// A symlinked ignore file is treated as absent rather than read: hashing
/// unconditionally follows symlinks with no containment check, so a
/// malicious repo could point `.gitignore` at an arbitrary file outside the
/// workspace and have its bytes folded into the fingerprint. Ignore files
/// are not expected to be symlinks in normal use, so this is a no-op for
/// legitimate workspaces.
fn append_file_marker(input: &mut String, label: &str, path: &Path) -> IndexResult<()> {
    let is_symlink =
        std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink());
    if is_symlink {
        input.push_str(label);
        input.push_str("=absent\n");
        return Ok(());
    }

    match std::fs::read(path) {
        Ok(bytes) => {
            input.push_str(label);
            input.push_str("=present:");
            input.push_str(&hash_bytes(&bytes));
            input.push('\n');
        }
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            // `NotADirectory` occurs in common git worktree/submodule setups
            // where `.git` is a file, so `.git/info/exclude` cannot be
            // reached as a subpath; treat it the same as absent rather than
            // failing staleness detection with a noisy warning on every run.
            input.push_str(label);
            input.push_str("=absent\n");
        }
        Err(e) => {
            return Err(IndexError::FileRead {
                path: path.to_path_buf(),
                source: e,
            });
        }
    }
    Ok(())
}

/// Hash of every input that determines which files a walk yields.
///
/// Covers: workspace `.codannaignore`, `.gitignore`, `.git/info/exclude`,
/// `settings.indexing.ignore_patterns`, `settings.indexing.follow_links`.
/// Absent and empty files hash differently (see `append_file_marker`).
///
/// Lives beside [`build_walker`] so the fingerprint and the builder cannot
/// drift apart — every input `build_walker` reads to decide which files a
/// walk yields must also be reflected here.
///
/// Known gap: nested (non-root) `.gitignore` files are not covered. This is
/// a documented limitation, not a TODO — see the module's detect-and-report
/// scope.
///
/// # Errors
///
/// Returns [`IndexError::FileRead`] if an ignore file exists but cannot be
/// read (e.g. permission denied).
pub fn ignore_fingerprint(settings: &Settings, root: &Path) -> IndexResult<String> {
    // Mirrors `build_walker`'s own root resolution for `ignore_patterns`:
    // root at the workspace when known, falling back to the walk root.
    let ignore_root = settings.workspace_root.as_deref().unwrap_or(root);

    let mut input = String::new();
    append_file_marker(
        &mut input,
        "codannaignore",
        &ignore_root.join(".codannaignore"),
    )?;
    append_file_marker(&mut input, "gitignore", &ignore_root.join(".gitignore"))?;
    append_file_marker(
        &mut input,
        "git_exclude",
        &ignore_root.join(".git").join("info").join("exclude"),
    )?;

    input.push_str("ignore_patterns=[");
    for pattern in &settings.indexing.ignore_patterns {
        input.push_str(pattern);
        input.push('\x1f'); // unit separator between patterns
    }
    input.push_str("]\n");

    input.push_str(&format!(
        "follow_links={}\n",
        settings.indexing.follow_links
    ));

    Ok(calculate_hash(&input))
}

/// Warn once per entry when a symlinked directory is skipped because
/// `[indexing] follow_links` is disabled.
///
/// With `follow_links(false)`, `ignore::WalkBuilder` still delivers a
/// symlinked directory to the walk as a normal entry (see `ignore`
/// crate's `walk.rs`); it is simply not descended into. Left unchecked,
/// that entry is silently dropped downstream by extension/file-type
/// filtering, so the exclusion is invisible unless a caller checks here.
///
/// Call this from every walk site's per-entry closure (`FileWalker::walk`,
/// `DiscoverStage::run`, `DiscoverStage::collect_all_files`), before any
/// file-type or extension filtering discards the entry, so all three
/// walks report symlinked directories the same way instead of duplicating
/// (or omitting) this check independently.
///
/// `entry.path().is_dir()` follows the symlink via `metadata()` and is
/// only evaluated for entries already known to be symlinks, so this adds
/// one extra stat solely on the (rare) symlink path.
///
/// Depth 0 is exempt: a symlink named as the walk root *is* descended into
/// even with `follow_links(false)` (the `ignore` crate skips its entry
/// filtering at depth 0), so warning there would claim a skip that never
/// happens. This asymmetry — nested symlink skipped, same symlink walked
/// when named directly — is the documented behavior of the underlying walk.
pub fn warn_if_skipped_symlink_dir(entry: &ignore::DirEntry, follow_links: bool) {
    if follow_links || entry.depth() == 0 {
        return;
    }
    if entry.file_type().is_some_and(|ft| ft.is_symlink()) && entry.path().is_dir() {
        tracing::warn!(
            "skipping symlinked directory '{}' (not followed); set [indexing] follow_links = true to index it",
            entry.path().display()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn honors_codannaignore() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();

        fs::write(root.join(".codannaignore"), "ignored.rs\n").unwrap();
        fs::write(root.join("ignored.rs"), "fn ignored() {}").unwrap();
        fs::write(root.join("included.rs"), "fn included() {}").unwrap();

        let settings = Settings::default();
        let builder = build_walker(&settings, root).unwrap();

        let files: Vec<_> = builder
            .build()
            .flatten()
            .filter(|e| e.file_type().is_some_and(|ft| ft.is_file()))
            .map(|e| e.path().to_path_buf())
            .collect();

        assert!(files.iter().any(|p| p.ends_with("included.rs")));
        assert!(!files.iter().any(|p| p.ends_with("ignored.rs")));
    }

    #[test]
    fn honors_gitignore() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();

        fs::write(root.join(".gitignore"), "ignored.rs\n").unwrap();
        fs::write(root.join("ignored.rs"), "fn ignored() {}").unwrap();
        fs::write(root.join("included.rs"), "fn included() {}").unwrap();

        let settings = Settings::default();
        let builder = build_walker(&settings, root).unwrap();

        let files: Vec<_> = builder
            .build()
            .flatten()
            .filter(|e| e.file_type().is_some_and(|ft| ft.is_file()))
            .map(|e| e.path().to_path_buf())
            .collect();

        assert!(files.iter().any(|p| p.ends_with("included.rs")));
        assert!(!files.iter().any(|p| p.ends_with("ignored.rs")));
    }

    #[test]
    fn rejects_invalid_ignore_pattern() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();

        let mut settings = Settings::default();
        // An inverted character range (e.g. `[z-a]`) is rejected by
        // globset's glob parser as `ErrorKind::InvalidRange`.
        settings.indexing.ignore_patterns = vec!["[z-a]".to_string()];

        let result = build_walker(&settings, root);
        assert!(result.is_err());
        match result.unwrap_err() {
            IndexError::InvalidIgnorePattern { pattern, .. } => {
                assert_eq!(pattern, "[z-a]");
            }
            other => panic!("expected InvalidIgnorePattern, got {other:?}"),
        }
    }

    #[test]
    fn fingerprint_distinguishes_absent_from_empty_file() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();
        let settings = Settings::default();

        // No .gitignore present.
        let absent = ignore_fingerprint(&settings, root).unwrap();

        // An empty .gitignore is a different input than no file at all.
        fs::write(root.join(".gitignore"), "").unwrap();
        let empty = ignore_fingerprint(&settings, root).unwrap();

        assert_ne!(
            absent, empty,
            "absent and empty .gitignore must hash differently"
        );
    }

    #[test]
    fn fingerprint_changes_with_config_only() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();

        // No ignore files on disk at all; only settings differ.
        let mut settings = Settings::default();
        let baseline = ignore_fingerprint(&settings, root).unwrap();

        settings.indexing.ignore_patterns = vec!["*.tmp".to_string()];
        let with_patterns = ignore_fingerprint(&settings, root).unwrap();
        assert_ne!(
            baseline, with_patterns,
            "adding ignore_patterns must move the fingerprint"
        );

        let mut follow_links_changed = Settings::default();
        follow_links_changed.indexing.follow_links = !follow_links_changed.indexing.follow_links;
        let with_follow_links = ignore_fingerprint(&follow_links_changed, root).unwrap();
        assert_ne!(
            baseline, with_follow_links,
            "toggling follow_links must move the fingerprint"
        );
    }

    #[test]
    fn fingerprint_is_stable_across_runs() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();

        fs::write(root.join(".gitignore"), "target/\n").unwrap();
        fs::write(root.join(".codannaignore"), "*.log\n").unwrap();

        let mut settings = Settings::default();
        settings.indexing.ignore_patterns = vec!["*.bak".to_string()];

        let first = ignore_fingerprint(&settings, root).unwrap();
        let second = ignore_fingerprint(&settings, root).unwrap();

        assert_eq!(
            first, second,
            "fingerprint must be deterministic for identical inputs"
        );
    }
}
