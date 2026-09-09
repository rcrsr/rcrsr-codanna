//! Serve command - MCP server modes (stdio, HTTP, HTTPS).

use std::path::PathBuf;
use std::sync::Arc;

use crate::config::Settings;
use crate::indexing::facade::IndexFacade;
use crate::mcp::probe_tolerant_stdio;
use crate::serve_discovery::{PidLockError, PidLockGuard};

/// Arguments for the serve command.
pub struct ServeArgs {
    pub watch: bool,
    pub watch_interval: u64,
    pub http: bool,
    pub https: bool,
    pub proxy: bool,
    pub bind: String,
    /// List servers from the per-user server registry instead of starting one.
    pub list: bool,
    /// Stop a registered server (pid or workspace-root path) instead of starting one.
    pub stop: Option<String>,
    /// Prune stale registry entries instead of starting one.
    pub reap: bool,
    /// With `stop`, send SIGKILL instead of the default SIGTERM.
    pub force: bool,
    /// With `stop`, also allow a numeric pid selector that is not present in
    /// the per-user server registry, as long as it still independently
    /// passes `looks_like_codanna_serve`. Absent, `stop`'s selector
    /// resolution is registered-pids-only (today's behavior, unchanged).
    pub include_rogue: bool,
}

/// Resolve which server transport `codanna serve` should start.
///
/// Precedence mirrors `main.rs`'s `is_proxy_serve` so the pre-dispatch
/// resource predicates and the actual server startup never disagree about
/// which mode is in effect:
/// 1. CLI `--https` (highest precedence)
/// 2. CLI `--http`
/// 3. CLI `--proxy`, OR `config.server.mode == "proxy"` when no CLI transport
///    flag was given (bare `codanna serve` can default to proxy via
///    `settings.toml`)
/// 4. `config.server.mode == "http"`
/// 5. stdio (default)
fn resolve_server_mode(https: bool, http: bool, proxy: bool, config_mode: &str) -> &'static str {
    if https {
        "https"
    } else if http {
        "http"
    } else if proxy || config_mode == "proxy" {
        "proxy"
    } else if config_mode == "http" {
        "http"
    } else {
        "stdio"
    }
}

/// Run the serve command.
///
/// `facade` is `None` exactly when proxy mode is selected: `main.rs` computes
/// the effective mode ahead of index loading (§4.5) and skips constructing an
/// `IndexFacade` entirely for proxy, since the proxy process holds no index
/// state of its own -- it only relays to a backing HTTP server's facade.
pub async fn run(
    args: ServeArgs,
    config: Settings,
    settings: Arc<Settings>,
    facade: Option<IndexFacade>,
    index_path: PathBuf,
    config_path: Option<PathBuf>,
) {
    let ServeArgs {
        watch,
        watch_interval,
        http,
        https,
        proxy,
        bind,
        list,
        stop,
        reap,
        force,
        include_rogue,
    } = args;

    // Registry lifecycle operations (--list/--stop/--reap) are never
    // start-a-server operations: handle them here, before any transport mode
    // is resolved, and return without ever reaching the server-startup match
    // below. Clap's `conflicts_with_all` on these flags (see
    // `cli::args::Commands::Serve`) already guarantees `http`/`https`/`proxy`/
    // `bind` are all still at their defaults whenever one of these is set.
    if list || stop.is_some() || reap {
        run_registry_management(list, stop, reap, force, include_rogue).await;
        return;
    }

    let server_mode = resolve_server_mode(https, http, proxy, &config.server.mode);

    // Use bind address from CLI if provided, otherwise from config
    // For HTTPS, default to port 8443 if using default bind
    let bind_address = if bind != "127.0.0.1:8080" {
        // CLI flag was explicitly set (not default)
        bind
    } else if https {
        // For HTTPS, use port 8443 by default
        "127.0.0.1:8443".to_string()
    } else {
        // Use config value
        config.server.bind.clone()
    };

    // Use watch interval from CLI if provided, otherwise from config
    let actual_watch_interval = if watch_interval != 5 {
        // CLI flag was explicitly set (not default)
        watch_interval
    } else {
        config.server.watch_interval
    };

    match server_mode {
        "https" => {
            run_https_server(&config, watch, bind_address).await;
        }
        "http" => {
            run_http_server(config, watch, bind_address).await;
        }
        "proxy" => {
            run_proxy_server(config, config_path).await;
        }
        _ => {
            run_stdio_server(
                config,
                settings,
                facade.expect("stdio serve requires an already-loaded IndexFacade"),
                index_path,
                watch,
                actual_watch_interval,
            )
            .await;
        }
    }
}

