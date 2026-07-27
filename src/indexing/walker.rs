//! File system walker for discovering source files to index
//!
//! This module provides efficient directory traversal with support for:
//! - .gitignore rules
//! - Custom ignore patterns from configuration
//! - Language filtering
//! - Hidden file handling

use crate::Settings;
use crate::error::IndexResult;
use crate::indexing::walk_config::{build_walker, warn_if_skipped_symlink_dir};
use crate::parsing::get_registry;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Walks directories to find source files to index
#[derive(Debug)]
pub struct FileWalker {
    settings: Arc<Settings>,
}

impl FileWalker {
    /// Create a new file walker with the given settings
    pub fn new(settings: Arc<Settings>) -> Self {
        Self { settings }
    }

    /// Walk a directory and return an iterator of files to index.
    ///
    /// Warns once per skipped symlinked directory (see
    /// [`warn_if_skipped_symlink_dir`]). Use [`Self::walk_quiet`] for a
    /// count-only pass that shares a walk site with a caller that will also
    /// walk (and warn on) the same directory, to avoid a duplicate warning.
    pub fn walk(&self, root: &Path) -> IndexResult<impl Iterator<Item = PathBuf>> {
        self.walk_impl(root, true)
    }

    /// Same as [`Self::walk`], but suppresses the skipped-symlinked-directory
    /// warning. Intended for a count-only walk (e.g. sizing a progress bar)
    /// that runs immediately before another walk site walks the same
    /// directory for real and would otherwise warn a second time.
    pub fn walk_quiet(&self, root: &Path) -> IndexResult<impl Iterator<Item = PathBuf>> {
        self.walk_impl(root, false)
    }

    fn walk_impl(
        &self,
        root: &Path,
        warn_on_skip: bool,
    ) -> IndexResult<impl Iterator<Item = PathBuf>> {
        let mut builder = build_walker(&self.settings, root)?;
        builder.max_depth(None); // No depth limit

        // Get enabled extensions from the registry
        let enabled_extensions = self.get_enabled_extensions();
        let follow_links = self.settings.indexing.follow_links;

        // Build and filter the walker
        Ok(builder
            .build()
            .filter_map(Result::ok) // Skip files we can't access
            .inspect(move |entry| {
                if warn_on_skip {
                    warn_if_skipped_symlink_dir(entry, follow_links);
                }
            })
            .filter(|entry| entry.file_type().is_some_and(|ft| ft.is_file()))
            .filter_map(move |entry| {
                let path = entry.path();

                // Skip hidden files (files starting with .)
                if let Some(file_name) = path.file_name() {
                    if let Some(name_str) = file_name.to_str() {
                        if name_str.starts_with('.') {
                            return None;
                        }
                    }
                }

                // Check if this file extension is enabled
                if let Some(extension) = path.extension() {
                    if let Some(ext_str) = extension.to_str() {
                        if enabled_extensions.iter().any(|ext| ext == ext_str) {
                            return Some(path.to_path_buf());
                        }
                    }
                }

                None
            }))
    }

    /// Directories the index walk would traverse under `root`, ignore
    /// chains applied. Feeds watch registration for created directories;
    /// the walk root itself is always yielded (even dot-prefixed, so a
    /// scope explicitly named by the caller is never dropped), while
    /// dot-prefixed directories below the root are skipped (keeps `.git`
    /// trees out of watch sets). Does not warn on skipped symlinked
    /// directories, for the same reason as [`Self::walk_quiet`]: this is a
    /// second walk site over a tree the index walk already reports on. No
    /// depth limit.
    ///
    /// Known asymmetry with [`Self::walk`], documented rather than
    /// unified: the file walk filters hidden *files* by name but still
    /// descends *into* dot-prefixed directories, whereas this walk skips
    /// dot-prefixed directories below the root outright (so it never
    /// descends into them at all). Both are intentional for their own
    /// callers and are not reconciled here.
    pub fn walk_dirs(&self, root: &Path) -> IndexResult<impl Iterator<Item = PathBuf>> {
        let mut builder = build_walker(&self.settings, root)?;
        builder.max_depth(None); // No depth limit
        Ok(builder
            .build()
            .filter_map(Result::ok) // Skip entries we can't access; dir + dot-dir filters below
            .filter(|entry| entry.file_type().is_some_and(|ft| ft.is_dir()))
            .filter(|entry| {
                entry.depth() == 0
                    || entry
                        .file_name()
                        .to_str()
                        .is_some_and(|n| !n.starts_with('.'))
            })
            .map(|entry| entry.path().to_path_buf()))
    }

