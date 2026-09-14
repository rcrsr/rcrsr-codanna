//! Integration coverage for the watcher's bounded per-file replay journal
//! (decision 6b): a hot-reload generation swap that races an in-flight
//! watcher edit must not silently drop that edit from the generation now
//! being served.
//!
//! These drive a real `UnifiedWatcher::watch()` task against a real
//! filesystem (temp dirs, real `notify` events, real debounce timing) and a
//! real `IndexFacade`/`IndexPersistence` -- no mocks. `IndexFacade::swap_in`
//! is `pub(crate)` and unreachable from here, so the generation swap +
//! `IndexReloaded` broadcast it performs is reproduced directly (see
//! `swap_facade_and_broadcast` below); everything downstream of that swap
//! (the watcher's own `handle_index_reloaded` -> journal replay) is the real
//! production code path under test.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use codanna::config::Settings;
use codanna::indexing::facade::IndexFacade;
use codanna::mcp::notifications::{FileChangeEvent, NotificationBroadcaster};
use codanna::storage::{BuildMode, IndexPersistence};
use codanna::watcher::UnifiedWatcher;
use codanna::watcher::handlers::CodeFileHandler;

/// Poll `predicate` (async, re-evaluated against the live facade) until it
/// returns `true` or `timeout` elapses. Real filesystem watch events and
/// debounced reindexing are not synchronous, so every assertion downstream
/// of a real file edit or broadcast in this file polls rather than sleeping
/// a fixed guess and hoping.
async fn wait_until(
    facade: &Arc<RwLock<IndexFacade>>,
    timeout: Duration,
    mut predicate: impl FnMut(&IndexFacade) -> bool,
) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if predicate(&*facade.read().await) {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Replace the shared facade's contents and broadcast `IndexReloaded`,
/// reproducing the two effects of `IndexFacade::swap_in` this test depends
/// on (`swap_in` itself is `pub(crate)`, not reachable from an external
/// integration test). Deliberately does not reproduce `swap_in`'s
/// reindex-gate carryover or semantic-search re-attach: neither is
/// exercised by this test, since no `reindex_locked` call and no semantic
/// search are involved.
async fn swap_facade_and_broadcast(
    facade_arc: &Arc<RwLock<IndexFacade>>,
    broadcaster: &NotificationBroadcaster,
    new_facade: IndexFacade,
) {
    {
        let mut guard = facade_arc.write().await;
        *guard = new_facade;
    }
    broadcaster.send(FileChangeEvent::IndexReloaded);
}

fn settings_for(index_dir: &Path) -> Settings {
    Settings {
        index_path: index_dir.to_path_buf(),
        workspace_root: None,
        ..Default::default()
    }
}

/// Dual-generation replay: a watcher-driven edit lands on generation A
/// while generation B (whose build predates the edit) is being published
/// underneath it. Once B is swapped in, the watcher must replay the edit
/// against B rather than silently serving a B that never saw it.
///
/// The discriminating assertion is the final one: a query against the live
/// (now B) facade must return the symbol the edit added. A watcher that
/// never journals/replays would leave B without that symbol, since B's own
/// build walked the source tree before the edit happened.
#[tokio::test]
async fn dual_generation_swap_replays_edit_that_raced_the_build() {
    let temp = tempfile::tempdir().expect("create temp root");
    let source_dir = temp.path().join("src");
    std::fs::create_dir_all(&source_dir).expect("create source dir");
    let file_path = source_dir.join("existing.py");
    std::fs::write(&file_path, "def existing():\n    pass\n").expect("write initial fixture");

    let settings = Arc::new(settings_for(&temp.path().join("index")));

    let mut facade =
        IndexFacade::new(Arc::clone(&settings)).expect("create facade over temp index dir");
    facade
        .index_directory(&source_dir, false)
        .expect("index source dir into generation A");
    assert!(
        facade.find_symbol("existing").is_some(),
        "sanity: existing() must resolve in generation A before the watcher starts"
    );

    let facade_arc = Arc::new(RwLock::new(facade));
    let broadcaster = Arc::new(NotificationBroadcaster::new(16));

    // The handler's own `workspace_root` is deliberately NOT a prefix of
    // the real (canonical, absolute) event paths notify will report, so
    // `CodeFileHandler::to_relative`'s `strip_prefix` always falls through
    // to its absolute-path fallback. This sidesteps `IndexFacade::index_file`
    // re-`canonicalize()`-ing a relative path against the *test process's*
    // current directory (which is not `source_dir`'s parent) rather than
    // against the watcher's workspace root -- a mismatch that would make a
    // relative-path reindex fail to find the file at all.
    let mismatched_handler_root = temp.path().join("unused-handler-root");
    let code_handler = CodeFileHandler::new(Arc::clone(&facade_arc), mismatched_handler_root);

    let cancellation_token = CancellationToken::new();
    let watcher = UnifiedWatcher::builder()
        .broadcaster(Arc::clone(&broadcaster))
        .indexer(Arc::clone(&facade_arc))
        .workspace_root(temp.path().to_path_buf())
        .debounce_ms(20)
        .cancellation_token(cancellation_token.clone())
        .handler(code_handler)
        .build()
        .expect("build unified watcher");

    let watch_handle = tokio::spawn(async move {
        let _ = watcher.watch().await;
    });

    // Let the watcher finish its startup registration (handler refresh +
    // directory watch registration) before editing anything.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Build generation B now, BEFORE the edit below: its `Building` guard
    // stamps `COMPLETE.started_at` at (approximately) this instant, and its
    // walk of `source_dir` sees only the pre-edit content -- exactly the
    // "B's build predates the edit and does not include the new symbol"
    // setup this test needs, achieved by ordering rather than by hand-
    // editing the manifest.
    let persistence = IndexPersistence::new(settings.index_path.clone());
    let mut build = persistence
        .open_build(Arc::clone(&settings), BuildMode::Fresh)
        .expect("open generation B build");
    build
        .index_directory(&source_dir, false)
        .expect("index pre-edit source dir into generation B");
    assert!(
        build.find_symbol("added_symbol").is_none(),
        "sanity: generation B's build must predate the edit and not see added_symbol"
    );
    let (generation_b_id, warm_facade_b) = persistence
        .publish_into_facade(build)
        .expect("publish generation B");

    // Now edit the file the running watcher already tracks: this must
    // drive a real ReindexCode action against the still-live generation A,
    // recording a journal entry timestamped after B's `started_at`.
    std::fs::write(
        &file_path,
        "def existing():\n    pass\n\n\ndef added_symbol():\n    pass\n",
    )
    .expect("edit fixture to add a new symbol");

    let landed_in_a = wait_until(&facade_arc, Duration::from_secs(5), |facade| {
        facade.find_symbol("added_symbol").is_some()
    })
    .await;
    assert!(
        landed_in_a,
        "the watcher must reindex the edit into generation A before the swap"
    );

    // Swap generation B in and broadcast IndexReloaded, exactly as a real
    // hot-reload / force-reindex publish would -- the watcher's
    // `handle_index_reloaded` (subscribed to the same broadcaster) is what
    // drives the journal replay under test.
    swap_facade_and_broadcast(&facade_arc, &broadcaster, warm_facade_b).await;

    assert_eq!(
        facade_arc.read().await.generation_id(),
        &generation_b_id,
        "sanity: the live facade must now be generation B"
    );

    // Discriminating assertion: without replay, B would never see
    // `added_symbol` (its own build predates the edit). Only a correct
    // journal replay re-applies the edit against B.
    let replayed_into_b = wait_until(&facade_arc, Duration::from_secs(5), |facade| {
        facade.find_symbol("added_symbol").is_some()
    })
    .await;
    assert!(
        replayed_into_b,
        "the watcher must replay the journaled edit against generation B after the swap"
    );

    cancellation_token.cancel();
    let _ = watch_handle.await;
}

/// A genuine file removal (real `notify` event -> debounce -> removal wave
/// -> batch incremental sync) must broadcast `IndexReloaded` on the shared
/// broadcaster, and a second, independent subscriber (standing in for the
/// stdio MCP notification-forwarding task -- the W-1 wiring this exercises
/// end-to-end) must observe it.
#[tokio::test]
async fn stdio_watcher_broadcasts_index_reloaded_on_real_removal_wave() {
    let temp = tempfile::tempdir().expect("create temp root");
    let source_dir = temp.path().join("src");
    std::fs::create_dir_all(&source_dir).expect("create source dir");
    let file_path = source_dir.join("doomed.py");
    std::fs::write(&file_path, "def doomed():\n    pass\n").expect("write fixture");

    let mut settings = settings_for(&temp.path().join("index"));
    // `CodeFileHandler::watch_roots()` (and, through it,
    // `register_handler_roots`'s `batch_sync_roots`) reads
    // `indexed_paths_cache`, not the generation's own indexed-paths
    // tracking -- the batch-sync removal-wave lane (the only lane that
    // broadcasts `IndexReloaded`) only covers roots registered here.
    settings.indexed_paths_cache = vec![
        source_dir
            .canonicalize()
            .expect("canonicalize source dir for indexed_paths_cache"),
    ];
    let settings = Arc::new(settings);
    let mut facade =
        IndexFacade::new(Arc::clone(&settings)).expect("create facade over temp index dir");
    facade
        .index_directory(&source_dir, false)
        .expect("index source dir");
    assert!(facade.find_symbol("doomed").is_some());

    let facade_arc = Arc::new(RwLock::new(facade));
    let broadcaster = Arc::new(NotificationBroadcaster::new(16));

    // A second, independent subscriber, standing in for the stdio server's
    // notification-forwarding task (`broadcaster.subscribe()` in
    // `src/cli/commands/serve.rs`), rather than reusing the watcher's own
    // internal `broadcast_rx` -- what's under test here is fan-out to an
    // external subscriber, not the watcher's self-consumption.
    let mut external_subscriber = broadcaster.subscribe();

    let code_handler = CodeFileHandler::new(Arc::clone(&facade_arc), temp.path().to_path_buf());
    let cancellation_token = CancellationToken::new();
    let watcher = UnifiedWatcher::builder()
        .broadcaster(Arc::clone(&broadcaster))
        .indexer(Arc::clone(&facade_arc))
        .workspace_root(temp.path().to_path_buf())
        .debounce_ms(20)
        .cancellation_token(cancellation_token.clone())
        .handler(code_handler)
        .build()
        .expect("build unified watcher");

    let watch_handle = tokio::spawn(async move {
        let _ = watcher.watch().await;
    });

    tokio::time::sleep(Duration::from_millis(200)).await;

    std::fs::remove_file(&file_path).expect("delete tracked file to drive a removal wave");

    let reloaded = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match external_subscriber.recv().await {
                Ok(FileChangeEvent::IndexReloaded) => return true,
                Ok(_) => continue,
                Err(_) => return false,
            }
        }
    })
    .await
    .unwrap_or(false);

    assert!(
        reloaded,
        "a real file removal must drive a batch-sync removal wave that broadcasts \
         IndexReloaded to every subscriber, not just the watcher's own listener"
    );

    let removed = wait_until(&facade_arc, Duration::from_secs(5), |facade| {
        facade.find_symbol("doomed").is_none()
    })
    .await;
    assert!(
        removed,
        "the removal wave must actually drop the deleted file's symbol from the index"
    );

    cancellation_token.cancel();
    let _ = watch_handle.await;
}
