//! Portable, capability-routed transport for `zmux notify`.
//!
//! The GUI binds one ephemeral IPv4 loopback listener for the process, then
//! registers a separate random capability for every terminal. Only that
//! terminal inherits its serialized [`CliEndpoint`]. The server resolves the
//! capability to a server-minted [`CliRouteId`]; route identity is never taken
//! from client-supplied PIDs or target fields.
//!
//! Delivery is transactional. A successful client response is written only
//! after the GPUI consumer calls [`CliRequestCompletion::recorded`]. Queueing,
//! failing to resolve a live route, or dropping the completion handle cannot be
//! mistaken for a recorded notification.

use std::fmt;
use std::io::{ErrorKind, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream};
use std::str::FromStr;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU8, Ordering},
    mpsc::{Receiver as CompletionReceiver, RecvTimeoutError, SyncSender, sync_channel},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};
use async_channel::{Receiver, Sender, bounded};
use gpui::Global;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use uuid::Uuid;

/// Name of the environment variable inherited by terminals inside zmux.
pub const NOTIFY_ENDPOINT_ENV: &str = "ZMUX_NOTIFY_ENDPOINT";

const PROTOCOL_VERSION: u16 = 3;
const MAX_FRAME_BYTES: usize = 64 * 1024;
const MAX_TOKEN_BYTES: usize = 64;
const ROUTE_SELECTOR_BYTES: usize = 16;
const ROUTE_PROOF_KEY_BYTES: usize = 32;
const CLIENT_NONCE_BYTES: usize = 16;
const AUTH_PROOF_BYTES: usize = 32;
const MAX_TITLE_BYTES: usize = 512;
const MAX_SUBTITLE_BYTES: usize = 1_024;
const MAX_BODY_BYTES: usize = 32 * 1024;
const MAX_REJECTION_BYTES: usize = 1_024;
const MAX_REGISTERED_ROUTES: usize = 4_096;
const CONNECTION_QUEUE_CAPACITY: usize = 64;
const NOTIFICATION_QUEUE_CAPACITY: usize = 256;
const WORKER_COUNT: usize = 4;
// The accept socket stays nonblocking so shutdown is observable; 100ms bounds
// idle wakeups at 10/s while adding imperceptible latency to CLI deliveries.
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(100);
const SERVER_IO_TIMEOUT: Duration = Duration::from_secs(2);
const CLIENT_RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);
const PROCESSING_TIMEOUT: Duration = Duration::from_secs(3);
const IO_POLL_INTERVAL: Duration = Duration::from_millis(25);
const COMPLETION_POLL_INTERVAL: Duration = IO_POLL_INTERVAL;
const AUTH_TRANSCRIPT_DOMAIN: &[u8] = b"zmux-notify/server-auth/v3";

/// A notification submitted by the `zmux notify` command.
///
/// Origin is deliberately absent. The authenticated endpoint determines it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CliNotification {
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    #[serde(default)]
    pub body: String,
}

impl CliNotification {
    pub fn new(
        title: impl Into<String>,
        subtitle: Option<String>,
        body: impl Into<String>,
    ) -> Self {
        Self {
            title: title.into(),
            subtitle,
            body: body.into(),
        }
    }

    fn validate(&self) -> anyhow::Result<()> {
        if self.title.trim().is_empty() {
            bail!("notification title must not be empty");
        }
        if self.title.len() > MAX_TITLE_BYTES {
            bail!("notification title exceeds {MAX_TITLE_BYTES} bytes");
        }
        if self
            .subtitle
            .as_ref()
            .is_some_and(|subtitle| subtitle.len() > MAX_SUBTITLE_BYTES)
        {
            bail!("notification subtitle exceeds {MAX_SUBTITLE_BYTES} bytes");
        }
        if self.body.len() > MAX_BODY_BYTES {
            bail!("notification body exceeds {MAX_BODY_BYTES} bytes");
        }
        Ok(())
    }
}

/// Opaque, server-minted identity for one terminal route.
///
/// This ID is safe to use as an in-process map key. It is not the bearer
/// credential and cannot be used to authenticate a client.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CliRouteId(Uuid);

impl fmt::Display for CliRouteId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

/// A completion capability for exactly one accepted client request.
///
/// Consumers must call [`Self::begin_recording`] immediately before mutating
/// the canonical GPUI store, then [`Self::recorded`] after the row exists. If
/// the server deadline won first, `begin_recording` returns false and the
/// request must not be published. Any routing/store failure calls
/// [`Self::reject`].
#[must_use = "a CLI request must be completed after GPUI records or rejects it"]
pub struct CliRequestCompletion {
    sender: Option<SyncSender<ProcessingResult>>,
    lifecycle: Arc<RequestLifecycle>,
}

impl fmt::Debug for CliRequestCompletion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CliRequestCompletion")
            .field("pending", &self.sender.is_some())
            .field("state", &self.lifecycle.state.load(Ordering::Acquire))
            .finish()
    }
}

impl CliRequestCompletion {
    /// Atomically claim this still-live request immediately before publishing.
    /// A false result means the server already timed it out or shut down.
    #[must_use = "a canceled request must not be written to the notification store"]
    pub fn begin_recording(&self) -> bool {
        self.lifecycle.begin_recording()
    }

    /// Confirm that GPUI resolved the exact route and recorded the row.
    pub fn recorded(mut self) {
        if self.lifecycle.recorded()
            && let Some(sender) = self.sender.take()
        {
            let _ = sender.send(ProcessingResult::Recorded);
        }
    }

    /// Reject a request that could not be resolved or recorded.
    pub fn reject(mut self, reason: impl Into<String>) {
        if self.lifecycle.finish(REQUEST_REJECTED)
            && let Some(sender) = self.sender.take()
        {
            let _ = sender.send(ProcessingResult::Rejected(bounded_rejection(reason.into())));
        }
    }
}

impl Drop for CliRequestCompletion {
    fn drop(&mut self) {
        if self.lifecycle.finish(REQUEST_REJECTED)
            && let Some(sender) = self.sender.take()
        {
            let _ = sender.send(ProcessingResult::Rejected(
                "notification consumer dropped the request before recording it".to_owned(),
            ));
        }
    }
}

const REQUEST_PENDING: u8 = 0;
const REQUEST_RECORDING: u8 = 1;
const REQUEST_RECORDED: u8 = 2;
const REQUEST_REJECTED: u8 = 3;
const REQUEST_CANCELED: u8 = 4;

#[derive(Default)]
struct RequestLifecycle {
    state: AtomicU8,
}

