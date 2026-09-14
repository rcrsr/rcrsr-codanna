//! Integration tests driving the real public `IndexFacade` API over temp
//! index/source directories to verify the fresh-build-and-publish seam
//! (`IndexPersistence::open_build(Fresh)` -> `publish_into_facade`) and
//! full-force reindex provenance semantics.
//!
//! These tests use real `IndexFacade` instances backed by Tantivy indexes
//! on disk in temporary directories — no mocks.

use std::os::unix::fs::MetadataExt;
use std::sync::Arc;

use codanna::config::Settings;
use codanna::indexing::facade::IndexFacade;
use codanna::storage::{BuildMode, IndexLayout, IndexPersistence};

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
// Group 3a: a Fresh build-and-publish zeroes the index
// =============================================================================

#[test]
fn fresh_build_publish_zeroes_symbol_count_and_drops_known_symbol() {
    let temp = tempfile::tempdir().expect("create temp root");
    let source_dir = temp.path().join("src");
    std::fs::create_dir_all(&source_dir).expect("create source dir");
    std::fs::write(
        source_dir.join("known.py"),
        "def known_symbol():\n    pass\n",
    )
    .expect("write known fixture");

    let settings = Arc::new(settings_for(&temp.path().join("index")));
    let mut facade =
        IndexFacade::new(Arc::clone(&settings)).expect("create facade over temp index dir");

    facade
        .index_directory(&source_dir, false)
        .expect("index temp source dir");

    assert!(
        facade.symbol_count() > 0,
        "expected symbols after indexing a directory with a known symbol"
    );
    assert!(
        facade.find_symbol("known_symbol").is_some(),
        "expected known_symbol to resolve before publishing a fresh build"
    );

    // Publish a Fresh build generation over the same index root -- the
    // `IndexPersistence::open_build(Fresh)` -> `publish_into_facade` seam
    // `reindex_locked` drives for a full force reindex -- and rebind
    // `facade` to the resulting warm facade.
    let persistence = IndexPersistence::new(settings.index_path.clone());
    let build = persistence
        .open_build(Arc::clone(&settings), BuildMode::Fresh)
        .expect("open fresh build generation");
    let (_, facade) = persistence
        .publish_into_facade(build)
        .expect("publish fresh build generation");

    assert_eq!(
        facade.symbol_count(),
        0,
        "symbol_count must be zero after publishing a Fresh build generation"
    );
    assert_eq!(
        facade.semantic_search_embedding_count(),
        0,
        "semantic embedding count must be zero after publishing a Fresh build generation (no-op when semantic disabled)"
    );
    assert!(
        facade.find_symbol("known_symbol").is_none(),
        "known_symbol must no longer resolve after publishing a Fresh build generation"
    );
}

