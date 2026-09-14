//! Integration tests for tracked `indexed_paths` hygiene: out-of-tree roots
//! stay indexable (upstream supports indexing a project from outside its
//! directory, so `workspace_root` is not a containment boundary), and
//! `codanna index --prune-indexed-paths` drops ghost and stray entries from
//! the current generation's `index.meta` while keeping roots the user
//! configured deliberately.
//!
//! These tests use real `IndexFacade`/`IndexPersistence` instances backed
//! by Tantivy indexes on disk in temporary directories -- no mocks.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use codanna::cli::commands::index::run_prune_indexed_paths;
use codanna::config::Settings;
use codanna::indexing::facade::IndexFacade;
use codanna::storage::IndexPersistence;

fn settings_for(index_dir: &Path, workspace_root: Option<PathBuf>) -> Settings {
    Settings {
        index_path: index_dir.to_path_buf(),
        workspace_root,
        ..Default::default()
    }
}

fn write_py_fixture(dir: &Path, name: &str, symbol: &str) {
    std::fs::create_dir_all(dir).expect("create fixture dir");
    std::fs::write(dir.join(name), format!("def {symbol}():\n    pass\n")).expect("write fixture");
}

fn recorded_indexed_paths(persistence: &IndexPersistence) -> HashSet<PathBuf> {
    persistence
        .current_metadata()
        .expect("index.meta must exist")
        .indexed_paths
        .expect("indexed_paths must be recorded in index.meta")
        .into_iter()
        .collect()
}

#[test]
fn out_of_tree_directory_is_indexed_even_with_workspace_root_set() {
    let temp = tempfile::tempdir().expect("create temp root");

    let workspace_root = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace_root).expect("create workspace root dir");

    let corpus = temp.path().join("corpus");
    write_py_fixture(&corpus, "outside.py", "outside_symbol");

    let settings = Arc::new(settings_for(
        &temp.path().join("index"),
        Some(workspace_root),
    ));
    let mut facade = IndexFacade::new(Arc::clone(&settings)).expect("create facade");

    facade
        .index_directory(&corpus, false)
        .expect("indexing a directory outside workspace_root must succeed (out-of-tree indexing)");

    assert!(
        facade.find_symbol("outside_symbol").is_some(),
        "an out-of-tree directory must be walked/indexed normally"
    );
    let canonical_corpus = corpus.canonicalize().expect("canonicalize corpus");
    assert!(
        facade.get_indexed_paths().contains(&canonical_corpus),
        "an out-of-tree root must be tracked in indexed_paths"
    );
}

#[test]
fn prune_drops_ghost_and_stray_entries_but_keeps_configured_out_of_tree_roots() {
    let temp = tempfile::tempdir().expect("create temp root");

    let workspace_root = temp.path().join("workspace");
    let in_root = workspace_root.join("src");
    write_py_fixture(&in_root, "valid.py", "valid_symbol");

    // Out-of-tree, but listed in settings.toml: deliberate, must survive.
    let configured_out_of_tree = temp.path().join("configured-corpus");
    std::fs::create_dir_all(&configured_out_of_tree).expect("create configured corpus");

    // Out-of-tree and NOT listed in settings.toml: a stray root, pruned.
    let stray = temp.path().join("stray-scratch");
    std::fs::create_dir_all(&stray).expect("create stray dir");

    // Tracked but gone from disk: a ghost, pruned.
    let ghost = temp.path().join("ghost-was-here");

    let mut settings = settings_for(&temp.path().join("index"), Some(workspace_root.clone()));
    settings.indexing.indexed_paths = vec![
        in_root.canonicalize().expect("canonicalize in-root dir"),
        configured_out_of_tree
            .canonicalize()
            .expect("canonicalize configured corpus"),
    ];
    let settings = Arc::new(settings);

    let mut facade = IndexFacade::new(Arc::clone(&settings)).expect("create facade");
    let mut tracked = settings.indexing.indexed_paths.clone();
    tracked.push(stray.clone());
    tracked.push(ghost.clone());
    facade.set_indexed_paths(tracked);
    facade
        .index_directory(&in_root, false)
        .expect("seed index with the in-root directory's symbols");

    let persistence = IndexPersistence::new(settings.index_path.clone());
    persistence
        .save_facade(&facade)
        .expect("save seeded facade");
    drop(facade);

    let before = recorded_indexed_paths(&persistence);
    assert!(
        before.contains(&stray),
        "precondition: stray entry recorded"
    );
    assert!(
        before.contains(&ghost),
        "precondition: ghost entry recorded"
    );
    assert_eq!(before.len(), 4, "precondition: all four entries recorded");

    run_prune_indexed_paths(&settings);

    let after = recorded_indexed_paths(&persistence);
    assert!(!after.contains(&stray), "stray entry must be pruned");
    assert!(!after.contains(&ghost), "ghost entry must be pruned");
    assert!(
        after.contains(&in_root.canonicalize().expect("canonicalize in-root dir")),
        "in-root entry must survive pruning"
    );
    assert!(
        after.contains(
            &configured_out_of_tree
                .canonicalize()
                .expect("canonicalize configured corpus")
        ),
        "a configured out-of-tree root must survive pruning"
    );
    assert_eq!(after.len(), 2);
}

