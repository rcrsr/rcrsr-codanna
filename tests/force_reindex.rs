//! Integration tests driving the real public `IndexFacade` API over temp
//! index/source directories to verify `clear_index()` and full-force
//! reindex provenance semantics.
//!
//! These tests use real `IndexFacade` instances backed by Tantivy indexes
//! on disk in temporary directories — no mocks.

use std::sync::Arc;

use codanna::config::Settings;
use codanna::indexing::facade::IndexFacade;

/// Write a small set of Python fixture files into `dir`, each defining one
/// module-level function whose name is derived from the file stem. Returns
/// the sorted list of defined function names.
fn write_python_fixtures(dir: &std::path::Path, names: &[&str]) -> Vec<String> {
    std::fs::create_dir_all(dir).expect("create fixture dir");
    for name in names {
        std::fs::write(
            dir.join(format!("{name}.py")),
            format!("def {name}():\n    pass\n"),
        )
        .unwrap_or_else(|e| panic!("write {name}.py fixture: {e}"));
    }
    let mut sorted: Vec<String> = names.iter().map(|s| s.to_string()).collect();
    sorted.sort();
    sorted
}

/// Build a `Settings` value rooted at a fresh temp index directory.
fn settings_for(index_dir: &std::path::Path) -> Settings {
    Settings {
        index_path: index_dir.to_path_buf(),
        workspace_root: None,
        ..Default::default()
    }
}

// =============================================================================
// Group 3a: clear_index() zeroes the index
// =============================================================================

#[test]
fn clear_index_zeroes_symbol_count_and_drops_known_symbol() {
    let temp = tempfile::tempdir().expect("create temp root");
    let source_dir = temp.path().join("src");
    std::fs::create_dir_all(&source_dir).expect("create source dir");
    std::fs::write(
        source_dir.join("known.py"),
        "def known_symbol():\n    pass\n",
    )
    .expect("write known fixture");

    let settings = settings_for(&temp.path().join("index"));
    let mut facade =
        IndexFacade::new(Arc::new(settings)).expect("create facade over temp index dir");

    facade
        .index_directory(&source_dir, false)
        .expect("index temp source dir");

    assert!(
        facade.symbol_count() > 0,
        "expected symbols after indexing a directory with a known symbol"
    );
    assert!(
        facade.find_symbol("known_symbol").is_some(),
        "expected known_symbol to resolve before clear_index"
    );

    facade.clear_index().expect("clear_index should succeed");

    assert_eq!(
        facade.symbol_count(),
        0,
        "symbol_count must be zero after clear_index"
    );
    assert_eq!(
        facade.semantic_search_embedding_count(),
        0,
        "semantic embedding count must be zero after clear_index (no-op when semantic disabled)"
    );
    assert!(
        facade.find_symbol("known_symbol").is_none(),
        "known_symbol must no longer resolve after clear_index"
    );
}

/// Guards the writer.rs early-return path: calling `clear_index()` on a
/// facade that has never had anything indexed into it must return `Ok(())`
/// rather than erroring on a not-yet-populated index.
#[test]
fn clear_index_on_never_populated_index_returns_ok() {
    let temp = tempfile::tempdir().expect("create temp root");
    let settings = settings_for(&temp.path().join("index"));
    let mut facade =
        IndexFacade::new(Arc::new(settings)).expect("create facade over fresh temp index dir");

    // No index_file / index_directory call has ever happened against this
    // facade — this exercises the early-return guard for an index that has
    // never been populated.
    assert_eq!(facade.symbol_count(), 0, "fresh index has no symbols");

    let result = facade.clear_index();
    assert!(
        result.is_ok(),
        "clear_index on a never-initialized index must return Ok(()): {result:?}"
    );
    assert_eq!(
        facade.symbol_count(),
        0,
        "symbol_count remains zero after clearing a never-populated index"
    );
}

// =============================================================================
// Group 3b: force provenance / discriminating (defeats dead-clear and
// re-parse-only wrong implementations)
// =============================================================================

