//! HTTP server implementation for MCP
//!
//! Provides a persistent HTTP server with streamable HTTP transport
//! for multiple concurrent clients and real-time updates.

/// Checks whether an `Authorization` header value carries the dev-mode
/// Bearer token. Expects the standard "Bearer <token>" form; rmcp's
/// `auth_header` adds the prefix, so the comparison strips it rather than
/// composing a "Bearer <token>" literal.
#[cfg(feature = "http-server")]
fn is_authorized(auth_header: Option<&str>) -> bool {
    auth_header
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|token| token == crate::mcp::DUMMY_BEARER_TOKEN)
}

/// Current wall-clock time as unix seconds. Used for both the activity
/// timestamp stamped by inbound `/mcp` requests and the idle-timer's "now"
/// reading, so both sides of the comparison share one clock source. Falls
/// back to 0 only if the system clock is set before the epoch, which is not
/// a case worth failing startup over.
#[cfg(feature = "http-server")]
pub(crate) fn unix_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Captures the workspace root's filesystem device + inode, so a later poll
/// can tell "deleted and recreated" (new inode) apart from "renamed" (same
/// inode, different path -- must NOT trigger a shutdown). Unix only: on other
/// platforms there is no portable device/inode equivalent, so this returns
/// `None` and the periodic self-check falls back to a plain existence check.
#[cfg(all(feature = "http-server", unix))]
pub(crate) fn workspace_identity(root: &std::path::Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(root).ok().map(|m| (m.dev(), m.ino()))
}

#[cfg(all(feature = "http-server", not(unix)))]
pub(crate) fn workspace_identity(_root: &std::path::Path) -> Option<(u64, u64)> {
    None
}

/// Whether an inbound request to the protected `/mcp` router should reset
/// the idle-activity clock. Only JSON-RPC calls (POST) count; GET
/// (SSE stream open/reconnect) and DELETE (session teardown) reach the same
/// `nest_service` but must not count as activity, per the idle-timer spec.
/// Extracted as a pure function so `stamp_activity`'s method-scoping can be
/// unit-tested without spinning up an axum app.
#[cfg(feature = "http-server")]
pub(crate) fn should_stamp_activity(method: &axum::http::Method) -> bool {
    *method == axum::http::Method::POST
}

/// Pure elapsed/threshold comparison, extracted so it can be unit-tested in
/// isolation without waiting on a real clock or a real `idle_shutdown_minutes`
/// (whole-minute) duration. `now_secs` and `last_activity_secs` are unix
/// seconds; `idle_threshold` is the configured idle timeout expressed as a
/// `Duration` (test callers can inject sub-second thresholds; production
/// derives it from `idle_shutdown_minutes * 60`).
#[cfg(feature = "http-server")]
fn idle_timeout_exceeded(
    now_secs: u64,
    last_activity_secs: u64,
    idle_threshold: std::time::Duration,
) -> bool {
    std::time::Duration::from_secs(now_secs.saturating_sub(last_activity_secs)) >= idle_threshold
}

/// Which of the two independent periodic self-checks fired.
#[cfg(feature = "http-server")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShutdownTrigger {
    /// The workspace root no longer exists, or (Unix only) still exists at
    /// that path but is no longer the same filesystem object (deleted and
    /// recreated, or replaced) that was there at startup.
    WorkspaceGone,
    /// `idle_shutdown_minutes` was configured (> 0) and that many minutes
    /// have elapsed since the last recorded `/mcp` activity.
    Idle,
}

/// Whether the workspace root the server was launched against is gone, per
/// the same identity `wait_for_workspace_or_idle` was seeded with at
/// startup. A plain "does the path still exist" failure counts, and (Unix
/// only) so does "the path exists but names a different filesystem object
/// now" -- which catches delete-then-recreate at the same path, a case a
/// bare `Path::exists()` check would miss. Deliberately does NOT compare a
/// renamed directory as gone: a rename changes the path a *different* root
/// would be checked at, but this function is always called with the single
/// path the server was launched against, and renaming that specific
/// directory to a new name preserves its device/inode, so the identity
/// comparison here still matches and no shutdown is triggered by a rename
/// alone (only by moving/replacing what lives at the original path).
///
/// Uses `tokio::fs::metadata` (not `std::fs::metadata`) because this is
/// called on every tick of `wait_for_workspace_or_idle`'s poll loop, which
/// runs for the life of every `--http`/`--https` backing server; a
/// synchronous `stat()` there would block the tokio worker thread driving
/// this server's `select!` loop for as long as the idle-shutdown timer is
/// active (up to the configured `idle_shutdown_minutes`, or indefinitely
/// with `idle_shutdown_minutes = 0`).
#[cfg(feature = "http-server")]
pub(crate) async fn workspace_is_gone(
    workspace_root: &std::path::Path,
    workspace_dev: Option<u64>,
    workspace_ino: Option<u64>,
) -> bool {
    // Referenced only inside the `#[cfg(unix)]` arm below; touched
    // unconditionally here (both are `Copy`, so this is not a move) so the
    // non-Unix build does not warn about unused parameters.
    let _ = (workspace_dev, workspace_ino);
    match tokio::fs::metadata(workspace_root).await {
        Err(_) => true,
        #[cfg(unix)]
        Ok(meta) => {
            use std::os::unix::fs::MetadataExt;
            workspace_dev.is_some_and(|dev| dev != meta.dev())
                || workspace_ino.is_some_and(|ino| ino != meta.ino())
        }
        #[cfg(not(unix))]
        Ok(_meta) => false,
    }
}

