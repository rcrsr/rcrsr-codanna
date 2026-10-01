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
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ClientRequest, CompleteRequestParams,
    CompleteResult, ContentBlock, CustomNotification, CustomRequest, CustomResult,
    ErrorData as McpError, GetPromptRequestParams, GetPromptResponse, InitializeRequestParams,
    InitializeResult, ListPromptsResult, ListResourceTemplatesResult, ListResourcesResult,
    ListToolsResult, LoggingMessageNotificationParam, PaginatedRequestParams,
    ProgressNotificationParam, ReadResourceRequestParams, ReadResourceResponse,
    ResourceUpdatedNotificationParam, ResultType, ServerInfo, ServerNotification, ServerResult,
    SetLevelRequestParams, SubscribeRequestParams, Tool, UnsubscribeRequestParams,
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
use crate::mcp::server::{CodeIntelligenceServer, codanna_server_info, list_tools_result};
use crate::serve_discovery::{self, DiscoveryError, ServeScheme};
use crate::serve_registry;
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

/// Current wall-clock time as unix seconds, for the proxy's own
/// [`serve_registry::RegistryEntry::start_time`]. Falls back to 0 only if
/// the system clock is set before the epoch, which is not a case worth
/// failing proxy startup over. Mirrors `serve_discovery::unix_now_secs` and
/// `mcp::http_server::unix_now_secs` (kept private to each module rather
/// than shared, since all three are trivial one-liners with no other
/// coupling).
fn unix_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

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

/// Classifies a `ServiceError` as evidence the upstream *transport* is dead
/// (worth reviving the connection) versus every other failure mode, which
/// must be passed through unchanged.
///
/// Only `TransportSend` (a send against the wire failed, e.g. connection
/// reset) and `TransportClosed` indicate the upstream process is actually
/// gone. `McpError` is a healthy server returning a well-formed protocol
/// error and must keep flowing through [`map_service_err`] unchanged --
/// reviving on it would replace a live server because it validly rejected a
/// request. `UnexpectedResponse`, `Cancelled`, and `Timeout` are
/// request-shape or scheduling outcomes on an otherwise-live transport, not
/// evidence the backing process is gone; reviving on any of those would spawn
/// a second server while the first is merely slow or this one request was
/// cancelled/mismatched -- exactly the respawn-triggers-rebuild churn the
/// fork's `startup_catch_up` documentation already warns about for a slow
/// reindex. The proxy's own [`UPSTREAM_CALL_TIMEOUT`] expiry is a
/// `tokio::time::error::Elapsed`, not a `ServiceError` at all, so it can
/// never reach this function and never triggers a revive either.
fn is_dead_transport(err: &ServiceError) -> bool {
    matches!(
        err,
        ServiceError::TransportSend(_) | ServiceError::TransportClosed
    )
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

impl DownstreamState {
    /// Buffer a custom notification received before `downstream` is set,
    /// enforcing the bounded drop-oldest policy
    /// (`PENDING_CUSTOM_NOTIFICATIONS_CAP`). This is the exact code the
    /// pre-init branch of [`NotificationRelay::on_custom_notification`] runs,
    /// factored out so it is unit-tested directly instead of through a copy.
    fn buffer_pending(&mut self, notification: CustomNotification) {
        if self.pending.len() >= PENDING_CUSTOM_NOTIFICATIONS_CAP {
            self.pending.pop_front();
        }
        self.pending.push_back(notification);
    }

    /// Take all buffered notifications in FIFO order, emptying the buffer.
    /// This is the exact drain [`DelegatingProxyHandler::initialize`] performs
    /// after setting `downstream`.
    fn drain_pending(&mut self) -> Vec<CustomNotification> {
        self.pending.drain(..).collect()
    }

    /// Route an inbound custom notification under the caller's lock: if a
    /// downstream peer is present, return `Some((peer, notification))` for the
    /// caller to forward off-lock; otherwise buffer it (bounded, drop-oldest)
    /// and return `None`. This encapsulates the entire branch
    /// [`NotificationRelay::on_custom_notification`] takes, so a regression
    /// that failed to buffer when no downstream peer is set is caught by a
    /// unit test that drives this method directly.
    fn route_custom_notification(
        &mut self,
        notification: CustomNotification,
    ) -> Option<(Peer<RoleServer>, CustomNotification)> {
        match self.downstream.clone() {
            Some(peer) => Some((peer, notification)),
            None => {
                self.buffer_pending(notification);
                None
            }
        }
    }
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
/// Deliberately does NOT derive `Default`. A `NotificationRelay::default()`
/// would carry a fresh, nobody-else-holds-it `state` whose `downstream` stays
/// `None` forever -- the silent-failure mode described on [`Dialer::connect`].
/// Withholding the derive turns that mistake into a compile error rather than
/// something a test has to catch after the fact.
#[derive(Clone)]
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
        // Decide forward-vs-buffer under a single lock acquisition (closing
        // the set-then-drain TOCTOU with `initialize`), then send off-lock.
        let forward = {
            let mut state = self.state.lock().await;
            state.route_custom_notification(notification)
        };
        if let Some((peer, notification)) = forward {
            let _ = peer
                .send_notification(ServerNotification::CustomNotification(notification))
                .await;
        }
    }
}

/// Dials the backing HTTP MCP server: the single dial site. Its only caller
/// is [`UpstreamHandle::revive`], which both the initial background dial and
/// every request-triggered retry go through, so the HTTPS cert-pinning branch
/// (and any future scheme handling) exists exactly once.
///
/// Re-runs `discover_or_spawn` (and therefore re-reads a fresh
/// [`serve_discovery::ServeRecord`]) on every call, so a backing server that
/// comes back on a different scheme -- e.g. was `--http` before and is
/// manually restarted as `--https` -- is dialed correctly on revive rather
/// than replaying whatever scheme happened to be current at proxy startup.
struct Dialer {
    workspace_root: PathBuf,
    config: Settings,
    config_path: Option<PathBuf>,
    /// Shared with [`DelegatingProxyHandler::state`] so every dial's
    /// [`NotificationRelay`] forwards to the same downstream peer -- see the
    /// "HIGHEST-RISK SILENT FAILURE" note on [`Dialer::connect`].
    state: Arc<Mutex<DownstreamState>>,
}

/// Builds the proxy's OWN registry entry. The single builder shared by
/// `serve_proxy` (initial write, default scheme) and [`Dialer::connect`]
/// (refresh with the dialed scheme). The status is always `Healthy`, never
/// `Spawning`: the discovery guards treat a `Spawning` entry as a backing
/// server still starting, which a proxy entry must never look like.
fn proxy_registry_entry(
    workspace_root: &std::path::Path,
    scheme: ServeScheme,
) -> serve_registry::RegistryEntry {
    serve_registry::RegistryEntry {
        pid: std::process::id(),
        port: 0,
        scheme,
        workspace_root: workspace_root.to_path_buf(),
        start_time: unix_now_secs(),
        status: serve_registry::ServerStatus::Healthy,
        role: serve_registry::ServerRole::Proxy,
        version: env!("CARGO_PKG_VERSION").to_string(),
    }
}

