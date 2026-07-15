//! CodeIntelligenceServer: construction, server plumbing, custom requests.

use rmcp::model::ErrorData as McpError;
use rmcp::model::*;
use rmcp::{
    ServerHandler,
    handler::server::router::tool::ToolRouter,
    service::{Peer, RequestContext, RoleServer, ServiceError},
    tool_handler,
};
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

use crate::Settings;
use crate::documents::DocumentStore;
use crate::indexing::facade::IndexFacade;

/// Generate guidance for MCP tool responses
pub(crate) fn generate_mcp_guidance(
    settings: &Settings,
    tool: &str,
    result_count: usize,
) -> Option<String> {
    use crate::io::guidance_engine::generate_guidance_from_config;
    generate_guidance_from_config(&settings.guidance, tool, None, result_count)
}

/// Format a Unix timestamp as relative time (e.g., "2 hours ago")
pub fn format_relative_time(timestamp: u64) -> String {
    use chrono::{DateTime, Utc};

    let now = Utc::now();
    let then = DateTime::from_timestamp(timestamp as i64, 0).unwrap_or_else(Utc::now);

    let diff = (now.timestamp() - then.timestamp()) as u64;

    if diff < 60 {
        "just now".to_string()
    } else if diff < 3600 {
        let mins = diff / 60;
        format!("{} minute{} ago", mins, if mins == 1 { "" } else { "s" })
    } else if diff < 86400 {
        let hours = diff / 3600;
        format!("{} hour{} ago", hours, if hours == 1 { "" } else { "s" })
    } else if diff < 604800 {
        let days = diff / 86400;
        format!("{} day{} ago", days, if days == 1 { "" } else { "s" })
    } else {
        // For older dates, show the actual formatted date
        then.format("%Y-%m-%d").to_string()
    }
}

#[derive(Clone)]
pub struct CodeIntelligenceServer {
    pub facade: Arc<RwLock<IndexFacade>>,
    pub document_store: Option<Arc<RwLock<DocumentStore>>>,
    tool_router: ToolRouter<Self>,
    pub(super) peer: Arc<Mutex<Option<Peer<RoleServer>>>>,
}

impl CodeIntelligenceServer {
    pub fn new(facade: IndexFacade) -> Self {
        Self {
            facade: Arc::new(RwLock::new(facade)),
            document_store: None,
            tool_router: Self::symbols_router() + Self::search_router() + Self::admin_router(),
            peer: Arc::new(Mutex::new(None)),
        }
    }

    /// Create server from an already-loaded facade (most efficient)
    pub fn from_facade(facade: Arc<RwLock<IndexFacade>>) -> Self {
        Self {
            facade,
            document_store: None,
            tool_router: Self::symbols_router() + Self::search_router() + Self::admin_router(),
            peer: Arc::new(Mutex::new(None)),
        }
    }

    /// Create server with existing facade and settings (for HTTP server)
    pub fn new_with_facade(facade: Arc<RwLock<IndexFacade>>, _settings: Arc<Settings>) -> Self {
        Self {
            facade,
            document_store: None,
            tool_router: Self::symbols_router() + Self::search_router() + Self::admin_router(),
            peer: Arc::new(Mutex::new(None)),
        }
    }

    /// Add document store for document search capability
    pub fn with_document_store(mut self, store: DocumentStore) -> Self {
        self.document_store = Some(Arc::new(RwLock::new(store)));
        self
    }

    /// Add document store from existing Arc (for sharing with watcher)
    pub fn with_document_store_arc(mut self, store: Arc<RwLock<DocumentStore>>) -> Self {
        self.document_store = Some(store);
        self
    }

    /// Get a reference to the facade Arc for external management (e.g., hot-reload)
    pub fn get_facade_arc(&self) -> Arc<RwLock<IndexFacade>> {
        self.facade.clone()
    }