/// Polls, on one shared cadence, for either of two independent shutdown
/// triggers: the workspace root disappearing, or (only when `idle_minutes >
/// 0`) the idle threshold being exceeded. Deliberately independent checks --
/// with `idle_shutdown_minutes = 0` (a valid, supported setting that
/// disables the idle timer) the workspace-disappeared check must still fire;
/// piggybacking it on the idle gate would silently disable both together.
#[cfg(feature = "http-server")]
pub(crate) async fn wait_for_workspace_or_idle(
    workspace_root: Option<&std::path::Path>,
    workspace_dev: Option<u64>,
    workspace_ino: Option<u64>,
    last_activity: &std::sync::atomic::AtomicU64,
    idle_threshold: std::time::Duration,
    idle_minutes: u64,
    poll_interval: std::time::Duration,
) -> ShutdownTrigger {
    let mut ticker = tokio::time::interval(poll_interval);
    loop {
        ticker.tick().await;

        if let Some(root) = workspace_root
            && workspace_is_gone(root, workspace_dev, workspace_ino).await
        {
            return ShutdownTrigger::WorkspaceGone;
        }

        if idle_minutes > 0 {
            let now = unix_now_secs();
            let last = last_activity.load(std::sync::atomic::Ordering::Relaxed);
            if idle_timeout_exceeded(now, last, idle_threshold) {
                return ShutdownTrigger::Idle;
            }
        }
    }
}