#[test]
fn full_force_reindex_drops_symbols_from_deconfigured_directory() {
    let temp = tempfile::tempdir().expect("create temp root");

    let dir1 = temp.path().join("d1");
    let dir2 = temp.path().join("d2");
    std::fs::create_dir_all(&dir1).expect("create d1");
    std::fs::create_dir_all(&dir2).expect("create d2");

    std::fs::write(dir1.join("alpha.py"), "def alpha():\n    pass\n").expect("write alpha");
    std::fs::write(dir2.join("beta.py"), "def beta():\n    pass\n").expect("write beta");

    let settings = settings_for(&temp.path().join("index"));
    let mut facade =
        IndexFacade::new(Arc::new(settings)).expect("create facade over temp index dir");

    // Index both directories into ONE index; both become tracked indexed
    // paths.
    facade
        .index_directory(&dir1, false)
        .expect("index d1 (alpha)");
    facade
        .index_directory(&dir2, false)
        .expect("index d2 (beta)");

    assert_eq!(
        facade.get_indexed_paths().len(),
        2,
        "both d1 and d2 must be tracked as indexed paths"
    );
    assert!(
        facade.find_symbol("alpha").is_some(),
        "alpha must resolve after indexing d1"
    );
    assert!(
        facade.find_symbol("beta").is_some(),
        "beta must resolve after indexing d2"
    );

    // Reconfigure indexed_paths to ONLY d1, then run the full-force path:
    // clear_index() followed by reindexing over the (now D1-only)
    // indexed_paths set.
    let d1_only = vec![dir1.canonicalize().expect("canonicalize d1")];
    facade.set_indexed_paths(d1_only.clone());

    facade
        .clear_index()
        .expect("clear_index during full-force path");

    // clear_index() resets indexed_paths tracking, so re-apply the
    // D1-only configuration before reindexing over it.
    facade.set_indexed_paths(d1_only.clone());

    for path in &d1_only {
        facade
            .index_directory(path, true)
            .expect("force reindex over D1-only indexed_paths");
    }

    // KEY ASSERTIONS: alpha is rebuilt (still present after force
    // reindexing D1), while beta — a symbol from a directory no longer
    // present in indexed_paths — is gone. This defeats both a dead-clear
    // implementation (which would leave beta present) and a
    // re-parse-only implementation that never clears (same failure mode).
    assert!(
        facade.find_symbol("alpha").is_some(),
        "alpha must be rebuilt after force reindex over D1-only indexed_paths"
    );
    assert!(
        facade.find_symbol("beta").is_none(),
        "beta must be gone: it belongs to a directory no longer in indexed_paths after full-force reindex"
    );
}

// =============================================================================
// Group 3c: off-lock reindex seam equivalence
//
// `snapshot_reindex_handles()` + `ReindexHandles::run(...)` is the seam the
// MCP server drives with no facade lock held (src/mcp/server.rs
// `run_reindex`). These tests assert it produces the same outcome as the
// pre-existing `index_directory` path when driven directly against the same
// fixture, for both non-force and force runs.
// =============================================================================

/// Build a `Settings` value rooted at a fresh temp index directory with
/// `indexing.indexed_paths` pre-populated, mirroring what `ReindexHandles::run`
/// reads when invoked with `paths: None` (the server's default-reindex path).
fn settings_with_indexed_paths(
    index_dir: &std::path::Path,
    indexed_paths: Vec<std::path::PathBuf>,
) -> Settings {
    let mut settings = settings_for(index_dir);
    settings.indexing.indexed_paths = indexed_paths;
    settings
}

#[test]
fn off_lock_reindex_matches_index_directory_non_force() {
    let temp = tempfile::tempdir().expect("create temp root");
    let source_dir = temp.path().join("src");
    let names = write_python_fixtures(&source_dir, &["alpha", "beta", "gamma"]);

    // Reference path: index_directory driven directly.
    let settings_a = settings_for(&temp.path().join("index_a"));
    let mut facade_a = IndexFacade::new(Arc::new(settings_a)).expect("create reference facade");
    let stats_a = facade_a
        .index_directory(&source_dir, false)
        .expect("index via index_directory");
    let symbol_count_a = facade_a.symbol_count();
    for name in &names {
        assert!(
            facade_a.find_symbol(name).is_some(),
            "reference facade must resolve {name} after index_directory"
        );
    }

    // Off-lock seam: snapshot_reindex_handles() + ReindexHandles::run(None, false).
    let settings_b =
        settings_with_indexed_paths(&temp.path().join("index_b"), vec![source_dir.clone()]);
    let mut facade_b = IndexFacade::new(Arc::new(settings_b)).expect("create off-lock-seam facade");
    let handles = facade_b
        .snapshot_reindex_handles()
        .expect("snapshot reindex handles");
    let outcome = handles
        .run(None, false)
        .expect("run off-lock reindex walk (non-force)");

    assert_eq!(
        outcome.reindexed, stats_a.files_indexed,
        "off-lock seam must reindex the same file count as index_directory"
    );
    assert_eq!(
        outcome.symbol_count, symbol_count_a,
        "off-lock seam must produce the same symbol_count as index_directory"
    );
    for name in &names {
        assert!(
            facade_b.find_symbol(name).is_some(),
            "off-lock-seam facade must resolve {name} after ReindexHandles::run"
        );
    }
}