impl RequestLifecycle {
    fn begin_recording(&self) -> bool {
        self.state
            .compare_exchange(
                REQUEST_PENDING,
                REQUEST_RECORDING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    fn finish(&self, next: u8) -> bool {
        let mut current = self.state.load(Ordering::Acquire);
        loop {
            if !matches!(current, REQUEST_PENDING | REQUEST_RECORDING) {
                return false;
            }
            match self.state.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(observed) => current = observed,
            }
        }
    }

    fn recorded(&self) -> bool {
        self.state
            .compare_exchange(
                REQUEST_RECORDING,
                REQUEST_RECORDED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    fn cancel_if_pending(&self) -> bool {
        self.state
            .compare_exchange(
                REQUEST_PENDING,
                REQUEST_CANCELED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    fn state(&self) -> u8 {
        self.state.load(Ordering::Acquire)
    }
}

/// A validated request whose route was derived from a registered capability.
#[derive(Debug)]
pub struct ReceivedCliNotification {
    pub route_id: CliRouteId,
    pub notification: CliNotification,
    pub peer_addr: SocketAddr,
    pub completion: CliRequestCompletion,
}

/// Authenticated endpoint inherited by one terminal only.
///
/// `proof_key` authenticates the listener before the client discloses
/// `token` or notification content. Debug output deliberately redacts both.
#[derive(Clone, PartialEq, Eq)]
pub struct CliEndpoint {
    address: SocketAddrV4,
    route_selector: [u8; ROUTE_SELECTOR_BYTES],
    proof_key: [u8; ROUTE_PROOF_KEY_BYTES],
    token: String,
}

impl CliEndpoint {
    pub fn address(&self) -> SocketAddrV4 {
        self.address
    }

    /// Serialize this endpoint for [`NOTIFY_ENDPOINT_ENV`].
    pub fn to_env_value(&self) -> String {
        format!(
            "v{PROTOCOL_VERSION};{};{};{};{}",
            self.address,
            encode_hex(&self.route_selector),
            encode_hex(&self.proof_key),
            self.token
        )
    }
}

impl fmt::Debug for CliEndpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CliEndpoint")
            .field("version", &PROTOCOL_VERSION)
            .field("address", &self.address)
            .field("route_selector", &"<opaque>")
            .field("proof_key", &"<redacted>")
            .field("token", &"<redacted>")
            .finish()
    }
}

impl FromStr for CliEndpoint {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let mut fields = value.split(';');
        let version = fields
            .next()
            .context("notification endpoint has no version")?;
        let address = fields
            .next()
            .context("notification endpoint has no address")?;
        let route_selector = fields
            .next()
            .context("notification endpoint has no route selector")?;
        let proof_key = fields
            .next()
            .context("notification endpoint has no route proof key")?;
        let token = fields
            .next()
            .context("notification endpoint has no capability")?;
        if fields.next().is_some() {
            bail!("notification endpoint contains unexpected fields");
        }
        if version != format!("v{PROTOCOL_VERSION}") {
            bail!("unsupported notification endpoint version {version:?}");
        }
        let address = address
            .parse::<SocketAddrV4>()
            .context("invalid notification endpoint address")?;
        if !address.ip().is_loopback() {
            bail!("notification endpoint must use an IPv4 loopback address");
        }
        let route_selector = decode_fixed_hex::<ROUTE_SELECTOR_BYTES>(route_selector)
            .context("invalid notification endpoint route selector")?;
        let proof_key = decode_fixed_hex::<ROUTE_PROOF_KEY_BYTES>(proof_key)
            .context("invalid notification endpoint route proof key")?;
        if token.is_empty() || token.len() > MAX_TOKEN_BYTES {
            bail!("invalid notification endpoint capability");
        }

        Ok(Self {
            address,
            route_selector,
            proof_key,
            token: token.to_owned(),
        })
    }
}

#[derive(Clone, Copy)]
enum RouteLookup {
    Active(CliRouteId),
    Pending,
    Unknown,
}

struct RouteRecord {
    id: CliRouteId,
    selector: [u8; ROUTE_SELECTOR_BYTES],
    proof_key: [u8; ROUTE_PROOF_KEY_BYTES],
    token: String,
    active: bool,
}

#[derive(Default)]
struct RouteRegistry {
    closed: AtomicBool,
    routes: Mutex<Vec<RouteRecord>>,
}

impl RouteRegistry {
    fn len(&self) -> usize {
        self.routes
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .len()
    }

    fn register(
        &self,
    ) -> anyhow::Result<(
        CliRouteId,
        [u8; ROUTE_SELECTOR_BYTES],
        [u8; ROUTE_PROOF_KEY_BYTES],
        String,
    )> {
        if self.closed.load(Ordering::Acquire) {
            bail!("notification route registry is closed");
        }

        let mut routes = self
            .routes
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if self.closed.load(Ordering::Acquire) {
            bail!("notification route registry is closed");
        }
        if routes.len() >= MAX_REGISTERED_ROUTES {
            bail!("notification route limit of {MAX_REGISTERED_ROUTES} reached");
        }

        let (id, selector, proof_key, token) = loop {
            let id = CliRouteId(Uuid::new_v4());
            let selector = random_bytes::<ROUTE_SELECTOR_BYTES>()?;
            let proof_key = random_bytes::<ROUTE_PROOF_KEY_BYTES>()?;
            let token = encode_hex(&random_bytes::<32>()?);
            if routes
                .iter()
                .all(|route| route.id != id && route.selector != selector && route.token != token)
            {
                break (id, selector, proof_key, token);
            }
        };
        routes.push(RouteRecord {
            id,
            selector,
            proof_key,
            token: token.clone(),
            active: false,
        });
        Ok((id, selector, proof_key, token))
    }

    fn activate(&self, id: CliRouteId) -> bool {
        if self.closed.load(Ordering::Acquire) {
            return false;
        }
        let mut routes = self
            .routes
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let Some(route) = routes.iter_mut().find(|route| route.id == id) else {
            return false;
        };
        route.active = true;
        true
    }

    fn unregister(&self, id: CliRouteId) {
        let mut routes = self
            .routes
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        routes.retain(|route| route.id != id);
    }

    fn lookup(&self, selector: &[u8], candidate: &str) -> RouteLookup {
        if self.closed.load(Ordering::Acquire) {
            return RouteLookup::Unknown;
        }
        let routes = self
            .routes
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let mut matched = RouteLookup::Unknown;

        // Scan every registered route and compare a fixed number of bytes. Do
        // not expose which capability prefix, length, or map bucket matched.
        for route in routes.iter() {
            if constant_time_bytes_eq(&route.selector, selector)
                && constant_time_token_eq(&route.token, candidate)
            {
                matched = if route.active {
                    RouteLookup::Active(route.id)
                } else {
                    RouteLookup::Pending
                };
            }
        }
        matched
    }

    fn proof_key_for_selector(&self, selector: &[u8]) -> Option<[u8; ROUTE_PROOF_KEY_BYTES]> {
        if self.closed.load(Ordering::Acquire) || selector.len() != ROUTE_SELECTOR_BYTES {
            return None;
        }
        let routes = self
            .routes
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        routes
            .iter()
            .find(|route| constant_time_bytes_eq(&route.selector, selector))
            .map(|route| route.proof_key)
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.routes
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
    }
}

/// RAII lease for one terminal's route capability.
///
/// Create this before spawning the terminal, inject [`Self::endpoint_env`] only
/// into that terminal, bind [`Self::route_id`] to the exact runtime target, and
/// then call [`Self::activate`]. Retain the lease for exactly as long as that
/// target is live; dropping it immediately revokes descendants' stale tokens.
pub struct CliRouteRegistration {
    id: CliRouteId,
    endpoint: CliEndpoint,
    registry: Arc<RouteRegistry>,
}

impl fmt::Debug for CliRouteRegistration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CliRouteRegistration")
            .field("id", &self.id)
            .field("endpoint", &self.endpoint)
            .finish()
    }
}