/// Test-only override for the idle-shutdown threshold, read from
/// `CODANNA_TEST_IDLE_THRESHOLD_MS`. Exists so the subprocess-driven
/// end-to-end test (`tests/cli/test_idle_shutdown.rs`) can exercise the real
/// `serve --http` idle-exit path on a millisecond timescale instead of
/// waiting on `idle_shutdown_minutes`' whole-minute production granularity.
/// Unset (the production default) or unparseable values fall back to the
/// config-derived threshold; this is not a documented/supported
/// configuration knob. Gated on `debug_assertions` (in addition to the
/// `http-server` feature, which is in the default feature set) so this
/// env-var override never compiles into a release build and can't silently
/// override an operator's `idle_shutdown_minutes`.
#[cfg(all(feature = "http-server", debug_assertions))]
pub(crate) fn idle_threshold_override() -> Option<std::time::Duration> {
    std::env::var("CODANNA_TEST_IDLE_THRESHOLD_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(std::time::Duration::from_millis)
}

/// Release-build stand-in for [`idle_threshold_override`]: the env-var
/// override is compiled out entirely, so this always returns `None`.
#[cfg(all(feature = "http-server", not(debug_assertions)))]
pub(crate) fn idle_threshold_override() -> Option<std::time::Duration> {
    None
}

/// Test-only override for the idle-timer poll interval, read from
/// `CODANNA_TEST_IDLE_POLL_MS`. Paired with [`idle_threshold_override`] so
/// the e2e test can also shrink the poll cadence and avoid padding the
/// observed shutdown time relative to the (also shrunk) threshold. Gated on
/// `debug_assertions` for the same reason as `idle_threshold_override`.
#[cfg(all(feature = "http-server", debug_assertions))]
pub(crate) fn idle_poll_interval_override() -> Option<std::time::Duration> {
    std::env::var("CODANNA_TEST_IDLE_POLL_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(std::time::Duration::from_millis)
}

/// Release-build stand-in for [`idle_poll_interval_override`]: the env-var
/// override is compiled out entirely, so this always returns `None`.
#[cfg(all(feature = "http-server", not(debug_assertions)))]
pub(crate) fn idle_poll_interval_override() -> Option<std::time::Duration> {
    None
}

#[cfg(feature = "http-server")]
pub async fn serve_http(config: crate::Settings, watch: bool, bind: String) -> anyhow::Result<()> {
    use crate::IndexPersistence;
    use crate::indexing::facade::IndexFacade;
    use crate::mcp::{CodeIntelligenceServer, notifications::NotificationBroadcaster};
    use crate::watcher::HotReloadWatcher;
    use axum::Router;
    use rmcp::transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    };
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::RwLock;
    use tokio_util::sync::CancellationToken;

    // Initialize logging with config
    crate::logging::init_with_config(&config.logging);

    crate::log_event!("http", "starting", "MCP server on {bind}");

    // Create notification broadcaster for file change events
    let broadcaster = Arc::new(NotificationBroadcaster::new(100));

    // Create shared facade
    let settings = Arc::new(config.clone());
    let persistence = IndexPersistence::new(config.index_path.clone());

    let facade = if persistence.exists() {
        match persistence.load_facade(settings.clone()) {
            Ok(loaded) => {
                let symbol_count = loaded.symbol_count();
                crate::log_event!("http", "loaded", "{symbol_count} symbols");
                loaded
            }
            Err(e) => {
                tracing::warn!("[http] failed to load index: {e}");
                crate::log_event!("http", "starting", "empty index");
                IndexFacade::new(settings.clone())?
            }
        }
    } else {
        crate::log_event!("http", "starting", "no existing index");
        IndexFacade::new(settings.clone())?
    };
    let indexer = Arc::new(RwLock::new(facade));

    // Create cancellation token for coordinated shutdown
    let ct = CancellationToken::new();

    // Idle-shutdown activity tracking (unix seconds). Stamped to `now` by the
    // `stamp_activity` middleware on every inbound POST /mcp request; read by
    // the idle-timer `select!` arm below. SSE keep-alives
    // (`with_sse_keep_alive` below, 15s) are outbound comments on an
    // already-open GET stream and generate no inbound POST, so they correctly
    // do NOT reset this timestamp.
    let last_activity = Arc::new(std::sync::atomic::AtomicU64::new(unix_now_secs()));

    // Start index watcher if watch mode is enabled.
    //
    // The `JoinHandle` is kept (not discarded) so the caller can `.await` it
    // after cancelling `ct`. Unlike the unified watcher below,
    // `HotReloadWatcher::watch()` never uses `spawn_blocking` -- its facade
    // swap takes the async `RwLock::write().await`, and dropping that await
    // (e.g. because the outer `select!` below picked the cancellation arm)
    // simply abandons the pending lock acquisition rather than leaving a
    // detached OS thread holding the write guard. Awaiting the handle here
    // is still worth doing so a reload-in-progress settles (rather than the
    // task being silently dropped) before the process exits, but it is not
    // closing the same race the unified watcher's threading does.
    let mut hot_reload_handle: Option<tokio::task::JoinHandle<()>> = None;
    if watch {
        let index_watcher_indexer = indexer.clone();
        let index_watcher_settings = Arc::new(config.clone());
        let index_watcher_broadcaster = broadcaster.clone();
        let index_watcher_ct = ct.clone();

        // Default to 5 second interval
        let watch_interval = 5u64;

        let hot_reload_watcher = HotReloadWatcher::new(
            index_watcher_indexer,
            index_watcher_settings,
            Duration::from_secs(watch_interval),
        )
        .with_broadcaster(index_watcher_broadcaster);

        hot_reload_handle = Some(tokio::spawn(async move {
            tokio::select! {
                _ = hot_reload_watcher.watch() => {
                    crate::log_event!("hot-reload", "ended");
                }
                _ = index_watcher_ct.cancelled() => {
                    crate::log_event!("hot-reload", "stopped");
                }
            }
        }));

        crate::log_event!("hot-reload", "started", "polling every {watch_interval}s");
    }

    // Load document store once (shared between MCP server instances and watcher)
    let document_store_arc = crate::documents::load_from_settings(&config);
    if document_store_arc.is_some() {
        tracing::debug!(target: "mcp", "document store loaded for MCP server");
    }

    // Start unified file watcher if enabled
    let mut unified_watcher_handle: Option<tokio::task::JoinHandle<()>> = None;
    if watch || config.file_watch.enabled {
        use crate::watcher::UnifiedWatcher;
        use crate::watcher::handlers::{CodeFileHandler, ConfigFileHandler, DocumentFileHandler};

        let workspace_root = config
            .workspace_root
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

        let settings_path = workspace_root.join(".codanna/settings.toml");
        let debounce_ms = config.file_watch.debounce_ms;

        // Build unified watcher with handlers
        let mut builder = UnifiedWatcher::builder()
            .broadcaster(broadcaster.clone())
            .indexer(indexer.clone())
            .index_path(config.index_path.clone())
            .workspace_root(workspace_root.clone())
            .debounce_ms(debounce_ms)
            .refresh_on_overflow(config.file_watch.refresh_on_overflow)
            .startup_catch_up(config.file_watch.startup_catch_up)
            .cancellation_token(ct.clone());

        // Add code file handler
        builder = builder.handler(CodeFileHandler::new(
            indexer.clone(),
            workspace_root.clone(),
        ));

        // Add config file handler
        match ConfigFileHandler::new(settings_path.clone()) {
            Ok(config_handler) => {
                builder = builder.handler(config_handler);
            }
            Err(e) => {
                tracing::warn!("[config] failed to create handler: {e}");
            }
        }

        // Add document handler using shared document store
        if let Some(ref store_arc) = document_store_arc {
            tracing::debug!(target: "mcp", "adding document handler to watcher");
            builder = builder
                .document_store(store_arc.clone())
                .chunking_config(config.documents.defaults.clone())
                .handler(DocumentFileHandler::new(
                    store_arc.clone(),
                    workspace_root.clone(),
                ));
        }

        // Build and start the unified watcher. The `JoinHandle` is kept so the
        // caller can `.await` it after cancelling `ct`. Cancellation is wired
        // into `watch()` itself (via `.cancellation_token(ct.clone())` above)
        // rather than raced against `watch()` in an external `select!` here:
        // `watch()` only observes the token between its own loop iterations,
        // never while a handler-spawned `spawn_blocking` closure (e.g. inside
        // `process_removal_wave`/`execute_action`) that may hold the facade's
        // write guard is still being awaited. That guarantees `watch()`
        // returns only once any such closure has finished, so awaiting this
        // handle after cancelling `ct` truly waits for that work to finish.
        match builder.build() {
            Ok(unified_watcher) => {
                unified_watcher_handle = Some(tokio::spawn(async move {
                    if let Err(e) = unified_watcher.watch().await {
                        tracing::error!("[watcher] error: {e}");
                    }
                    crate::log_event!("watcher", "stopped");
                }));
                crate::log_event!(
                    "watcher",
                    "started",
                    "debounce: {debounce_ms}ms, config: {}",
                    crate::parsing::paths::render_absolute_path(&settings_path).display()
                );
            }
            Err(e) => {
                tracing::warn!("[watcher] failed to start: {e}");
                tracing::warn!("[watcher] continuing without file watching");
            }
        }
    }

    // Create streamable HTTP service for MCP connections
    let indexer_for_service = indexer.clone();
    let config_for_service = Arc::new(config.clone());
    let broadcaster_for_service = broadcaster.clone();
    let ct_for_service = ct.clone();
    let document_store_for_service = document_store_arc.clone();

    let mcp_service = StreamableHttpService::new(
        move || {
            crate::debug_event!("mcp", "creating server instance");
            let server = CodeIntelligenceServer::new_with_facade(
                indexer_for_service.clone(),
                config_for_service.clone(),
            )
            .with_broadcaster(broadcaster.clone());

            // Attach document store if available
            let server = if let Some(ref store_arc) = document_store_for_service {
                server.with_document_store_arc(store_arc.clone())
            } else {
                server
            };

            // Start notification listener for this connection
            // Note: We need to wait for initialize() to be called first
            let server_clone = server.clone();
            let receiver = broadcaster_for_service.subscribe();
            let listener_ct = ct_for_service.clone();
            crate::debug_event!("mcp", "subscribing to broadcaster");
            tokio::spawn(async move {
                // Wait a bit for the MCP handshake to complete
                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                crate::debug_event!("mcp", "notification listener started");

                // Run listener until cancelled
                tokio::select! {
                    _ = server_clone.start_notification_listener(receiver) => {
                        crate::debug_event!("mcp", "notification listener ended");
                    }
                    _ = listener_ct.cancelled() => {
                        crate::debug_event!("mcp", "notification listener stopped");
                    }
                }
            });

            Ok(server)
        },
        LocalSessionManager::default().into(),
        {
            let cfg = StreamableHttpServerConfig::default()
                .with_cancellation_token(ct.child_token())
                .with_sse_keep_alive(Some(Duration::from_secs(15)))
                .with_sse_retry(None)
                .with_legacy_session_mode(true)
                .with_json_response(false);
            let cfg = match config.mcp.allowed_hosts.clone() {
                Some(hosts) => cfg.with_allowed_hosts(hosts),
                None => cfg,
            };
            match config.mcp.allowed_origins.clone() {
                Some(origins) => cfg.with_allowed_origins(origins),
                None => cfg,
            }
        },
    );

    // Health check endpoint. Echoes back `launch_token` (generated below, once
    // per process launch) so `serve_discovery::check_health` can bind trust in
    // a `serve.json` record to the specific process that actually wrote it,
    // rather than to any process that happens to answer 200 on the recorded
    // port -- see `ServeRecord::token`'s doc comment. A plain `Fn` closure
    // (not the previous free `async fn`) is what lets this capture the token;
    // axum clones it once per request via its `Clone` bound on `Handler`.

    // Create OAuth metadata handler with the bind address
    let bind_for_metadata = bind.clone();
    let oauth_metadata = move || async move {
        eprintln!("OAuth metadata endpoint called");
        // Return OAuth metadata that supports authorization code flow
        axum::Json(serde_json::json!({
            "issuer": format!("http://{}", bind_for_metadata.clone()),
            "authorization_endpoint": format!("http://{}/oauth/authorize", bind_for_metadata.clone()),
            "token_endpoint": format!("http://{}/oauth/token", bind_for_metadata.clone()),
            "registration_endpoint": format!("http://{}/oauth/register", bind_for_metadata),
            "scopes_supported": ["mcp"],
            "response_types_supported": ["code"],
            "grant_types_supported": ["authorization_code", "refresh_token"],
            "code_challenge_methods_supported": ["S256", "plain"],
            "token_endpoint_auth_methods_supported": ["none"]
        }))
    };

    // Dummy OAuth register endpoint - accepts any registration
    async fn oauth_register(
        axum::Json(payload): axum::Json<serde_json::Value>,
    ) -> axum::Json<serde_json::Value> {
        eprintln!("OAuth register endpoint called with: {payload:?}");
        // Return a dummy client registration response that matches the request
        // Use empty string for public clients (Claude Code expects a string, not null)
        axum::Json(serde_json::json!({
            "client_id": "dummy-client-id",
            "client_secret": "",  // Empty string for public client
            "client_id_issued_at": 1234567890,
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "redirect_uris": payload.get("redirect_uris").unwrap_or(&serde_json::json!([])).clone(),
            "client_name": payload.get("client_name").unwrap_or(&serde_json::json!("MCP Client")).clone(),
            "token_endpoint_auth_method": "none"
        }))
    }

    // OAuth token endpoint - exchanges authorization code for access token
    async fn oauth_token(body: String) -> axum::Json<serde_json::Value> {
        eprintln!("OAuth token endpoint called with body: {body}");

        // Parse form-encoded data (OAuth uses application/x-www-form-urlencoded)
        let params: std::collections::HashMap<String, String> =
            serde_urlencoded::from_str(&body).unwrap_or_default();

        eprintln!("Token request params: {params:?}");

        // Check grant type
        let grant_type = params.get("grant_type").cloned().unwrap_or_default();
        let code = params.get("code").cloned().unwrap_or_default();

        // IMPORTANT: Reject refresh_token grant type (like the SDK example)
        if grant_type == "refresh_token" {
            eprintln!("Rejecting refresh_token grant type");
            return axum::Json(serde_json::json!({
                "error": "unsupported_grant_type",
                "error_description": "only authorization_code is supported"
            }));
        }

        // For authorization_code grant, verify the code
        if grant_type == "authorization_code" && code == "dummy-auth-code" {
            // Return access token WITHOUT refresh token
            axum::Json(serde_json::json!({
                "access_token": crate::mcp::DUMMY_BEARER_TOKEN,
                "token_type": "Bearer",
                "expires_in": 3600,
                "scope": "mcp"
            }))
        } else {
            // Invalid request
            eprintln!("Invalid token request: grant_type={grant_type}, code={code}");
            axum::Json(serde_json::json!({
                "error": "invalid_grant",
                "error_description": "Invalid authorization code or grant type"
            }))
        }
    }

    // Dummy OAuth authorize endpoint - redirects back with auth code
    async fn oauth_authorize(
        axum::extract::Query(params): axum::extract::Query<
            std::collections::HashMap<String, String>,
        >,
    ) -> impl axum::response::IntoResponse {
        eprintln!("OAuth authorize endpoint called with params: {params:?}");

        // Extract redirect_uri and state from query params
        let redirect_uri = params
            .get("redirect_uri")
            .cloned()
            .unwrap_or_else(|| "http://localhost:3118/callback".to_string());
        let state = params.get("state").cloned().unwrap_or_default();

        // Build the callback URL with authorization code
        let callback_url = format!("{redirect_uri}?code=dummy-auth-code&state={state}");

        // Return HTML with auto-redirect and manual button
        let html = format!(
            r#"
<!DOCTYPE html>
<html>
<head>
    <title>Authorize Codanna</title>
    <meta charset="utf-8">
    <style>
        body {{
            font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif;
            display: flex;
            justify-content: center;
            align-items: center;
            height: 100vh;
            margin: 0;
            background: linear-gradient(135deg, #667eea 0%, #764ba2 100%);
        }}
        .container {{
            background: white;
            padding: 2rem;
            border-radius: 10px;
            box-shadow: 0 10px 40px rgba(0,0,0,0.2);
            text-align: center;
            max-width: 400px;
        }}
        h1 {{
            color: #333;
            margin-bottom: 1rem;
        }}
        p {{
            color: #666;
            margin-bottom: 2rem;
        }}
        button {{
            background: #667eea;
            color: white;
            border: none;
            padding: 12px 30px;
            border-radius: 5px;
            font-size: 16px;
            cursor: pointer;
            transition: background 0.3s;
        }}
        button:hover {{
            background: #764ba2;
        }}
    </style>
</head>
<body>
    <div class="container">
        <h1>🔐 Authorize Codanna</h1>
        <p>Grant access to Claude Code?</p>
        <p>Click Continue to complete the authorization.</p>
        <button onclick="window.location.href='{callback_url}'">Continue</button>
    </div>
</body>
</html>
"#
        );

        axum::response::Html(html)
    }

    // Helper function for shutdown signal with cancellation token
    // Also listens for SIGTERM on Unix (in addition to Ctrl+C/SIGINT) so
    // `codanna serve --stop <pid>` -- which sends SIGTERM by default, only
    // escalating to SIGKILL with `--force` -- reaches this SAME graceful
    // shutdown arm, and therefore the SAME `remove_record`/
    // `serve_registry::remove_entry` cleanup Ctrl+C already triggers.
    // Without this, the default (unhandled) disposition of SIGTERM is
    // immediate termination, which would skip this `select!` entirely and
    // leave both the discovery record and the registry entry behind.
    #[cfg(unix)]
    async fn shutdown_signal() {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigterm =
            signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                eprintln!("Received shutdown signal (Ctrl+C)");
            }
            _ = sigterm.recv() => {
                eprintln!("Received shutdown signal (SIGTERM)");
            }
        }
    }

    #[cfg(not(unix))]
    async fn shutdown_signal() {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to listen for ctrl+c");
        eprintln!("Received shutdown signal (Ctrl+C)");
    }

    // Bearer token validation middleware - only for MCP endpoints
    async fn validate_bearer_token(
        req: axum::http::Request<axum::body::Body>,
        next: axum::middleware::Next,
    ) -> Result<axum::response::Response, axum::http::StatusCode> {
        // Check for Bearer token in Authorization header
        let auth_str = req
            .headers()
            .get("Authorization")
            .and_then(|h| h.to_str().ok());
        if is_authorized(auth_str) {
            eprintln!("MCP request authorized with Bearer token");
            return Ok(next.run(req).await);
        }

        // For OPTIONS requests (CORS preflight), allow without auth
        if req.method() == axum::http::Method::OPTIONS {
            return Ok(next.run(req).await);
        }

        eprintln!("MCP request rejected - invalid or missing Bearer token");
        Err(axum::http::StatusCode::UNAUTHORIZED)
    }

    // Idle-timer activity stamp middleware. Implemented as an
    // `axum::middleware::from_fn` closure (rather than threading
    // `last_activity` through the `CodeIntelligenceServer` constructors or a
    // hand-rolled `call_tool`) because `#[tool_handler]` auto-generates
    // `call_tool` for the tool router: a hand-rolled override would be a
    // duplicate-method compile error, and per-tool stamping would duplicate
    // this single stamp across every tool. Layered inside
    // `validate_bearer_token` so only requests that pass (or are exempt from,
    // e.g. CORS preflight) auth validation count as activity.
    //
    // Only inbound POST /mcp (JSON-RPC calls) count as activity. GET (SSE
    // stream open/reconnect) and DELETE (session teardown) reach this same
    // `nest_service` but must not reset the idle clock, so `req.method()` is
    // checked before stamping.
    let last_activity_for_middleware = last_activity.clone();
    let stamp_activity = move |req: axum::http::Request<axum::body::Body>,
                               next: axum::middleware::Next| {
        let last_activity = last_activity_for_middleware.clone();
        async move {
            if should_stamp_activity(req.method()) {
                last_activity.store(unix_now_secs(), std::sync::atomic::Ordering::Relaxed);
            }
            next.run(req).await
        }
    };

    // Create protected MCP router with Bearer token validation
    let protected_mcp_router = Router::new()
        .nest_service("/mcp", mcp_service)
        .layer(axum::middleware::from_fn(stamp_activity))
        .layer(axum::middleware::from_fn(validate_bearer_token));

    // Fresh per-launch token this process's `/health` endpoint echoes back,
    // and that gets written into `ServeRecord::token` below. Generated once
    // here, not derived from the pid/port/anything else an observer could
    // predict -- see `serve_discovery::generate_launch_token`'s doc comment.
    let launch_token = crate::serve_discovery::generate_launch_token();

    // Resolved once, up front, so both the `/health` response headers and the
    // `ServeRecord` written below (and the periodic workspace-disappeared
    // self-check further down) agree on the exact same workspace root and
    // captured identity.
    let workspace_root = crate::serve_discovery::resolve_workspace_root(&config);
    let (workspace_dev, workspace_ino): (Option<u64>, Option<u64>) = workspace_root
        .as_ref()
        .and_then(|root| workspace_identity(root))
        .unzip();

    // The `/health` body MUST remain the bare launch token -- `check_health`
    // asserts `body.trim() == expected_token` verbatim. Workspace identity is
    // exposed via response headers instead, never folded into the body.
    let health_check = {
        let launch_token = launch_token.clone();
        move || {
            let launch_token = launch_token.clone();
            async move {
                let mut headers = axum::http::HeaderMap::new();
                if let Some(dev) = workspace_dev {
                    if let Ok(value) = axum::http::HeaderValue::from_str(&dev.to_string()) {
                        headers.insert("x-codanna-workspace-dev", value);
                    }
                }
                if let Some(ino) = workspace_ino {
                    if let Ok(value) = axum::http::HeaderValue::from_str(&ino.to_string()) {
                        headers.insert("x-codanna-workspace-ino", value);
                    }
                }
                (headers, launch_token)
            }
        }
    };

    // Create main router - OAuth endpoints FIRST (no auth), then MCP endpoints (with auth)
    let router = Router::new()
        // OAuth endpoints - NO authentication required
        .route(
            "/.well-known/oauth-authorization-server",
            axum::routing::get(oauth_metadata),
        )
        .route("/oauth/register", axum::routing::post(oauth_register))
        .route("/oauth/token", axum::routing::post(oauth_token))
        .route("/oauth/authorize", axum::routing::get(oauth_authorize))
        // Health check - NO authentication required
        .route("/health", axum::routing::get(health_check))
        // MCP endpoint - Bearer token authentication required
        .merge(protected_mcp_router);

    // Bind and serve
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    let actual_port = listener.local_addr()?.port();
    eprintln!("HTTP MCP server listening on http://{bind}");
    eprintln!("MCP endpoint: http://{bind}/mcp");
    eprintln!("Health check: http://{bind}/health");
    eprintln!("Press Ctrl+C to stop the server");

    // Publish a discovery record under the tree's `.codanna/` so other tools
    // (e.g. the CLI proxy) can find this server without guessing ports.
    // `local_addr().port()` is read above rather than parsing `bind` so an
    // ephemeral `:0` bind resolves to the actual assigned port.
    //
    // The directory is derived from the workspace root, not `index_path`:
    // `index_path` may be absolute or resolved relative to a `--config`
    // file's parent (`init::resolve_index_path`), so it can diverge from
    // `.codanna` in exactly the cases `discover_or_spawn` relies on.
    //
    // `workspace_root`/`workspace_dev`/`workspace_ino` were already resolved
    // above (before `health_check` was built); reused here so the record
    // written to disk and the runtime self-check below agree with what
    // `/health` reports.
    let codanna_dir = workspace_root
        .as_ref()
        .map(|root| crate::serve_discovery::discovery_dir(root));

    match &codanna_dir {
        Some(codanna_dir) => {
            let serve_record = crate::serve_discovery::ServeRecord {
                pid: std::process::id(),
                port: actual_port,
                scheme: crate::serve_discovery::ServeScheme::Http,
                token: Some(launch_token.clone()),
                workspace_dev,
                workspace_ino,
            };
            if let Err(e) = crate::serve_discovery::write_record(codanna_dir, &serve_record) {
                tracing::warn!(target: "mcp", "failed to write serve discovery record: {e}");
            }
        }
        None => {
            tracing::warn!(
                target: "mcp",
                "no workspace root (.codanna) found; not publishing a discovery record -- \
                 `codanna serve --proxy` cannot discover this server. Run `codanna init` in the project root."
            );
            eprintln!(
                "Warning: no workspace root (.codanna) found; not publishing a discovery record -- \
                 `codanna serve --proxy` cannot discover this server. Run `codanna init` in the project root."
            );
        }
    }

    // Publish this server to the per-user server registry
    // (`crate::serve_registry`), independent of whether a per-workspace
    // `.codanna` discovery record could be written above: `codanna serve
    // --list/--stop/--reap` must be able to see and manage this server even
    // for a workspace with no `.codanna` directory.
    let registry_workspace_root = workspace_root
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let registry_entry = crate::serve_registry::RegistryEntry {
        pid: std::process::id(),
        port: actual_port,
        scheme: crate::serve_discovery::ServeScheme::Http,
        workspace_root: registry_workspace_root,
        start_time: unix_now_secs(),
        status: crate::serve_registry::ServerStatus::Healthy,
        role: crate::serve_registry::ServerRole::Server,
        version: env!("CARGO_PKG_VERSION").to_string(),
    };
    if let Err(e) = crate::serve_registry::write_entry(&registry_entry) {
        tracing::warn!(target: "mcp", "failed to write server registry entry: {e}");
    }

    // Create server future
    let server = axum::serve(listener, router);

    // Idle-shutdown configuration: 0 disables the timer entirely (the
    // `select!` arm below is guarded on `idle_minutes > 0` and is never
    // polled in that case). The poll interval is intentionally short (a few
    // seconds) relative to `idle_shutdown_minutes` granularity so the actual
    // shutdown fires close to the configured threshold rather than being
    // padded by a coarse poll.
    //
    // `idle_threshold`/`idle_poll_interval` accept a test-only env var
    // override (see `idle_threshold_override`/`idle_poll_interval_override`)
    // so the real subprocess-driven e2e test can drive the actual idle-exit
    // `select!` arm below without waiting on `idle_shutdown_minutes`'
    // whole-minute granularity. `idle_minutes > 0` still gates whether the
    // arm is enabled at all.
    let idle_minutes = config.server.idle_shutdown_minutes;
    let idle_threshold = idle_threshold_override()
        .unwrap_or_else(|| Duration::from_secs(idle_minutes.saturating_mul(60)));
    let idle_poll_interval = idle_poll_interval_override().unwrap_or(Duration::from_secs(5));

    // Handle graceful shutdown with tokio::select!
    let mut server_result: Option<std::io::Result<()>> = None;
    tokio::select! {
        result = server => {
            ct.cancel();
            if let Some(codanna_dir) = &codanna_dir {
                crate::serve_discovery::remove_record(codanna_dir);
            }
            crate::serve_registry::remove_entry(std::process::id());
            server_result = Some(result);
        }
        _ = shutdown_signal() => {
            eprintln!("Shutting down HTTP server...");
            ct.cancel();
            if let Some(codanna_dir) = &codanna_dir {
                crate::serve_discovery::remove_record(codanna_dir);
            }
            crate::serve_registry::remove_entry(std::process::id());
        }
        // Respawn after either self-exit is already handled by
        // discover_or_spawn once serve.json is gone: removing the record
        // here (before exiting) makes this backing server undiscoverable,
        // so the next `discover_or_spawn` call for this workspace spawns a
        // fresh one.
        //
        // Gated on `idle_minutes > 0 OR a workspace root was resolved` --
        // deliberately NOT solely on `idle_minutes > 0` -- so the
        // workspace-disappeared check still runs when idle-shutdown is
        // disabled (`idle_shutdown_minutes = 0`, a valid setting). The two
        // triggers inside `wait_for_workspace_or_idle` remain independent:
        // idle-shutdown being off never suppresses the workspace check.
        trigger = wait_for_workspace_or_idle(
            workspace_root.as_deref(),
            workspace_dev,
            workspace_ino,
            &last_activity,
            idle_threshold,
            idle_minutes,
            idle_poll_interval,
        ), if idle_minutes > 0 || workspace_root.is_some() => {
            match trigger {
                ShutdownTrigger::WorkspaceGone => {
                    eprintln!("Shutting down HTTP server: workspace root no longer exists...");
                }
                ShutdownTrigger::Idle => {
                    eprintln!("Shutting down HTTP server after {idle_minutes} minute(s) of inactivity...");
                }
            }
            ct.cancel();
            if let Some(codanna_dir) = &codanna_dir {
                crate::serve_discovery::remove_record(codanna_dir);
            }
            crate::serve_registry::remove_entry(std::process::id());
        }
    }

    // `ct` is cancelled on every branch above (including the `server`
    // future's own natural completion). Awaiting these handles here -- not
    // just relying on `ct.cancel()` -- is what lets the unified watcher's
    // `watch()` task finish any `spawn_blocking` closure holding the
    // facade's write guard, and join any in-flight catch-up reindex task,
    // before this function returns: `watch()` only observes `ct` between
    // its own loop iterations (see `UnifiedWatcher::cancellation_token`),
    // and its cancellation arm blocks on the catch-up task's completion
    // rather than dropping it, so by the time its `JoinHandle` resolves, no
    // such closure or task is still running, and it is safe for the caller
    // to tear the process down right after this function returns.
    if let Some(handle) = hot_reload_handle {
        let _ = handle.await;
    }
    if let Some(handle) = unified_watcher_handle {
        let _ = handle.await;
    }

    if let Some(result) = server_result {
        result?;
    }

    eprintln!("HTTP server shut down gracefully");
    Ok(())
}

