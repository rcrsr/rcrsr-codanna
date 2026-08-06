//! Discover stage - parallel file system walk
//!
//! Uses the `ignore` crate's parallel walker for high-performance
//! file discovery. Filters by supported extensions.
//!
//! Supports two modes:
//! - Full: Discovers all files (for initial indexing or force re-index)
//! - Incremental: Compares disk state to index, returns new/modified/deleted

use crate::Settings;
use crate::indexing::file_info::calculate_hash;
use crate::indexing::pipeline::types::{
    DiscoverResult, FileContent, PipelineError, PipelineResult,
};
use crate::indexing::walk_config::{build_walker, warn_if_skipped_symlink_dir};
use crate::parsing::get_registry;
use crate::storage::DocumentIndex;
use crossbeam_channel::Sender;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Discover stage for parallel file walking.
pub struct DiscoverStage {
    root: PathBuf,
    threads: usize,
    /// Optional index for incremental mode.
    index: Option<Arc<DocumentIndex>>,
    /// Workspace root for path normalization.
    workspace_root: Option<PathBuf>,
    /// Settings used to build the canonical WalkBuilder.
    settings: Option<Arc<Settings>>,
}

impl DiscoverStage {
    /// Create a new discover stage.
    pub fn new(root: impl Into<PathBuf>, threads: usize) -> Self {
        Self {
            root: root.into(),
            threads: threads.max(1),
            index: None,
            workspace_root: None,
            settings: None,
        }
    }

    /// Add an index for incremental mode.
    pub fn with_index(mut self, index: Arc<DocumentIndex>) -> Self {
        self.index = Some(index);
        self
    }

    /// Set workspace root for path normalization.
    pub fn with_workspace_root(mut self, root: Option<PathBuf>) -> Self {
        self.workspace_root = root;
        self
    }

    /// Set settings used to build the canonical WalkBuilder (see
    /// `crate::indexing::walk_config::build_walker`).
    pub fn with_settings(mut self, settings: Arc<Settings>) -> Self {
        self.settings = Some(settings);
        self
    }

    /// Resolve the settings to use for the walk, falling back to defaults
    /// when none were configured via `with_settings`.
    fn settings_or_default(&self) -> Arc<Settings> {
        self.settings
            .clone()
            .unwrap_or_else(|| Arc::new(Settings::default()))
    }

    /// Normalize a path relative to workspace_root.
    fn normalize_path(&self, path: &Path) -> PathBuf {
        if path.is_absolute() {
            if let Some(ref root) = self.workspace_root {
                path.strip_prefix(root)
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(|_| path.to_path_buf())
            } else {
                path.to_path_buf()
            }
        } else {
            path.to_path_buf()
        }
    }

    /// Run the discover stage, sending paths to the provided channel.
    ///
    /// Returns the number of files discovered.
    pub fn run(&self, sender: Sender<PathBuf>) -> PipelineResult<usize> {
        let extensions = get_supported_extensions()?;
        let count = Arc::new(AtomicUsize::new(0));

        let settings = self.settings_or_default();
        let follow_links = settings.indexing.follow_links;
        let mut builder = build_walker(&settings, &self.root)?;
        builder.threads(self.threads);

        let walker = builder.build_parallel();

        let count_clone = count.clone();
        let extensions = Arc::new(extensions);

        walker.run(|| {
            let sender = sender.clone();
            let extensions = extensions.clone();
            let count = count_clone.clone();

            Box::new(move |entry| {
                let entry = match entry {
                    Ok(e) => e,
                    Err(_) => return ignore::WalkState::Continue,
                };

                warn_if_skipped_symlink_dir(&entry, follow_links);

                // Skip directories
                if entry.file_type().is_some_and(|ft| ft.is_dir()) {
                    return ignore::WalkState::Continue;
                }

                let path = entry.path();

                // Skip hidden files (files starting with .) - matches FileWalker behavior
                if let Some(file_name) = path.file_name() {
                    if let Some(name_str) = file_name.to_str() {
                        if name_str.starts_with('.') {
                            return ignore::WalkState::Continue;
                        }
                    }
                }

                // Filter by extension
                if !has_supported_extension(path, &extensions) {
                    return ignore::WalkState::Continue;
                }

                // Send path to channel
                count.fetch_add(1, Ordering::Relaxed);
                if sender.send(path.to_path_buf()).is_err() {
                    // Channel closed, stop walking
                    return ignore::WalkState::Quit;
                }

                ignore::WalkState::Continue
            })
        });

        Ok(count.load(Ordering::Relaxed))
    }

