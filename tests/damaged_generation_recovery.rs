//! Integration test for the damaged-generation-current recovery UX: when
//! `current` names a generation that fails validation, `IndexFacade::new`'s
//! normal startup path must roll back to the newest valid generation on
//! disk, keep serving that generation's data, and surface which generation
//! it recovered from via `recovered_from()` / `index_info_data`'s
//! `generation.recovered_from` field.
//!
//! Uses real `IndexFacade`/`IndexPersistence` instances backed by Tantivy
//! indexes on disk in a temporary directory -- no mocks.

use std::sync::Arc;

use codanna::config::Settings;
use codanna::indexing::facade::IndexFacade;
use codanna::mcp::service::index_info_data;
use codanna::storage::{BuildMode, IndexLayout, IndexPersistence};

fn settings_for(index_dir: &std::path::Path) -> Settings {
    Settings {
        index_path: index_dir.to_path_buf(),
        workspace_root: None,
        ..Default::default()
    }
}

#[test]
fn startup_recovers_from_a_damaged_current_generation_and_reports_recovered_from() {
    let temp = tempfile::tempdir().expect("create temp root");
    let source_dir = temp.path().join("src");
    std::fs::create_dir_all(&source_dir).expect("create source dir");
    std::fs::write(
        source_dir.join("known.py"),
        "def known_symbol():\n    pass\n",
    )
    .expect("write known fixture");

    let settings = Arc::new(settings_for(&temp.path().join("index")));

    // Generation A: bootstrap-empty, then populated in place via
    // index_directory -- this is the valid generation recovery must fall
    // back to.
    let mut facade =
        IndexFacade::new(Arc::clone(&settings)).expect("create facade over temp index dir");
    facade
        .index_directory(&source_dir, false)
        .expect("index temp source dir into generation A");
    let generation_a = facade.generation_id().clone();
    assert!(
        facade.find_symbol("known_symbol").is_some(),
        "known_symbol must resolve after seeding generation A"
    );

    // Generation B: a CloneCurrent build published over A, becoming the new
    // `current`. It starts out identical to A (cloned, not re-indexed), so
    // it carries the same symbol data.
    let persistence = IndexPersistence::new(settings.index_path.clone());
    let build = persistence
        .open_build(Arc::clone(&settings), BuildMode::CloneCurrent)
        .expect("open clone-current build seeded from generation A");
    let (generation_b, _published_facade) = persistence
        .publish_into_facade(build)
        .expect("publish clone-current build as generation B");
    assert_ne!(
        generation_a, generation_b,
        "CloneCurrent must publish a generation distinct from its parent"
    );

    // Damage generation B's on-disk index.meta so it fails validation, while
    // `current` still names it -- the exact "current names a specific
    // generation that fails validation" recovery path.
    let layout = IndexLayout::new(settings.index_path.clone());
    std::fs::write(
        layout.gen_dir(&generation_b).join("index.meta"),
        b"{ not json",
    )
    .expect("corrupt generation B's index.meta to force validation failure");

    // Load a facade via the normal startup path: IndexFacade::new must
    // recover past the damaged current generation (B) and fall back to the
    // newest valid one (A).
    let recovered_facade =
        IndexFacade::new(Arc::clone(&settings)).expect("startup path must recover from damage");

    assert_eq!(
        recovered_facade.generation_id(),
        &generation_a,
        "recovery must roll `current` back to generation A, the newest valid generation"
    );
    assert_eq!(
        recovered_facade.recovered_from(),
        Some(&generation_b),
        "facade must record which generation it recovered from"
    );
    assert!(
        recovered_facade.find_symbol("known_symbol").is_some(),
        "the recovered facade must still serve generation A's (valid) data correctly"
    );

    let info = index_info_data(&recovered_facade);
    assert_eq!(
        info.generation.id,
        generation_a.as_str(),
        "reported generation id must be the recovered-to generation (A)"
    );
    assert_eq!(
        info.generation.recovered_from,
        Some(generation_b.as_str().to_string()),
        "index_info_data must surface recovered_from = Some(damaged generation id)"
    );
}