impl CliRouteRegistration {
    pub fn route_id(&self) -> CliRouteId {
        self.id
    }

    pub fn endpoint(&self) -> &CliEndpoint {
        &self.endpoint
    }

    pub fn endpoint_env(&self) -> String {
        self.endpoint.to_env_value()
    }

    /// Make the capability usable after its exact runtime target is bound.
    #[must_use = "activation can fail if the server or registration was closed"]
    pub fn activate(&self) -> bool {
        self.registry.activate(self.id)
    }
}

impl Drop for CliRouteRegistration {
    fn drop(&mut self) {
        self.registry.unregister(self.id);
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClientHello {
    version: u16,
    route_selector: Vec<u8>,
    nonce: Vec<u8>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ServerHello {
    version: u16,
    proof: Vec<u8>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestEnvelope {
    version: u16,
    token: String,
    notification: CliNotification,
}

#[derive(Debug, Serialize, Deserialize)]
struct ResponseEnvelope {
    version: u16,
    status: ResponseStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ResponseStatus {
    Recorded,
    Unauthorized,
    Pending,
    Busy,
    Invalid,
    Rejected,
    TimedOut,
}

enum ProcessingResult {
    Recorded,
    Rejected(String),
    TimedOut,
    ShuttingDown,
}

struct AcceptedConnection {
    stream: TcpStream,
    read_deadline: Instant,
}

/// Cross-platform notification server. Dropping it revokes every route, stops
/// the listener, and joins all worker threads.
pub struct CliServer {
    address: SocketAddrV4,
    routes: Arc<RouteRegistry>,
    notifications: Receiver<ReceivedCliNotification>,
    running: Arc<AtomicBool>,
    connections: Sender<AcceptedConnection>,
    accept_thread: Option<JoinHandle<()>>,
    worker_threads: Vec<JoinHandle<()>>,
}

impl Global for CliServer {}

impl CliServer {
    /// Bind a new server on `127.0.0.1:0`. This does not create a shared
    /// credential; call [`Self::register_route`] for each terminal spawn.
    pub fn start() -> anyhow::Result<Self> {
        let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
            .context("failed to bind zmux notification listener")?;
        let address = match listener.local_addr()? {
            SocketAddr::V4(address) => address,
            SocketAddr::V6(_) => unreachable!("an IPv4 listener returned an IPv6 address"),
        };
        listener
            .set_nonblocking(true)
            .context("failed to make notification listener nonblocking")?;

        let routes = Arc::new(RouteRegistry::default());
        let running = Arc::new(AtomicBool::new(true));
        let (connection_tx, connection_rx) = bounded(CONNECTION_QUEUE_CAPACITY);
        let (notification_tx, notification_rx) = bounded(NOTIFICATION_QUEUE_CAPACITY);

        let accept_running = Arc::clone(&running);
        let accept_connections = connection_tx.clone();
        let accept_thread = thread::Builder::new()
            .name("zmux-notify-accept".to_owned())
            .spawn(move || accept_loop(listener, accept_connections, accept_running))
            .context("failed to start notification listener thread")?;

        let mut worker_threads: Vec<JoinHandle<()>> = Vec::with_capacity(WORKER_COUNT);
        for index in 0..WORKER_COUNT {
            let worker_running = Arc::clone(&running);
            let worker_connections = connection_rx.clone();
            let worker_notifications = notification_tx.clone();
            let worker_routes = Arc::clone(&routes);
            let worker = match thread::Builder::new()
                .name(format!("zmux-notify-worker-{index}"))
                .spawn(move || {
                    worker_loop(
                        worker_connections,
                        worker_notifications,
                        worker_routes,
                        worker_running,
                        address,
                    );
                }) {
                Ok(worker) => worker,
                Err(error) => {
                    running.store(false, Ordering::Release);
                    routes.close();
                    connection_tx.close();
                    let _ = accept_thread.join();
                    for worker in worker_threads {
                        let _ = worker.join();
                    }
                    return Err(error).context("failed to start notification worker thread");
                }
            };
            worker_threads.push(worker);
        }
        drop(notification_tx);

        Ok(Self {
            address,
            routes,
            notifications: notification_rx,
            running,
            connections: connection_tx,
            accept_thread: Some(accept_thread),
            worker_threads,
        })
    }

    pub fn address(&self) -> SocketAddrV4 {
        self.address
    }

    /// Return the number of live route leases without exposing their selectors,
    /// proof keys, tokens, or activation state. This read-only diagnostic is
    /// useful for verifying that canceled terminal spawns release staged routes.
    pub fn registered_route_count(&self) -> usize {
        self.routes.len()
    }

    /// Mint a new pending route capability for one terminal spawn.
    pub fn register_route(&self) -> anyhow::Result<CliRouteRegistration> {
        let (id, route_selector, proof_key, token) = self.routes.register()?;
        Ok(CliRouteRegistration {
            id,
            endpoint: CliEndpoint {
                address: self.address,
                route_selector,
                proof_key,
                token,
            },
            registry: Arc::clone(&self.routes),
        })
    }

    /// Clone the sole logical delivery channel for an async runtime consumer.
    /// Multiple receivers compete for messages, so applications should call
    /// this once and keep the returned receiver alive.
    pub fn receiver(&self) -> Receiver<ReceivedCliNotification> {
        self.notifications.clone()
    }

    /// Send through the per-terminal endpoint inherited in
    /// [`NOTIFY_ENDPOINT_ENV`].
    pub fn notify(notification: CliNotification) -> anyhow::Result<()> {
        let endpoint = std::env::var(NOTIFY_ENDPOINT_ENV).with_context(|| {
            format!("{NOTIFY_ENDPOINT_ENV} is not set; run `zmux notify` inside a zmux terminal")
        })?;
        let endpoint = endpoint.parse::<CliEndpoint>()?;
        Self::send_to(&endpoint, notification)
    }

    /// Send to an explicit per-terminal endpoint.
    pub fn send_to(endpoint: &CliEndpoint, notification: CliNotification) -> anyhow::Result<()> {
        notification.validate()?;
        Self::send_to_until(
            endpoint,
            notification,
            Instant::now() + CLIENT_RESPONSE_TIMEOUT,
        )
    }

    fn send_to_until(
        endpoint: &CliEndpoint,
        notification: CliNotification,
        deadline: Instant,
    ) -> anyhow::Result<()> {
        loop {
            let attempt = match Self::send_once(endpoint, &notification, deadline) {
                Ok(attempt) => attempt,
                Err(error) if Instant::now() >= deadline => {
                    return Err(error).context(
                        "zmux notification route was not ready before the client deadline",
                    );
                }
                Err(error) => return Err(error),
            };
            match attempt {
                SendAttempt::Recorded => return Ok(()),
                SendAttempt::Pending => {
                    let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                        bail!("zmux notification route was not ready before the client deadline");
                    };
                    thread::sleep(IO_POLL_INTERVAL.min(remaining));
                }
            }
        }
    }

    fn send_once(
        endpoint: &CliEndpoint,
        notification: &CliNotification,
        deadline: Instant,
    ) -> anyhow::Result<SendAttempt> {
        let address = SocketAddr::V4(endpoint.address);
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .context("zmux notification client deadline elapsed before connecting")?;
        let mut stream = TcpStream::connect_timeout(&address, SERVER_IO_TIMEOUT.min(remaining))
            .context("failed to connect to zmux notification listener")?;
        configure_client_stream(&stream)?;

        // Authenticate the listener before disclosing the per-terminal route
        // capability or any notification content. A stale process that rebinds
        // this port sees only a fresh nonce.
        let nonce = random_client_nonce()?;
        write_frame(
            &mut stream,
            &ClientHello {
                version: PROTOCOL_VERSION,
                route_selector: endpoint.route_selector.to_vec(),
                nonce: nonce.to_vec(),
            },
        )?;
        let hello: ServerHello = read_frame_until(&mut stream, deadline, None)
            .context("failed to authenticate zmux notification listener")?;
        if hello.version != PROTOCOL_VERSION
            || hello.proof.len() != AUTH_PROOF_BYTES
            || !verify_server_proof(
                &endpoint.proof_key,
                endpoint.address,
                &endpoint.route_selector,
                &nonce,
                &hello.proof,
            )
        {
            bail!("failed to authenticate zmux notification listener");
        }

        write_frame(
            &mut stream,
            &RequestEnvelope {
                version: PROTOCOL_VERSION,
                token: endpoint.token.clone(),
                notification: notification.clone(),
            },
        )?;

        let response: ResponseEnvelope = read_frame_until(&mut stream, deadline, None)?;
        if response.version != PROTOCOL_VERSION {
            bail!(
                "zmux notification listener replied with unsupported protocol version {}",
                response.version
            );
        }
        match response.status {
            ResponseStatus::Recorded => Ok(SendAttempt::Recorded),
            ResponseStatus::Unauthorized => {
                bail!("zmux notification route is unknown, stale, or unauthorized")
            }
            ResponseStatus::Pending => Ok(SendAttempt::Pending),
            ResponseStatus::Busy => bail!(
                "zmux notification route is not ready or the listener is busy{}",
                response_suffix(response.message.as_deref())
            ),
            ResponseStatus::Invalid => bail!(
                "zmux notification listener rejected the message{}",
                response_suffix(response.message.as_deref())
            ),
            ResponseStatus::Rejected => bail!(
                "zmux notification was not recorded{}",
                response_suffix(response.message.as_deref())
            ),
            ResponseStatus::TimedOut => bail!(
                "zmux notification timed out before it was recorded{}",
                response_suffix(response.message.as_deref())
            ),
        }
    }
}

enum SendAttempt {
    Recorded,
    Pending,
}

impl Drop for CliServer {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        self.routes.close();
        self.connections.close();

        if let Some(thread) = self.accept_thread.take() {
            let _ = thread.join();
        }
        for thread in self.worker_threads.drain(..) {
            let _ = thread.join();
        }
    }
}

fn accept_loop(
    listener: TcpListener,
    connections: Sender<AcceptedConnection>,
    running: Arc<AtomicBool>,
) {
    while running.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, peer_addr)) => {
                if !peer_addr.ip().is_loopback() {
                    continue;
                }
                if configure_server_stream(&stream).is_err() {
                    continue;
                }
                let connection = AcceptedConnection {
                    stream,
                    read_deadline: Instant::now() + SERVER_IO_TIMEOUT,
                };
                match connections.try_send(connection) {
                    Ok(()) => {}
                    // Do not emit an unauthenticated response before the v3
                    // server-proof handshake. Closing is a safe busy signal.
                    Err(async_channel::TrySendError::Full(_)) => {}
                    Err(async_channel::TrySendError::Closed(_)) => break,
                }
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                thread::sleep(ACCEPT_POLL_INTERVAL);
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
}

fn worker_loop(
    connections: Receiver<AcceptedConnection>,
    notifications: Sender<ReceivedCliNotification>,
    routes: Arc<RouteRegistry>,
    running: Arc<AtomicBool>,
    address: SocketAddrV4,
) {
    while running.load(Ordering::Acquire) {
        let Ok(mut connection) = connections.recv_blocking() else {
            break;
        };
        if !running.load(Ordering::Acquire) {
            break;
        }
        handle_connection(
            &mut connection.stream,
            connection.read_deadline,
            &notifications,
            &routes,
            &running,
            address,
        );
    }
}

fn handle_connection(
    stream: &mut TcpStream,
    read_deadline: Instant,
    notifications: &Sender<ReceivedCliNotification>,
    routes: &RouteRegistry,
    running: &AtomicBool,
    address: SocketAddrV4,
) {
    let hello = match read_frame_until::<ClientHello>(stream, read_deadline, Some(running)) {
        Ok(hello) => hello,
        Err(_) => return,
    };
    if hello.version != PROTOCOL_VERSION
        || hello.route_selector.len() != ROUTE_SELECTOR_BYTES
        || hello.nonce.len() != CLIENT_NONCE_BYTES
    {
        return;
    }
    let Some(proof_key) = routes.proof_key_for_selector(&hello.route_selector) else {
        return;
    };
    let proof = server_proof(&proof_key, address, &hello.route_selector, &hello.nonce);
    if write_frame(
        stream,
        &ServerHello {
            version: PROTOCOL_VERSION,
            proof: proof.to_vec(),
        },
    )
    .is_err()
    {
        return;
    }

    let request = match read_frame_until::<RequestEnvelope>(stream, read_deadline, Some(running)) {
        Ok(request) => request,
        Err(error) => {
            let _ = write_response(stream, ResponseStatus::Invalid, Some(error.to_string()));
            return;
        }
    };

    if request.version != PROTOCOL_VERSION {
        let _ = write_response(
            stream,
            ResponseStatus::Invalid,
            Some(format!("unsupported protocol version {}", request.version)),
        );
        return;
    }
    let route_id = match routes.lookup(&hello.route_selector, &request.token) {
        RouteLookup::Active(route_id) => route_id,
        RouteLookup::Pending => {
            let _ = write_response(
                stream,
                ResponseStatus::Pending,
                Some("terminal route has not been activated yet".to_owned()),
            );
            return;
        }
        RouteLookup::Unknown => {
            let _ = write_response(stream, ResponseStatus::Unauthorized, None);
            return;
        }
    };
    if let Err(error) = request.notification.validate() {
        let _ = write_response(stream, ResponseStatus::Invalid, Some(error.to_string()));
        return;
    }

    let peer_addr = match stream.peer_addr() {
        Ok(peer_addr) => peer_addr,
        Err(error) => {
            let _ = write_response(stream, ResponseStatus::Invalid, Some(error.to_string()));
            return;
        }
    };
    let (completion_sender, completion_receiver) = sync_channel(1);
    let lifecycle = Arc::new(RequestLifecycle::default());
    let received = ReceivedCliNotification {
        route_id,
        notification: request.notification,
        peer_addr,
        completion: CliRequestCompletion {
            sender: Some(completion_sender),
            lifecycle: Arc::clone(&lifecycle),
        },
    };

    match notifications.try_send(received) {
        Ok(()) => match wait_for_completion(completion_receiver, &lifecycle, running) {
            ProcessingResult::Recorded => {
                let _ = write_response(stream, ResponseStatus::Recorded, None);
            }
            ProcessingResult::Rejected(reason) => {
                let _ = write_response(stream, ResponseStatus::Rejected, Some(reason));
            }
            ProcessingResult::TimedOut => {
                let _ = write_response(
                    stream,
                    ResponseStatus::TimedOut,
                    Some("GPUI did not complete the request before the deadline".to_owned()),
                );
            }
            ProcessingResult::ShuttingDown => {
                let _ = write_response(
                    stream,
                    ResponseStatus::Rejected,
                    Some("notification server is shutting down".to_owned()),
                );
            }
        },
        Err(async_channel::TrySendError::Full(_)) => {
            let _ = write_response(stream, ResponseStatus::Busy, None);
        }
        Err(async_channel::TrySendError::Closed(_)) => {
            let _ = write_response(
                stream,
                ResponseStatus::Rejected,
                Some("notification consumer is unavailable".to_owned()),
            );
        }
    }
}

fn wait_for_completion(
    receiver: CompletionReceiver<ProcessingResult>,
    lifecycle: &RequestLifecycle,
    running: &AtomicBool,
) -> ProcessingResult {
    let deadline = Instant::now() + PROCESSING_TIMEOUT;
    loop {
        if !running.load(Ordering::Acquire) {
            return ProcessingResult::ShuttingDown;
        }
        let now = Instant::now();
        // Once GPUI has atomically claimed the request, publishing and
        // completion happen synchronously on that thread. After the deadline,
        // cancel only Pending; Recording must finish so the server cannot race
        // a timeout response against a canonical row insertion.
        if now >= deadline
            && (lifecycle.cancel_if_pending() || lifecycle.state() == REQUEST_CANCELED)
        {
            return ProcessingResult::TimedOut;
        }
        let wait = if now < deadline {
            COMPLETION_POLL_INTERVAL.min(deadline.saturating_duration_since(now))
        } else {
            COMPLETION_POLL_INTERVAL
        };
        match receiver.recv_timeout(wait) {
            Ok(result) => return result,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                if lifecycle.state() == REQUEST_CANCELED {
                    return ProcessingResult::TimedOut;
                }
                return ProcessingResult::Rejected(
                    "notification completion channel disconnected".to_owned(),
                );
            }
        }
    }
}

fn configure_server_stream(stream: &TcpStream) -> anyhow::Result<()> {
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(SERVER_IO_TIMEOUT))?;
    stream.set_write_timeout(Some(SERVER_IO_TIMEOUT))?;
    Ok(())
}

