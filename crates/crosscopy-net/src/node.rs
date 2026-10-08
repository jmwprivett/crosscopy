//! The QUIC node: listens for paired peers, keeps dialing the ones that are
//! not connected, and moves [`Message`]s in both directions.
//!
//! Peer addresses come from mDNS discovery, falling back to a configured
//! address. The peer set can change at runtime via [`Node::set_peers`].
//!
//! Both sides dial each other, so two connections to the same peer can
//! briefly coexist. Both ends deterministically keep the one dialed by the
//! lower [`DeviceId`] and retire the other after a grace period, during which
//! it is still read so nothing already in flight is lost.

use crate::discovery::{Discovery, DiscoveryEvent, SYNC_SERVICE};
use crate::identity::Identity;
use crate::tls::{self, PinSet, Trust};
use anyhow::{Context, Result};
use crosscopy_core::{ALPN, DeviceId, MAX_MESSAGE_BYTES, Message};
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::{Connection, Endpoint, IdleTimeout, TransportConfig};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Notify, mpsc};
use tokio::task::AbortHandle;
use tracing::{debug, info, warn};

const KEEP_ALIVE: Duration = Duration::from_secs(5);
const IDLE_TIMEOUT: Duration = Duration::from_secs(15);
const MIN_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
const CONNECTED_RECHECK: Duration = Duration::from_secs(2);
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
/// Messages queued per connection before new ones are dropped.
const SEND_QUEUE: usize = 16;
/// How long a duplicate connection stays readable before it is closed.
const DUPLICATE_GRACE: Duration = Duration::from_secs(2);

#[derive(Debug, Clone)]
pub struct Peer {
    pub id: DeviceId,
    pub name: String,
    /// Fallback `host:port` used when discovery hasn't found the peer.
    /// Re-resolved on every dial so DHCP and `.local` names work.
    pub address: Option<String>,
}

pub struct NodeOptions {
    pub listen: SocketAddr,
    /// Advertised to other devices over mDNS.
    pub device_name: String,
    pub discovery: bool,
}

#[derive(Debug)]
pub struct Incoming {
    pub from: DeviceId,
    pub from_name: String,
    pub message: Message,
}

#[derive(Clone)]
pub struct Node {
    inner: Arc<Inner>,
}

struct Inner {
    identity: Identity,
    endpoint: Endpoint,
    pins: PinSet,
    peers: Mutex<HashMap<DeviceId, PeerEntry>>,
    /// Best-first addresses from mDNS, for paired and unpaired devices alike.
    discovered: Mutex<HashMap<DeviceId, Vec<SocketAddr>>>,
    /// The active (sending) connection for each connected peer.
    connections: Mutex<HashMap<DeviceId, PeerConnection>>,
    discovery: Mutex<Option<Discovery>>,
    inbox: mpsc::Sender<Incoming>,
}

struct PeerEntry {
    peer: Peer,
    /// Cuts the dial loop's backoff short, e.g. when discovery sees the peer.
    wake: Arc<Notify>,
    dialer: AbortHandle,
}

struct PeerConnection {
    conn: Connection,
    outbox: mpsc::Sender<Arc<[u8]>>,
    /// Which side opened this connection.
    dialer: DeviceId,
}

enum Registration {
    /// First connection to this peer.
    New,
    /// Became the active connection, displacing this one.
    Replaced(Connection),
    /// Another connection is preferred; this one should be retired.
    Duplicate,
}