#[cfg(not(feature = "http-server"))]
pub async fn serve_http(
    _config: crate::Settings,
    _watch: bool,
    _bind: String,
) -> anyhow::Result<()> {
    eprintln!("HTTP server support is not compiled in.");
    eprintln!("Please rebuild with: cargo build --features http-server");
    std::process::exit(1);
}

#[cfg(all(test, feature = "http-server"))]
mod tests {
    use super::is_authorized;
    use super::{
        ShutdownTrigger, idle_timeout_exceeded, should_stamp_activity, unix_now_secs,
        wait_for_workspace_or_idle,
    };
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::time::Duration;

    #[test]
    fn accepts_bearer_prefixed_dummy_token() {
        assert!(is_authorized(Some("Bearer mcp-access-token-dummy")));
    }

    #[test]
    fn rejects_bare_token_without_bearer_prefix() {
        assert!(!is_authorized(Some("mcp-access-token-dummy")));
    }

    #[test]
    fn rejects_wrong_token() {
        assert!(!is_authorized(Some("Bearer wrong")));
    }

    #[test]
    fn rejects_missing_header() {
        assert!(!is_authorized(None));
    }

    // -------------------------------------------------------------------
    // Idle timer: method-scoping of the activity stamp. GET (SSE stream
    // open/reconnect) and DELETE (session teardown) reach the same
    // `nest_service("/mcp", ...)` as POST but must not reset the idle
    // clock.
    // -------------------------------------------------------------------