    /// Single traversal yielding both the directories and the files
    /// `Self::walk_dirs` and `Self::walk` would each report under `root`,
    /// applying the same filters as both (dot-directory skip for dirs,
    /// hidden-file/extension filters for files). Exists so a caller that
    /// needs both results (e.g. watch registration plus catch-up indexing
    /// for a newly created directory) walks the subtree once instead of
    /// twice; see `Self::walk` / `Self::walk_dirs` for the individual
    /// filter semantics this preserves.
    pub fn walk_dirs_and_files(&self, root: &Path) -> IndexResult<(Vec<PathBuf>, Vec<PathBuf>)> {
        let mut builder = build_walker(&self.settings, root)?;
        builder.max_depth(None); // No depth limit

        let enabled_extensions = self.get_enabled_extensions();
        let follow_links = self.settings.indexing.follow_links;

        let mut dirs = Vec::new();
        let mut files = Vec::new();

        for entry in builder.build().filter_map(Result::ok) {
            warn_if_skipped_symlink_dir(&entry, follow_links);

            if entry.file_type().is_some_and(|ft| ft.is_dir()) {
                let keep = entry.depth() == 0
                    || entry
                        .file_name()
                        .to_str()
                        .is_some_and(|n| !n.starts_with('.'));
                if keep {
                    dirs.push(entry.path().to_path_buf());
                }
                continue;
            }

            if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                continue;
            }

            let path = entry.path();
            if let Some(file_name) = path.file_name() {
                if let Some(name_str) = file_name.to_str() {
                    if name_str.starts_with('.') {
                        continue;
                    }
                }
            }
            if let Some(extension) = path.extension() {
                if let Some(ext_str) = extension.to_str() {
                    if enabled_extensions.iter().any(|ext| ext == ext_str) {
                        files.push(path.to_path_buf());
                    }
                }
            }
        }