    /// Send a notification when a file is re-indexed
    pub async fn notify_file_reindexed(&self, file_path: &str) {
        let peer_guard = self.peer.lock().await;
        if let Some(peer) = peer_guard.as_ref() {
            // Send a resource updated notification
            let _ = peer
                .notify_resource_updated(ResourceUpdatedNotificationParam::new(format!(
                    "file://{file_path}"
                )))
                .await;

            // Also send a logging message for visibility. Logging is deprecated by
            // SEP-2577; keep emitting it for client compatibility until rmcp removes it.
            #[allow(deprecated)]
            let _ = peer
                .notify_logging_message(
                    LoggingMessageNotificationParam::new(
                        LoggingLevel::Info,
                        serde_json::json!({
                            "action": "re-indexed",
                            "file": file_path
                        }),
                    )
                    .with_logger("codanna"),
                )
                .await;
        }
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for CodeIntelligenceServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .build(),
        )
        .with_server_info(
            Implementation::new("codanna", env!("CARGO_PKG_VERSION"))
                .with_title("Codanna Code Intelligence")
                .with_website_url("https://github.com/bartolli/codanna"),
        )
        .with_instructions(
            "This server provides code intelligence tools for analyzing this codebase. \
            WORKFLOW: Start with 'semantic_search_with_context' or 'semantic_search_docs' to anchor on the right files and APIs - they provide the highest-quality context. \
            Then use 'find_symbol' and 'search_symbols' to lock onto exact files and kinds. \
            Treat 'get_calls', 'find_callers', and 'analyze_impact' as hints; confirm with code reading or tighter queries (unique names, kind filters). \
            Use 'search_documents' to find relevant project documentation (markdown files). \
            Use 'get_index_info' to understand what's indexed.",
        )
    }

    async fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, McpError> {
        // Register client capabilities (required for MCP handshake)
        if context.peer.peer_info().is_none() {
            context.peer.set_peer_info(request);
        }

        // Store the peer reference for sending notifications
        let mut peer_guard = self.peer.lock().await;
        *peer_guard = Some(context.peer.clone());

        // Return the server info
        Ok(self.get_info())
    }

    async fn on_custom_request(
        &self,
        request: CustomRequest,
        _context: RequestContext<RoleServer>,
    ) -> Result<CustomResult, McpError> {
        match request.method.as_str() {
            "requests/codanna/force-reindex" => self.handle_force_reindex(request).await,
            "requests/codanna/index-stats" => self.handle_index_stats().await,
            _ => Err(McpError::new(
                ErrorCode::METHOD_NOT_FOUND,
                format!("Unknown method: {}", request.method),
                None,
            )),
        }
    }
}

// Custom request handlers
impl CodeIntelligenceServer {
    /// Handle force-reindex request
    async fn handle_force_reindex(&self, request: CustomRequest) -> Result<CustomResult, McpError> {
        use std::time::Instant;

        let start = Instant::now();

        // Parse optional paths parameter
        let paths: Option<Vec<String>> = request
            .params
            .as_ref()
            .and_then(|p| p.get("paths"))
            .and_then(|v| serde_json::from_value(v.clone()).ok());

        // Parse optional force parameter (defaults to false for backward compatibility)
        let force: bool = request
            .params
            .as_ref()
            .and_then(|p| p.get("force"))
            .and_then(|v| serde_json::from_value::<bool>(v.clone()).ok())
            .unwrap_or(false);

        let (reindexed, symbols) = self.run_reindex(paths, force).await?;

        let duration_ms = start.elapsed().as_millis() as u64;

        Ok(CustomResult(serde_json::json!({
            "reindexed": reindexed,
            "symbols": symbols,
            "duration_ms": duration_ms
        })))
    }

    /// Run a reindex over the given paths (or all indexed_paths from settings if None),
    /// optionally clearing the index first when `force` is true.
    /// Returns (reindexed_files, symbol_count).
    ///
    /// During a force reindex, concurrent readers are no longer blocked but may
    /// transiently observe an empty/repopulating index (clear-then-rebuild is not
    /// atomic; atomic build-and-swap is intentionally out of scope for this change).
    pub(crate) async fn run_reindex(
        &self,
        paths: Option<Vec<String>>,
        force: bool,
    ) -> Result<(usize, usize), McpError> {
        self.run_reindex_with_phase2_signal(paths, force, None)
            .await
    }

    /// Test-only entry point that fires `phase2_started` the moment the
    /// write-lock-held phase 1 (optional clear + handle snapshot) has
    /// completed and the write guard has been dropped, i.e. immediately
    /// before the off-lock phase 2 walk is kicked off. Lets a test wait
    /// for the reindex task to have actually released the write lock
    /// before sampling `try_read`/`try_write`, rather than racing the
    /// task's own scheduling.
    #[cfg(test)]
    pub(crate) async fn run_reindex_for_test(
        &self,
        paths: Option<Vec<String>>,
        force: bool,
        phase2_started: tokio::sync::oneshot::Sender<()>,
    ) -> Result<(usize, usize), McpError> {
        self.run_reindex_with_phase2_signal(paths, force, Some(phase2_started))
            .await
    }