impl Node {
    /// Binds the endpoint and starts accepting, dialing and discovery in the
    /// background. Must be called from within a Tokio runtime.
    pub fn start(
        identity: &Identity,
        options: NodeOptions,
        peers: Vec<Peer>,
    ) -> Result<(Self, mpsc::Receiver<Incoming>)> {
        let pins = PinSet::default();
        let server_tls = tls::server_config(identity, Trust::Pinned(pins.clone()), ALPN)?;
        let mut server = quinn::ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(server_tls)?));
        server.transport_config(transport());
        let endpoint = Endpoint::server(server, options.listen)
            .with_context(|| format!("binding UDP {}", options.listen))?;
        let port = endpoint.local_addr()?.port();

        let (inbox, inbox_rx) = mpsc::channel(64);
        let inner = Arc::new(Inner {
            identity: identity.clone(),
            endpoint,
            pins,
            peers: Mutex::default(),
            discovered: Mutex::default(),
            connections: Mutex::default(),
            discovery: Mutex::default(),
            inbox,
        });
        tokio::spawn(accept_loop(inner.clone()));

        let node = Self { inner: inner.clone() };
        node.set_peers(peers);

        if options.discovery {
            match Discovery::start(SYNC_SERVICE, identity.id, &options.device_name, port) {
                Ok((discovery, events)) => {
                    *inner.discovery.lock().unwrap() = Some(discovery);
                    tokio::spawn(discovery_loop(inner, events));
                }
                Err(e) => warn!("mDNS discovery unavailable, using configured addresses only: {e:#}"),
            }
        }
        Ok((node, inbox_rx))
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.inner.endpoint.local_addr()
    }

    /// Replaces the set of paired peers. New peers are dialed, removed peers
    /// are disconnected and can no longer connect.
    pub fn set_peers(&self, peers: Vec<Peer>) {
        let new: HashMap<DeviceId, Peer> = peers.into_iter().map(|p| (p.id, p)).collect();
        *self.inner.pins.write().unwrap() = new.keys().copied().collect();

        let mut removed = Vec::new();
        {
            let mut entries = self.inner.peers.lock().unwrap();
            entries.retain(|id, entry| {
                let keep = new.contains_key(id);
                if !keep {
                    entry.dialer.abort();
                    removed.push((*id, entry.peer.name.clone()));
                }
                keep
            });
            for (id, peer) in new {
                match entries.get_mut(&id) {
                    Some(entry) => {
                        entry.peer = peer;
                        entry.wake.notify_one();
                    }
                    None => {
                        let wake = Arc::new(Notify::new());
                        let dialer = tokio::spawn(dial_loop(self.inner.clone(), id, wake.clone()));
                        entries.insert(id, PeerEntry { peer, wake, dialer: dialer.abort_handle() });
                    }
                }
            }
        }

        for (id, name) in removed {
            if let Some(pc) = self.inner.connections.lock().unwrap().remove(&id) {
                pc.conn.close(0u32.into(), b"unpaired");
            }
            info!(peer = %name, "unpaired");
        }
    }

    /// Queues `message` for every connected peer; returns how many it reached.
    pub fn broadcast(&self, message: &Message) -> usize {
        let bytes: Arc<[u8]> = message.encode().into();
        let mut queued = 0;
        let mut full = Vec::new();
        for (id, pc) in self.inner.connections.lock().unwrap().iter() {
            match pc.outbox.try_send(bytes.clone()) {
                Ok(()) => queued += 1,
                Err(_) => full.push(*id),
            }
        }
        for id in full {
            warn!(peer = %self.inner.name(id), "send queue full; dropping message");
        }
        queued
    }

    /// Names of currently connected peers.
    pub fn connected_peers(&self) -> Vec<String> {
        let ids: Vec<DeviceId> = self.inner.connections.lock().unwrap().keys().copied().collect();
        ids.into_iter().map(|id| self.inner.name(id)).collect()
    }

    /// Withdraws the mDNS advertisement, closes all connections and waits
    /// briefly for peers to be notified.
    pub async fn shutdown(&self) {
        self.inner.discovery.lock().unwrap().take();
        self.inner.endpoint.close(0u32.into(), b"shutdown");
        let _ = tokio::time::timeout(Duration::from_secs(1), self.inner.endpoint.wait_idle()).await;
    }
}

impl Inner {
    fn name(&self, id: DeviceId) -> String {
        self.peers
            .lock()
            .unwrap()
            .get(&id)
            .map_or_else(|| id.to_string(), |e| e.peer.name.clone())
    }

    fn is_connected(&self, id: DeviceId) -> bool {
        self.connections.lock().unwrap().contains_key(&id)
    }

    /// Discovered addresses first (best-first), then the configured fallback.
    async fn candidates(&self, id: DeviceId) -> Vec<SocketAddr> {
        let mut out = self.discovered.lock().unwrap().get(&id).cloned().unwrap_or_default();
        let fallback = self.peers.lock().unwrap().get(&id).and_then(|e| e.peer.address.clone());
        if let Some(address) = fallback {
            match resolve(&address).await {
                Ok(addr) if !out.contains(&addr) => out.push(addr),
                Ok(_) => {}
                Err(e) => debug!("{e:#}"),
            }
        }
        out
    }