#[test]
fn prune_saves_through_the_current_checked_path_with_no_concurrent_publish() {
    // End-to-end confirmation that routing the prune save through
    // `save_facade_current_checked` (instead of a bare `save_facade`) does
    // not change observable behavior when nothing else publishes
    // concurrently: the pruned set still lands in `index.meta`. The actual
    // CAS-refusal race is exercised at the `persistence.rs` unit level,
    // where a concurrent publish can be injected out of band.
    let temp = tempfile::tempdir().expect("create temp root");

    let corpus = temp.path().join("corpus");
    write_py_fixture(&corpus, "valid.py", "valid_symbol");
    let ghost = temp.path().join("ghost-was-here");

    let settings = Arc::new(settings_for(&temp.path().join("index"), None));
    let mut facade = IndexFacade::new(Arc::clone(&settings)).expect("create facade");
    facade.set_indexed_paths(vec![
        corpus.canonicalize().expect("canonicalize corpus"),
        ghost.clone(),
    ]);
    facade.index_directory(&corpus, false).expect("seed index");

    let persistence = IndexPersistence::new(settings.index_path.clone());
    persistence
        .save_facade(&facade)
        .expect("save seeded facade");
    drop(facade);

    run_prune_indexed_paths(&settings);

    let after = recorded_indexed_paths(&persistence);
    assert!(
        !after.contains(&ghost),
        "ghost entry must still be pruned via the CAS-guarded save path"
    );
    assert!(after.contains(&corpus.canonicalize().expect("canonicalize corpus")));
    assert_eq!(after.len(), 1);
}

#[test]
fn prune_without_workspace_root_drops_only_ghosts() {
    let temp = tempfile::tempdir().expect("create temp root");

    let corpus = temp.path().join("corpus");
    write_py_fixture(&corpus, "valid.py", "valid_symbol");
    let unlisted_but_present = temp.path().join("elsewhere");
    std::fs::create_dir_all(&unlisted_but_present).expect("create elsewhere dir");
    let ghost = temp.path().join("ghost-was-here");

    let settings = Arc::new(settings_for(&temp.path().join("index"), None));
    let mut facade = IndexFacade::new(Arc::clone(&settings)).expect("create facade");
    facade.set_indexed_paths(vec![
        corpus.canonicalize().expect("canonicalize corpus"),
        unlisted_but_present.clone(),
        ghost.clone(),
    ]);
    facade.index_directory(&corpus, false).expect("seed index");

    let persistence = IndexPersistence::new(settings.index_path.clone());
    persistence
        .save_facade(&facade)
        .expect("save seeded facade");
    drop(facade);

    run_prune_indexed_paths(&settings);

    let after = recorded_indexed_paths(&persistence);
    assert!(!after.contains(&ghost), "ghost entry must be pruned");
    assert!(
        after.contains(&unlisted_but_present),
        "with no workspace_root there is no stray criterion; existing dirs survive"
    );
    assert_eq!(after.len(), 2);
}