    /// Run incremental discovery, comparing disk state to index.
    ///
    /// Returns categorized files: new, modified, and deleted.
    /// Requires an index to be set via `with_index()`.
    pub fn run_incremental(&self) -> PipelineResult<DiscoverResult> {
        let index = self.index.as_ref().ok_or_else(|| PipelineError::Parse {
            path: self.root.clone(),
            reason: "Incremental mode requires an index".to_string(),
        })?;

        // Step 1: Collect all current files on disk, normalized to relative paths
        let disk_files = self.collect_all_files()?;
        let disk_set: HashSet<PathBuf> = disk_files
            .into_iter()
            .map(|p| self.normalize_path(&p))
            .collect();

        // Step 2: Get indexed paths from Tantivy, filtered to only those under our root
        // This prevents marking files from other indexed directories as "deleted"
        let normalized_root = self.normalize_path(&self.root);
        let indexed_paths = index.get_all_indexed_paths()?;
        let indexed_set: HashSet<PathBuf> = indexed_paths
            .into_iter()
            .filter(|p| p.starts_with(&normalized_root))
            .collect();

        tracing::debug!(
            target: "pipeline",
            "incremental: root={}, normalized_root={}, disk={}, indexed={}",
            self.root.display(),
            normalized_root.display(),
            disk_set.len(),
            indexed_set.len()
        );

        // Step 3: Categorize files
        let mut result = DiscoverResult::default();

        // New files: on disk but not in index
        for path in &disk_set {
            if !indexed_set.contains(path) {
                result.new_files.push(path.clone());
            }
        }

        // Deleted files: in index but not on disk
        for path in &indexed_set {
            if !disk_set.contains(path) {
                result.deleted_files.push(path.clone());
            }
        }

        // Modified files: in both, but hash differs
        for path in disk_set.intersection(&indexed_set) {
            if self.is_modified(path, index)? {
                result.modified_files.push(path.clone());
            }
        }

        // Step 4: pair deleted x new by exact content hash. A rename
        // surfaces as deleted(old) + new(new); the stored hash of the old
        // path against the computed hash of the new file is identity-grade
        // evidence that the file moved, so its inbound edges relocate
        // instead of dying with genuine-deletion semantics.
        if !result.new_files.is_empty() && !result.deleted_files.is_empty() {
            let mut deleted_hashed = Vec::new();
            for path in &result.deleted_files {
                let path_str = path.to_string_lossy();
                if let Some((_file_id, hash, _mtime)) = index.get_file_info(&path_str)? {
                    deleted_hashed.push((path.clone(), hash));
                }
            }
            // Read once here (not again in READ): the same disk read this
            // pairing pass already needs to compute the new file's hash is
            // cached and handed to READ via `preloaded_content`, so a hit
            // there reuses this content instead of reading+hashing again.
            let mut preloaded: HashMap<PathBuf, FileContent> = HashMap::new();
            let mut new_hashed = Vec::new();
            for path in &result.new_files {
                let read_path = self.resolve_read_path(path);
                match fs::read_to_string(&read_path) {
                    Ok(content) => {
                        let hash = calculate_hash(&content);
                        new_hashed.push((path.clone(), hash.clone()));
                        preloaded
                            .insert(path.clone(), FileContent::new(path.clone(), content, hash));
                    }
                    Err(e) => {
                        // Unreadable now means unpairable now; the file is
                        // read again at parse, where failure is surfaced.
                        tracing::debug!(
                            target: "pipeline",
                            "rename pairing skipped {}: {e}",
                            path.display()
                        );
                    }
                }
            }
            let pairs = pair_relocations(&deleted_hashed, &new_hashed);
            if !pairs.is_empty() {
                {
                    let old_set: HashSet<&PathBuf> = pairs.iter().map(|(old, _)| old).collect();
                    let new_set: HashSet<&PathBuf> = pairs.iter().map(|(_, new)| new).collect();
                    result.deleted_files.retain(|p| !old_set.contains(p));
                    result.new_files.retain(|p| !new_set.contains(p));
                }
                result.renamed_files = pairs;
            }
            result.preloaded_content = preloaded;
        }

        tracing::debug!(
            target: "pipeline",
            "incremental result: new={}, modified={}, deleted={}, renamed={}",
            result.new_files.len(),
            result.modified_files.len(),
            result.deleted_files.len(),
            result.renamed_files.len()
        );

        Ok(result)
    }