    async fn run_reindex_with_phase2_signal(
        &self,
        paths: Option<Vec<String>>,
        force: bool,
        #[cfg_attr(not(test), allow(unused_variables))] phase2_started: Option<
            tokio::sync::oneshot::Sender<()>,
        >,
    ) -> Result<(usize, usize), McpError> {
        // Bounds the number of explicitly-passed paths only (protects against
        // unbounded request payloads); it must never silently skip files
        // discovered while walking a given path/directory.
        const MAX_REINDEX_PATHS: usize = 1024;
        if let Some(paths) = &paths
            && paths.len() > MAX_REINDEX_PATHS
        {
            return Err(McpError::new(
                ErrorCode::INVALID_PARAMS,
                format!(
                    "Too many paths requested for reindex: {} (max {MAX_REINDEX_PATHS})",
                    paths.len()
                ),
                None,
            ));
        }

        // Phase 1: brief write lock to optionally clear the index and snapshot
        // cloneable handles for the off-lock reindex walk.
        let handles = {
            let mut indexer = self.facade.write().await;

            if paths.is_none() && force {
                indexer.clear_index().map_err(|e| {
                    McpError::new(
                        ErrorCode::INTERNAL_ERROR,
                        format!("Failed to clear index before force reindex: {e}"),
                        None,
                    )
                })?;
            }

            indexer.snapshot_reindex_handles().map_err(|e| {
                McpError::new(
                    ErrorCode::INTERNAL_ERROR,
                    format!("Failed to snapshot reindex handles: {e}"),
                    None,
                )
            })?
        };

        // The write guard above is dropped at the end of its block, strictly
        // before this point. Signal test observers that phase 2 (the
        // off-lock walk) is about to begin.
        if let Some(tx) = phase2_started {
            let _ = tx.send(());
        }

        // Phase 2: run the heavy reindex walk with no facade lock held.
        let outcome = tokio::task::spawn_blocking(move || handles.run(paths, force))
            .await
            .map_err(|e| {
                McpError::new(
                    ErrorCode::INTERNAL_ERROR,
                    format!("reindex task panicked: {e}"),
                    None,
                )
            })?
            .map_err(|e| {
                McpError::new(
                    ErrorCode::INTERNAL_ERROR,
                    format!("Reindex failed: {e}"),
                    None,
                )
            })?;

        // Phase 3: brief write lock to record any newly indexed directories.
        {
            let mut indexer = self.facade.write().await;
            for dir in &outcome.indexed_dirs {
                indexer.add_indexed_path(dir);
            }
        }

        Ok((outcome.reindexed, outcome.symbol_count))
    }

    /// Handle index-stats request
    async fn handle_index_stats(&self) -> Result<CustomResult, McpError> {
        let indexer = self.facade.read().await;

        let semantic = if let Some(metadata) = indexer.get_semantic_metadata() {
            let live_count = indexer.semantic_search_embedding_count();
            serde_json::json!({
                "enabled": true,
                "model": metadata.model_name,
                "embeddings": live_count,
                "dimensions": metadata.dimension
            })
        } else {
            serde_json::json!({
                "enabled": false
            })
        };

        Ok(CustomResult(serde_json::json!({
            "symbols": indexer.symbol_count(),
            "files": indexer.file_count(),
            "relationships": indexer.relationship_count(),
            "semantic": semantic
        })))
    }