/// Dispatch `codanna serve --list`/`--stop`/`--reap`. Runs each requested
/// operation in turn (reap, then stop, then list) so `--stop --list` reflects
/// the post-stop state and `--reap --list` reflects the post-reap state.
async fn run_registry_management(
    list: bool,
    stop: Option<String>,
    reap: bool,
    force: bool,
    include_rogue: bool,
) {
    if reap {
        reap_stale_entries();
    }
    if let Some(selector) = stop {
        stop_server(&selector, force, include_rogue).await;
    }
    if list {
        print_registry_list();
    }
}

/// Remove every registry entry whose process is no longer alive (via the
/// zombie-safe `pid_is_alive`). This never signals a process -- it only
/// prunes stale registry files left behind by a server that did not
/// self-deregister (e.g. it was killed with SIGKILL).
fn reap_stale_entries() {
    let mut reaped = 0u32;
    for entry in crate::serve_registry::list_entries() {
        if crate::serve_registry::entry_is_stale(&entry) {
            crate::serve_registry::remove_entry(entry.pid);
            reaped += 1;
        }
    }
    eprintln!(
        "Reaped {reaped} stale server registry {}.",
        if reaped == 1 { "entry" } else { "entries" }
    );
}

/// Print a table of every registered server whose process is currently
/// alive.
///
/// `codanna serve --list` is deprecated in favor of `codanna ls`, which owns
/// the sole merge/table-building logic for this listing (see the module doc
/// on `crate::cli::commands::ls`). This prints a one-line deprecation notice
/// to stderr, then delegates entirely to `ls::run` so there is exactly one
/// source of listing logic -- this function must never rebuild the table
/// itself.
fn print_registry_list() {
    eprintln!(
        "DEPRECATION: `codanna serve --list` is deprecated in favor of `codanna ls`; it will be removed in a future release."
    );
    crate::cli::commands::ls::run();
}

/// Resolve a `--stop` selector to a pid: either a literal pid, or a
/// workspace-root path matched (exactly, or after canonicalization) against
/// every registered entry's `workspace_root`.
///
/// A numeric selector must name a pid that still looks like a `codanna
/// serve` process -- otherwise `codanna serve --stop <pid>` would
/// SIGTERM/SIGKILL any process the invoking user can signal, not just a
/// codanna server, contradicting the "Stop a registered server..." help
/// text. By default it must additionally be registered (present in the
/// per-user server registry); `allow_rogue` is an explicit, non-default
/// opt-in (`--include-rogue`) that waives the registry-membership check but
/// never the `looks_like_codanna_serve` identity check, so this can never
/// become a generic kill-any-pid primitive.
fn resolve_selector_to_pid(selector: &str, allow_rogue: bool) -> Option<u32> {
    if let Ok(pid) = selector.parse::<u32>() {
        if !crate::serve_registry::looks_like_codanna_serve(pid) {
            return None;
        }
        let is_registered = crate::serve_registry::list_entries()
            .iter()
            .any(|entry| entry.pid == pid);
        return (allow_rogue || is_registered).then_some(pid);
    }

    let path = PathBuf::from(selector);
    let canonical = std::fs::canonicalize(&path).ok();

    crate::serve_registry::list_entries()
        .into_iter()
        .find_map(|entry| {
            let matches = entry.workspace_root == path
                || canonical.as_deref() == Some(entry.workspace_root.as_path());
            matches.then_some(entry.pid)
        })
}