/// Guards the publish path on a root that has never had a generation built
/// into it at all: `open_build(Fresh)` followed by `publish_into_facade`
/// must return `Ok(())` rather than erroring on a not-yet-populated index.
#[test]
fn fresh_build_publish_on_never_populated_index_returns_ok() {
    let temp = tempfile::tempdir().expect("create temp root");
    let settings = Arc::new(settings_for(&temp.path().join("index")));

    // No facade / generation has ever been created against this index
    // root -- this exercises the build-and-publish seam on a genuinely
    // never-populated index.
    let persistence = IndexPersistence::new(settings.index_path.clone());
    let build = persistence
        .open_build(Arc::clone(&settings), BuildMode::Fresh)
        .expect("open_build(Fresh) on a never-populated index root must succeed");

    let result = persistence.publish_into_facade(build);
    assert!(
        result.is_ok(),
        "publish_into_facade on a never-populated index root must return Ok(()): {:?}",
        result.err()
    );
    let (_, facade) = result.expect("checked Ok above");
    assert_eq!(
        facade.symbol_count(),
        0,
        "symbol_count remains zero after publishing over a never-populated index root"
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

    let settings = Arc::new(settings_for(&temp.path().join("index")));
    let mut facade =
        IndexFacade::new(Arc::clone(&settings)).expect("create facade over temp index dir");

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

    let d1_only = vec![dir1.canonicalize().expect("canonicalize d1")];

    // Reconfigure indexed_paths to ONLY d1, then run the full-force path:
    // publish a Fresh build generation over the same index root -- the
    // `IndexPersistence::open_build(Fresh)` -> `publish_into_facade` seam
    // `reindex_locked` drives for `paths: None, force: true` -- then reindex
    // over the (now D1-only) indexed_paths set.
    let persistence = IndexPersistence::new(settings.index_path.clone());
    let build = persistence
        .open_build(Arc::clone(&settings), BuildMode::Fresh)
        .expect("open fresh build generation during full-force path");
    let (_, mut facade) = persistence
        .publish_into_facade(build)
        .expect("publish fresh build generation during full-force path");

    // A Fresh build starts with empty indexed_paths tracking, so apply the
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

    // Reference path: the pre-existing facade-level force sequence —
    // publish a Fresh build generation, then reindex the (now-empty) index
    // via `index_directory`. This mirrors what `run_reindex`'s Phase 1
    // (open + publish a Fresh build under lock) + a direct (non-off-lock)
    // Phase 2 reindex would do, and is the semantic definition of
    // "full-force reindex" for the `paths: None` case per
    // `ReindexHandles::run`'s doc comment (force is only meaningful there
    // via a prior fresh-build publish, not via a `force: true` pipeline
    // call).
    let settings_a = Arc::new(settings_for(&temp.path().join("index_a")));
    let mut facade_a = IndexFacade::new(Arc::clone(&settings_a)).expect("create reference facade");
    facade_a
        .index_directory(&source_dir, false)
        .expect("seed reference facade");

    let persistence_a = IndexPersistence::new(settings_a.index_path.clone());
    let build_a = persistence_a
        .open_build(Arc::clone(&settings_a), BuildMode::Fresh)
        .expect("open fresh build generation before reference force reindex");
    let (_, mut facade_a) = persistence_a
        .publish_into_facade(build_a)
        .expect("publish fresh build generation before reference force reindex");

    let stats_a = facade_a
        .index_directory(&source_dir, false)
        .expect("reindex via index_directory after fresh-build publish");
    let symbol_count_a = facade_a.symbol_count();
    for name in &names {
        assert!(
            facade_a.find_symbol(name).is_some(),
            "reference facade must resolve {name} after force index_directory"
        );
    }

    // Off-lock seam: same seed-then-force sequence, but mirroring the actual
    // `run_reindex` Phase 1/Phase 2 split — when `paths` is `None` and
    // `force` is true, the caller opens and publishes a Fresh build
    // generation under lock *before* snapshotting handles, and
    // `ReindexHandles::run` then walks relying on that prior publish (see
    // facade.rs `ReindexHandles::run` doc comment).
    let settings_b = Arc::new(settings_with_indexed_paths(
        &temp.path().join("index_b"),
        vec![source_dir.clone()],
    ));
    let mut facade_b =
        IndexFacade::new(Arc::clone(&settings_b)).expect("create off-lock-seam facade");
    facade_b
        .index_directory(&source_dir, false)
        .expect("seed off-lock-seam facade");

    let persistence_b = IndexPersistence::new(settings_b.index_path.clone());
    let build_b = persistence_b
        .open_build(Arc::clone(&settings_b), BuildMode::Fresh)
        .expect("open fresh build generation before off-lock force reindex (mirrors run_reindex Phase 1)");
    let (_, mut facade_b) = persistence_b
        .publish_into_facade(build_b)
        .expect("publish fresh build generation before off-lock force reindex (mirrors run_reindex Phase 1)");

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

/// Discriminating: driving `ReindexHandles::run` against a facade produced by
/// publishing a Fresh build generation (mirrors the server's Phase 1 open+
/// publish under lock, immediately followed by the off-lock Phase 2 walk)
/// must repopulate the index from scratch rather than leave it empty.
#[test]
fn off_lock_reindex_repopulates_after_fresh_build_publish() {
    let temp = tempfile::tempdir().expect("create temp root");
    let source_dir = temp.path().join("src");
    let names = write_python_fixtures(&source_dir, &["alpha", "beta", "gamma"]);

    let settings = Arc::new(settings_with_indexed_paths(
        &temp.path().join("index"),
        vec![source_dir.clone()],
    ));
    let mut facade = IndexFacade::new(Arc::clone(&settings)).expect("create facade");

    // Seed the index so the fresh-build publish has something to drop.
    facade
        .index_directory(&source_dir, false)
        .expect("seed facade before fresh-build publish");
    assert!(
        facade.symbol_count() > 0,
        "facade must have symbols before fresh-build publish"
    );

    // Mirrors run_reindex's Phase 1 (open + publish a Fresh build under
    // lock, snapshot handles) immediately followed by Phase 2 (off-lock
    // walk).
    let persistence = IndexPersistence::new(settings.index_path.clone());
    let build = persistence
        .open_build(Arc::clone(&settings), BuildMode::Fresh)
        .expect("open fresh build generation before off-lock force reindex");
    let (_, mut facade) = persistence
        .publish_into_facade(build)
        .expect("publish fresh build generation before off-lock force reindex");
    assert_eq!(
        facade.symbol_count(),
        0,
        "symbol_count must be zero immediately after publishing a Fresh build generation"
    );

    let handles = facade
        .snapshot_reindex_handles()
        .expect("snapshot reindex handles after fresh-build publish");
    let outcome = handles
        .run(None, true)
        .expect("off-lock reindex walk must repopulate after fresh-build publish");

    assert!(
        outcome.symbol_count > 0,
        "off-lock reindex must repopulate symbols after fresh-build publish, got {}",
        outcome.symbol_count
    );
    for name in &names {
        assert!(
            facade.find_symbol(name).is_some(),
            "{name} must resolve again after off-lock reindex repopulates a freshly published build"
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

// =============================================================================
// Group 3d: scoped multi-path force reindex on a single registered root
//
// Regression coverage for the single-root `SymbolLookupCache` reuse fast
// path in `Pipeline::index_full`: it must be gated on the number of
// directories being processed in the CURRENT reindex batch, not merely on
// `settings.indexing.indexed_paths.len()`. A single registered root force
// reindexed over several explicit sub-paths in one `ReindexHandles::run`
// call must still resolve cross-directory symbols between those sub-paths.
// =============================================================================

/// Discriminating: a scoped force reindex over two explicit sub-paths of one
/// registered root must resolve a call from a symbol in one sub-path to a
/// symbol imported from the other, even though neither sub-path has ever
/// been indexed together with the other before this batch. A buggy
/// implementation that gates the single-root cache-reuse fast path on
/// `indexed_paths.len() == 1` alone (ignoring how many paths are in the
/// current batch) scopes each walk's symbol cache to only that walk's own
/// files, silently dropping the cross-directory call edge -- moduleB is
/// walked and force-reindexed FIRST so `target_func` is freshly persisted
/// before moduleA's walk, but moduleA's own run-scoped cache never contains
/// `target_func` at all (it isn't defined in moduleA), so only a
/// symbol_cache built from the persisted index (not the fast path's
/// walk-scoped cache) can resolve moduleA's import of it.
#[test]
fn scoped_multi_path_force_reindex_resolves_cross_directory_import() {
    let temp = tempfile::tempdir().expect("create temp root");
    let source_dir = temp.path().join("src");
    let module_a_dir = source_dir.join("moduleA");
    let module_b_dir = source_dir.join("moduleB");
    std::fs::create_dir_all(&module_a_dir).expect("create moduleA dir");
    std::fs::create_dir_all(&module_b_dir).expect("create moduleB dir");

    // moduleB.b defines target_func. Module paths are computed relative to
    // `workspace_root`, so they stay stable (`moduleA.a` / `moduleB.b`)
    // whether the whole tree or just one sub-directory is walked in a given
    // call.
    std::fs::write(module_b_dir.join("b.py"), "def target_func():\n    pass\n")
        .expect("write moduleB/b.py");

    // Single registered root over the whole tree, matching the bug report's
    // `indexed_paths == ["src"]` scenario.
    let mut settings = settings_for(&temp.path().join("index"));
    settings.workspace_root = Some(source_dir.clone());
    settings.indexing.indexed_paths = vec![source_dir.clone()];
    let mut facade = IndexFacade::new(Arc::new(settings)).expect("create facade");

    // Seed ONLY moduleB, so moduleA and moduleB have never been indexed
    // together: the only way `caller_func` (added below, indexed for the
    // first time in the scoped batch) can resolve `target_func` is via
    // symbol data visible during THIS batch's Phase 2, not via any
    // pre-existing cross-directory edge left over from an earlier combined
    // index run.
    facade
        .index_directory(&module_b_dir, false)
        .expect("seed index over moduleB only");
    assert!(
        facade.find_symbol("target_func").is_some(),
        "target_func must resolve after seeding moduleB"
    );

    // moduleA.a is only written now, and only ever indexed via the scoped
    // batch below.
    std::fs::write(
        module_a_dir.join("a.py"),
        "from moduleB.b import target_func\n\ndef caller_func():\n    target_func()\n",
    )
    .expect("write moduleA/a.py");

    // Scoped force reindex over BOTH sub-paths in one batch: this is the
    // MCP `reindex(paths: ["src/moduleA", "src/moduleB"], force: true)`
    // seam (`ReindexHandles::run`'s explicit-paths branch), which walks
    // each path with its own `index_incremental`/`index_full` call.
    let handles = facade
        .snapshot_reindex_handles()
        .expect("snapshot reindex handles");
    let module_a_str = module_a_dir
        .to_str()
        .expect("utf8 moduleA path")
        .to_string();
    let module_b_str = module_b_dir
        .to_str()
        .expect("utf8 moduleB path")
        .to_string();
    // moduleB first, moduleA second: moduleA's own walk-scoped Phase 1
    // never touches b.py at all, so its cache can only ever resolve
    // target_func by seeing the persisted index built up so far in this
    // batch -- which is exactly what the single-root fast path skips.
    handles
        .run(Some(vec![module_b_str, module_a_str]), true)
        .expect("run scoped multi-path force reindex");

    let caller_id = facade
        .find_symbol("caller_func")
        .expect("caller_func must resolve after the scoped force reindex indexes moduleA");

    assert!(
        facade
            .find_symbols_by_name("target_func", None)
            .into_iter()
            .any(|target| {
                facade
                    .get_calling_functions(target.id)
                    .iter()
                    .any(|s| s.id == caller_id)
            }),
        "caller_func (indexed for the first time by this scoped batch) must resolve as a \
         caller of SOME target_func symbol, even though only one root (src) is registered in \
         indexed_paths: gating the single-root cache-reuse fast path on indexed_paths.len() \
         alone (ignoring the current batch's path count) scopes each walk's symbol cache to \
         only that walk's own files, dropping this cross-directory edge entirely"
    );
}

// =============================================================================
// Group 3e: incremental staged reindex (`BuildMode::CloneCurrent`) shares
// Tantivy inodes with the generation it was cloned from
//
// Mirrors the inode-sharing check in
// `tests/cli/test_index_generations_publish.rs`
// (`second_run_shares_tantivy_inodes_and_publishes_a_new_generation`) and the
// equivalence-checking approach of `off_lock_reindex_matches_index_directory_non_force`
// above, applied to the `force: false, paths: None` staged-build path
// (`open_build(CloneCurrent)` -> `ReindexHandles::run` ->
// `publish_into_facade`) that `reindex_locked` drives for a non-force full
// reindex.
// =============================================================================

/// `(file name, inode)` for every `*.store` Tantivy segment file directly
/// under `tantivy_dir`. Tantivy may merge some segments away between two
/// builds (which segments survive unmerged is an implementation detail, not
/// something a test should pin down), so an inode-sharing assertion must
/// check "at least one segment file survived unmerged and shares its
/// inode", not any single named file picked in advance.
fn segment_store_files(tantivy_dir: &std::path::Path) -> Vec<(String, u64)> {
    std::fs::read_dir(tantivy_dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", tantivy_dir.display()))
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            name.ends_with(".store").then_some(name)
        })
        .map(|name| {
            let ino = std::fs::metadata(tantivy_dir.join(&name))
                .unwrap_or_else(|e| panic!("stat {}: {e}", tantivy_dir.join(&name).display()))
                .ino();
            (name, ino)
        })
        .collect()
}

#[test]
fn incremental_staged_reindex_shares_tantivy_inodes_and_matches_oracle_symbol_set() {
    let temp = tempfile::tempdir().expect("create temp root");
    let source_dir = temp.path().join("src");
    let names = write_python_fixtures(&source_dir, &["alpha", "beta", "gamma"]);

    let settings = Arc::new(settings_with_indexed_paths(
        &temp.path().join("index"),
        vec![source_dir.clone()],
    ));
    let mut facade = IndexFacade::new(Arc::clone(&settings)).expect("create facade");
    facade
        .index_directory(&source_dir, false)
        .expect("seed facade with the parent generation");
    let parent_generation = facade.generation_id().clone();

    let layout = IndexLayout::new(settings.index_path.clone());
    let parent_segments = segment_store_files(&layout.tantivy_dir(&parent_generation));
    assert!(
        !parent_segments.is_empty(),
        "seeded parent generation must have at least one Tantivy segment"
    );

    // Ground-truth oracle: index the same fixture in-place (no generation
    // staging at all) via `index_directory`, independent of the staged-build
    // seam under test.
    let oracle_settings = Arc::new(settings_for(&temp.path().join("oracle_index")));
    let mut oracle_facade = IndexFacade::new(oracle_settings).expect("create oracle facade");
    oracle_facade
        .index_directory(&source_dir, false)
        .expect("index fixture directory for the oracle facade");
    let expected_symbol_count = oracle_facade.symbol_count();
    assert!(
        expected_symbol_count > 0,
        "oracle facade must produce a non-zero symbol count"
    );

    // Incremental staged reindex: force:false, paths:None -> a
    // `BuildMode::CloneCurrent` build, hardlinking the parent generation's
    // Tantivy segments rather than rebuilding from scratch, then the
    // off-lock walk seam, then publish.
    let persistence = IndexPersistence::new(settings.index_path.clone());
    let mut build = persistence
        .open_build(Arc::clone(&settings), BuildMode::CloneCurrent)
        .expect("open clone-current build");
    let handles = build
        .snapshot_reindex_handles()
        .expect("snapshot reindex handles from the clone-current build facade");
    handles
        .run(None, false)
        .expect("run incremental staged reindex walk");

    let (_, published_facade) = persistence
        .publish_into_facade(build)
        .expect("publish the clone-current build generation");

    assert_eq!(
        published_facade.symbol_count(),
        expected_symbol_count,
        "incremental staged reindex must produce the same symbol set as the in-place oracle facade"
    );
    for name in &names {
        assert!(
            published_facade.find_symbol(name).is_some(),
            "{name} must resolve after the incremental staged reindex"
        );
    }

    let staged_generation = published_facade.generation_id().clone();
    assert_ne!(
        parent_generation, staged_generation,
        "CloneCurrent must publish a generation distinct from its parent"
    );

    let staged_tantivy_dir = layout.tantivy_dir(&staged_generation);
    let shared_unmerged_segment = parent_segments.iter().find_map(|(name, parent_ino)| {
        let staged_meta = std::fs::metadata(staged_tantivy_dir.join(name)).ok()?;
        (staged_meta.ino() == *parent_ino).then_some((name.clone(), staged_meta.nlink()))
    });
    let (shared_name, shared_nlink) = shared_unmerged_segment.unwrap_or_else(|| {
        panic!(
            "expected at least one of the parent generation's segment files \
             ({parent_segments:?}) to survive unmerged and share its inode with the staged \
             generation's clone under {}",
            staged_tantivy_dir.display()
        )
    });
    assert!(
        shared_nlink >= 2,
        "segment file {shared_name} shared between the parent and staged generations must have \
         nlink >= 2, got {shared_nlink}"
    );
}