    /// Collect all files on disk (synchronous, for incremental comparison).
    fn collect_all_files(&self) -> PipelineResult<Vec<PathBuf>> {
        let extensions = get_supported_extensions()?;
        let mut files = Vec::new();

        // Use sequential walker for simplicity in incremental mode
        let settings = self.settings_or_default();
        let follow_links = settings.indexing.follow_links;
        let builder = build_walker(&settings, &self.root)?;

        let walker = builder.build();

        for entry in walker.flatten() {
            warn_if_skipped_symlink_dir(&entry, follow_links);

            if entry.file_type().is_some_and(|ft| ft.is_dir()) {
                continue;
            }

            let path = entry.path();

            // Skip hidden files (files starting with .) - matches FileWalker behavior
            if let Some(file_name) = path.file_name() {
                if let Some(name_str) = file_name.to_str() {
                    if name_str.starts_with('.') {
                        continue;
                    }
                }
            }

            if has_supported_extension(path, &extensions) {
                files.push(path.to_path_buf());
            }
        }

        Ok(files)
    }

    /// Resolve a workspace-relative path to a readable filesystem path.
    ///
    /// `disk_set`/`indexed_set` in `run_incremental` carry paths normalized
    /// (relative to `workspace_root`) so they can be compared against the
    /// index's stored rows. Opening a relative path as-is resolves against
    /// the process CWD, not the workspace root -- the same class of bug
    /// fixed for `ReadStage` (see its `run()` comment). Mirror that fix here
    /// for any filesystem read performed on one of those normalized paths.
    fn resolve_read_path(&self, path: &Path) -> PathBuf {
        match &self.workspace_root {
            Some(root) if path.is_relative() => root.join(path),
            _ => path.to_path_buf(),
        }
    }

    /// Check if a file has been modified.
    /// Uses mtime as fast heuristic - only reads file if mtime changed.
    fn is_modified(&self, path: &Path, index: &DocumentIndex) -> PipelineResult<bool> {
        let path_str = path.to_string_lossy();

        // Get stored info from index
        let stored_info = index.get_file_info(&path_str)?;
        let Some((_file_id, stored_hash, stored_mtime)) = stored_info else {
            // Not in index = treat as new
            tracing::trace!(target: "pipeline", "is_modified: {} not in index", path.display());
            return Ok(true);
        };

        // `path` is workspace-relative (see `resolve_read_path`); resolve it
        // against workspace_root before touching the filesystem.
        let read_path = self.resolve_read_path(path);

        // Fast path: check mtime first (stat only, no file read)
        let current_mtime = crate::indexing::file_info::get_file_mtime(&read_path).unwrap_or(0);
        if stored_mtime > 0 && current_mtime == stored_mtime {
            // mtime unchanged = file unchanged
            return Ok(false);
        }

        // mtime changed or unknown - verify with hash (requires file read)
        let content = fs::read_to_string(&read_path).map_err(|e| PipelineError::FileRead {
            path: read_path.clone(),
            source: e,
        })?;
        let current_hash = calculate_hash(&content);

        let modified = current_hash != stored_hash;
        if modified {
            tracing::trace!(
                target: "pipeline",
                "is_modified: {} hash changed (stored_mtime={}, current_mtime={})",
                path.display(),
                stored_mtime,
                current_mtime
            );
        }

        Ok(modified)
    }
}

/// Pair deleted x new files by exact content hash.
///
/// A pair forms only when a hash matches exactly one deleted and exactly one
/// new path. Every other multiplicity -- zero, many-to-one, one-to-many -- is
/// not identity evidence, so both sides keep genuine-deletion semantics.
/// Multiplicity decides, never candidate order: iteration order cannot mint
/// a pair.
fn pair_relocations(
    deleted: &[(PathBuf, String)],
    new: &[(PathBuf, String)],
) -> Vec<(PathBuf, PathBuf)> {
    use std::collections::HashMap;

    let mut deleted_by_hash: HashMap<&str, Vec<&PathBuf>> = HashMap::new();
    for (path, hash) in deleted {
        deleted_by_hash.entry(hash.as_str()).or_default().push(path);
    }
    let mut new_by_hash: HashMap<&str, Vec<&PathBuf>> = HashMap::new();
    for (path, hash) in new {
        new_by_hash.entry(hash.as_str()).or_default().push(path);
    }

    let mut pairs = Vec::new();
    for (hash, olds) in &deleted_by_hash {
        let news = new_by_hash.get(hash).map(Vec::as_slice).unwrap_or_default();
        if let ([old], [new_path]) = (olds.as_slice(), news) {
            pairs.push(((*old).clone(), (*new_path).clone()));
        }
    }
    pairs.sort();
    pairs
}

