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

/// Dials the backing HTTP MCP server: the single site both `serve_proxy`'s
/// initial connection and [`UpstreamHandle::revive`]'s reconnect go through,
/// so the HTTPS cert-pinning branch (and any future scheme handling) exists
/// exactly once (`connect()` is invoked from two call sites, but is one
/// dial implementation).
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

        Ok(Arc::new(service))
    }
}

/// The current upstream connection plus a generation counter bumped on every
/// COMPLETED revive round -- success or failure alike -- so a caller that
/// observed generation `g` before a call failed can tell -- after taking the
/// reconnect gate -- whether someone else already ran a round in the meantime
/// (generation moved past `g`) or it must dial itself. On a failed round
/// `service` is left unchanged (there is no new connection to install); only
/// `generation` advances, paired with the cached failure in
/// [`UpstreamHandle::last_failure`] that [`single_flight_revive`]'s
/// `read_current` closure also returns.
struct UpstreamSlot {
    service: Arc<RunningService<RoleClient, NotificationRelay>>,
    generation: u64,
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

/// Owns the live upstream connection behind a lock that is read
/// synchronously (from [`ServerHandler::get_info`], which is NOT `async`) and
/// revived on demand when a delegated call observes a dead transport.
///
/// `slot` is `std::sync::RwLock`, not `tokio::sync::RwLock`: `get_info` must
/// read the current service to call `peer_info()` and cannot itself be
/// `async`, so the lock it reads through must be a synchronous one. Every
/// critical section under `slot` is a single `Arc` clone or a single
/// assignment -- it never spans an `.await` -- so holding a `std::sync`
/// guard across it can never block the async runtime.
struct UpstreamHandle {
    slot: std::sync::RwLock<UpstreamSlot>,
    /// Serializes revives so concurrent delegated calls that all observed the
    /// same dead generation dial exactly once between them, on both the
    /// success AND failure path (see [`single_flight_revive`]). Cross-process
    /// dedup for the underlying `discover_or_spawn` call is already handled
    /// by its own `O_EXCL` `.codanna/http.lock`; this mutex closes the
    /// *intra-process* gate that primitive does not cover.
    reconnect: tokio::sync::Mutex<()>,
    /// Error from the most recently COMPLETED revive round, if that round
    /// failed; `None` if the round at the current `slot.generation` succeeded
    /// (or no revive has run yet). Cleared back to `None` on every successful
    /// round so a stale failure from an earlier generation is never confused
    /// with the current one. Read/written only under `reconnect`'s gate (via
    /// [`single_flight_revive`]'s `read_current`/`store` closures below), so
    /// a plain `std::sync::Mutex` -- never awaited across -- suffices; it does
    /// not need to be part of `slot` because a failed round never touches
    /// `slot.service`.
    last_failure: std::sync::Mutex<Option<McpError>>,
    dial: Dialer,
}

impl UpstreamHandle {
    /// Returns the current upstream connection and its generation. Reads
    /// through `slot`'s synchronous lock and clones the `Arc`; never awaits,
    /// so it is safe to call from synchronous code such as `get_info`.
    fn current(&self) -> (Arc<RunningService<RoleClient, NotificationRelay>>, u64) {
        // The critical section is a single `Arc` clone and cannot panic, so a
        // poisoned lock (left by an unrelated panic elsewhere while holding
        // it) still carries a fully-valid `UpstreamSlot`; recovering the
        // inner value is safe rather than propagating the poison.
        let slot = self
            .slot
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (slot.service.clone(), slot.generation)
    }