fn configure_client_stream(stream: &TcpStream) -> anyhow::Result<()> {
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(CLIENT_RESPONSE_TIMEOUT))?;
    stream.set_write_timeout(Some(SERVER_IO_TIMEOUT))?;
    Ok(())
}

fn write_response(
    stream: &mut TcpStream,
    status: ResponseStatus,
    message: Option<String>,
) -> anyhow::Result<()> {
    write_frame(
        stream,
        &ResponseEnvelope {
            version: PROTOCOL_VERSION,
            status,
            message: message.map(bounded_rejection),
        },
    )
}

fn write_frame<W: Write, T: Serialize>(writer: &mut W, value: &T) -> anyhow::Result<()> {
    let payload = serde_json::to_vec(value)?;
    if payload.len() > MAX_FRAME_BYTES {
        bail!("notification frame exceeds {MAX_FRAME_BYTES} bytes");
    }
    let length = u32::try_from(payload.len()).expect("64 KiB always fits in u32");
    writer.write_all(&length.to_be_bytes())?;
    writer.write_all(&payload)?;
    writer.flush()?;
    Ok(())
}

#[cfg(test)]
fn read_frame<R: Read, T: for<'de> Deserialize<'de>>(reader: &mut R) -> anyhow::Result<T> {
    let mut length = [0_u8; 4];
    reader.read_exact(&mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    if length > MAX_FRAME_BYTES {
        bail!("notification frame exceeds {MAX_FRAME_BYTES} bytes");
    }
    let mut payload = vec![0_u8; length];
    reader.read_exact(&mut payload)?;
    serde_json::from_slice(&payload).context("invalid notification JSON")
}

/// Read one socket frame against a single absolute deadline. The short poll
/// timeout makes server shutdown observable even when a peer is idle, while
/// the unchanged absolute deadline prevents slowloris clients from extending
/// their budget by dribbling partial bytes.
fn read_frame_until<T: for<'de> Deserialize<'de>>(
    stream: &mut TcpStream,
    deadline: Instant,
    running: Option<&AtomicBool>,
) -> anyhow::Result<T> {
    let mut length = [0_u8; 4];
    read_exact_until(stream, &mut length, deadline, running)?;
    let length = u32::from_be_bytes(length) as usize;
    if length > MAX_FRAME_BYTES {
        bail!("notification frame exceeds {MAX_FRAME_BYTES} bytes");
    }
    let mut payload = vec![0_u8; length];
    read_exact_until(stream, &mut payload, deadline, running)?;
    serde_json::from_slice(&payload).context("invalid notification JSON")
}

fn read_exact_until(
    stream: &mut TcpStream,
    mut buffer: &mut [u8],
    deadline: Instant,
    running: Option<&AtomicBool>,
) -> anyhow::Result<()> {
    while !buffer.is_empty() {
        if running.is_some_and(|running| !running.load(Ordering::Acquire)) {
            bail!("notification server is shutting down");
        }
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            bail!("notification frame read timed out");
        };
        if remaining.is_zero() {
            bail!("notification frame read timed out");
        }
        stream.set_read_timeout(Some(remaining.min(IO_POLL_INTERVAL)))?;
        match stream.read(buffer) {
            Ok(0) => bail!("notification peer closed before completing its frame"),
            Ok(count) => buffer = &mut buffer[count..],
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                continue;
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

type HmacSha256 = Hmac<Sha256>;

fn server_proof(
    proof_key: &[u8; ROUTE_PROOF_KEY_BYTES],
    address: SocketAddrV4,
    route_selector: &[u8],
    nonce: &[u8],
) -> [u8; AUTH_PROOF_BYTES] {
    let mut mac = HmacSha256::new_from_slice(proof_key).expect("HMAC accepts a 32-byte key");
    mac.update(AUTH_TRANSCRIPT_DOMAIN);
    mac.update(&PROTOCOL_VERSION.to_be_bytes());
    mac.update(&address.ip().octets());
    mac.update(&address.port().to_be_bytes());
    mac.update(&(route_selector.len() as u16).to_be_bytes());
    mac.update(route_selector);
    mac.update(&(nonce.len() as u16).to_be_bytes());
    mac.update(nonce);
    mac.finalize().into_bytes().into()
}

fn verify_server_proof(
    proof_key: &[u8; ROUTE_PROOF_KEY_BYTES],
    address: SocketAddrV4,
    route_selector: &[u8],
    nonce: &[u8],
    proof: &[u8],
) -> bool {
    let mut mac = HmacSha256::new_from_slice(proof_key).expect("HMAC accepts a 32-byte key");
    mac.update(AUTH_TRANSCRIPT_DOMAIN);
    mac.update(&PROTOCOL_VERSION.to_be_bytes());
    mac.update(&address.ip().octets());
    mac.update(&address.port().to_be_bytes());
    mac.update(&(route_selector.len() as u16).to_be_bytes());
    mac.update(route_selector);
    mac.update(&(nonce.len() as u16).to_be_bytes());
    mac.update(nonce);
    mac.verify_slice(proof).is_ok()
}

fn random_bytes<const N: usize>() -> anyhow::Result<[u8; N]> {
    let mut bytes = [0_u8; N];
    getrandom::fill(&mut bytes).context("failed to obtain operating system randomness")?;
    Ok(bytes)
}

fn random_client_nonce() -> anyhow::Result<[u8; CLIENT_NONCE_BYTES]> {
    random_bytes()
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[usize::from(byte >> 4)] as char);
        encoded.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    encoded
}

fn decode_fixed_hex<const N: usize>(encoded: &str) -> anyhow::Result<[u8; N]> {
    if encoded.len() != N * 2 {
        bail!("expected {} hexadecimal characters", N * 2);
    }
    let mut decoded = [0_u8; N];
    for (index, pair) in encoded.as_bytes().chunks_exact(2).enumerate() {
        decoded[index] = (decode_hex_nibble(pair[0])? << 4) | decode_hex_nibble(pair[1])?;
    }
    Ok(decoded)
}

fn decode_hex_nibble(byte: u8) -> anyhow::Result<u8> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => bail!("invalid hexadecimal digit"),
    }
}

