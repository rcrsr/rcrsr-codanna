//! Mechanical drift guard for the three hand-maintained serve-builder call
//! sites that must wire `FileWatchConfig::startup_catch_up` into
//! `UnifiedWatcherBuilder`.
//!
//! §CDNA.7 anti-pattern row 1 ("update every touch point in the same
//! change") applies here exactly as it does to a new MCP tool or a new
//! language registration: the compiler has no way to notice a missing
//! `.startup_catch_up(...)` call on one of the three unified-watcher builder
//! chains (`src/cli/commands/serve.rs`, `src/mcp/http_server.rs`,
//! `src/mcp/https_server.rs`) -- each site independently constructs its own
//! `UnifiedWatcher::builder()` chain, so an omission compiles cleanly and
//! only shows up as one serve mode silently never arming the startup
//! catch-up reindex. This mirrors `tests/binstall_metadata_tests.rs`'s
//! guard over the four hand-maintained binstall `pkg-url`/`bin-dir`
//! literals.

use std::fs;
use std::path::Path;

/// The wiring call every serve builder site must contain, verbatim.
const REQUIRED_CALL: &str = ".startup_catch_up(config.file_watch.startup_catch_up)";

/// Source files that build a `UnifiedWatcher` and must wire in the
/// startup-catch-up config key. `--proxy` mode is deliberately excluded: it
/// holds no index (`IndexFacade` is `None`) and starts no watcher, so there
/// is no fourth site.
const WATCHER_BUILDER_SITES: [&str; 3] = [
    "src/cli/commands/serve.rs",
    "src/mcp/http_server.rs",
    "src/mcp/https_server.rs",
];

#[test]
fn all_serve_builder_sites_wire_startup_catch_up() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));

    let missing: Vec<&str> = WATCHER_BUILDER_SITES
        .iter()
        .filter(|relative_path| {
            let path = manifest_dir.join(relative_path);
            let contents = fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("must be able to read {relative_path}: {e}"));
            !contents.contains(REQUIRED_CALL)
        })
        .copied()
        .collect();

    assert!(
        missing.is_empty(),
        "§CDNA.7: every hand-maintained UnifiedWatcher builder call site must \
         wire `{REQUIRED_CALL}` in the same change as the config key/builder \
         method it depends on, or that serve mode silently never arms the \
         startup catch-up reindex with no compile-time signal. Missing from: \
         {missing:?}. The complete set of touch points is: {WATCHER_BUILDER_SITES:?}."
    );
}
