//! Indexed-path management: the indexed_paths list and its canonicalized cache.

use super::Settings;
use std::path::{Path, PathBuf};

impl Settings {
    /// The cache is the comparison surface (strip-base selection tests
    /// canonicalized file paths against it); hand-edited or legacy entries
    /// may carry symlink components, so canonicalize here. The serialized
    /// `indexing.indexed_paths` stays verbatim to round-trip the user's file.
    pub(super) fn sync_indexed_path_cache(&mut self) {
        self.indexed_paths_cache = self
            .indexing
            .indexed_paths
            .iter()
            .map(|p| p.canonicalize().unwrap_or_else(|_| p.clone()))
            .collect();
    }

    /// Add a folder to the list of indexed paths
    pub fn add_indexed_path(&mut self, path: PathBuf) -> Result<(), String> {
        // Canonicalize the path to avoid duplicates
        let canonical_path = path
            .canonicalize()
            .map_err(|e| format!("Invalid path: {e}"))?;

        // Track whether we should remove child paths that are covered by the new entry
        let mut has_descendants = false;

        // Check if path already exists or is covered by an existing parent
        for existing in &self.indexed_paths_cache {
            if *existing == canonical_path {
                return Err(format!("Path already indexed: {}", path.display()));
            }

            // If an existing entry is an ancestor of the new path, treat as already indexed
            if canonical_path.starts_with(existing) {
                return Err(format!(
                    "Path already indexed: {} (covered by {})",
                    path.display(),
                    crate::parsing::paths::render_absolute_path(existing).display()
                ));
            }

            // Record descendant paths so we can prune them before inserting the parent
            if existing.starts_with(&canonical_path) {
                has_descendants = true;
            }
        }

        if has_descendants {
            // Remove any paths that are descendants of the new canonical path
            self.indexing
                .indexed_paths
                .retain(|existing| !existing.starts_with(&canonical_path));
            self.indexed_paths_cache
                .retain(|existing| !existing.starts_with(&canonical_path));
        }

        // Add the path
        self.indexing.indexed_paths.push(canonical_path.clone());
        self.indexed_paths_cache.push(canonical_path);
        Ok(())
    }

    /// Remove a folder from the list of indexed paths.
    ///
    /// A path that no longer exists on disk cannot be canonicalized, but it
    /// may still be recorded (the directory was deleted after `add-dir`);
    /// it is then matched by the absolute form of `path` as given.
    pub fn remove_indexed_path(&mut self, path: &Path) -> Result<(), String> {
        let canonical_path = match path.canonicalize() {
            Ok(canonical) => canonical,
            Err(_) if !path.exists() => {
                std::path::absolute(path).map_err(|e| format!("Invalid path: {e}"))?
            }
            Err(e) => return Err(format!("Invalid path: {e}")),
        };

        let original_len = self.indexing.indexed_paths.len();
        self.indexing.indexed_paths.retain(|p| p != &canonical_path);
        self.indexed_paths_cache.retain(|p| p != &canonical_path);

        if self.indexing.indexed_paths.len() == original_len {
            return Err(format!(
                "Path not found in indexed paths: {}",
                path.display()
            ));
        }

        Ok(())
    }

    /// Get all indexed paths
    /// Returns empty vector if none are configured (maintains backward compatibility)
    pub fn get_indexed_paths(&self) -> Vec<PathBuf> {
        self.indexing.indexed_paths.clone()
    }
}

#[cfg(test)]
mod tests {
    use crate::Settings;

    #[test]
    fn remove_indexed_path_drops_a_directory_that_no_longer_exists() {
        let dir = tempfile::tempdir().expect("tempdir");
        let gone = dir.path().join("gone");
        std::fs::create_dir(&gone).expect("create dir");

        let mut settings = Settings::default();
        settings.add_indexed_path(gone.clone()).expect("add");
        std::fs::remove_dir(&gone).expect("delete dir");

        settings
            .remove_indexed_path(&gone)
            .expect("remove after delete");
        assert!(settings.get_indexed_paths().is_empty());
    }

    #[test]
    fn remove_indexed_path_still_rejects_an_unknown_missing_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut settings = Settings::default();

        let err = settings
            .remove_indexed_path(&dir.path().join("never-added"))
            .expect_err("unknown path must be rejected");
        assert!(err.contains("not found"), "{err}");
    }
}