    #[test]
    fn post_to_mcp_counts_as_activity() {
        assert!(should_stamp_activity(&axum::http::Method::POST));
    }

    #[test]
    fn get_to_mcp_does_not_count_as_activity() {
        assert!(!should_stamp_activity(&axum::http::Method::GET));
    }

    #[test]
    fn delete_to_mcp_does_not_count_as_activity() {
        assert!(!should_stamp_activity(&axum::http::Method::DELETE));
    }

    // -------------------------------------------------------------------
    // Idle timer: elapsed/threshold computation, tested in isolation with
    // no real waiting.
    // -------------------------------------------------------------------

    #[test]
    fn idle_timeout_not_exceeded_before_threshold() {
        assert!(!idle_timeout_exceeded(100, 95, Duration::from_secs(10)));
    }

    #[test]
    fn idle_timeout_exceeded_exactly_at_threshold() {
        assert!(idle_timeout_exceeded(105, 95, Duration::from_secs(10)));
    }

    #[test]
    fn idle_timeout_exceeded_well_past_threshold() {
        assert!(idle_timeout_exceeded(1_000, 0, Duration::from_secs(10)));
    }

    #[test]
    fn idle_timeout_not_exceeded_when_now_precedes_last_activity() {
        // Clock skew / stamped-in-the-future guard: `saturating_sub` must
        // keep this from underflowing into a huge elapsed value.
        assert!(!idle_timeout_exceeded(5, 100, Duration::from_secs(10)));
    }

