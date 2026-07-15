//! Integration tests driving the real public `IndexFacade` API over temp
//! index/source directories to verify `clear_index()` and full-force
//! reindex provenance semantics.
//!
//! These tests use real `IndexFacade` instances backed by Tantivy indexes
//! on disk in temporary directories — no mocks.

use std::sync::Arc;

use codanna::config::Settings;
use codanna::indexing::facade::IndexFacade;

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