/// Compare bearer capabilities in a fixed number of iterations.
fn constant_time_bytes_eq(expected: &[u8], candidate: &[u8]) -> bool {
    let mut difference = expected.len() ^ candidate.len();
    for index in 0..ROUTE_SELECTOR_BYTES {
        let left = expected.get(index).copied().unwrap_or(0);
        let right = candidate.get(index).copied().unwrap_or(0);
        difference |= usize::from(left ^ right);
    }
    difference == 0
}

fn constant_time_token_eq(expected: &str, candidate: &str) -> bool {
    let expected = expected.as_bytes();
    let candidate = candidate.as_bytes();
    let mut difference = expected.len() ^ candidate.len();
    for index in 0..MAX_TOKEN_BYTES {
        let left = expected.get(index).copied().unwrap_or(0);
        let right = candidate.get(index).copied().unwrap_or(0);
        difference |= usize::from(left ^ right);
    }
    difference == 0
}

fn bounded_rejection(message: String) -> String {
    let message: String = message
        .chars()
        .map(|character| {
            if character.is_control() {
                '\u{fffd}'
            } else {
                character
            }
        })
        .collect();
    if message.len() <= MAX_REJECTION_BYTES {
        return message;
    }
    let mut boundary = MAX_REJECTION_BYTES - '…'.len_utf8();
    while !message.is_char_boundary(boundary) {
        boundary -= 1;
    }
    format!("{}…", &message[..boundary])
}