    /// Send a custom notification to the connected client
    pub async fn notify_custom(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<(), ServiceError> {
        let peer_guard = self.peer.lock().await;
        if let Some(peer) = peer_guard.as_ref() {
            peer.send_notification(ServerNotification::CustomNotification(
                CustomNotification::new(method, Some(params)),
            ))
            .await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Settings;
    use std::time::Duration;

    /// Terminal-state / provenance regression for `run_reindex`'s off-lock
    /// phase 2 walk. `run_reindex` is `pub(crate)` and the facade lives
    /// behind `Arc<RwLock<IndexFacade>>` on the server, so only an in-module
    /// test can reach it directly (tests/ integration cannot).
    ///
    /// Drives a force reindex over a non-trivial fixture (many source
    /// files) and asserts a read guard is repeatedly acquirable via
    /// `try_read()` while the reindex is still in flight. This is the
    /// discriminating check: a long-write-hold implementation (the entire
    /// reindex under one write lock) would fail `try_read()` for the whole
    /// wall-clock duration of the reindex, while the current
    /// snapshot-handles-then-off-lock-walk implementation releases the
    /// write lock before the heavy walk runs. A symbol-count check alone
    /// would not falsify a long-write-hold implementation.
    ///
    /// Crucially, sampling only begins *after* `run_reindex_for_test`
    /// signals (via a oneshot channel fired the instant phase 1's write
    /// guard has been dropped) that phase 2 is about to start. Without this
    /// synchronization, `tokio::spawn` merely schedules the reindex task; a
    /// naive poll loop could observe `try_read()` succeeding purely because
    /// the spawned task had not yet been polled at all (the pre-start
    /// window), which trivially succeeds under BOTH the correct
    /// off-lock implementation and a buggy long-write-hold implementation,
    /// and therefore would not discriminate between them. Requiring several
    /// consecutive successful samples, spread across the task's measured
    /// in-flight lifetime and gated behind the phase-2-started signal,
    /// ensures the read guard is genuinely acquirable *during* the walk
    /// rather than merely before the task has started running.
    #[tokio::test]
    async fn run_reindex_releases_write_lock_during_off_lock_walk() {
        let temp = tempfile::tempdir().expect("create temp root");
        let source_dir = temp.path().join("src");
        std::fs::create_dir_all(&source_dir).expect("create source dir");

        // Enough files (each with multiple symbols) that the off-lock walk
        // in phase 2 takes long enough to reliably observe a read guard
        // acquired while it is still in flight.
        const FILE_COUNT: usize = 300;
        for i in 0..FILE_COUNT {
            std::fs::write(
                source_dir.join(format!("mod_{i}.py")),
                format!(
                    "def fn_{i}_a():\n    pass\n\n\ndef fn_{i}_b():\n    pass\n\n\nclass Cls{i}:\n    def method(self):\n        pass\n"
                ),
            )
            .unwrap_or_else(|e| panic!("write mod_{i}.py fixture: {e}"));
        }

        let mut settings = Settings {
            index_path: temp.path().join("index"),
            workspace_root: None,
            ..Default::default()
        };
        settings.indexing.indexed_paths = vec![source_dir.clone()];

        // Ground truth for the expected terminal symbol count: index the
        // same fixture directly via the pre-existing `index_directory` path
        // on an independent facade/index dir, rather than hand-deriving a
        // formula that depends on parser internals (e.g. whether `self`
        // parameters are indexed as symbols).
        let expected_symbol_count = {
            let expected_settings = Settings {
                index_path: temp.path().join("expected_index"),
                workspace_root: None,
                ..Default::default()
            };
            let mut expected_facade =
                IndexFacade::new(Arc::new(expected_settings)).expect("create ground-truth facade");
            expected_facade
                .index_directory(&source_dir, false)
                .expect("index fixture directory for ground truth");
            expected_facade.symbol_count()
        };
        assert!(
            expected_symbol_count > 0,
            "fixture must produce a non-zero ground-truth symbol count"
        );

        let facade =
            IndexFacade::new(Arc::new(settings)).expect("create facade over temp index dir");
        let server = CodeIntelligenceServer::new(facade);

        let (phase2_started_tx, phase2_started_rx) = tokio::sync::oneshot::channel();

        let reindex_server = server.clone();
        let reindex_task = tokio::spawn(async move {
            reindex_server
                .run_reindex_for_test(None, true, phase2_started_tx)
                .await
        });

        // Wait for the reindex task to signal that phase 1 (the brief write
        // lock used to optionally clear the index and snapshot reindex
        // handles) has completed and its write guard has been dropped. This
        // rules out the pre-start window where `try_read()` would trivially
        // succeed simply because the spawned task had not yet been polled.
        phase2_started_rx
            .await
            .expect("run_reindex_for_test must signal phase 2 start before returning");

        // Now sample `try_read()` while the reindex task is still in
        // flight. Require several consecutive successes spread over the
        // task's measured lifetime (rather than a single success) so a
        // regression that re-introduces a long-write-hold -- which would
        // still fire the phase-2-started signal, since that call is
        // unconditional, but would keep the write guard alive across the
        // walk -- reliably fails this assertion instead of getting lucky on
        // a single sample.
        const REQUIRED_CONSECUTIVE_SUCCESSES: u32 = 5;
        let mut consecutive_successes = 0u32;
        let mut attempts = 0;
        while !reindex_task.is_finished() && attempts < 200_000 {
            if server.facade.try_read().is_ok() {
                consecutive_successes += 1;
                if consecutive_successes >= REQUIRED_CONSECUTIVE_SUCCESSES {
                    break;
                }
            } else {
                consecutive_successes = 0;
            }
            attempts += 1;
            if attempts % 100 == 0 {
                tokio::time::sleep(Duration::from_micros(50)).await;
            } else {
                tokio::task::yield_now().await;
            }
        }

        let acquired_while_in_flight = consecutive_successes >= REQUIRED_CONSECUTIVE_SUCCESSES;

        let result = reindex_task.await.expect("reindex task must not panic");

        assert!(
            acquired_while_in_flight,
            "expected try_read() to succeed {REQUIRED_CONSECUTIVE_SUCCESSES} times in a row \
             while run_reindex was still in flight (after phase 2 started); with a \
             long-write-hold implementation try_read() fails for the reindex's entire \
             in-flight duration"
        );

        let (_reindexed, symbol_count) = result.expect("run_reindex must return Ok");
        assert_eq!(
            symbol_count, expected_symbol_count,
            "expected run_reindex to produce the ground-truth symbol count for the fixture"
        );
    }
}