#[test]
fn off_lock_reindex_matches_index_directory_force() {
    let temp = tempfile::tempdir().expect("create temp root");
    let source_dir = temp.path().join("src");
    let names = write_python_fixtures(&source_dir, &["alpha", "beta", "gamma"]);

    // Reference path: the pre-existing facade-level force sequence — clear
    // the index, then reindex the (now-empty) index via `index_directory`.
    // This mirrors what `run_reindex`'s Phase 1 (clear under lock) + a
    // direct (non-off-lock) Phase 2 reindex would do, and is the semantic
    // definition of "full-force reindex" for the `paths: None` case per
    // `ReindexHandles::run`'s doc comment (force is only meaningful there
    // via a prior clear, not via a `force: true` pipeline call).
    let settings_a = settings_for(&temp.path().join("index_a"));
    let mut facade_a = IndexFacade::new(Arc::new(settings_a)).expect("create reference facade");
    facade_a
        .index_directory(&source_dir, false)
        .expect("seed reference facade");
    facade_a
        .clear_index()
        .expect("clear_index before reference force reindex");
    let stats_a = facade_a
        .index_directory(&source_dir, false)
        .expect("reindex via index_directory after clear");
    let symbol_count_a = facade_a.symbol_count();
    for name in &names {
        assert!(
            facade_a.find_symbol(name).is_some(),
            "reference facade must resolve {name} after force index_directory"
        );
    }

    // Off-lock seam: same seed-then-force sequence, but mirroring the actual
    // `run_reindex` Phase 1/Phase 2 split — when `paths` is `None` and
    // `force` is true, the caller clears the index under lock *before*
    // snapshotting handles, and `ReindexHandles::run` then walks relying on
    // that prior clear (see facade.rs `ReindexHandles::run` doc comment).
    let settings_b =
        settings_with_indexed_paths(&temp.path().join("index_b"), vec![source_dir.clone()]);
    let mut facade_b = IndexFacade::new(Arc::new(settings_b)).expect("create off-lock-seam facade");
    facade_b
        .index_directory(&source_dir, false)
        .expect("seed off-lock-seam facade");
    facade_b
        .clear_index()
        .expect("clear_index before off-lock force reindex (mirrors run_reindex Phase 1)");
    let handles = facade_b
        .snapshot_reindex_handles()
        .expect("snapshot reindex handles");
    let outcome = handles
        .run(None, true)
        .expect("run off-lock reindex walk (force)");

    assert_eq!(
        outcome.reindexed, stats_a.files_indexed,
        "off-lock seam must reindex the same file count as force index_directory"
    );
    assert_eq!(
        outcome.symbol_count, symbol_count_a,
        "off-lock seam must produce the same symbol_count as force index_directory"
    );
    for name in &names {
        assert!(
            facade_b.find_symbol(name).is_some(),
            "off-lock-seam facade must resolve {name} after force ReindexHandles::run"
        );
    }
}