    /// Exercises the private `wait_for_workspace_or_idle` seam directly with
    /// millisecond-scale injected `Duration`s -- proving the loop actually
    /// resolves once the threshold is exceeded, without waiting on a real
    /// `idle_shutdown_minutes`-scale (whole-minute) timeout. `workspace_root`
    /// is `None` so only the idle trigger is exercised here.
    #[tokio::test]
    async fn wait_for_idle_resolves_once_threshold_elapses() {
        // Recorded activity far enough in the past that even the very first
        // poll tick observes the threshold as exceeded.
        let last_activity = AtomicU64::new(0);

        let trigger = tokio::time::timeout(
            Duration::from_millis(500),
            wait_for_workspace_or_idle(
                None,
                None,
                None,
                &last_activity,
                Duration::from_millis(1),
                1,
                Duration::from_millis(10),
            ),
        )
        .await
        .expect(
            "wait_for_workspace_or_idle should resolve well within the timeout once the \
             threshold is exceeded",
        );
        assert_eq!(trigger, ShutdownTrigger::Idle);
    }

    /// Complement of the resolves-once-idle test: proves ongoing activity
    /// actually holds the idle timer off. A concurrent stamper refreshes
    /// `last_activity` to "now" on a tight cadence -- exactly what the
    /// `stamp_activity` middleware does on each inbound POST /mcp -- and
    /// `wait_for_workspace_or_idle` must NOT resolve while that traffic
    /// continues. A timer
    /// that ignored `last_activity` would fire at the threshold regardless and
    /// fail this test; a correct one, re-reading the refreshed timestamp on
    /// every poll, keeps waiting for the full window.
    ///
    /// The 2s threshold is deliberately two seconds, not one: `unix_now_secs`
    /// / `idle_timeout_exceeded` are whole-second granular, so right at a
    /// second boundary a poll can momentarily observe `last_activity` as one
    /// second stale even while the 40ms stamper is keeping it current. A 2s
    /// threshold leaves a full one-second margin over that boundary artifact,
    /// so the assertion is not flaky, while still being tripped by a timer
    /// that ignores activity entirely.
    #[tokio::test]
    async fn wait_for_idle_does_not_resolve_while_activity_stays_fresh() {
        let last_activity = Arc::new(AtomicU64::new(unix_now_secs()));
        let keep_stamping = Arc::new(AtomicBool::new(true));

        let stamp_target = last_activity.clone();
        let stamp_flag = keep_stamping.clone();
        let stamper = tokio::spawn(async move {
            while stamp_flag.load(Ordering::Relaxed) {
                stamp_target.store(unix_now_secs(), Ordering::Relaxed);
                tokio::time::sleep(Duration::from_millis(40)).await;
            }
        });

        // Poll for 2.2s -- past the 2s threshold, so a timer ignoring
        // `last_activity` would resolve -- and require that it does not.
        // `workspace_root` is `None` so only the idle trigger is exercised.
        let result = tokio::time::timeout(
            Duration::from_millis(2200),
            wait_for_workspace_or_idle(
                None,
                None,
                None,
                &last_activity,
                Duration::from_secs(2),
                1,
                Duration::from_millis(25),
            ),
        )
        .await;

        keep_stamping.store(false, Ordering::Relaxed);
        stamper.await.unwrap();

        assert!(
            result.is_err(),
            "wait_for_workspace_or_idle must not resolve while activity is continuously refreshed"
        );
    }
}
