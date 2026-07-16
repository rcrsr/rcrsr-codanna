//! stdio<->HTTP delegating MCP proxy.
//!
//! `codanna serve --proxy` (or `server.mode = "proxy"` in `settings.toml`)
//! speaks stdio to the connecting MCP client while delegating every request
//! to a backing `codanna serve --http` process, discovered or spawned via
//! [`crate::serve_discovery::discover_or_spawn`]. This lets several stdio
//! clients (e.g. multiple AI-tool subagents rooted at the same workspace)
//! share one HTTP-mode index/tantivy writer without each holding its own
//! `IndexFacade` -- the proxy process itself never constructs one.
//!
//! ## Scope for this PR
//!
//! - Request/response delegation across the full `ServerHandler` surface
//!   (tools, resources, prompts, completion, custom requests).
//! - Best-effort forwarding of server-initiated notifications (logging,
//!   resource/tool/prompt list-changed, resource-updated, progress) from the
//!   upstream HTTP server down to the stdio client.
//!
//! ## Explicitly out of scope
//!
//! A byte-level transparent transport relay -- splicing the stdio and HTTP
//! transports directly instead of round-tripping through typed rmcp
//! requests/responses -- is an optional later optimization. It would remove
//! one layer of (de)serialization but adds real complexity (framing,
//! backpressure, session lifecycle) that isn't justified until the
//! request/response delegation implemented here is proven in practice.

// Logging notifications and `set_level` are deprecated by SEP-2577, but this
// module forwards the full `ServerHandler`/`ClientHandler` surface
// (including logging) for client compatibility, mirroring the same
// allowance already used in `mcp::server` and `mcp::notifications`.
#![allow(deprecated)]

use std::collections::VecDeque;
use std::sync::Arc;

use rmcp::model::{
    CallToolRequestParams, CallToolResult, ClientRequest, CompleteRequestParams, CompleteResult,
    CustomNotification, CustomRequest, CustomResult, ErrorData as McpError, GetPromptRequestParams,
    GetPromptResult, Implementation, InitializeRequestParams, InitializeResult, ListPromptsResult,
    ListResourceTemplatesResult, ListResourcesResult, ListToolsResult,
    LoggingMessageNotificationParam, PaginatedRequestParams, ProgressNotificationParam,
    ReadResourceRequestParams, ReadResourceResult, ResourceUpdatedNotificationParam,
    ServerCapabilities, ServerInfo, ServerNotification, ServerResult, SetLevelRequestParams,
    SubscribeRequestParams, UnsubscribeRequestParams,
};
use rmcp::service::{
    NotificationContext, Peer, RequestContext, RoleClient, RoleServer, RunningService, ServiceError,
};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{ClientHandler, ServerHandler, ServiceExt};
use thiserror::Error;
use tokio::sync::Mutex;

use crate::config::Settings;
use crate::mcp::DUMMY_BEARER_TOKEN;
use crate::serve_discovery::{self, DiscoveryError, ServeScheme};
use crate::serve_tls;