/// Get all supported file extensions from the language registry.
fn get_supported_extensions() -> PipelineResult<HashSet<&'static str>> {
    let registry = get_registry();
    let registry = registry.lock().map_err(|e| PipelineError::Parse {
        path: PathBuf::new(),
        reason: format!("Failed to acquire registry lock: {e}"),
    })?;

    let mut extensions = HashSet::new();
    for def in registry.iter_all() {
        for ext in def.extensions() {
            extensions.insert(*ext);
        }
    }

    Ok(extensions)
}

/// Check if a path has a supported extension.
fn has_supported_extension(path: &Path, extensions: &HashSet<&str>) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| extensions.contains(ext))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::bounded;

    #[test]
    fn test_discover_examples_directory() {
        let (sender, receiver) = bounded(1000);

        let stage = DiscoverStage::new("examples", 4).with_settings(Arc::new(Settings::default()));
        let result = stage.run(sender);

        assert!(result.is_ok(), "Discover should succeed");
        let count = result.unwrap();

        // Collect all discovered paths
        let paths: Vec<PathBuf> = receiver.iter().collect();

        println!("Discovered {count} files:");
        for path in &paths {
            println!("  - {}", path.display());
        }

        assert_eq!(paths.len(), count, "Count should match received paths");
        assert!(
            count > 0,
            "Should discover at least some files in examples/"
        );

        // Verify all paths have supported extensions
        let extensions = get_supported_extensions().unwrap();
        for path in &paths {
            assert!(
                has_supported_extension(path, &extensions),
                "Path {} should have supported extension",
                path.display()
            );
        }
    }

    #[test]
    fn test_discover_respects_gitignore() {
        let (sender, receiver) = bounded(1000);

        let stage = DiscoverStage::new(".", 4).with_settings(Arc::new(Settings::default()));
        let _count = stage.run(sender);

        let paths: Vec<PathBuf> = receiver.iter().collect();

        // Should not include target/ directory contents
        for path in &paths {
            let path_str = path.to_string_lossy();
            assert!(
                !path_str.contains("target/debug") && !path_str.contains("target/release"),
                "Should not include target/ contents: {}",
                path.display()
            );
        }
    }

    /// Mixed new+deleted incremental batch: run_incremental must populate
    /// `preloaded_content` for every surviving new-file candidate, keyed in
    /// the same normalized form the categorized vecs carry, with content
    /// and hash matching what's actually on disk. This discriminates
    /// absolute-vs-relative key-form bugs -- a mismatched key form means
    /// READ's `.get(&path)` always misses.
    #[test]
    fn run_incremental_populates_preloaded_content_for_new_files() {
        use crate::Settings;
        use crate::indexing::pipeline::Pipeline;
        use crate::storage::DocumentIndex;
        use tempfile::TempDir;

        let temp_dir = TempDir::new().expect("tempdir");
        let src_dir = temp_dir.path().join("src");
        fs::create_dir_all(&src_dir).expect("create src dir");

        let original_content = "fn kept() {}\n";
        fs::write(src_dir.join("original.rs"), original_content).expect("write original.rs");
        let to_delete_content = "fn going_away() {}\n";
        fs::write(src_dir.join("to_delete.rs"), to_delete_content).expect("write to_delete.rs");

        let index_dir = temp_dir.path().join("index");
        fs::create_dir_all(&index_dir).expect("create index dir");

        let settings = Settings::default();
        let index = DocumentIndex::new(&index_dir, &settings).expect("create index");
        let index = Arc::new(index);
        let settings = Arc::new(settings);
        let pipeline = Pipeline::with_settings(Arc::clone(&settings));

        pipeline
            .index_directory(&src_dir, Arc::clone(&index))
            .expect("initial index");

        // Mixed batch: delete one indexed file and add a brand-new,
        // content-unrelated file, so run_incremental's new+deleted pairing
        // pass runs (and does not accidentally pair the two as a rename).
        fs::remove_file(src_dir.join("to_delete.rs")).expect("remove to_delete.rs");
        let new_content = "fn added() {}\n";
        fs::write(src_dir.join("added.rs"), new_content).expect("write added.rs");

        let discover_stage = DiscoverStage::new(&src_dir, 1)
            .with_index(Arc::clone(&index))
            .with_workspace_root(settings.workspace_root.clone())
            .with_settings(Arc::clone(&settings));

        let result = discover_stage
            .run_incremental()
            .expect("run_incremental succeeds");

        assert_eq!(result.new_files.len(), 1, "expected exactly one new file");
        assert_eq!(
            result.deleted_files.len(),
            1,
            "expected exactly one deleted file"
        );
        let new_path = &result.new_files[0];

        let preloaded = result
            .preloaded_content
            .get(new_path)
            .unwrap_or_else(|| panic!("preloaded_content missing entry for {new_path:?}"));

        assert_eq!(preloaded.content, new_content);
        assert_eq!(preloaded.hash, calculate_hash(new_content));
    }

    // Pinning locks for the relocation pairing gate: a pair needs a hash
    // matching exactly one deleted and exactly one new path. Multiplicity
    // decides; ambiguity keeps genuine-deletion semantics.
    #[test]
    fn pair_relocations_pairs_unique_hash_match() {
        let pairs = pair_relocations(
            &[(PathBuf::from("old/a.go"), "h1".to_string())],
            &[(PathBuf::from("new/a.go"), "h1".to_string())],
        );
        assert_eq!(
            pairs,
            vec![(PathBuf::from("old/a.go"), PathBuf::from("new/a.go"))]
        );
    }

    #[test]
    fn pair_relocations_refuses_ambiguous_multiplicities() {
        // Two deleted share the hash of one new: no pair.
        let two_olds = pair_relocations(
            &[
                (PathBuf::from("a.go"), "h1".to_string()),
                (PathBuf::from("b.go"), "h1".to_string()),
            ],
            &[(PathBuf::from("c.go"), "h1".to_string())],
        );
        assert!(two_olds.is_empty(), "many-to-one must not pair");

        // One deleted matches two new: no pair.
        let two_news = pair_relocations(
            &[(PathBuf::from("a.go"), "h1".to_string())],
            &[
                (PathBuf::from("b.go"), "h1".to_string()),
                (PathBuf::from("c.go"), "h1".to_string()),
            ],
        );
        assert!(two_news.is_empty(), "one-to-many must not pair");

        // Hash mismatch (rename plus edit): no pair.
        let edited = pair_relocations(
            &[(PathBuf::from("a.go"), "h1".to_string())],
            &[(PathBuf::from("b.go"), "h2".to_string())],
        );
        assert!(edited.is_empty(), "a content edit must fail closed");
    }

    #[test]
    fn pair_relocations_pairs_each_unique_hash_independently() {
        // A directory rename decomposes per-file: unique hashes pair,
        // the duplicate pair stays on deletion semantics.
        let pairs = pair_relocations(
            &[
                (PathBuf::from("old/a.go"), "ha".to_string()),
                (PathBuf::from("old/b.go"), "hb".to_string()),
                (PathBuf::from("old/dup1.go"), "hd".to_string()),
                (PathBuf::from("old/dup2.go"), "hd".to_string()),
            ],
            &[
                (PathBuf::from("new/a.go"), "ha".to_string()),
                (PathBuf::from("new/b.go"), "hb".to_string()),
                (PathBuf::from("new/dup1.go"), "hd".to_string()),
                (PathBuf::from("new/dup2.go"), "hd".to_string()),
            ],
        );
        assert_eq!(
            pairs,
            vec![
                (PathBuf::from("old/a.go"), PathBuf::from("new/a.go")),
                (PathBuf::from("old/b.go"), PathBuf::from("new/b.go")),
            ]
        );
    }

    #[test]
    fn test_get_supported_extensions() {
        let extensions = get_supported_extensions().unwrap();

        println!("Supported extensions: {extensions:?}");

        // Should include common extensions
        assert!(extensions.contains("rs"), "Should support .rs");
        assert!(extensions.contains("py"), "Should support .py");
        assert!(extensions.contains("ts"), "Should support .ts");
        assert!(extensions.contains("go"), "Should support .go");
    }
}