fn response_suffix(message: Option<&str>) -> String {
    message
        .map(|message| format!(": {message}"))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::sync::mpsc;

    use super::*;

    fn test_notification() -> CliNotification {
        CliNotification::new(
            "Build finished",
            Some("agent-2".to_owned()),
            "All checks passed",
        )
    }

    fn spawn_client(endpoint: CliEndpoint) -> mpsc::Receiver<Result<(), String>> {
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let result = CliServer::send_to(&endpoint, test_notification())
                .map_err(|error| error.to_string());
            let _ = sender.send(result);
        });
        receiver
    }

    fn active_route(server: &CliServer) -> CliRouteRegistration {
        let registration = server.register_route().unwrap();
        assert!(registration.activate());
        registration
    }

    fn authenticated_stream(endpoint: &CliEndpoint) -> TcpStream {
        let address = SocketAddr::V4(endpoint.address());
        let mut stream = TcpStream::connect_timeout(&address, SERVER_IO_TIMEOUT).unwrap();
        configure_client_stream(&stream).unwrap();
        let nonce = random_client_nonce().unwrap();
        write_frame(
            &mut stream,
            &ClientHello {
                version: PROTOCOL_VERSION,
                route_selector: endpoint.route_selector.to_vec(),
                nonce: nonce.to_vec(),
            },
        )
        .unwrap();
        let hello: ServerHello =
            read_frame_until(&mut stream, Instant::now() + CLIENT_RESPONSE_TIMEOUT, None).unwrap();
        assert_eq!(hello.version, PROTOCOL_VERSION);
        assert!(verify_server_proof(
            &endpoint.proof_key,
            endpoint.address,
            &endpoint.route_selector,
            &nonce,
            &hello.proof,
        ));
        stream
    }

    #[test]
    fn route_endpoint_round_trips_and_redacts_capability() {
        let server = CliServer::start().unwrap();
        let registration = server.register_route().unwrap();
        let serialized = registration.endpoint_env();
        let parsed: CliEndpoint = serialized.parse().unwrap();

        assert_eq!(parsed, *registration.endpoint());
        assert!(serialized.contains("127.0.0.1:"));
        assert!(!format!("{parsed:?}").contains(&encode_hex(&parsed.proof_key)));
        assert!(!format!("{parsed:?}").contains(&parsed.token));
        assert!(!format!("{registration:?}").contains(&encode_hex(&parsed.proof_key)));
        assert!(!format!("{registration:?}").contains(&parsed.token));
    }

    #[test]
    fn client_success_waits_until_gpui_confirms_the_record() {
        let server = CliServer::start().unwrap();
        let registration = active_route(&server);
        let results = spawn_client(registration.endpoint().clone());
        let received = server.receiver().recv_blocking().unwrap();

        assert_eq!(received.route_id, registration.route_id());
        assert!(results.recv_timeout(Duration::from_millis(100)).is_err());

        assert!(received.completion.begin_recording());
        received.completion.recorded();
        assert_eq!(
            results.recv_timeout(Duration::from_secs(1)).unwrap(),
            Ok(())
        );
    }

    #[test]
    fn consumer_rejection_is_reported_as_client_failure() {
        let server = CliServer::start().unwrap();
        let registration = active_route(&server);
        let results = spawn_client(registration.endpoint().clone());
        let received = server.receiver().recv_blocking().unwrap();

        received.completion.reject("exact terminal route is stale");
        let error = results
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap_err();
        assert!(error.contains("not recorded"));
        assert!(error.contains("stale"));
    }

    #[test]
    fn dropped_completion_never_reports_success() {
        let server = CliServer::start().unwrap();
        let registration = active_route(&server);
        let results = spawn_client(registration.endpoint().clone());
        let received = server.receiver().recv_blocking().unwrap();

        drop(received);

        let error = results
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap_err();
        assert!(error.contains("not recorded"));
        assert!(error.contains("dropped"));
    }

    #[test]
    fn timed_out_request_cannot_be_recorded_later() {
        let server = CliServer::start().unwrap();
        let registration = active_route(&server);
        let results = spawn_client(registration.endpoint().clone());
        let received = server.receiver().recv_blocking().unwrap();

        let error = results
            .recv_timeout(PROCESSING_TIMEOUT + Duration::from_secs(2))
            .unwrap()
            .unwrap_err();
        assert!(error.contains("timed out"));
        assert!(
            !received.completion.begin_recording(),
            "GPUI must not claim a request after the timeout response"
        );
        received.completion.recorded();
    }

    #[test]
    fn client_retries_pending_route_until_activation_then_records_once() {
        let server = CliServer::start().unwrap();
        let registration = server.register_route().unwrap();
        let results = spawn_client(registration.endpoint().clone());

        assert!(results.recv_timeout(Duration::from_millis(100)).is_err());
        assert!(server.receiver().try_recv().is_err());
        assert!(registration.activate());
        let received = server
            .receiver()
            .recv_blocking()
            .expect("retry should deliver after activation");
        assert_eq!(received.route_id, registration.route_id());
        assert!(received.completion.begin_recording());
        received.completion.recorded();

        assert_eq!(
            results.recv_timeout(Duration::from_secs(1)).unwrap(),
            Ok(())
        );
        assert!(server.receiver().try_recv().is_err());
    }

    #[test]
    fn pending_route_retry_is_bounded_by_one_absolute_deadline() {
        let server = CliServer::start().unwrap();
        let registration = server.register_route().unwrap();
        let start = Instant::now();
        let error = CliServer::send_to_until(
            registration.endpoint(),
            test_notification(),
            start + Duration::from_millis(250),
        )
        .unwrap_err();

        assert!(error.to_string().contains("deadline"));
        assert!(start.elapsed() < Duration::from_secs(1));
        assert!(server.receiver().try_recv().is_err());
    }

    #[test]
    fn dropping_registration_revokes_stale_descendants() {
        let server = CliServer::start().unwrap();
        let registration = active_route(&server);
        let endpoint = registration.endpoint().clone();
        drop(registration);

        let error = CliServer::send_to(&endpoint, test_notification()).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("failed to authenticate zmux notification listener")
        );
        assert!(server.receiver().try_recv().is_err());
    }

    #[test]
    fn capabilities_select_server_routes_not_client_claims() {
        let server = CliServer::start().unwrap();
        let first = active_route(&server);
        let second = active_route(&server);
        let results = spawn_client(first.endpoint().clone());
        let received = server.receiver().recv_blocking().unwrap();

        assert_eq!(received.route_id, first.route_id());
        assert_ne!(received.route_id, second.route_id());
        assert!(received.completion.begin_recording());
        received.completion.recorded();
        assert_eq!(
            results.recv_timeout(Duration::from_secs(1)).unwrap(),
            Ok(())
        );
    }

    #[test]
    fn route_token_is_bound_to_the_selector_that_authenticated_the_server() {
        let server = CliServer::start().unwrap();
        let first = active_route(&server);
        let second = active_route(&server);
        let mut stream = authenticated_stream(first.endpoint());

        write_frame(
            &mut stream,
            &RequestEnvelope {
                version: PROTOCOL_VERSION,
                token: second.endpoint().token.clone(),
                notification: test_notification(),
            },
        )
        .unwrap();
        let response: ResponseEnvelope =
            read_frame_until(&mut stream, Instant::now() + CLIENT_RESPONSE_TIMEOUT, None).unwrap();

        assert!(matches!(response.status, ResponseStatus::Unauthorized));
        assert!(server.receiver().try_recv().is_err());
    }

    #[test]
    fn wire_payload_cannot_smuggle_a_client_claimed_origin() {
        let server = CliServer::start().unwrap();
        let registration = active_route(&server);
        let mut stream = authenticated_stream(registration.endpoint());

        write_frame(
            &mut stream,
            &serde_json::json!({
                "version": PROTOCOL_VERSION,
                "token": registration.endpoint().token.clone(),
                "notification": {
                    "title": "forged",
                    "body": "must not route",
                    "process_ancestry": [999_999_u32],
                    "target": "another-pane"
                }
            }),
        )
        .unwrap();
        let response: ResponseEnvelope = read_frame(&mut stream).unwrap();

        assert!(matches!(response.status, ResponseStatus::Invalid));
        assert!(server.receiver().try_recv().is_err());
    }

    #[test]
    fn rejects_an_invalid_capability_without_delivery() {
        let server = CliServer::start().unwrap();
        let registration = active_route(&server);
        let mut endpoint = registration.endpoint().clone();
        endpoint.token = "00000000000000000000000000000000".to_owned();

        let error = CliServer::send_to(&endpoint, test_notification()).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("unknown, stale, or unauthorized")
        );
        assert!(server.receiver().try_recv().is_err());
    }

    #[test]
    fn token_comparison_checks_value_and_length() {
        assert!(constant_time_token_eq("abcd", "abcd"));
        assert!(!constant_time_token_eq("abcd", "abce"));
        assert!(!constant_time_token_eq("abcd", "abcd\0"));
        assert!(!constant_time_token_eq("abcd", "abc"));
    }

    #[test]
    fn another_route_cannot_impersonate_a_stale_listener_or_read_payload() {
        let (attacker_endpoint, endpoint) = {
            let server = CliServer::start().unwrap();
            let attacker = active_route(&server);
            let victim = active_route(&server);
            (attacker.endpoint().clone(), victim.endpoint().clone())
        };
        let listener = TcpListener::bind(endpoint.address()).unwrap();
        let (captured_tx, captured_rx) = mpsc::channel();
        let forged_proof_key = attacker_endpoint.proof_key;
        let proof_address = endpoint.address();
        let impersonator = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            configure_server_stream(&stream).unwrap();
            let hello: ClientHello = read_frame(&mut stream).unwrap();
            let forged_proof = server_proof(
                &forged_proof_key,
                proof_address,
                &hello.route_selector,
                &hello.nonce,
            );
            write_frame(
                &mut stream,
                &ServerHello {
                    version: PROTOCOL_VERSION,
                    proof: forged_proof.to_vec(),
                },
            )
            .unwrap();
            let mut trailing = [0_u8; 1_024];
            let trailing = match stream.read(&mut trailing) {
                Ok(count) => trailing[..count].to_vec(),
                Err(_) => Vec::new(),
            };
            captured_tx.send((hello, trailing)).unwrap();
        });

        let error = CliServer::send_to(&endpoint, test_notification()).unwrap_err();
        assert!(error.to_string().contains("failed to authenticate"));

        let (hello, trailing) = captured_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        impersonator.join().unwrap();
        assert_eq!(hello.version, PROTOCOL_VERSION);
        assert_eq!(hello.route_selector, endpoint.route_selector);
        assert_eq!(hello.nonce.len(), CLIENT_NONCE_BYTES);
        assert!(
            trailing.is_empty(),
            "client disclosed bytes after forged proof"
        );
        let captured_hello = serde_json::to_string(&hello).unwrap();
        assert!(!captured_hello.contains(&endpoint.token));
        assert!(!captured_hello.contains(&encode_hex(&endpoint.proof_key)));
        assert!(!captured_hello.contains("Build finished"));
        assert!(!captured_hello.contains("All checks passed"));
    }

    #[test]
    fn partial_frame_progress_cannot_extend_the_absolute_deadline() {
        let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let dripper = thread::spawn(move || {
            let mut stream = TcpStream::connect(address).unwrap();
            // Announce a 1 KiB frame, then make continuous partial progress
            // that would defeat a per-read inactivity timeout.
            stream.write_all(&1_024_u32.to_be_bytes()).unwrap();
            for _ in 0..30 {
                if stream.write_all(b"x").is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
        });
        let (mut stream, _) = listener.accept().unwrap();
        configure_server_stream(&stream).unwrap();
        let started = Instant::now();
        let error = read_frame_until::<serde_json::Value>(
            &mut stream,
            started + Duration::from_millis(120),
            None,
        )
        .unwrap_err();
        assert!(error.to_string().contains("timed out"));
        assert!(started.elapsed() < Duration::from_millis(400));
        drop(stream);
        dripper.join().unwrap();
    }

    #[test]
    fn shutdown_interrupts_workers_blocked_on_partial_frames() {
        let server = CliServer::start().unwrap();
        let address = SocketAddr::V4(server.address());
        let mut clients = Vec::new();
        for _ in 0..WORKER_COUNT {
            let mut stream = TcpStream::connect_timeout(&address, SERVER_IO_TIMEOUT).unwrap();
            stream.write_all(&[0]).unwrap();
            clients.push(stream);
        }
        thread::sleep(Duration::from_millis(100));

        let started = Instant::now();
        drop(server);
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "server shutdown waited for socket inactivity timeouts"
        );
        drop(clients);
    }

    #[test]
    fn recording_and_timeout_claims_are_linearizable() {
        for _ in 0..64 {
            let lifecycle = Arc::new(RequestLifecycle::default());
            let barrier = Arc::new(std::sync::Barrier::new(3));
            let recording = {
                let lifecycle = Arc::clone(&lifecycle);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    lifecycle.begin_recording()
                })
            };
            let canceling = {
                let lifecycle = Arc::clone(&lifecycle);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    lifecycle.cancel_if_pending()
                })
            };
            barrier.wait();
            let recording_won = recording.join().unwrap();
            let canceling_won = canceling.join().unwrap();
            assert_ne!(recording_won, canceling_won, "exactly one CAS must win");
            assert_eq!(
                lifecycle.state(),
                if recording_won {
                    REQUEST_RECORDING
                } else {
                    REQUEST_CANCELED
                }
            );
        }
    }

    #[test]
    fn rejects_oversized_frames_before_allocating_payload() {
        let mut bytes = Cursor::new(((MAX_FRAME_BYTES as u32) + 1).to_be_bytes());
        let error = read_frame::<_, serde_json::Value>(&mut bytes).unwrap_err();
        assert!(error.to_string().contains("exceeds"));
    }

    #[test]
    fn rejection_messages_are_bounded_and_strip_terminal_controls() {
        let rejection = bounded_rejection(format!("bad\x1b[31m{}", "x".repeat(2_000)));

        assert!(rejection.len() <= MAX_REJECTION_BYTES);
        assert!(!rejection.contains('\x1b'));
    }

    #[test]
    fn registrations_use_unique_ids_capabilities_and_shared_listener() {
        let server = CliServer::start().unwrap();
        let first = server.register_route().unwrap();
        let second = server.register_route().unwrap();

        assert_ne!(first.route_id(), second.route_id());
        assert_ne!(
            first.endpoint().route_selector,
            second.endpoint().route_selector
        );
        assert_ne!(first.endpoint().proof_key, second.endpoint().proof_key);
        assert_ne!(first.endpoint().token, second.endpoint().token);
        assert_eq!(first.endpoint().address(), second.endpoint().address());
    }

    #[test]
    fn dropping_the_server_closes_its_listener() {
        let server = CliServer::start().unwrap();
        let address = SocketAddr::V4(server.address());

        drop(server);

        assert!(TcpStream::connect_timeout(&address, Duration::from_millis(100)).is_err());
    }
}