        Ok((dirs, files))
    }

    /// Get list of enabled file extensions from the registry
    fn get_enabled_extensions(&self) -> Vec<String> {
        let registry = get_registry();
        if let Ok(registry) = registry.lock() {
            registry
                .enabled_extensions(&self.settings)
                .map(|ext| ext.to_string())
                .collect()
        } else {
            // Fallback to empty if registry lock fails
            Vec::new()
        }
    }

    /// Count files that would be indexed (useful for dry runs)
    pub fn count_files(&self, root: &Path) -> IndexResult<usize> {
        Ok(self.walk(root)?.count())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn create_test_settings() -> Arc<Settings> {
        let mut settings = Settings::default();
        // Disable Python and PHP for testing (only Rust enabled)
        settings.languages.get_mut("python").unwrap().enabled = false;
        settings.languages.get_mut("php").unwrap().enabled = false;
        // Rust remains enabled by default
        Arc::new(settings)
    }

    #[test]
    fn test_walk_directory() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();

        // Create some test files
        fs::write(root.join("main.rs"), "fn main() {}").unwrap();
        fs::write(root.join("lib.rs"), "pub fn lib() {}").unwrap();
        fs::write(root.join("test.py"), "def test(): pass").unwrap();
        fs::write(root.join("README.md"), "# Test").unwrap();

        let settings = create_test_settings();
        let walker = FileWalker::new(settings);

        let files: Vec<_> = walker.walk(root).unwrap().collect();

        // Should find only Rust files (Python and PHP disabled in test settings)
        assert_eq!(files.len(), 2);
        assert!(files.iter().any(|p| p.ends_with("main.rs")));
        assert!(files.iter().any(|p| p.ends_with("lib.rs")));
    }

    #[test]
    fn test_ignore_hidden_files() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();

        // Create hidden file and visible file
        fs::write(root.join(".hidden.rs"), "fn hidden() {}").unwrap();
        fs::write(root.join("visible.rs"), "fn visible() {}").unwrap();

        let settings = create_test_settings();
        let walker = FileWalker::new(settings);

        let files: Vec<_> = walker.walk(root).unwrap().collect();

        // Should only find the visible file (hidden files are filtered out)
        assert_eq!(files.len(), 1);
        assert!(files[0].ends_with("visible.rs"));
    }

    // walk_dirs feeds the watcher's directory-chain registration: a new
    // directory subtree gets watches only where the index walk would
    // traverse -- an ignored tree (node_modules, generated/) is pruned
    // by the same chains before any kernel watch is added.
    #[test]
    fn walk_dirs_prunes_ignored_subtrees() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();
        fs::create_dir_all(root.join("newmod/empty_sub")).unwrap();
        fs::create_dir_all(root.join("generated/deep")).unwrap();
        fs::write(root.join(".gitignore"), "generated/\n").unwrap();

        let settings = create_test_settings();
        let walker = FileWalker::new(settings);

        let mut dirs: Vec<_> = walker
            .walk_dirs(root)
            .unwrap()
            .map(|p| p.strip_prefix(root).unwrap().to_path_buf())
            .filter(|p| !p.as_os_str().is_empty())
            .collect();
        dirs.sort();

        assert_eq!(
            dirs,
            vec![
                std::path::PathBuf::from("newmod"),
                std::path::PathBuf::from("newmod/empty_sub"),
            ],
            "empty traversable dirs are yielded, ignored subtrees pruned"
        );
    }

    #[test]
    fn test_gitignore_respected() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();

        // Create .gitignore (should work without git init due to require_git(false))
        fs::write(root.join(".gitignore"), "ignored.rs\n").unwrap();

        // Create files
        fs::write(root.join("ignored.rs"), "fn ignored() {}").unwrap();
        fs::write(root.join("included.rs"), "fn included() {}").unwrap();

        let settings = create_test_settings();
        let walker = FileWalker::new(settings);

        let files: Vec<_> = walker.walk(root).unwrap().collect();

        // Should only find the included file
        assert_eq!(files.len(), 1);
        assert!(files[0].ends_with("included.rs"));
    }

    #[test]
    fn walk_dirs_honors_ignore_patterns() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();
        fs::create_dir_all(root.join("proj/keep")).unwrap();
        fs::create_dir_all(root.join("proj/skipped/nested")).unwrap();

        let mut settings = Settings::default();
        settings.languages.get_mut("python").unwrap().enabled = false;
        settings.languages.get_mut("php").unwrap().enabled = false;
        // Bare directory form, not "skipped/**": per the gitignore dialect,
        // `skipped/**` matches the directory's contents but not the
        // directory entry itself, which would make this assertion
        // ambiguous about whether the directory entry was actually pruned.
        settings.indexing.ignore_patterns = vec!["skipped/".to_string()];
        let walker = FileWalker::new(Arc::new(settings));

        let dirs: Vec<_> = walker
            .walk_dirs(root)
            .unwrap()
            .map(|p| p.strip_prefix(root).unwrap().to_path_buf())
            .collect();

        assert!(dirs.contains(&std::path::PathBuf::from("proj/keep")));
        assert!(!dirs.contains(&std::path::PathBuf::from("proj/skipped")));
        assert!(!dirs.contains(&std::path::PathBuf::from("proj/skipped/nested")));
    }

    #[test]
    fn walk_dirs_honors_gitignore_and_codannaignore() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();
        fs::create_dir_all(root.join("ignored_dir")).unwrap();
        fs::create_dir_all(root.join("other_dir")).unwrap();
        fs::create_dir_all(root.join("kept_dir")).unwrap();
        fs::write(root.join(".gitignore"), "ignored_dir/\n").unwrap();
        fs::write(root.join(".codannaignore"), "other_dir/\n").unwrap();

        let settings = create_test_settings();
        let walker = FileWalker::new(settings);

        let dirs: Vec<_> = walker
            .walk_dirs(root)
            .unwrap()
            .map(|p| p.strip_prefix(root).unwrap().to_path_buf())
            .collect();

        assert!(dirs.contains(&std::path::PathBuf::from("kept_dir")));
        assert!(!dirs.contains(&std::path::PathBuf::from("ignored_dir")));
        assert!(!dirs.contains(&std::path::PathBuf::from("other_dir")));
    }

    #[cfg(unix)]
    #[test]
    fn walk_dirs_respects_follow_links_setting() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();
        fs::create_dir_all(root.join("real_target/inner")).unwrap();
        std::os::unix::fs::symlink(root.join("real_target"), root.join("linked")).unwrap();

        let mut settings_no_follow = Settings::default();
        settings_no_follow
            .languages
            .get_mut("python")
            .unwrap()
            .enabled = false;
        settings_no_follow.languages.get_mut("php").unwrap().enabled = false;
        settings_no_follow.indexing.follow_links = false;
        let walker_no_follow = FileWalker::new(Arc::new(settings_no_follow));

        let dirs_no_follow: Vec<_> = walker_no_follow
            .walk_dirs(root)
            .unwrap()
            .map(|p| p.strip_prefix(root).unwrap().to_path_buf())
            .collect();
        assert!(!dirs_no_follow.contains(&std::path::PathBuf::from("linked/inner")));

        let mut settings_follow = Settings::default();
        settings_follow.languages.get_mut("python").unwrap().enabled = false;
        settings_follow.languages.get_mut("php").unwrap().enabled = false;
        settings_follow.indexing.follow_links = true;
        let walker_follow = FileWalker::new(Arc::new(settings_follow));

        let dirs_follow: Vec<_> = walker_follow
            .walk_dirs(root)
            .unwrap()
            .map(|p| p.strip_prefix(root).unwrap().to_path_buf())
            .collect();
        assert!(dirs_follow.contains(&std::path::PathBuf::from("linked/inner")));
    }

    #[cfg(unix)]
    #[test]
    fn walk_dirs_refuses_symlink_escaping_workspace_root() {
        let workspace = TempDir::new().unwrap();
        let root = workspace.path();
        let outside = TempDir::new().unwrap();
        fs::create_dir_all(outside.path().join("escaped/deep")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("escaped"), root.join("linked")).unwrap();

        let mut settings = Settings::default();
        settings.languages.get_mut("python").unwrap().enabled = false;
        settings.languages.get_mut("php").unwrap().enabled = false;
        settings.indexing.follow_links = true;
        settings.workspace_root = Some(root.to_path_buf());
        let walker = FileWalker::new(Arc::new(settings));

        let dirs: Vec<_> = walker
            .walk_dirs(root)
            .unwrap()
            .map(|p| p.strip_prefix(root).unwrap().to_path_buf())
            .collect();

        assert!(!dirs.contains(&std::path::PathBuf::from("linked/deep")));
    }

    #[test]
    fn walk_dirs_yields_root_even_when_dot_prefixed() {
        let temp_dir = TempDir::new().unwrap();
        let hidden_scope = temp_dir.path().join(".hidden_scope");
        fs::create_dir_all(&hidden_scope).unwrap();

        let settings = create_test_settings();
        let walker = FileWalker::new(settings);

        let dirs: Vec<_> = walker.walk_dirs(&hidden_scope).unwrap().collect();

        assert!(dirs.contains(&hidden_scope));
    }

    #[test]
    fn walk_dirs_skips_dot_directories_below_root() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::create_dir_all(root.join("src")).unwrap();

        let settings = create_test_settings();
        let walker = FileWalker::new(settings);

        let dirs: Vec<_> = walker
            .walk_dirs(root)
            .unwrap()
            .map(|p| p.strip_prefix(root).unwrap().to_path_buf())
            .collect();

        assert!(!dirs.contains(&std::path::PathBuf::from(".git")));
        assert!(dirs.contains(&std::path::PathBuf::from("src")));
    }

    #[test]
    fn walk_dirs_and_files_matches_the_two_separate_walks() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("main.rs"), "fn main() {}").unwrap();
        fs::write(root.join("sub/lib.rs"), "pub fn lib() {}").unwrap();
        fs::write(root.join(".hidden.rs"), "fn hidden() {}").unwrap();
        fs::write(root.join("README.md"), "# Test").unwrap();

        let settings = create_test_settings();
        let walker = FileWalker::new(settings);

        let mut expected_dirs: Vec<_> = walker.walk_dirs(root).unwrap().collect();
        let mut expected_files: Vec<_> = walker.walk(root).unwrap().collect();
        expected_dirs.sort();
        expected_files.sort();

        let (mut dirs, mut files) = walker.walk_dirs_and_files(root).unwrap();
        dirs.sort();
        files.sort();

        assert_eq!(dirs, expected_dirs);
        assert_eq!(files, expected_files);
    }
}