/// Best-effort write of the proxy's registry entry: it exists only so
/// `codanna serve --list` can attribute this pid to the workspace it proxies
/// for, so a failed write is logged and never fails the proxy.
fn write_proxy_entry(workspace_root: &std::path::Path, scheme: ServeScheme) {
    let entry = proxy_registry_entry(workspace_root, scheme);
    if let Err(e) = serve_registry::write_entry(&entry) {
        tracing::warn!(
            target: "proxy",
            "failed to write registry entry for proxy pid {}: {e}",
            entry.pid
        );
    }
}

impl Dialer {
    /// Discover-or-spawn a backing HTTP MCP server and connect to it,
    /// printing the same "Proxy: delegating to ..." line on every dial
    /// (startup or revive) so operators can see a reconnect happen from the
    /// proxy's own stderr.
    ///
    /// Builds its relay through [`Dialer::relay`], which clones the handle's
    /// existing `state` Arc. A relay carrying any OTHER `state` -- a fresh,
    /// nobody-else-holds-it `Arc<Mutex<DownstreamState>>` -- has `downstream:
    /// None` forever: every server-to-client notification after a revive,
    /// including the fork's `notifications/codanna/*` hot-reload signals,
    /// would be buffered into a `VecDeque` that is never drained (downstream
    /// `initialize` already ran once and will not run again after a revive).
    /// No error, no log -- just silence. Reusing the handle's existing
    /// `state` Arc is what keeps a revived connection wired to the same
    /// downstream peer; [`NotificationRelay`] withholds `Default` so the
    /// fresh-state mistake cannot compile.
    /// The single construction site for this dialer's [`NotificationRelay`],
    /// factored out of [`Dialer::connect`] so a unit test can assert the
    /// state-Arc identity invariant against the SAME code path production
    /// uses, rather than re-deriving it (the seam `revive_preserves_downstream_state`
    /// drives). Every dial -- initial connect and later revive alike -- goes
    /// through here, so a relay can never be built from anything but the
    /// handle's shared `state`.
    fn relay(&self) -> NotificationRelay {
        NotificationRelay {
            state: self.state.clone(),
        }
    }

    async fn connect(&self) -> ProxyResult<Arc<RunningService<RoleClient, NotificationRelay>>> {
        let record = serve_discovery::discover_or_spawn(
            &self.workspace_root,
            &self.config,
            self.config_path.as_deref(),
        )
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

        let relay = self.relay();

        let service = match record.scheme {
            // `from_config` uses rmcp's own bundled reqwest client (gated
            // behind the `transport-streamable-http-client-reqwest` feature)
            // rather than a hand-rolled HTTP client, per the preference for
            // rmcp's default client transport.
            ServeScheme::Http => {
                let transport = StreamableHttpClientTransport::from_config(transport_config);
                relay
                    .serve(transport)
                    .await
                    .map_err(|e| ProxyError::UpstreamConnect(e.to_string()))?
            }
            // The backing server is `--https`: dial it ONLY through the
            // cert-pinning client (`serve_tls::pinned_client`), never through
            // `from_config`'s bundled client. A pinning failure
            // (missing/mismatched persisted cert) must fail outright rather
            // than silently falling back to an unauthenticated/
            // plaintext-trusting transport.
            ServeScheme::Https => {
                let client = serve_tls::pinned_client()?;
                let transport =
                    StreamableHttpClientTransport::with_client(client, transport_config);
                relay
                    .serve(transport)
                    .await
                    .map_err(|e| ProxyError::UpstreamConnect(e.to_string()))?
            }
        };

        // Refresh the proxy's registry entry with the scheme actually dialed.
        write_proxy_entry(&self.workspace_root, record.scheme);

        Ok(Arc::new(service))
    }
}

/// Production upstream service type.
type UpstreamService = Arc<RunningService<RoleClient, NotificationRelay>>;

/// Readiness of the upstream connection. `S` is the live service type
/// ([`UpstreamService`] in production); it is generic so the state machine is
/// unit-tested without a real `RunningService`.
#[derive(Clone)]
enum UpstreamConn<S> {
    /// A dial is in flight (or about to start); nothing to delegate to yet.
    Connecting,
    /// Connected; requests are delegated to `S`.
    Ready(S),
    /// The last dial round failed; the next request flips the slot back to
    /// `Connecting` and starts one retry dial.
    Failed(McpError),
}

/// The current upstream connection state plus a generation counter bumped on
/// every COMPLETED dial round -- success or failure alike -- so a caller that
/// observed generation `g` can tell, after taking the reconnect gate, whether
/// someone else already ran a round in the meantime. Flips between `Ready`/
/// `Failed` and `Connecting` keep the generation, so only the caller that
/// observed `g` wins the flip.
struct UpstreamSlot<S> {
    conn: UpstreamConn<S>,
    generation: u64,
}

/// Flips `Ready`/`Failed` at `seen_generation` to `Connecting`, keeping the
/// generation. Returns `true` only for the single caller that performed the
/// flip (and therefore owes the background dial); `false` if the generation
/// moved on or the slot is already `Connecting`. The generation compare and
/// the flip happen under one write lock.
fn flip_to_connecting<S>(slot: &std::sync::RwLock<UpstreamSlot<S>>, seen_generation: u64) -> bool {
    // Critical section is a compare and an assignment; a poisoned lock still
    // holds a fully-valid slot, so recovering the inner value is safe.
    let mut slot = slot
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if slot.generation != seen_generation {
        return false;
    }
    match slot.conn {
        UpstreamConn::Connecting => false,
        UpstreamConn::Ready(_) | UpstreamConn::Failed(_) => {
            slot.conn = UpstreamConn::Connecting;
            true
        }
    }
}

/// `read_current` for [`single_flight_revive`] over a slot: the current
/// state, its generation, and the round's error if that round failed.
fn read_round<S: Clone>(
    slot: &std::sync::RwLock<UpstreamSlot<S>>,
) -> (UpstreamConn<S>, u64, Option<McpError>) {
    let slot = slot
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let failure = match &slot.conn {
        UpstreamConn::Failed(err) => Some(err.clone()),
        _ => None,
    };
    (slot.conn.clone(), slot.generation, failure)
}

/// `store` for [`single_flight_revive`] over a slot: a failed round stores
/// `Failed(err)`, a successful one the dialed `Ready(..)` value, both at the
/// new generation. The previous service is simply dropped on success; rmcp's
/// `Drop for RunningService` closes the old connection once the last clone
/// goes away.
fn commit_round<S>(
    slot: &std::sync::RwLock<UpstreamSlot<S>>,
    value: UpstreamConn<S>,
    generation: u64,
    failure: Option<McpError>,
) {
    let conn = match failure {
        Some(err) => UpstreamConn::Failed(err),
        None => value,
    };
    *slot
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = UpstreamSlot { conn, generation };
}