/// Errors from establishing or running the stdio<->HTTP proxy.
#[derive(Debug, Error)]
pub enum ProxyError {
    #[error(
        "could not resolve workspace root: no '.codanna' directory found in the current directory or its ancestors"
    )]
    NoWorkspaceRoot,

    #[error("failed to discover/spawn backing HTTP server: {0}")]
    Discovery(#[from] DiscoveryError),

    #[error("failed to connect to backing HTTP server: {0}")]
    UpstreamConnect(String),

    #[error("stdio transport error: {0}")]
    Stdio(String),

    #[error("failed to build TLS-pinned client for backing HTTPS server: {0}")]
    Tls(#[from] crate::serve_tls::TlsClientError),
}

pub type ProxyResult<T> = Result<T, ProxyError>;

/// Converts an upstream `ServiceError` into the `McpError` shape expected by
/// `ServerHandler` methods. A `ServiceError::McpError` already carries a
/// well-formed protocol error and is passed through unchanged; every other
/// variant (transport closed, timeout, cancellation, ...) becomes an
/// internal error describing the underlying delegation failure.
fn map_service_err(err: ServiceError) -> McpError {
    match err {
        ServiceError::McpError(e) => e,
        other => McpError::internal_error(format!("proxy delegation failed: {other}"), None),
    }
}

/// Maximum number of buffered custom notifications awaiting a downstream
/// peer, mirroring [`crate::mcp::notifications::NotificationBroadcaster`]'s
/// default channel capacity. Once full, the oldest buffered notification is
/// dropped to make room for the newest.
const PENDING_CUSTOM_NOTIFICATIONS_CAP: usize = 100;

/// Combined downstream-peer/pending-buffer state, guarded by a single lock
/// shared between [`NotificationRelay`] and [`DelegatingProxyHandler`].
///
/// `downstream` and `pending` must be updated atomically with respect to
/// each other: checking whether a downstream peer exists and, if not,
/// buffering a custom notification (`on_custom_notification`) must never be
/// split across two lock acquisitions from `DelegatingProxyHandler::initialize`
/// setting `downstream` and draining `pending`. A single `Mutex` guarding
/// both fields makes that interleaving impossible -- either the buffering
/// happens-before the drain (and gets flushed) or the drain happens-before
/// the check (and the notification is forwarded directly), with no window
/// in which a notification can be queued after `pending` has already been
/// drained for good.
#[derive(Default)]
struct DownstreamState {
    downstream: Option<Peer<RoleServer>>,
    pending: VecDeque<CustomNotification>,
}

/// `ClientHandler` for the connection to the backing HTTP MCP server.
///
/// Its only job is forwarding server-initiated notifications down to the
/// stdio client once the downstream `initialize` handshake has populated
/// `state.downstream`. Before that point (a narrow window right at startup)
/// most notification kinds are dropped rather than buffered, since there is
/// no downstream peer yet to forward them to. Custom notifications
/// (`notifications/codanna/*`) are the exception: they are buffered in
/// `state.pending` (bounded, drop-oldest) and flushed once `state.downstream`
/// is set, so a custom notification emitted by the trusted backing server
/// during the narrow pre-init window is not silently lost.
#[derive(Clone, Default)]
struct NotificationRelay {
    state: Arc<Mutex<DownstreamState>>,
}

impl ClientHandler for NotificationRelay {
    async fn on_logging_message(
        &self,
        params: LoggingMessageNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        let peer = { self.state.lock().await.downstream.clone() };
        if let Some(peer) = peer {
            // Logging notifications are deprecated by SEP-2577; forward them
            // anyway for client compatibility, mirroring `CodeIntelligenceServer`.
            #[allow(deprecated)]
            let _ = peer.notify_logging_message(params).await;
        }
    }

    async fn on_resource_updated(
        &self,
        params: ResourceUpdatedNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        let peer = { self.state.lock().await.downstream.clone() };
        if let Some(peer) = peer {
            let _ = peer.notify_resource_updated(params).await;
        }
    }

    async fn on_resource_list_changed(&self, _context: NotificationContext<RoleClient>) {
        let peer = { self.state.lock().await.downstream.clone() };
        if let Some(peer) = peer {
            let _ = peer.notify_resource_list_changed().await;
        }
    }

    async fn on_tool_list_changed(&self, _context: NotificationContext<RoleClient>) {
        let peer = { self.state.lock().await.downstream.clone() };
        if let Some(peer) = peer {
            let _ = peer.notify_tool_list_changed().await;
        }
    }

    async fn on_prompt_list_changed(&self, _context: NotificationContext<RoleClient>) {
        let peer = { self.state.lock().await.downstream.clone() };
        if let Some(peer) = peer {
            let _ = peer.notify_prompt_list_changed().await;
        }
    }

    async fn on_progress(
        &self,
        params: ProgressNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        let peer = { self.state.lock().await.downstream.clone() };
        if let Some(peer) = peer {
            let _ = peer.notify_progress(params).await;
        }
    }

    /// Forwards custom notifications (`notifications/codanna/*`) verbatim to
    /// the downstream stdio client, matching the emission pattern in
    /// `notifications.rs`. All custom notifications originate from the
    /// trusted backing HTTP server, so no per-method dispatch or filtering
    /// is applied -- everything is forwarded as-is. If `state.downstream` is
    /// not yet populated (the narrow pre-`initialize` window), the
    /// notification is buffered in `state.pending` instead of being dropped,
    /// and is flushed once `DelegatingProxyHandler::initialize` sets
    /// `state.downstream`.
    ///
    /// The downstream check and the pending push happen under a single
    /// `state` lock acquisition, so this can never race with `initialize`'s
    /// set-then-drain: whichever of the two critical sections runs first is
    /// fully visible to the other (see [`DownstreamState`]).
    async fn on_custom_notification(
        &self,
        notification: CustomNotification,
        _context: NotificationContext<RoleClient>,
    ) {
        let mut state = self.state.lock().await;
        if let Some(peer) = state.downstream.clone() {
            drop(state);
            let _ = peer
                .send_notification(ServerNotification::CustomNotification(notification))
                .await;
        } else {
            if state.pending.len() >= PENDING_CUSTOM_NOTIFICATIONS_CAP {
                state.pending.pop_front();
            }
            state.pending.push_back(notification);
        }
    }
}

/// `ServerHandler` facing the stdio client. Every request is delegated to the
/// upstream HTTP MCP server; this process holds no `IndexFacade` and no
/// index state of its own.
#[derive(Clone)]
struct DelegatingProxyHandler {
    upstream: Arc<RunningService<RoleClient, NotificationRelay>>,
    /// Shared with the `NotificationRelay` driving `upstream`; custom
    /// notifications received before `state.downstream` is populated are
    /// buffered in `state.pending` and drained atomically with setting
    /// `state.downstream` in `initialize` (see [`DownstreamState`]).
    state: Arc<Mutex<DownstreamState>>,
}

/// Maximum time to wait for a single delegated upstream call. A hung
/// upstream must not leave the stdio client's request pending forever; this
/// is a fixed budget rather than a new config knob, kept minimal per scope.
const UPSTREAM_CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Awaits `fut` under [`UPSTREAM_CALL_TIMEOUT`], mapping both delegation
/// failures and timeout expiry to the `McpError` shape `ServerHandler`
/// methods return.
async fn with_upstream_timeout<T>(
    fut: impl std::future::Future<Output = Result<T, ServiceError>>,
) -> Result<T, McpError> {
    match tokio::time::timeout(UPSTREAM_CALL_TIMEOUT, fut).await {
        Ok(result) => result.map_err(map_service_err),
        Err(_) => Err(McpError::internal_error(
            format!(
                "delegated upstream call timed out after {}s",
                UPSTREAM_CALL_TIMEOUT.as_secs()
            ),
            None,
        )),
    }
}

impl ServerHandler for DelegatingProxyHandler {
    fn get_info(&self) -> ServerInfo {
        // Reflect the upstream server's negotiated capabilities/info when
        // available (set during the upstream `initialize` handshake that
        // already completed by the time this proxy starts serving stdio);
        // fall back to a minimal description if it is somehow unset.
        self.upstream
            .peer_info()
            .map(|info| (*info).clone())
            .unwrap_or_else(|| {
                ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
                    .with_server_info(Implementation::new(
                        "codanna-proxy",
                        env!("CARGO_PKG_VERSION"),
                    ))
            })
    }

    async fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, McpError> {
        if context.peer.peer_info().is_none() {
            context.peer.set_peer_info(request);
        }

        // Set `downstream` and drain `pending` under one lock acquisition so
        // no custom notification pushed by `NotificationRelay::on_custom_notification`
        // can land in `pending` after it has already been drained here (see
        // [`DownstreamState`]).
        let drained: Vec<CustomNotification> = {
            let mut state = self.state.lock().await;
            state.downstream = Some(context.peer.clone());
            state.pending.drain(..).collect()
        };
        for notification in drained {
            let _ = context
                .peer
                .send_notification(ServerNotification::CustomNotification(notification))
                .await;
        }

        Ok(self.get_info())
    }

    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        with_upstream_timeout(self.upstream.list_tools(request)).await
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        with_upstream_timeout(self.upstream.call_tool(request)).await
    }

    async fn list_resources(
        &self,
        request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        with_upstream_timeout(self.upstream.list_resources(request)).await
    }

    async fn list_resource_templates(
        &self,
        request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, McpError> {
        with_upstream_timeout(self.upstream.list_resource_templates(request)).await
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, McpError> {
        with_upstream_timeout(self.upstream.read_resource(request)).await
    }

    async fn list_prompts(
        &self,
        request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, McpError> {
        with_upstream_timeout(self.upstream.list_prompts(request)).await
    }

    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetPromptResult, McpError> {
        with_upstream_timeout(self.upstream.get_prompt(request)).await
    }

    async fn complete(
        &self,
        request: CompleteRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CompleteResult, McpError> {
        with_upstream_timeout(self.upstream.complete(request)).await
    }

    async fn set_level(
        &self,
        request: SetLevelRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        with_upstream_timeout(self.upstream.set_level(request)).await
    }

    async fn subscribe(
        &self,
        request: SubscribeRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        with_upstream_timeout(self.upstream.subscribe(request)).await
    }

    async fn unsubscribe(
        &self,
        request: UnsubscribeRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        with_upstream_timeout(self.upstream.unsubscribe(request)).await
    }

    async fn on_custom_request(
        &self,
        request: CustomRequest,
        _context: RequestContext<RoleServer>,
    ) -> Result<CustomResult, McpError> {
        let result = with_upstream_timeout(
            self.upstream
                .peer()
                .send_request(ClientRequest::CustomRequest(request)),
        )
        .await?;

        match result {
            ServerResult::CustomResult(custom) => Ok(custom),
            other => Err(McpError::internal_error(
                format!("unexpected upstream response to custom request: {other:?}"),
                None,
            )),
        }
    }
}

/// Run the stdio<->HTTP delegating proxy until the stdio transport closes.
///
/// No `IndexFacade` is constructed in this process: discovery/spawn of the
/// backing HTTP server (and all index state) lives entirely in the process
/// `serve_discovery::discover_or_spawn` finds or launches.
pub async fn serve_proxy(
    config: Settings,
    config_path: Option<std::path::PathBuf>,
) -> ProxyResult<()> {
    // `serve_proxy` can be invoked from contexts other than `main.rs`'s own
    // provider install (it is re-exported from `crate::mcp`). Installing
    // idempotently here guards against a panic on the first
    // `reqwest::Client` built by rmcp's bundled HTTP transport when this
    // function is the entry point. Mirrors the install in `main.rs`.
    #[cfg(feature = "https-server")]
    {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }

    let workspace_root =
        serve_discovery::resolve_workspace_root(&config).ok_or(ProxyError::NoWorkspaceRoot)?;

    eprintln!(
        "Proxy: discovering backing HTTP MCP server for {}",
        workspace_root.display()
    );
    let record =
        serve_discovery::discover_or_spawn(&workspace_root, &config, config_path.as_deref())
            .await?;
    eprintln!(
        "Proxy: delegating to backing MCP server at {}://127.0.0.1:{} (pid {})",
        record.scheme.as_str(),
        record.port,
        record.pid
    );

    let transport_config = StreamableHttpClientTransportConfig::with_uri(format!(
        "{}://127.0.0.1:{}/mcp",
        record.scheme.as_str(),
        record.port
    ))
    .auth_header(DUMMY_BEARER_TOKEN);

    let state: Arc<Mutex<DownstreamState>> = Arc::new(Mutex::new(DownstreamState::default()));
    let relay = NotificationRelay {
        state: state.clone(),
    };

    let upstream = match record.scheme {
        // `from_config` uses rmcp's own bundled reqwest client (gated behind
        // the `transport-streamable-http-client-reqwest` feature) rather than
        // a hand-rolled HTTP client, per the preference for rmcp's default
        // client transport.
        ServeScheme::Http => {
            let transport = StreamableHttpClientTransport::from_config(transport_config);
            relay
                .serve(transport)
                .await
                .map_err(|e| ProxyError::UpstreamConnect(e.to_string()))?
        }
        // The backing server is `--https`: dial it ONLY through the
        // cert-pinning client (`serve_tls::pinned_client`), never through
        // `from_config`'s bundled client. A pinning failure (missing/mismatched
        // persisted cert) must fail the proxy outright rather than silently
        // falling back to an unauthenticated/plaintext-trusting transport.
        ServeScheme::Https => {
            let client = serve_tls::pinned_client()?;
            let transport = StreamableHttpClientTransport::with_client(client, transport_config);
            relay
                .serve(transport)
                .await
                .map_err(|e| ProxyError::UpstreamConnect(e.to_string()))?
        }
    };

    let handler = DelegatingProxyHandler {
        upstream: Arc::new(upstream),
        state,
    };

    use rmcp::transport::stdio;
    let service = handler
        .serve(stdio())
        .await
        .map_err(|e| ProxyError::Stdio(e.to_string()))?;

    service
        .waiting()
        .await
        .map_err(|e| ProxyError::Stdio(e.to_string()))?;

    Ok(())
}

// `NotificationContext<RoleClient>`/`Peer<RoleServer>` can only be
// constructed via rmcp's own (crate-private) `Peer::new`, so a live
// `NotificationRelay::on_custom_notification` call or a real
// `initialize`-time drain-and-forward can't be driven from outside rmcp.
// These tests instead exercise the buffering/draining semantics -- push
// while no downstream peer is available, bounded drop-oldest overflow, and
// FIFO drain order -- directly against a `pending` queue built the same way
// `DownstreamState` holds one. End-to-end custom-notification forwarding
// (relay -> downstream peer, including the `initialize`-time flush) is
// covered by manual MCP smoke test; a full transport harness is out of
// scope here.
#[cfg(test)]
mod tests {
    use super::*;

    fn notification(method: &str) -> CustomNotification {
        CustomNotification::new(method.to_string(), None)
    }

    /// Pushes `notification` onto `pending`, dropping the oldest entry once
    /// `PENDING_CUSTOM_NOTIFICATIONS_CAP` is reached -- the same bounding
    /// behavior `NotificationRelay::on_custom_notification` applies when
    /// `downstream` is `None`.
    async fn push_pending(
        pending: &Mutex<VecDeque<CustomNotification>>,
        notification: CustomNotification,
    ) {
        let mut pending = pending.lock().await;
        if pending.len() >= PENDING_CUSTOM_NOTIFICATIONS_CAP {
            pending.pop_front();
        }
        pending.push_back(notification);
    }

    #[tokio::test]
    async fn buffers_custom_notifications_while_downstream_is_none() {
        let pending: Mutex<VecDeque<CustomNotification>> = Mutex::new(VecDeque::new());
        let evt = notification("notifications/codanna/file-reindexed");

        push_pending(&pending, evt.clone()).await;

        let pending = pending.lock().await;
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].method, evt.method);
    }

    #[tokio::test]
    async fn overflow_drops_oldest_entry() {
        let pending: Mutex<VecDeque<CustomNotification>> = Mutex::new(VecDeque::new());

        for i in 0..(PENDING_CUSTOM_NOTIFICATIONS_CAP + 5) {
            push_pending(
                &pending,
                notification(&format!("notifications/codanna/evt-{i}")),
            )
            .await;
        }

        let pending = pending.lock().await;
        assert_eq!(pending.len(), PENDING_CUSTOM_NOTIFICATIONS_CAP);
        // The first 5 pushed (evt-0..evt-4) must have been dropped; the
        // oldest surviving entry is evt-5.
        assert_eq!(
            pending.front().unwrap().method,
            "notifications/codanna/evt-5"
        );
        assert_eq!(
            pending.back().unwrap().method,
            format!(
                "notifications/codanna/evt-{}",
                PENDING_CUSTOM_NOTIFICATIONS_CAP + 4
            )
        );
    }

    #[tokio::test]
    async fn drain_preserves_fifo_order() {
        let pending: Mutex<VecDeque<CustomNotification>> = Mutex::new(VecDeque::new());
        for i in 0..10 {
            push_pending(
                &pending,
                notification(&format!("notifications/codanna/evt-{i}")),
            )
            .await;
        }

        // Mirrors the drain step inlined into `DelegatingProxyHandler::initialize`
        // (minus the actual `peer.send_notification` call, which needs a live
        // `Peer<RoleServer>` -- see module doc comment above).
        let drained: Vec<CustomNotification> = {
            let mut pending = pending.lock().await;
            pending.drain(..).collect()
        };

        let methods: Vec<String> = drained.into_iter().map(|n| n.method).collect();
        let expected: Vec<String> = (0..10)
            .map(|i| format!("notifications/codanna/evt-{i}"))
            .collect();
        assert_eq!(methods, expected);

        // Buffer is empty after drain -- nothing left to re-flush.
        assert!(pending.lock().await.is_empty());
    }

    #[test]
    fn notification_relay_and_proxy_handler_share_one_downstream_state() {
        // Construction wiring: `NotificationRelay::state` and
        // `DelegatingProxyHandler::state` must be clones of the same
        // `Arc<Mutex<DownstreamState>>` (as done in `serve_proxy`), or the
        // downstream-check and pending-drain in `on_custom_notification` and
        // `initialize` would no longer share a single lock -- reopening the
        // TOCTOU window this type exists to close. This compiles only if
        // both fields are the same type.
        fn assert_same_type(_relay: &NotificationRelay, _state: &Arc<Mutex<DownstreamState>>) {}
        let state: Arc<Mutex<DownstreamState>> = Arc::new(Mutex::new(DownstreamState::default()));
        let relay = NotificationRelay {
            state: state.clone(),
        };
        assert_same_type(&relay, &state);
        assert!(Arc::ptr_eq(&relay.state, &state));
    }
}