/// Stop a registered server: resolve `selector` to a pid, send SIGTERM (or
/// SIGKILL with `force`), then poll -- bounded, a few seconds -- for the
/// target to exit. SIGTERM is the default; SIGKILL is only ever sent when
/// `force` is set explicitly.
async fn stop_server(selector: &str, force: bool, include_rogue: bool) {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, Signal, System};

    let Some(pid) = resolve_selector_to_pid(selector, include_rogue) else {
        if let Ok(pid) = selector.parse::<u32>() {
            eprintln!("No registered server with pid {pid}.");
        } else {
            eprintln!(
                "No registered server matches '{selector}' (expected a pid or a workspace-root path)."
            );
        }
        std::process::exit(1);
    };

    let mut sys = System::new();
    let target = Pid::from_u32(pid);
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[target]),
        true,
        ProcessRefreshKind::nothing(),
    );

    let Some(process) = sys.process(target) else {
        eprintln!(
            "No running process with pid {pid} (its registry entry may be stale; try --reap)."
        );
        std::process::exit(1);
    };

    let signal = if force { Signal::Kill } else { Signal::Term };
    match process.kill_with(signal) {
        Some(true) => {}
        Some(false) => {
            let name = if force { "SIGKILL" } else { "SIGTERM" };
            eprintln!("Failed to send {name} to pid {pid}.");
            std::process::exit(1);
        }
        None => {
            eprintln!("Sending signals is not supported on this platform (pid {pid}).");
            std::process::exit(1);
        }
    }

    let deadline = std::time::Duration::from_secs(5);
    let poll_interval = std::time::Duration::from_millis(100);
    let start = std::time::Instant::now();
    while start.elapsed() < deadline && crate::serve_registry::pid_is_alive(pid) {
        tokio::time::sleep(poll_interval).await;
    }

    if crate::serve_registry::pid_is_alive(pid) {
        eprintln!(
            "Server pid {pid} did not exit within {}s of receiving the signal.",
            deadline.as_secs()
        );
        std::process::exit(1);
    }

    if force {
        eprintln!(
            "Killed server pid {pid} (SIGKILL); its registry entry may remain since a killed \
             process cannot self-deregister -- run `codanna serve --reap` to prune it."
        );
    } else {
        eprintln!("Stopped server pid {pid} (SIGTERM).");
    }
}

async fn run_https_server(config: &Settings, watch: bool, bind_address: String) {
    // HTTPS mode - secure server with TLS
    tracing::info!(target: "mcp", "starting HTTPS server on {bind_address}");
    if watch || config.file_watch.enabled {
        tracing::debug!(
            target: "mcp",
            "file watching enabled with {}ms debounce",
            config.file_watch.debounce_ms
        );
    }

    // Use the HTTPS server implementation
    #[cfg(feature = "https-server")]
    {
        use crate::mcp::https_server::serve_https;
        if let Err(e) = serve_https(config.clone(), watch, bind_address).await {
            eprintln!("HTTPS server error: {e}");
            std::process::exit(1);
        }
    }

    #[cfg(not(feature = "https-server"))]
    {
        eprintln!("HTTPS server support is not compiled in.");
        eprintln!("Please rebuild with: cargo build --features https-server");
        std::process::exit(1);
    }
}

async fn run_proxy_server(config: Settings, config_path: Option<PathBuf>) {
    // Proxy mode - stdio-facing delegate that discovers/spawns a backing
    // `codanna serve --http` and relays MCP traffic to it. No IndexFacade is
    // constructed in this process (§4.5).
    eprintln!("Starting MCP server in proxy mode (stdio <-> HTTP delegate)");

    if let Err(e) = crate::mcp::proxy::serve_proxy(config, config_path).await {
        eprintln!("Proxy server error: {e}");
        std::process::exit(1);
    }
}

async fn run_http_server(config: Settings, watch: bool, bind_address: String) {
    // HTTP mode - persistent server with event-driven file watching
    eprintln!("Starting MCP server in HTTP mode");
    eprintln!("Bind address: {bind_address}");
    if watch || config.file_watch.enabled {
        eprintln!(
            "File watching: ENABLED (event-driven with {}ms debounce)",
            config.file_watch.debounce_ms
        );
    }

    // Use the HTTP server implementation
    use crate::mcp::http_server::serve_http;
    if let Err(e) = serve_http(config, watch, bind_address).await {
        eprintln!("HTTP server error: {e}");
        std::process::exit(1);
    }
}