/// Generation-gated single-flight reconnect, generic over the async `dial`
/// closure and over `read_current`/`store` accessors rather than a concrete
/// slot type, so [`concurrent_revive_dials_once`] and
/// [`failed_revive_dials_once`] (in `tests` below) can drive it with a cheap
/// counting closure and a plain `Mutex<(T, u64)>` instead of a real
/// [`RunningService`] -- avoiding a `Dialer` trait with a single production
/// implementation just to make this testable (a generic helper plus a test
/// closure is the smaller seam).
///
/// `seen_generation` is the generation the caller observed before its own
/// delegated call failed. Under the `reconnect` gate:
///
/// - If the stored generation has already moved past `seen_generation`,
///   another caller already completed a round while this one waited for the
///   gate. `read_current` reports both the last-good value (unchanged if that
///   round failed) and, via its third tuple element, the error from that
///   round IF it failed -- so this caller returns that cached error rather
///   than re-dialing (a `None` third element means the round succeeded, so
///   the last-good value is returned instead).
/// - Otherwise this caller performs the one dial for the round. On success it
///   stores the new value at `generation + 1` with no cached failure. On
///   failure it stores the OLD value unchanged at `generation + 1` alongside
///   the error, so any waiter arriving after this point sees the failure
///   without dialing again -- closing the gap where only the success path
///   was previously single-flighted.
///
/// `E: Clone` is required because a cached failure must be handed to every
/// waiter of the round it belongs to, not just the caller that dialed.
async fn single_flight_revive<T, E, D, Fut>(
    reconnect: &tokio::sync::Mutex<()>,
    seen_generation: u64,
    read_current: impl Fn() -> (T, u64, Option<E>),
    store: impl FnOnce(T, u64, Option<E>),
    dial: D,
) -> Result<T, E>
where
    T: Clone,
    E: Clone,
    D: FnOnce() -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    let _reconnect_guard = reconnect.lock().await;

    let (current_value, current_generation, current_failure) = read_current();
    if current_generation != seen_generation {
        // Another caller already completed a round while we waited for the
        // gate. Hand back whatever that round produced -- its cached failure
        // if it failed, otherwise the value it installed.
        return match current_failure {
            Some(err) => Err(err),
            None => Ok(current_value),
        };
    }

    match dial().await {
        Ok(dialed) => {
            store(dialed.clone(), current_generation + 1, None);
            Ok(dialed)
        }
        Err(err) => {
            store(current_value, current_generation + 1, Some(err.clone()));
            Err(err)
        }
    }
}