    fn register(&self, peer: DeviceId, new: PeerConnection) -> Registration {
        let preferred_dialer = self.identity.id.min(peer);
        let mut connections = self.connections.lock().unwrap();
        let Some(existing) = connections.get(&peer) else {
            connections.insert(peer, new);
            return Registration::New;
        };
        // Ties (same dialer) go to the newer connection: the older one is
        // most likely a dead link that has not timed out yet.
        if new.dialer == preferred_dialer || existing.dialer != preferred_dialer {
            let old = connections.insert(peer, new).expect("checked above");
            Registration::Replaced(old.conn)
        } else {
            Registration::Duplicate
        }
    }

    /// Removes `conn` if it is the active connection; returns whether it was.
    fn unregister(&self, peer: DeviceId, conn: &Connection) -> bool {
        let mut connections = self.connections.lock().unwrap();
        let active = connections
            .get(&peer)
            .is_some_and(|pc| pc.conn.stable_id() == conn.stable_id());
        if active {
            connections.remove(&peer);
        }
        active
    }
}

fn retire_later(conn: Connection) {
    tokio::spawn(async move {
        tokio::time::sleep(DUPLICATE_GRACE).await;
        conn.close(0u32.into(), b"duplicate");
    });
}

pub(crate) fn transport() -> Arc<TransportConfig> {
    let mut transport = TransportConfig::default();
    transport.keep_alive_interval(Some(KEEP_ALIVE));
    transport.max_idle_timeout(Some(IdleTimeout::try_from(IDLE_TIMEOUT).expect("valid idle timeout")));
    Arc::new(transport)
}

async fn discovery_loop(inner: Arc<Inner>, mut events: mpsc::Receiver<DiscoveryEvent>) {
    while let Some(event) = events.recv().await {
        match event {
            DiscoveryEvent::Found { id, name, addrs } => {
                debug!(device = %name, ?addrs, "discovered");
                inner.discovered.lock().unwrap().insert(id, addrs);
                let wake = inner.peers.lock().unwrap().get(&id).map(|e| e.wake.clone());
                if let Some(wake) = wake {
                    wake.notify_one();
                }
            }
            DiscoveryEvent::Lost { id } => {
                inner.discovered.lock().unwrap().remove(&id);
            }
        }
    }
}

async fn accept_loop(inner: Arc<Inner>) {
    while let Some(incoming) = inner.endpoint.accept().await {
        let inner = inner.clone();
        tokio::spawn(async move {
            let remote = incoming.remote_address();
            match incoming.await {
                Ok(conn) => match tls::peer_id(&conn) {
                    // Defense in depth: the verifier already checked the pin,
                    // but the peer set may have changed since.
                    Some(id) if !inner.pins.read().unwrap().contains(&id) => {
                        conn.close(0u32.into(), b"not paired");
                    }
                    Some(id) => match send(&conn, &Message::Hello.encode()).await {
                        Ok(()) => serve(inner, id, conn, false).await,
                        Err(e) => debug!(%remote, "failed to greet peer: {e:#}"),
                    },
                    None => warn!(%remote, "connection without a peer certificate"),
                },
                Err(e) => debug!(%remote, "incoming handshake failed: {e}"),
            }
        });
    }
}

/// Wait for `delay`, or less if something wakes this peer's dial loop.
async fn pause(delay: Duration, wake: &Notify) {
    tokio::select! {
        _ = tokio::time::sleep(delay) => {}
        _ = wake.notified() => {}
    }
}

