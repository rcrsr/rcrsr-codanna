//! Integration test for the "startup GC" behavior added to the three serve
//! entry points (stdio in `src/cli/commands/serve.rs`, HTTP in
//! `src/mcp/http_server.rs`, HTTPS in `src/mcp/https_server.rs`): once per
//! process start, immediately after the first successful facade load, each
//! site calls `gc_logged(&layout, true, "startup")` to reclaim stale
//! generations left behind by a prior run.
//!
//! This test does not exercise a specific serve entry point directly (they
//! are CLI/network-bound and covered by the mechanical drift guard in
//! `tests/serve_watcher_wiring_tests.rs` instead). It drives the same
//! sequence those sites drive -- load a facade via `IndexFacade::new`, then
//! call `gc_logged(&layout, true, "startup")` -- over a real on-disk index
//! root seeded with an orphan generation, and asserts GC actually reclaims
//! it.
//!
//! Uses a real `IndexFacade`/`IndexLayout` backed by a Tantivy index on disk
//! in a temporary directory -- no mocks.

use std::sync::Arc;

use codanna::config::Settings;
use codanna::indexing::facade::IndexFacade;
use codanna::storage::IndexLayout;
use codanna::storage::generation::{GenerationId, gc_logged};

fn settings_for(index_dir: &std::path::Path) -> Settings {
    Settings {
        index_path: index_dir.to_path_buf(),
        workspace_root: None,
        ..Default::default()
    }
}

/// A generation dir with no `BUILDING` marker, no `COMPLETE` manifest, and
/// not named by `current`: classifies as `GenerationState::Orphan`, exactly
/// as `storage/generation/gc.rs`'s own `write_orphan` test fixture does.
/// Mirrors an interrupted build that never finished and never got cleaned
/// up (e.g. the process was killed mid-build, well before it acquired a
/// `BUILDING` marker or before an earlier, now-dead builder's marker was
/// itself reclassified).
fn write_orphan(layout: &IndexLayout) -> GenerationId {
    let id = GenerationId::generate();
    std::fs::create_dir_all(layout.gen_dir(&id)).expect("create orphan generation dir");
    id
}

#[test]
fn startup_gc_reclaims_an_orphan_generation_present_at_process_start() {
    let temp = tempfile::tempdir().expect("create temp root");
    let settings = Arc::new(settings_for(&temp.path().join("index")));

    // Mirror each serve entry point's first step: load a facade via the
    // normal startup path. A fresh root bootstraps its own valid `current`
    // generation, exactly like a first-ever `codanna serve` invocation would.
    let facade = IndexFacade::new(Arc::clone(&settings)).expect("startup facade load must succeed");

    let layout = IndexLayout::new(settings.index_path.clone());

    // Seed an orphan generation left behind by some prior, never-completed
    // build, independent of the generation the facade above just bootstrapped.
    let orphan = write_orphan(&layout);
    assert!(
        layout.gen_dir(&orphan).exists(),
        "orphan fixture must exist on disk before GC runs"
    );

    // Mirror each serve entry point's next step: run the once-per-startup GC
    // pass immediately after the facade load, exactly as
    // `src/cli/commands/serve.rs`, `src/mcp/http_server.rs`, and
    // `src/mcp/https_server.rs` do.
    let summary =
        gc_logged(&layout, true, "startup").expect("startup gc_logged run must not error");

    assert!(
        summary.removed.contains(&orphan),
        "gc_logged's summary must report the orphan generation as removed"
    );
    assert!(
        !layout.gen_dir(&orphan).exists(),
        "the orphan generation directory must be gone from disk after startup gc_logged runs"
    );

    // The facade's own (valid, current) generation must be untouched by the
    // startup GC pass -- only the orphan is stale.
    assert!(
        layout.gen_dir(facade.generation_id()).is_dir(),
        "the facade's current generation must survive startup gc_logged"
    );
}
