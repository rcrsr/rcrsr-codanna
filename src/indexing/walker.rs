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

    /// Walk a directory and return an iterator of files to index
    pub fn walk(&self, root: &Path) -> IndexResult<impl Iterator<Item = PathBuf>> {
        let mut builder = build_walker(&self.settings, root)?;
        builder.max_depth(None); // No depth limit

        // Get enabled extensions from the registry
        let enabled_extensions = self.get_enabled_extensions();
        let follow_links = self.settings.indexing.follow_links;

        // Build and filter the walker
        Ok(builder
            .build()
            .filter_map(Result::ok) // Skip files we can't access
            .inspect(move |entry| warn_if_skipped_symlink_dir(entry, follow_links))
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
}