async fn run_stdio_server(
    config: Settings,
    settings: Arc<Settings>,
    facade: IndexFacade,
    index_path: PathBuf,
    watch: bool,
    actual_watch_interval: u64,
) {
    // Acquire the stdio serve lock before doing anything else. Bound at
    // function scope so the guard removes the lockfile on return / unwind.
    // The process::exit arms below must drop it explicitly: exit skips
    // destructors and would leave the lockfile behind.
    let serve_lock = match PidLockGuard::acquire(&index_path.join("serve.lock")) {
        Ok(guard) => guard,
        Err(PidLockError::Held { pid, lock_path }) => {
            eprintln!(
                "Another codanna serve is already running for this index (PID {pid}, lock at {}).",
                crate::parsing::paths::render_absolute_path(&lock_path).display()
            );
            eprintln!();
            eprintln!("Subagents and other AI tools may have spawned a duplicate. To run multiple");
            eprintln!("clients against one index, use HTTP mode:");
            eprintln!("  codanna serve --http --watch");
            eprintln!("HTTP mode supports concurrent clients without lock conflicts.");
            eprintln!();
            eprintln!(
                "If you are sure no other codanna serve is running, remove {} and retry.",
                crate::parsing::paths::render_absolute_path(&lock_path).display()
            );
            std::process::exit(1);
        }
        Err(PidLockError::Io(e)) => {
            eprintln!(
                "Failed to acquire serve lock under {}: {e}",
                crate::parsing::paths::render_absolute_path(&index_path).display()
            );
            std::process::exit(1);
        }
    };

    // stdio mode - current implementation
    eprintln!("Starting MCP server on stdio transport");
    if watch {
        eprintln!("Index watching enabled (interval: {actual_watch_interval}s)");
    }
    eprintln!("To test: npx @modelcontextprotocol/inspector cargo run -- serve");

    // Create MCP server using the already-loaded facade
    tracing::debug!(
        target: "mcp",
        "creating server with facade - symbols: {}, semantic: {}",
        facade.symbol_count(),
        facade.has_semantic_search()
    );
    let broadcaster = Arc::new(crate::mcp::notifications::NotificationBroadcaster::new(100));
    let server =
        crate::mcp::CodeIntelligenceServer::new(facade).with_broadcaster(broadcaster.clone());

    // Load document store and attach to server (shared with watcher later)
    let document_store_arc = crate::documents::load_from_settings(&config);
    let server = if let Some(ref store_arc) = document_store_arc {
        tracing::debug!(target: "mcp", "attaching document store to server");
        server.with_document_store_arc(store_arc.clone())
    } else {
        server
    };

    // If watch mode is enabled, start the hot-reload watcher
    if watch {
        use crate::watcher::HotReloadWatcher;
        use std::time::Duration;

        let facade_arc = server.get_facade_arc();
        let watcher = HotReloadWatcher::new(
            facade_arc,
            settings.clone(),
            Duration::from_secs(actual_watch_interval),
        );

        // Spawn watcher in background
        tokio::spawn(async move {
            watcher.watch().await;
        });

        eprintln!("Hot-reload watcher started");
    }

    // Start unified file watcher if enabled
    if watch || config.file_watch.enabled {
        use crate::watcher::UnifiedWatcher;
        use crate::watcher::handlers::{CodeFileHandler, ConfigFileHandler, DocumentFileHandler};

        let workspace_root = config
            .workspace_root
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

        let settings_path = workspace_root.join(".codanna/settings.toml");
        let debounce_ms = config.file_watch.debounce_ms;
        let facade_arc = server.get_facade_arc();

        // Build unified watcher with handlers
        let mut builder = UnifiedWatcher::builder()
            .broadcaster(broadcaster.clone())
            .indexer(facade_arc.clone())
            .index_path(index_path.clone())
            .workspace_root(workspace_root.clone())
            .debounce_ms(debounce_ms)
            .refresh_on_overflow(config.file_watch.refresh_on_overflow)
            .startup_catch_up(config.file_watch.startup_catch_up);

        // Add code file handler
        builder = builder.handler(CodeFileHandler::new(
            facade_arc.clone(),
            workspace_root.clone(),
        ));

        // Add config file handler
        match ConfigFileHandler::new(settings_path.clone()) {
            Ok(config_handler) => {
                builder = builder.handler(config_handler);
            }
            Err(e) => {
                eprintln!("Failed to create config handler: {e}");
            }
        }

        // Add document handler using shared document store
        if let Some(store_arc) = document_store_arc {
            tracing::debug!(target: "mcp", "adding document handler to watcher");
            builder = builder
                .document_store(store_arc.clone())
                .chunking_config(config.documents.defaults.clone())
                .handler(DocumentFileHandler::new(store_arc, workspace_root.clone()));
        }

        // Subscribe to broadcaster for MCP notifications
        let notification_receiver = broadcaster.subscribe();
        let notification_server = server.clone();

        // Build and start the unified watcher
        match builder.build() {
            Ok(unified_watcher) => {
                tokio::spawn(async move {
                    if let Err(e) = unified_watcher.watch().await {
                        eprintln!("Unified watcher error: {e}");
                    }
                });
                eprintln!(
                    "Unified watcher started (debounce: {debounce_ms}ms, config: {})",
                    crate::parsing::paths::render_absolute_path(&settings_path).display()
                );

                // Start notification listener to forward events to MCP client
                tokio::spawn(async move {
                    notification_server
                        .start_notification_listener(notification_receiver)
                        .await;
                });
            }
            Err(e) => {
                eprintln!("Failed to start unified watcher: {e}");
            }
        }
    }

    // Start server with stdio transport
    use rmcp::{ServerHandler, ServiceExt};
    let discover_result = serde_json::to_value(rmcp::model::DiscoverResult::from_server_info(
        server.supported_protocol_versions().into_owned(),
        server.get_info(),
    ))
    .expect("DiscoverResult serializes: closed struct of strings and maps");
    let service = match server.serve(probe_tolerant_stdio(discover_result)).await {
        Ok(service) => service,
        Err(e) => {
            eprintln!("Failed to start MCP server: {e}");
            drop(serve_lock);
            std::process::exit(1);
        }
    };

    // Wait for server to complete
    if let Err(e) = service.waiting().await {
        eprintln!("MCP server error: {e}");
        drop(serve_lock);
        std::process::exit(1);
    }
}