/// Owns the upstream connection state behind a lock that is only ever held
/// for a clone, a compare or an assignment -- never across an `.await` -- so a
/// `std::sync` guard cannot block the async runtime.
///
/// Dials happen only in background tasks ([`UpstreamHandle::start_background_dial`]);
/// request handlers read the state and never wait for a dial.
struct UpstreamHandle {
    slot: std::sync::RwLock<UpstreamSlot<UpstreamService>>,
    /// Serializes dial rounds so at most one dial is in flight, on both the
    /// success AND failure path (see [`single_flight_revive`]). Cross-process
    /// dedup for the underlying `discover_or_spawn` call is already handled
    /// by its own `O_EXCL` `.codanna/http.lock`; this mutex closes the
    /// *intra-process* gate that primitive does not cover.
    reconnect: tokio::sync::Mutex<()>,
    dial: Dialer,
    /// The single in-flight (or last) background dial supervisor, kept so
    /// shutdown can abort it.
    dial_task: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl UpstreamHandle {
    /// Returns the current upstream state and its generation. Never awaits.
    fn snapshot(&self) -> (UpstreamConn<UpstreamService>, u64) {
        let (conn, generation, _) = read_round(&self.slot);
        (conn, generation)
    }

    /// Runs one dial round for `seen_generation` (see [`single_flight_revive`]):
    /// stores `Ready(svc)` at `g + 1` on success, `Failed(err)` at `g + 1` on
    /// failure. [`Dialer::connect`] is the only dial site and this is its only
    /// caller.
    async fn revive(
        &self,
        seen_generation: u64,
    ) -> Result<UpstreamConn<UpstreamService>, McpError> {
        let workspace_root = self.dial.workspace_root.clone();
        single_flight_revive(
            &self.reconnect,
            seen_generation,
            || read_round(&self.slot),
            |value, generation, failure| commit_round(&self.slot, value, generation, failure),
            || async {
                self.dial
                    .connect()
                    .await
                    .map(UpstreamConn::Ready)
                    .map_err(|err| {
                        McpError::internal_error(
                            format!(
                                "failed to reach backing MCP server for workspace '{}': {err}",
                                workspace_root.display()
                            ),
                            None,
                        )
                    })
            },
        )
        .await
    }

    /// Spawns a supervised task running one dial round for `seen_generation`
    /// and records it as the single in-flight dial (see [`Self::shutdown_dial`]).
    /// A failed round is cached as `Failed` in the slot and reported on
    /// stderr; it never exits the process. If the dial itself panics, the
    /// supervisor commits `Failed` and reports it, so the slot never stays
    /// `Connecting` forever.
    fn start_background_dial(self: &Arc<Self>, seen_generation: u64) {
        let this = Arc::clone(self);
        let supervisor = tokio::spawn(async move {
            let dialer = Arc::clone(&this);
            let inner = tokio::spawn(async move {
                if let Err(err) = dialer.revive(seen_generation).await {
                    eprintln!("Proxy: backing MCP server unavailable: {err}");
                }
            });
            // Aborting the supervisor (shutdown) drops this guard, which
            // aborts the inner dial too.
            let _abort_inner = AbortOnDrop(inner.abort_handle());
            if let Err(e) = inner.await
                && !e.is_cancelled()
            {
                eprintln!("Proxy: background dial task failed: {e}");
                this.fail_if_connecting(
                    seen_generation,
                    McpError::internal_error(format!("background dial task failed: {e}"), None),
                );
            }
        });
        *self
            .dial_task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(supervisor);
    }

    /// Commits `Failed(err)` at `seen_generation + 1` if the slot is still the
    /// `Connecting` state of that round (a panicked dial never committed).
    fn fail_if_connecting(&self, seen_generation: u64, err: McpError) {
        let mut slot = self
            .slot
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if slot.generation == seen_generation && matches!(slot.conn, UpstreamConn::Connecting) {
            *slot = UpstreamSlot {
                conn: UpstreamConn::Failed(err),
                generation: seen_generation + 1,
            };
        }
    }

    /// Flips the slot at `seen_generation` to `Connecting` and, for the one
    /// caller that wins the flip, starts the background dial.
    fn begin_redial(self: &Arc<Self>, seen_generation: u64) {
        if flip_to_connecting(&self.slot, seen_generation) {
            self.start_background_dial(seen_generation);
        }
    }

    /// Aborts the in-flight dial, if any, and waits for it to stop. Used at
    /// shutdown; an unfinished dial is not waited for.
    async fn shutdown_dial(&self) {
        let task = self
            .dial_task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(task) = task {
            task.abort();
            if let Err(e) = task.await
                && !e.is_cancelled()
            {
                eprintln!("Proxy: background dial task failed: {e}");
            }
        }
    }
}

/// Aborts the wrapped task when dropped.
struct AbortOnDrop(tokio::task::AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// `ServerHandler` facing the stdio client. `initialize`, `get_info` and
/// `tools/list` are answered locally from the in-binary tool router and server
/// info; every other request is delegated to the upstream HTTP MCP server once
/// it is ready. This process holds no `IndexFacade` and no index state.
#[derive(Clone)]
struct DelegatingProxyHandler {
    /// `Arc<UpstreamHandle>` keeps `#[derive(Clone)]` on this handler cheap
    /// (one `Arc` clone) while still sharing the single connection state,
    /// generation counter, and reconnect gate across every clone of the
    /// handler.
    upstream: Arc<UpstreamHandle>,
    /// Shared with the `NotificationRelay` driving `upstream`; custom
    /// notifications received before `state.downstream` is populated are
    /// buffered in `state.pending` and drained atomically with setting
    /// `state.downstream` in `initialize` (see [`DownstreamState`]).
    state: Arc<Mutex<DownstreamState>>,
    /// Tool list served locally, computed once per proxy.
    tools: Arc<Vec<Tool>>,
}

/// Maximum time to wait for a single delegated upstream call. A hung upstream
/// must not leave the stdio client's request pending forever; this is a fixed
/// budget rather than a new config knob, kept minimal per scope.
const UPSTREAM_CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Text returned while the backing server is being dialed.
const NOT_READY: &str =
    "codanna index not available yet \u{2014} backend starting, check back shortly";

/// Stable prefix of the text returned after a failed dial round.
const FAILED_PREFIX: &str = "codanna backend unavailable:";

fn failed_text(err: &McpError) -> String {
    format!("{FAILED_PREFIX} {} (the next call retries)", err.message)
}

fn upstream_timeout_error() -> McpError {
    McpError::internal_error(
        format!(
            "delegated upstream call timed out after {}s",
            UPSTREAM_CALL_TIMEOUT.as_secs()
        ),
        None,
    )
}

/// Outcome of [`DelegatingProxyHandler::delegate`]: the upstream value, or the
/// text explaining why no upstream was available. An outcome type, not an
/// error: `call_tool` renders `Unavailable` as an `isError` tool result while
/// every other method renders it as a protocol error.
enum Delegated<T> {
    Value(T),
    Unavailable(String),
}

impl<T> Delegated<T> {
    fn or_protocol_error(self) -> Result<T, McpError> {
        match self {
            Self::Value(value) => Ok(value),
            Self::Unavailable(text) => Err(McpError::internal_error(text, None)),
        }
    }
}

/// A complete `isError` tool result carrying `text`.
fn unavailable_tool_response(text: String) -> CallToolResponse {
    CallToolResponse::Complete(CallToolResult::error(vec![ContentBlock::text(text)]))
        .mark_complete()
}

impl DelegatingProxyHandler {
    /// Delegates one inbound request to the upstream server without ever
    /// waiting for a dial:
    ///
    /// - `Connecting`: `Unavailable(NOT_READY)`.
    /// - `Failed`: `Unavailable(failed text)`, and the caller that flips the
    ///   slot back to `Connecting` starts one retry dial.
    /// - `Ready`: runs `op`. A dead transport flips the slot to `Connecting`
    ///   (single winner), starts the background dial, and returns
    ///   `Unavailable(NOT_READY)` immediately; any other upstream error passes
    ///   through unchanged.
    ///
    /// [`UPSTREAM_CALL_TIMEOUT`] bounds the one attempt.
    async fn delegate<T, F, Fut>(&self, op: F) -> Result<Delegated<T>, McpError>
    where
        F: FnOnce(UpstreamService) -> Fut,
        Fut: Future<Output = Result<T, ServiceError>>,
    {
        let (conn, generation) = self.upstream.snapshot();
        match conn {
            UpstreamConn::Connecting => Ok(Delegated::Unavailable(NOT_READY.to_string())),
            UpstreamConn::Failed(err) => {
                self.upstream.begin_redial(generation);
                Ok(Delegated::Unavailable(failed_text(&err)))
            }
            UpstreamConn::Ready(service) => {
                match tokio::time::timeout(UPSTREAM_CALL_TIMEOUT, op(service)).await {
                    Ok(Ok(value)) => Ok(Delegated::Value(value)),
                    // A healthy server's own protocol error (or any
                    // non-transport failure) passes through unchanged.
                    Ok(Err(err)) if !is_dead_transport(&err) => Err(map_service_err(err)),
                    Ok(Err(_)) => {
                        self.upstream.begin_redial(generation);
                        Ok(Delegated::Unavailable(NOT_READY.to_string()))
                    }
                    // The proxy's own timeout, not a `ServiceError` -- never
                    // triggers a redial (see `is_dead_transport`'s doc).
                    Err(_) => Err(upstream_timeout_error()),
                }
            }
        }
    }

    /// [`Self::delegate`] for methods that have no tool-result channel:
    /// unavailability becomes a protocol error.
    async fn delegate_or_error<T, F, Fut>(&self, op: F) -> Result<T, McpError>
    where
        F: FnOnce(UpstreamService) -> Fut,
        Fut: Future<Output = Result<T, ServiceError>>,
    {
        self.delegate(op).await?.or_protocol_error()
    }
}

/// Fills an absent `resultType` with `"complete"` on a forwarded result.
///
/// The upstream leg negotiates a pre-2026-07-28 session, so the backing
/// server strips `resultType` (`strip_result_type_for_legacy_peer`). A
/// downstream that negotiated 2026-07-28 MUST receive it, and absent means
/// complete per the spec's back-compat rule. rmcp strips it again for
/// legacy downstreams, so their wire shape is unchanged.
trait MarkComplete {
    fn mark_complete(self) -> Self;
}

macro_rules! impl_mark_complete {
    ($($ty:ty),*) => {$(
        impl MarkComplete for $ty {
            fn mark_complete(mut self) -> Self {
                self.result_type.get_or_insert(ResultType::COMPLETE);
                self
            }
        }
    )*};
}

impl_mark_complete!(
    ListToolsResult,
    ListResourcesResult,
    ListResourceTemplatesResult,
    ListPromptsResult,
    CompleteResult
);

impl MarkComplete for CallToolResponse {
    fn mark_complete(self) -> Self {
        match self {
            Self::Complete(mut r) => {
                r.result_type.get_or_insert(ResultType::COMPLETE);
                Self::Complete(r)
            }
            other => other,
        }
    }
}

impl MarkComplete for ReadResourceResponse {
    fn mark_complete(self) -> Self {
        match self {
            Self::Complete(mut r) => {
                r.result_type.get_or_insert(ResultType::COMPLETE);
                Self::Complete(r)
            }
            other => other,
        }
    }
}

impl MarkComplete for GetPromptResponse {
    fn mark_complete(self) -> Self {
        match self {
            Self::Complete(mut r) => {
                r.result_type.get_or_insert(ResultType::COMPLETE);
                Self::Complete(r)
            }
            other => other,
        }
    }
}

/// The server info the proxy advertises, identical to the backing server's
/// (built from the same in-binary source) minus `resources.subscribe`.
///
/// `resources.subscribe = true` advertises support for the 2026-07-28
/// `subscriptions/listen` request, which this handler cannot honor: it
/// overrides neither `accepted_subscription_filter` nor `listen`, so the SDK's
/// default implementation rejects every such request with `method_not_found`.
/// Strip the flag so advertised capabilities never promise more than the proxy
/// actually serves; the legacy `resources/subscribe`/`unsubscribe` RPCs
/// (forwarded in `subscribe`/`unsubscribe`) are unaffected and keep working.
fn local_server_info() -> ServerInfo {
    let mut info = codanna_server_info(None);
    if let Some(resources) = info.capabilities.resources.as_mut() {
        resources.subscribe = None;
    }
    info
}

/// The full tool list, from the same router the backing server registers.
fn local_tools() -> Vec<Tool> {
    CodeIntelligenceServer::full_tool_router().list_all()
}

impl ServerHandler for DelegatingProxyHandler {
    fn get_info(&self) -> ServerInfo {
        local_server_info()
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
            state.drain_pending()
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
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(list_tools_result(self.tools.as_ref().clone()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let outcome = self
            .delegate(|up| async move { up.call_tool_once(request).await })
            .await?;
        Ok(match outcome {
            Delegated::Value(response) => response.mark_complete(),
            Delegated::Unavailable(text) => unavailable_tool_response(text),
        })
    }

    async fn list_resources(
        &self,
        request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        self.delegate_or_error(|up| {
            let request = request.clone();
            async move { up.list_resources(request).await }
        })
        .await
        .map(MarkComplete::mark_complete)
    }

    async fn list_resource_templates(
        &self,
        request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, McpError> {
        self.delegate_or_error(|up| {
            let request = request.clone();
            async move { up.list_resource_templates(request).await }
        })
        .await
        .map(MarkComplete::mark_complete)
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, McpError> {
        self.delegate_or_error(|up| {
            let request = request.clone();
            async move { up.read_resource_once(request).await }
        })
        .await
        .map(MarkComplete::mark_complete)
    }

    async fn list_prompts(
        &self,
        request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, McpError> {
        self.delegate_or_error(|up| {
            let request = request.clone();
            async move { up.list_prompts(request).await }
        })
        .await
        .map(MarkComplete::mark_complete)
    }

    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetPromptResponse, McpError> {
        self.delegate_or_error(|up| {
            let request = request.clone();
            async move { up.get_prompt_once(request).await }
        })
        .await
        .map(MarkComplete::mark_complete)
    }

    async fn complete(
        &self,
        request: CompleteRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CompleteResult, McpError> {
        self.delegate_or_error(|up| {
            let request = request.clone();
            async move { up.complete(request).await }
        })
        .await
        .map(MarkComplete::mark_complete)
    }

    async fn set_level(
        &self,
        request: SetLevelRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        self.delegate_or_error(|up| {
            let request = request.clone();
            async move { up.set_level(request).await }
        })
        .await
    }

    async fn subscribe(
        &self,
        request: SubscribeRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        self.delegate_or_error(|up| {
            let request = request.clone();
            async move { up.subscribe(request).await }
        })
        .await
    }

    async fn unsubscribe(
        &self,
        request: UnsubscribeRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        self.delegate_or_error(|up| {
            let request = request.clone();
            async move { up.unsubscribe(request).await }
        })
        .await
    }

    async fn on_custom_request(
        &self,
        request: CustomRequest,
        _context: RequestContext<RoleServer>,
    ) -> Result<CustomResult, McpError> {
        let result = self
            .delegate_or_error(|up| {
                let request = request.clone();
                async move {
                    up.peer()
                        .send_request(ClientRequest::CustomRequest(request))
                        .await
                }
            })
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
/// `serve_discovery::discover_or_spawn` finds or launches. That dial runs in a
/// background task started before stdio is served, so the proxy answers
/// `initialize` and `tools/list` immediately; delegated calls report "not
/// ready" until the dial completes. A failed dial is reported on stderr and
/// cached; the next delegated call retries it. Only `NoWorkspaceRoot` and
/// stdio errors make this return `Err`.
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

    let state: Arc<Mutex<DownstreamState>> = Arc::new(Mutex::new(DownstreamState::default()));
    let proxy_workspace_root = workspace_root.clone();
    let upstream = Arc::new(UpstreamHandle {
        slot: std::sync::RwLock::new(UpstreamSlot {
            conn: UpstreamConn::Connecting,
            generation: 0,
        }),
        reconnect: tokio::sync::Mutex::new(()),
        dial: Dialer {
            workspace_root,
            config,
            config_path,
            state: state.clone(),
        },
        dial_task: std::sync::Mutex::new(None),
    });

    // Register this proxy before dialing so `codanna serve --list` can
    // attribute the pid; `Dialer::connect` refreshes the scheme once dialed.
    write_proxy_entry(&proxy_workspace_root, ServeScheme::default());
    let proxy_pid = std::process::id();

    upstream.start_background_dial(0);

    let handler = DelegatingProxyHandler {
        upstream: Arc::clone(&upstream),
        state,
        tools: Arc::new(local_tools()),
    };

    let discover_result = serde_json::to_value(rmcp::model::DiscoverResult::from_server_info(
        handler.supported_protocol_versions().into_owned(),
        handler.get_info(),
    ))
    .expect("DiscoverResult serializes: closed struct of strings and maps");
    let served = handler
        .serve(crate::mcp::probe_tolerant_stdio(discover_result))
        .await
        .map_err(|e| ProxyError::Stdio(e.to_string()));

    let wait_result = match served {
        Ok(service) => service
            .waiting()
            .await
            .map(|_| ())
            .map_err(|e| ProxyError::Stdio(e.to_string())),
        Err(e) => Err(e),
    };

    // Graceful shutdown: do not wait for an unfinished dial. Dropping a
    // mid-flight `discover_or_spawn` is safe (the lock guard's Drop removes
    // `http.lock`; a detached backend child and its `Spawning` entry survive).
    upstream.shutdown_dial().await;

    // Remove this proxy's own registry entry regardless of whether stdio
    // returned an error, so a clean exit never leaves a stale row behind for
    // `ls`. A crash/kill leaves it for the existing `entry_is_stale`/`--reap`
    // machinery.
    serve_registry::remove_entry(proxy_pid);

    wait_result
}

// A live `NotificationRelay::on_custom_notification` / `initialize`-time
// drain-and-forward can't be driven from outside rmcp: both need a real
// `Peer<RoleServer>`, constructible only via rmcp's crate-private
// `Peer::new`. So the tests below drive the buffering/routing/draining
// through the *same* `DownstreamState` methods the production handlers call
// (`route_custom_notification`, `buffer_pending`, `drain_pending`) -- not a
// reimplementation -- so a regression in the pre-init buffering or the
// bounded drop-oldest / FIFO-drain policy fails a test. Only the final
// `peer.send_notification` hop (the `Some(peer)` arm's off-lock send and the
// `initialize` flush) needs a live peer and is left to the manual MCP smoke
// test.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mark_complete_fills_absent_result_type() {
        let legacy: ListToolsResult = serde_json::from_str(r#"{"tools":[]}"#).unwrap();
        assert!(legacy.result_type.is_none(), "fixture omits resultType");
        let marked = legacy.mark_complete();
        assert_eq!(marked.result_type, Some(ResultType::COMPLETE));

        let call: rmcp::model::CallToolResult = serde_json::from_str(r#"{"content":[]}"#).unwrap();
        match CallToolResponse::Complete(call).mark_complete() {
            CallToolResponse::Complete(r) => assert_eq!(r.result_type, Some(ResultType::COMPLETE)),
            other => panic!("variant changed: {other:?}"),
        }

        // The remaining four `impl_mark_complete!` types plus the other two
        // Complete-variant response enums are macro/match-generated
        // identically to the two above; covered here so a future refactor
        // that renames one enum's `Complete`-variant field is caught rather
        // than only breaking at runtime. `..T::with_all_items(..)`/`..T::new(..)`
        // struct-update syntax builds a base instance (each type is
        // `#[non_exhaustive]`, so it can't be built as a full literal outside
        // the defining crate) with `result_type` overridden to `None`.
        let resources = ListResourcesResult {
            result_type: None,
            ..ListResourcesResult::with_all_items(Vec::new())
        };
        assert_eq!(
            resources.mark_complete().result_type,
            Some(ResultType::COMPLETE)
        );

        let templates = ListResourceTemplatesResult {
            result_type: None,
            ..ListResourceTemplatesResult::with_all_items(Vec::new())
        };
        assert_eq!(
            templates.mark_complete().result_type,
            Some(ResultType::COMPLETE)
        );

        let prompts = ListPromptsResult {
            result_type: None,
            ..ListPromptsResult::with_all_items(Vec::new())
        };
        assert_eq!(
            prompts.mark_complete().result_type,
            Some(ResultType::COMPLETE)
        );

        // `CompleteResult`/`ReadResourceResult`/`GetPromptResult` are
        // `#[non_exhaustive]`, which (unlike the `ListXResult` types above)
        // also blocks `..base` struct-update syntax outside the defining
        // crate -- build via their constructor, then clear `result_type` by
        // field assignment on the existing instance instead.
        let mut complete = CompleteResult::default();
        complete.result_type = None;
        assert_eq!(
            complete.mark_complete().result_type,
            Some(ResultType::COMPLETE)
        );

        let mut read = rmcp::model::ReadResourceResult::new(Vec::new());
        read.result_type = None;
        match ReadResourceResponse::Complete(read).mark_complete() {
            ReadResourceResponse::Complete(r) => {
                assert_eq!(r.result_type, Some(ResultType::COMPLETE))
            }
            other => panic!("variant changed: {other:?}"),
        }

        let mut prompt = rmcp::model::GetPromptResult::new(Vec::new());
        prompt.result_type = None;
        match GetPromptResponse::Complete(prompt).mark_complete() {
            GetPromptResponse::Complete(r) => {
                assert_eq!(r.result_type, Some(ResultType::COMPLETE))
            }
            other => panic!("variant changed: {other:?}"),
        }
    }

    fn notification(method: &str) -> CustomNotification {
        CustomNotification::new(method.to_string(), None)
    }

    #[test]
    fn routes_to_buffer_when_downstream_is_none() {
        // The production pre-init branch: with no downstream peer,
        // `route_custom_notification` returns `None` (nothing to forward) and
        // buffers the notification rather than dropping it. A wrong impl that
        // silently discarded the notification would fail here.
        let mut state = DownstreamState::default();
        let evt = notification("notifications/codanna/file-reindexed");

        let forward = state.route_custom_notification(evt.clone());

        assert!(
            forward.is_none(),
            "no downstream peer -> nothing to forward yet"
        );
        assert_eq!(state.pending.len(), 1, "notification must be buffered");
        assert_eq!(state.pending[0].method, evt.method);
    }

    #[test]
    fn overflow_drops_oldest_entry() {
        let mut state = DownstreamState::default();

        for i in 0..(PENDING_CUSTOM_NOTIFICATIONS_CAP + 5) {
            state.buffer_pending(notification(&format!("notifications/codanna/evt-{i}")));
        }

        assert_eq!(state.pending.len(), PENDING_CUSTOM_NOTIFICATIONS_CAP);
        // The first 5 pushed (evt-0..evt-4) must have been dropped; the
        // oldest surviving entry is evt-5.
        assert_eq!(
            state.pending.front().unwrap().method,
            "notifications/codanna/evt-5"
        );
        assert_eq!(
            state.pending.back().unwrap().method,
            format!(
                "notifications/codanna/evt-{}",
                PENDING_CUSTOM_NOTIFICATIONS_CAP + 4
            )
        );
    }

    #[test]
    fn drain_preserves_fifo_order_and_empties_buffer() {
        let mut state = DownstreamState::default();
        for i in 0..10 {
            state.buffer_pending(notification(&format!("notifications/codanna/evt-{i}")));
        }

        // The exact drain `DelegatingProxyHandler::initialize` performs.
        let drained = state.drain_pending();

        let methods: Vec<String> = drained.into_iter().map(|n| n.method).collect();
        let expected: Vec<String> = (0..10)
            .map(|i| format!("notifications/codanna/evt-{i}"))
            .collect();
        assert_eq!(methods, expected);

        // Buffer is empty after drain -- nothing left to re-flush.
        assert!(state.pending.is_empty());
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

    #[test]
    fn dead_transport_classification() {
        // Both dead-transport variants are covered. `TransportSend` matters
        // more than its sibling in production, not less: a SIGKILLed or
        // idle-exited backing server is observed when the NEXT send hits the
        // severed connection, which surfaces as `TransportSend(reqwest
        // error)` rather than `TransportClosed`. A wrong `is_dead_transport`
        // that dropped the `TransportSend(_)` arm would leave the most
        // common revive trigger dead, so it must not be possible for this
        // test to pass without it.
        //
        // `DynamicTransportError::from_parts` is rmcp's public, explicitly
        // test-fixture-oriented constructor -- unlike `new`, it needs no
        // concrete `Transport` impl.
        assert!(
            is_dead_transport(&ServiceError::TransportSend(
                rmcp::transport::DynamicTransportError::from_parts(
                    "test-transport",
                    std::any::TypeId::of::<()>(),
                    Box::new(std::io::Error::other("connection reset by peer")),
                )
            )),
            "TransportSend must be classified as a dead transport -- it is how a killed or \
             idle-exited upstream is actually observed"
        );
        assert!(
            is_dead_transport(&ServiceError::TransportClosed),
            "TransportClosed must be classified as a dead transport"
        );

        assert!(
            !is_dead_transport(&ServiceError::McpError(McpError::internal_error(
                "healthy server, protocol-level error",
                None
            ))),
            "a healthy server's own protocol error must NOT trigger a revive"
        );
        assert!(
            !is_dead_transport(&ServiceError::UnexpectedResponse),
            "UnexpectedResponse is a request-shape mismatch on a live transport, not a dead one"
        );
        assert!(
            !is_dead_transport(&ServiceError::Cancelled {
                reason: Some("test".to_string())
            }),
            "Cancelled is a per-request outcome, not evidence the transport is dead"
        );
        assert!(
            !is_dead_transport(&ServiceError::Timeout {
                timeout: std::time::Duration::from_secs(1)
            }),
            "Timeout must NOT trigger a revive -- a slow-but-alive server must not be replaced"
        );
    }

    #[tokio::test]
    async fn concurrent_revive_dials_once() {
        // Drives `single_flight_revive` directly (the generic helper
        // `UpstreamHandle::revive` delegates to) with a plain `(value,
        // generation)` tuple behind a `std::sync::Mutex` and a counting dial
        // closure, instead of a real `RunningService` -- exactly the smaller
        // seam called for instead of a `Dialer` trait with one production
        // implementation.
        use std::sync::atomic::{AtomicUsize, Ordering};

        let reconnect = Arc::new(tokio::sync::Mutex::new(()));
        // (value, generation, failure-from-the-round-that-produced-`generation`).
        let slot: Arc<std::sync::Mutex<(u64, u64, Option<()>)>> =
            Arc::new(std::sync::Mutex::new((0, 0, None)));
        let dial_count = Arc::new(AtomicUsize::new(0));

        const CALLERS: usize = 8;
        let mut handles = Vec::with_capacity(CALLERS);
        for _ in 0..CALLERS {
            let reconnect = reconnect.clone();
            let slot = slot.clone();
            let dial_count = dial_count.clone();
            handles.push(tokio::spawn(async move {
                // Every caller observed the same pre-revive generation (0):
                // this is what N concurrent delegated calls that all failed
                // against the same dead connection look like.
                single_flight_revive::<u64, (), _, _>(
                    &reconnect,
                    0,
                    || {
                        let guard = slot
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        (guard.0, guard.1, guard.2)
                    },
                    |value, generation, failure| {
                        let mut guard = slot
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        *guard = (value, generation, failure);
                    },
                    || {
                        let dial_count = dial_count.clone();
                        async move {
                            dial_count.fetch_add(1, Ordering::SeqCst);
                            // Yield so other waiting callers get a chance to
                            // race for the reconnect gate before this dial
                            // "completes", making the single-flight gate the
                            // thing actually preventing a second dial rather
                            // than mere scheduling luck.
                            tokio::task::yield_now().await;
                            Ok::<u64, ()>(42)
                        }
                    },
                )
                .await
            }));
        }

        let mut results = Vec::with_capacity(CALLERS);
        for handle in handles {
            results.push(handle.await.expect("revive task should not panic"));
        }

        assert_eq!(
            dial_count.load(Ordering::SeqCst),
            1,
            "exactly one dial should happen for {CALLERS} concurrent callers observing the same \
             generation"
        );
        for result in &results {
            assert_eq!(
                result,
                &Ok(42),
                "every caller should observe the single dial's result, not fail or dial again"
            );
        }

        let final_state = *slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(
            final_state,
            (42, 1, None),
            "generation should have advanced by exactly one for all {CALLERS} callers combined, \
             with no cached failure since the round succeeded"
        );
    }

    #[tokio::test]
    async fn failed_revive_dials_once() {
        // The failure-path sibling of `concurrent_revive_dials_once`: N
        // concurrent callers all observing the same dead generation, but this
        // time the dial itself always fails. Before the fix, only the caller
        // that actually took the gate stored anything -- every OTHER caller
        // that took the gate afterwards would see `current_generation ==
        // seen_generation` still (nothing advanced it) and dial again itself,
        // serializing N dials instead of sharing one failure. This test pins
        // that the generation now advances and the failure is cached on a
        // failed round too, so every caller shares the ONE dial's error.
        use std::sync::atomic::{AtomicUsize, Ordering};

        let reconnect = Arc::new(tokio::sync::Mutex::new(()));
        // (value, generation, failure-from-the-round-that-produced-`generation`).
        let slot: Arc<std::sync::Mutex<(u64, u64, Option<&'static str>)>> =
            Arc::new(std::sync::Mutex::new((0, 0, None)));
        let dial_count = Arc::new(AtomicUsize::new(0));

        const CALLERS: usize = 8;
        let mut handles = Vec::with_capacity(CALLERS);
        for _ in 0..CALLERS {
            let reconnect = reconnect.clone();
            let slot = slot.clone();
            let dial_count = dial_count.clone();
            handles.push(tokio::spawn(async move {
                single_flight_revive::<u64, &'static str, _, _>(
                    &reconnect,
                    0,
                    || {
                        let guard = slot
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        (guard.0, guard.1, guard.2)
                    },
                    |value, generation, failure| {
                        let mut guard = slot
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        *guard = (value, generation, failure);
                    },
                    || {
                        let dial_count = dial_count.clone();
                        async move {
                            dial_count.fetch_add(1, Ordering::SeqCst);
                            // Same rationale as `concurrent_revive_dials_once`:
                            // yield so other waiters actually race for the
                            // gate while this dial is "in flight", rather than
                            // relying on scheduling luck to exercise the gate.
                            tokio::task::yield_now().await;
                            Err::<u64, &'static str>("dial failed: connection refused")
                        }
                    },
                )
                .await
            }));
        }

        let mut results = Vec::with_capacity(CALLERS);
        for handle in handles {
            results.push(handle.await.expect("revive task should not panic"));
        }

        assert_eq!(
            dial_count.load(Ordering::SeqCst),
            1,
            "exactly one dial should happen for {CALLERS} concurrent callers observing the same \
             generation, even though that dial fails"
        );
        for result in &results {
            assert_eq!(
                result,
                &Err("dial failed: connection refused"),
                "every caller should observe the single failed dial's error, not succeed or \
                 dial again themselves"
            );
        }

        let final_state = *slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(
            final_state,
            (0, 1, Some("dial failed: connection refused")),
            "generation must still advance by exactly one on a failed round (unchanged value, \
             cached failure), or a waiter arriving after this round would see the stale \
             generation and dial again"
        );
    }

    #[test]
    fn revive_preserves_downstream_state() {
        // A live end-to-end proof that a revived connection's
        // `NotificationRelay` still forwards to the original downstream peer
        // needs a real backing HTTP server and a real revive -- covered by
        // `proxy_revives_dead_upstream_mid_session` in
        // `tests/cli/test_serve_proxy_discovery.rs`.
        //
        // This unit test drives `Dialer::relay` -- the SAME constructor
        // `Dialer::connect` calls on every dial -- rather than re-deriving
        // the invariant from `dial.state`. That distinction is the whole
        // point: asserting `Arc::ptr_eq(&dial.state.clone(), &state)` would
        // be a tautology about `Arc::clone`, true no matter what `connect`
        // actually builds. Going through `relay()` means a relay built from
        // any other state fails here.
        //
        // The complementary guard is structural: `NotificationRelay`
        // deliberately withholds `Default` (see its type docs), so the
        // fresh-state mistake this test targets cannot compile in the first
        // place.
        let state: Arc<Mutex<DownstreamState>> = Arc::new(Mutex::new(DownstreamState::default()));
        let dial = Dialer {
            workspace_root: PathBuf::from("/does/not/matter/for/this/test"),
            config: Settings::default(),
            config_path: None,
            state: state.clone(),
        };

        // Build the relay exactly as the initial dial does, then again as a
        // later revive does. `UpstreamHandle::revive` reuses the SAME
        // `Dialer` value rather than constructing a new one, so both dials
        // route through this one constructor.
        let relay_on_initial_connect = dial.relay();
        let relay_on_later_revive = dial.relay();

        assert!(
            Arc::ptr_eq(&relay_on_initial_connect.state, &state),
            "the initial dial's relay must carry the caller's state Arc, not a fresh one"
        );
        assert!(
            Arc::ptr_eq(&relay_on_later_revive.state, &state),
            "a later revive's relay must carry the SAME state Arc as the initial connect -- a \
             fresh one would leave downstream None forever and silently drop every \
             server-to-client notification after the revive"
        );
        assert!(
            Arc::ptr_eq(
                &relay_on_initial_connect.state,
                &relay_on_later_revive.state
            ),
            "initial connect and later revive must observe pointer-identical state"
        );
    }

    #[test]
    fn local_tool_list_matches_router_with_cache_hints() {
        let router_names: Vec<String> = CodeIntelligenceServer::full_tool_router()
            .list_all()
            .iter()
            .map(|t| t.name.to_string())
            .collect();
        let result = list_tools_result(local_tools());

        let names: Vec<String> = result.tools.iter().map(|t| t.name.to_string()).collect();
        assert_eq!(names, router_names);
        assert!(!names.is_empty());
        assert_eq!(result.result_type, Some(ResultType::COMPLETE));
        assert!(result.ttl_ms.is_some(), "ttl hint must be present");
        assert!(result.cache_scope.is_some(), "cache scope must be present");
    }

    #[test]
    fn local_server_info_is_codanna_without_resource_subscribe() {
        let info = local_server_info();

        assert_eq!(info.server_info.name, "codanna");
        let resources = info
            .capabilities
            .resources
            .as_ref()
            .expect("resources capability is advertised");
        assert_eq!(resources.subscribe, None);
        assert!(info.instructions.is_some());
    }

    fn slot_with(conn: UpstreamConn<u64>, generation: u64) -> std::sync::RwLock<UpstreamSlot<u64>> {
        std::sync::RwLock::new(UpstreamSlot { conn, generation })
    }

    fn read_slot(slot: &std::sync::RwLock<UpstreamSlot<u64>>) -> (&'static str, u64) {
        let guard = slot
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tag = match guard.conn {
            UpstreamConn::Connecting => "connecting",
            UpstreamConn::Ready(_) => "ready",
            UpstreamConn::Failed(_) => "failed",
        };
        (tag, guard.generation)
    }

    fn count_concurrent_flips(slot: Arc<std::sync::RwLock<UpstreamSlot<u64>>>, seen: u64) -> usize {
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let slot = slot.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    flip_to_connecting(&slot, seen)
                })
            })
            .collect();
        threads
            .into_iter()
            .map(|t| t.join().expect("flip thread should not panic"))
            .filter(|won| *won)
            .count()
    }

    #[test]
    fn failed_slot_flips_to_connecting_with_exactly_one_winner() {
        let slot = Arc::new(slot_with(
            UpstreamConn::Failed(McpError::internal_error("boom", None)),
            3,
        ));

        assert_eq!(count_concurrent_flips(slot.clone(), 3), 1);
        assert_eq!(read_slot(&slot), ("connecting", 3));
    }

    #[test]
    fn ready_slot_dead_transport_flip_has_exactly_one_winner() {
        let slot = Arc::new(slot_with(UpstreamConn::Ready(7), 2));

        assert_eq!(count_concurrent_flips(slot.clone(), 2), 1);
        assert_eq!(read_slot(&slot), ("connecting", 2));
    }

    #[test]
    fn connecting_and_stale_generation_never_flip() {
        let connecting = slot_with(UpstreamConn::Connecting, 0);
        assert!(!flip_to_connecting(&connecting, 0));
        assert_eq!(read_slot(&connecting), ("connecting", 0));

        let stale_failed = slot_with(UpstreamConn::Failed(McpError::internal_error("x", None)), 5);
        assert!(!flip_to_connecting(&stale_failed, 4));
        assert_eq!(read_slot(&stale_failed), ("failed", 5));

        let stale_ready = slot_with(UpstreamConn::Ready(1), 5);
        assert!(!flip_to_connecting(&stale_ready, 4));
        assert_eq!(read_slot(&stale_ready), ("ready", 5));
    }

    #[tokio::test]
    async fn revive_round_stores_failed_or_ready_at_next_generation() {
        let reconnect = tokio::sync::Mutex::new(());

        let failing = slot_with(UpstreamConn::Connecting, 0);
        let result = single_flight_revive(
            &reconnect,
            0,
            || read_round(&failing),
            |v, g, f| commit_round(&failing, v, g, f),
            || async { Err::<UpstreamConn<u64>, McpError>(McpError::internal_error("down", None)) },
        )
        .await;
        assert!(result.is_err());
        assert_eq!(read_slot(&failing), ("failed", 1));

        let succeeding = slot_with(UpstreamConn::Connecting, 0);
        let result = single_flight_revive(
            &reconnect,
            0,
            || read_round(&succeeding),
            |v, g, f| commit_round(&succeeding, v, g, f),
            || async { Ok::<UpstreamConn<u64>, McpError>(UpstreamConn::Ready(9)) },
        )
        .await;
        assert!(result.is_ok());
        assert_eq!(read_slot(&succeeding), ("ready", 1));
    }

    #[test]
    fn unavailable_responses_are_complete_tool_errors_with_pinned_text() {
        let not_ready = unavailable_tool_response(NOT_READY.to_string());
        let inner = McpError::internal_error("failed to reach backing MCP server: refused", None);
        let failed = unavailable_tool_response(failed_text(&inner));

        for (response, needle) in [
            (not_ready, "codanna index not available yet"),
            (failed, "codanna backend unavailable:"),
        ] {
            match response {
                CallToolResponse::Complete(r) => {
                    assert_eq!(r.is_error, Some(true));
                    assert_eq!(r.result_type, Some(ResultType::COMPLETE));
                    let text = serde_json::to_string(&r.content).expect("content serializes");
                    assert!(text.contains(needle), "{text} should contain {needle}");
                }
                other => panic!("expected a complete result: {other:?}"),
            }
        }
        assert!(failed_text(&inner).contains("refused"));
    }
}