async fn dial_loop(inner: Arc<Inner>, id: DeviceId, wake: Arc<Notify>) {
    let tls = match tls::client_config(&inner.identity, Trust::Exactly(id), ALPN)
        .and_then(|c| Ok(QuicClientConfig::try_from(c)?))
    {
        Ok(c) => c,
        Err(e) => {
            warn!("cannot build client config: {e:#}");
            return;
        }
    };
    let mut config = quinn::ClientConfig::new(Arc::new(tls));
    config.transport_config(transport());

    let mut backoff = MIN_BACKOFF;
    let mut attempt = 0usize;
    let mut reported = false;
    loop {
        if inner.is_connected(id) {
            backoff = MIN_BACKOFF;
            pause(CONNECTED_RECHECK, &wake).await;
            continue;
        }
        let name = inner.name(id);
        let candidates = inner.candidates(id).await;
        if candidates.is_empty() {
            if !reported {
                info!(peer = %name, "waiting for peer to appear on the network");
                reported = true;
            }
        } else {
            // Rotate through candidates so one dead address can't block us.
            let addr = candidates[attempt % candidates.len()];
            match dial(&inner, addr, config.clone()).await {
                Ok(conn) => {
                    backoff = MIN_BACKOFF;
                    attempt = 0;
                    reported = false;
                    serve(inner.clone(), id, conn, true).await;
                    continue;
                }
                Err(e) if !reported => {
                    info!(peer = %name, "waiting for peer ({e:#})");
                    reported = true;
                }
                Err(e) => debug!(peer = %name, "connect failed: {e:#}"),
            }
            attempt += 1;
        }
        pause(backoff, &wake).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

async fn dial(inner: &Inner, addr: SocketAddr, config: quinn::ClientConfig) -> Result<Connection> {
    let conn = inner
        .endpoint
        .connect_with(config, addr, tls::SERVER_NAME)?
        .await
        .with_context(|| format!("connecting to {addr}"))?;
    match tokio::time::timeout(HELLO_TIMEOUT, await_hello(&conn)).await {
        Ok(Ok(())) => Ok(conn),
        Ok(Err(e)) => {
            conn.close(0u32.into(), b"bad hello");
            Err(e.context(format!(
                "{addr} refused the connection; is this device paired on the other side?"
            )))
        }
        Err(_) => {
            conn.close(0u32.into(), b"hello timeout");
            anyhow::bail!("{addr} did not confirm the connection in time")
        }
    }
}

async fn await_hello(conn: &Connection) -> Result<()> {
    let bytes = conn.accept_uni().await?.read_to_end(MAX_MESSAGE_BYTES).await?;
    match Message::decode(&bytes)? {
        Message::Hello => Ok(()),
        other => anyhow::bail!("expected hello, got {other:?}"),
    }
}

/// Resolves to an IPv4 address, since the endpoint is bound to IPv4.
async fn resolve(address: &str) -> Result<SocketAddr> {
    tokio::net::lookup_host(address)
        .await
        .with_context(|| format!("resolving {address}"))?
        .find(SocketAddr::is_ipv4)
        .with_context(|| format!("{address} has no IPv4 address"))
}

/// Runs one connection until it closes: registers it for sending and reads
/// incoming messages in stream order.
async fn serve(inner: Arc<Inner>, id: DeviceId, conn: Connection, dialed: bool) {
    let name = inner.name(id);
    let (outbox, outbox_rx) = mpsc::channel(SEND_QUEUE);
    tokio::spawn(write_loop(conn.clone(), outbox_rx, name.clone()));
    let dialer = if dialed { inner.identity.id } else { id };
    match inner.register(id, PeerConnection { conn: conn.clone(), outbox, dialer }) {
        Registration::New => info!(peer = %name, remote = %conn.remote_address(), "connected"),
        Registration::Replaced(old) => {
            debug!(peer = %name, "switched to preferred connection");
            retire_later(old);
        }
        Registration::Duplicate => {
            debug!(peer = %name, "retiring duplicate connection");
            retire_later(conn.clone());
        }
    }

    let reason = loop {
        let mut stream = match conn.accept_uni().await {
            Ok(stream) => stream,
            Err(e) => break e,
        };
        let bytes = match stream.read_to_end(MAX_MESSAGE_BYTES).await {
            Ok(bytes) => bytes,
            Err(e) => {
                warn!(peer = %name, "failed to read message: {e}");
                continue;
            }
        };
        match Message::decode(&bytes) {
            Ok(Message::Hello) => debug!(peer = %name, "ignoring repeated hello"),
            Ok(message) => {
                let incoming = Incoming { from: id, from_name: name.clone(), message };
                if inner.inbox.send(incoming).await.is_err() {
                    return;
                }
            }
            Err(e) => warn!(peer = %name, "ignoring undecodable message: {e}"),
        }
    };

    // Dropping our PeerConnection closes its outbox, which ends write_loop.
    if inner.unregister(id, &conn) {
        info!(peer = %name, "disconnected: {reason}");
    } else {
        debug!(peer = %name, "secondary connection closed: {reason}");
    }
}

async fn write_loop(conn: Connection, mut outbox: mpsc::Receiver<Arc<[u8]>>, name: String) {
    while let Some(bytes) = outbox.recv().await {
        if let Err(e) = send(&conn, &bytes).await {
            warn!(peer = %name, "send failed: {e:#}");
        }
    }
}

/// Sends one message on its own stream and waits for the peer to ack it.
async fn send(conn: &Connection, bytes: &[u8]) -> Result<()> {
    let mut stream = conn.open_uni().await?;
    stream.write_all(bytes).await?;
    stream.finish()?;
    stream.stopped().await?;
    Ok(())
}