/// Discriminating: driving `ReindexHandles::run` against a facade whose
/// index was just cleared (mirrors the server's Phase 1 `clear_index()` under
/// lock, immediately followed by the off-lock Phase 2 walk) must repopulate
/// the index from scratch rather than leave it empty.
#[test]
fn off_lock_reindex_repopulates_after_clear_index() {
    let temp = tempfile::tempdir().expect("create temp root");
    let source_dir = temp.path().join("src");
    let names = write_python_fixtures(&source_dir, &["alpha", "beta", "gamma"]);

    let settings =
        settings_with_indexed_paths(&temp.path().join("index"), vec![source_dir.clone()]);
    let mut facade = IndexFacade::new(Arc::new(settings)).expect("create facade");

    // Seed the index so clear_index() has something to drop.
    facade
        .index_directory(&source_dir, false)
        .expect("seed facade before clear");
    assert!(
        facade.symbol_count() > 0,
        "facade must have symbols before clear"
    );

    // Mirrors run_reindex's Phase 1 (clear under lock, snapshot handles)
    // immediately followed by Phase 2 (off-lock walk).
    facade
        .clear_index()
        .expect("clear_index before off-lock force reindex");
    assert_eq!(
        facade.symbol_count(),
        0,
        "symbol_count must be zero immediately after clear_index"
    );

    let handles = facade
        .snapshot_reindex_handles()
        .expect("snapshot reindex handles after clear");
    let outcome = handles
        .run(None, true)
        .expect("off-lock reindex walk must repopulate after clear_index");

    assert!(
        outcome.symbol_count > 0,
        "off-lock reindex must repopulate symbols after clear_index, got {}",
        outcome.symbol_count
    );
    for name in &names {
        assert!(
            facade.find_symbol(name).is_some(),
            "{name} must resolve again after off-lock reindex repopulates a cleared index"
        );
    }
}

/// Discriminating: `force: true` against a single explicit FILE path must
/// bypass the unchanged-content-hash skip in `Pipeline::index_file_single`,
/// not silently no-op. A file's content hash is unchanged between the seed
/// index and the forced reindex (so a non-force call would hit the
/// `SingleFileStats { cached: true, .. }` early return), which is exactly
/// the scenario a naive `force`-dropping implementation would still pass a
/// weaker "reindexed count > 0" assertion for, since `ReindexHandles::run`
/// counts any successfully processed explicit file path as reindexed
/// regardless of cache status. Instead this asserts the file's `FileId` is
/// reassigned, which can only happen if the force path actually removed and
/// re-inserted the file's index records rather than hitting the cached
/// early return.
#[test]
fn off_lock_reindex_force_bypasses_hash_skip_for_unchanged_file() {
    let temp = tempfile::tempdir().expect("create temp root");
    let source_dir = temp.path().join("src");
    std::fs::create_dir_all(&source_dir).expect("create source dir");
    let file_path = source_dir.join("alpha.py");
    std::fs::write(&file_path, "def alpha():\n    pass\n").expect("write alpha fixture");

    let settings = settings_for(&temp.path().join("index"));
    let mut facade = IndexFacade::new(Arc::new(settings)).expect("create facade");

    // Seed the index with the file via the normal single-file path.
    facade
        .index_file(&file_path)
        .expect("seed index with alpha.py");
    assert!(
        facade.find_symbol("alpha").is_some(),
        "alpha must resolve after the initial seed index"
    );

    let path_str = file_path.to_str().expect("utf8 path");
    let (original_file_id, original_hash, _mtime) = facade
        .document_index()
        .get_file_info(path_str)
        .expect("query file info after seed index")
        .expect("alpha.py must be tracked in the index after seed index");

    // File content is deliberately left unchanged, so a non-force reindex
    // (or a force reindex that silently drops `force`) would hit the
    // unchanged-hash `cached: true` early return in `index_file_single`.
    let handles = facade
        .snapshot_reindex_handles()
        .expect("snapshot reindex handles");
    let outcome = handles
        .run(Some(vec![path_str.to_string()]), true)
        .expect("run off-lock force reindex over the explicit file path");

    assert_eq!(
        outcome.reindexed, 1,
        "force reindex over a single explicit file path must count it as reindexed"
    );
    assert!(
        facade.find_symbol("alpha").is_some(),
        "alpha must still resolve after force reindex of the unchanged file"
    );

    let (new_file_id, new_hash, _mtime) = facade
        .document_index()
        .get_file_info(path_str)
        .expect("query file info after force reindex")
        .expect("alpha.py must still be tracked in the index after force reindex");

    assert_eq!(
        new_hash, original_hash,
        "file content (and therefore its hash) must be unchanged by this test"
    );
    assert_ne!(
        new_file_id, original_file_id,
        "force reindex of an explicit file path with an unchanged hash must remove and \
         re-insert the file's index records (yielding a new FileId) rather than silently \
         hitting the unchanged-hash cache skip"
    );
}