/// Serve a degraded stdio MCP session for a gate-refused index.
/// Completes the handshake with zero tools and heal instructions;
/// never touches the index, so no serve lock is taken and no watcher
/// starts. The caller exits with the gate code when this returns.
pub async fn run_stale_stdio(stored: Option<u32>, current: u32) {
    use rmcp::{ServerHandler, ServiceExt};

    let server = crate::mcp::StaleIndexServer::new(stored, current);
    let discover_result = serde_json::to_value(rmcp::model::DiscoverResult::from_server_info(
        server.supported_protocol_versions().into_owned(),
        server.get_info(),
    ))
    .expect("DiscoverResult serializes: closed struct of strings and maps");
    match server.serve(probe_tolerant_stdio(discover_result)).await {
        Ok(service) => {
            if let Err(e) = service.waiting().await {
                eprintln!("Degraded MCP server error: {e}");
            }
        }
        Err(e) => {
            eprintln!("Failed to start degraded MCP server: {e}");
        }
    }
}

#[cfg(test)]
mod server_mode_selection_tests {
    use super::resolve_server_mode;

    // These tests exercise `resolve_server_mode` in isolation: it is a pure
    // function of (https, http, proxy, config_mode) -> &'static str with no
    // I/O, so precedence can be asserted hermetically without spawning any
    // process or binding any port.
    //
    // The stdio<->HTTP delegating pump in `mcp::proxy` is intentionally NOT
    // unit tested here: exercising it for real requires a live backing HTTP
    // MCP server (a spawned child process) and a connected stdio client on
    // the other end, which is an integration/manual validation concern, not
    // a hermetic unit test.

    #[test]
    fn cli_proxy_flag_selects_proxy() {
        assert_eq!(resolve_server_mode(false, false, true, "stdio"), "proxy");
    }

    #[test]
    fn config_mode_proxy_selects_proxy_with_bare_serve() {
        // Bare `codanna serve` (no CLI transport flags) must be able to
        // default to proxy via settings.toml `server.mode = "proxy"`.
        assert_eq!(resolve_server_mode(false, false, false, "proxy"), "proxy");
    }

    #[test]
    fn cli_http_flag_wins_over_config_mode_proxy() {
        assert_eq!(resolve_server_mode(false, true, false, "proxy"), "http");
    }

    #[test]
    fn cli_https_flag_wins_over_everything() {
        assert_eq!(resolve_server_mode(true, false, false, "proxy"), "https");
        assert_eq!(resolve_server_mode(true, false, true, "http"), "https");
    }

    #[test]
    fn config_mode_http_selects_http_without_cli_flags() {
        assert_eq!(resolve_server_mode(false, false, false, "http"), "http");
    }

    #[test]
    fn default_is_stdio() {
        assert_eq!(resolve_server_mode(false, false, false, "stdio"), "stdio");
    }
}