    /// Revives the upstream connection if `seen_generation` is still current,
    /// otherwise returns whatever the round that already ran for that
    /// generation produced -- the revived connection if it succeeded, or the
    /// SAME error it failed with if it did not (rather than dialing again).
    /// See [`single_flight_revive`] for the gating logic and
    /// [`Dialer::connect`] for the dial itself.
    async fn revive(
        &self,
        seen_generation: u64,
    ) -> Result<Arc<RunningService<RoleClient, NotificationRelay>>, McpError> {
        let workspace_root = self.dial.workspace_root.clone();
        single_flight_revive(
            &self.reconnect,
            seen_generation,
            || {
                // Same panic-free critical section as `current` (see there).
                let slot = self
                    .slot
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let failure = self
                    .last_failure
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone();
                (slot.service.clone(), slot.generation, failure)
            },
            |service, generation, failure| {
                let mut slot = self
                    .slot
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                // The previous `Arc<RunningService<..>>` is simply dropped
                // here on a successful round. `RunningService::cancel` takes
                // `self` by value, which an `Arc` cannot yield while an
                // in-flight retry on the old connection may still hold a
                // clone, and rmcp's own `Drop for RunningService` already
                // closes the connection asynchronously once the last clone
                // goes away. On a FAILED round `service` is this same old
                // connection handed back unchanged -- there is nothing new to
                // install, only the generation and `last_failure` advance.
                *slot = UpstreamSlot {
                    service,
                    generation,
                };
                *self
                    .last_failure
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = failure;
            },
            || async {
                self.dial.connect().await.map_err(|err| {
                    McpError::internal_error(
                        format!(
                            "failed to revive backing MCP server for workspace '{}': {err}",
                            workspace_root.display()
                        ),
                        None,
                    )
                })
            },
        )
        .await
    }
}

/// `ServerHandler` facing the stdio client. Every request is delegated to the
/// upstream HTTP MCP server; this process holds no `IndexFacade` and no
/// index state of its own.
#[derive(Clone)]
struct DelegatingProxyHandler {
    /// `Arc<UpstreamHandle>` keeps `#[derive(Clone)]` on this handler cheap
    /// (one `Arc` clone) while still sharing the single live connection,
    /// generation counter, and reconnect gate across every clone of the
    /// handler.
    upstream: Arc<UpstreamHandle>,
    /// Shared with the `NotificationRelay` driving `upstream`; custom
    /// notifications received before `state.downstream` is populated are
    /// buffered in `state.pending` and drained atomically with setting
    /// `state.downstream` in `initialize` (see [`DownstreamState`]).
    state: Arc<Mutex<DownstreamState>>,
}

/// Maximum time to wait for a single delegated upstream call, applied
/// per-attempt: a revive-then-retry after a dead-transport failure gets a
/// fresh budget for its one retry, rather than sharing the first attempt's
/// timeout. A hung upstream must not leave the stdio client's request
/// pending forever; this is a fixed budget rather than a new config knob,
/// kept minimal per scope.
const UPSTREAM_CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

fn upstream_timeout_error() -> McpError {
    McpError::internal_error(
        format!(
            "delegated upstream call timed out after {}s",
            UPSTREAM_CALL_TIMEOUT.as_secs()
        ),
        None,
    )
}

impl DelegatingProxyHandler {
    /// Delegates one inbound request to the upstream server, with exactly one
    /// revive-and-retry on a dead-transport failure and no more: a second
    /// dead-transport error means the backing server is not coming back up,
    /// and that mapped error is returned rather than looping or backing off.
    ///
    /// `op` is `Fn`, not `FnOnce`, because it may be invoked up to twice (the
    /// original attempt and, only on a dead transport, the retry against the
    /// revived connection) -- every call site below clones its request params
    /// into the closure body per invocation rather than moving them in once.
    /// [`UPSTREAM_CALL_TIMEOUT`] is applied to each attempt independently.
    async fn delegate<T, F, Fut>(&self, op: F) -> Result<T, McpError>
    where
        F: Fn(Arc<RunningService<RoleClient, NotificationRelay>>) -> Fut,
        Fut: Future<Output = Result<T, ServiceError>>,
    {
        let (service, generation) = self.upstream.current();
        match tokio::time::timeout(UPSTREAM_CALL_TIMEOUT, op(service)).await {
            Ok(Ok(value)) => return Ok(value),
            // A healthy server's own protocol error (or any non-transport
            // failure) passes through unchanged -- no revive.
            Ok(Err(err)) if !is_dead_transport(&err) => return Err(map_service_err(err)),
            // Dead transport: fall through to the single revive-and-retry
            // below.
            Ok(Err(_)) => {}
            // The proxy's own timeout, not a `ServiceError` -- never
            // triggers a revive (see `is_dead_transport`'s doc comment).
            Err(_) => return Err(upstream_timeout_error()),
        }

        let revived = self.upstream.revive(generation).await?;
        match tokio::time::timeout(UPSTREAM_CALL_TIMEOUT, op(revived)).await {
            Ok(result) => result.map_err(map_service_err),
            Err(_) => Err(upstream_timeout_error()),
        }
    }
}

impl ServerHandler for DelegatingProxyHandler {
    fn get_info(&self) -> ServerInfo {
        // Reflect the upstream server's negotiated capabilities/info when
        // available (set during the upstream `initialize` handshake that
        // already completed by the time this proxy starts serving stdio);
        // fall back to a minimal description if it is somehow unset. Reads
        // the current connection synchronously via `UpstreamHandle::current`
        // -- this method is NOT `async` and cannot await a revive, so it
        // always reflects whatever connection is live right now.
        self.upstream
            .current()
            .0
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
        request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        self.delegate(|up| {
            let request = request.clone();
            async move { up.list_tools(request).await }
        })
        .await
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        self.delegate(|up| {
            let request = request.clone();
            async move { up.call_tool(request).await }
        })
        .await
    }

    async fn list_resources(
        &self,
        request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        self.delegate(|up| {
            let request = request.clone();
            async move { up.list_resources(request).await }
        })
        .await
    }

    async fn list_resource_templates(
        &self,
        request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, McpError> {
        self.delegate(|up| {
            let request = request.clone();
            async move { up.list_resource_templates(request).await }
        })
        .await
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, McpError> {
        self.delegate(|up| {
            let request = request.clone();
            async move { up.read_resource(request).await }
        })
        .await
    }

    async fn list_prompts(
        &self,
        request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, McpError> {
        self.delegate(|up| {
            let request = request.clone();
            async move { up.list_prompts(request).await }
        })
        .await
    }

    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetPromptResult, McpError> {
        self.delegate(|up| {
            let request = request.clone();
            async move { up.get_prompt(request).await }
        })
        .await
    }

    async fn complete(
        &self,
        request: CompleteRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CompleteResult, McpError> {
        self.delegate(|up| {
            let request = request.clone();
            async move { up.complete(request).await }
        })
        .await
    }

    async fn set_level(
        &self,
        request: SetLevelRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        self.delegate(|up| {
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
        self.delegate(|up| {
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
        self.delegate(|up| {
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
            .delegate(|up| {
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

    let state: Arc<Mutex<DownstreamState>> = Arc::new(Mutex::new(DownstreamState::default()));
    let dial = Dialer {
        workspace_root,
        config,
        config_path,
        state: state.clone(),
    };

    // `Dialer::connect` is the single dial site: this initial connection and
    // every later revive (`UpstreamHandle::revive`) both go through it, so
    // there is exactly one HTTPS-pinning branch, not two.
    let upstream = dial.connect().await?;

    let handler = DelegatingProxyHandler {
        upstream: Arc::new(UpstreamHandle {
            slot: std::sync::RwLock::new(UpstreamSlot {
                service: upstream,
                generation: 0,
            }),
            reconnect: tokio::sync::Mutex::new(()),
            last_failure: std::sync::Mutex::new(None),
            dial,
        }),
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
}
