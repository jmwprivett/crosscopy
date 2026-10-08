//! The QUIC node: listens for pinned peers, keeps dialing the ones that are
//! not connected, and moves [`Message`]s in both directions.
//!
//! Both sides dial each other, so two connections to the same peer can
//! briefly coexist. Both ends deterministically keep the one dialed by the
//! lower [`DeviceId`] and retire the other after a grace period, during which
//! it is still read so nothing already in flight is lost.

use crate::identity::Identity;
use crate::tls;
use anyhow::{Context, Result};
use crosscopy_core::{DeviceId, MAX_MESSAGE_BYTES, Message};
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::{Connection, Endpoint, IdleTimeout, TransportConfig};
use rustls::pki_types::CertificateDer;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

const KEEP_ALIVE: Duration = Duration::from_secs(5);
const IDLE_TIMEOUT: Duration = Duration::from_secs(15);
const MIN_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
const CONNECTED_RECHECK: Duration = Duration::from_secs(2);
/// Messages queued per connection before new ones are dropped.
const SEND_QUEUE: usize = 16;
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a duplicate connection stays readable before it is closed.
const DUPLICATE_GRACE: Duration = Duration::from_secs(2);

#[derive(Debug, Clone)]
pub struct Peer {
    pub id: DeviceId,
    pub name: String,
    /// `host:port`; re-resolved on every dial so DHCP and `.local` names work.
    pub address: String,
}

#[derive(Debug)]
pub struct Incoming {
    pub from: DeviceId,
    pub message: Message,
}

#[derive(Clone)]
pub struct Node {
    inner: Arc<Inner>,
}

struct Inner {
    local_id: DeviceId,
    endpoint: Endpoint,
    peers: HashMap<DeviceId, Peer>,
    /// The active (sending) connection for each connected peer.
    connections: Mutex<HashMap<DeviceId, PeerConnection>>,
    inbox: mpsc::Sender<Incoming>,
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
    /// Binds the endpoint and starts accepting and dialing in the background.
    /// Must be called from within a Tokio runtime.
    pub fn start(
        identity: &Identity,
        listen: SocketAddr,
        peers: Vec<Peer>,
    ) -> Result<(Self, mpsc::Receiver<Incoming>)> {
        let pinned = peers.iter().map(|p| p.id).collect();
        let server_crypto = QuicServerConfig::try_from(tls::server_config(identity, pinned)?)?;
        let mut server = quinn::ServerConfig::with_crypto(Arc::new(server_crypto));
        server.transport_config(transport());
        let endpoint =
            Endpoint::server(server, listen).with_context(|| format!("binding UDP {listen}"))?;

        let (inbox, inbox_rx) = mpsc::channel(64);
        let inner = Arc::new(Inner {
            local_id: identity.id,
            endpoint,
            peers: peers.iter().map(|p| (p.id, p.clone())).collect(),
            connections: Mutex::default(),
            inbox,
        });

        tokio::spawn(accept_loop(inner.clone()));
        for peer in peers {
            let client_crypto = QuicClientConfig::try_from(tls::client_config(identity, peer.id)?)?;
            let mut client = quinn::ClientConfig::new(Arc::new(client_crypto));
            client.transport_config(transport());
            tokio::spawn(dial_loop(inner.clone(), peer, client));
        }
        Ok((Self { inner }, inbox_rx))
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.inner.endpoint.local_addr()
    }

    /// Queues `message` for every connected peer; returns how many it reached.
    pub fn broadcast(&self, message: &Message) -> usize {
        let bytes: Arc<[u8]> = message.encode().into();
        let connections = self.inner.connections.lock().unwrap();
        let mut queued = 0;
        for (id, pc) in connections.iter() {
            match pc.outbox.try_send(bytes.clone()) {
                Ok(()) => queued += 1,
                Err(_) => warn!(peer = %self.inner.name(*id), "send queue full; dropping message"),
            }
        }
        queued
    }

    /// Closes all connections and waits briefly for peers to be notified.
    pub async fn shutdown(&self) {
        self.inner.endpoint.close(0u32.into(), b"shutdown");
        let _ = tokio::time::timeout(Duration::from_secs(1), self.inner.endpoint.wait_idle()).await;
    }
}

impl Inner {
    fn name(&self, id: DeviceId) -> String {
        self.peers.get(&id).map_or_else(|| id.to_string(), |p| p.name.clone())
    }

    fn is_connected(&self, id: DeviceId) -> bool {
        self.connections.lock().unwrap().contains_key(&id)
    }

    fn register(&self, peer: DeviceId, new: PeerConnection) -> Registration {
        let preferred_dialer = self.local_id.min(peer);
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

fn transport() -> Arc<TransportConfig> {
    let mut transport = TransportConfig::default();
    transport.keep_alive_interval(Some(KEEP_ALIVE));
    transport.max_idle_timeout(Some(IdleTimeout::try_from(IDLE_TIMEOUT).expect("valid idle timeout")));
    Arc::new(transport)
}

async fn accept_loop(inner: Arc<Inner>) {
    while let Some(incoming) = inner.endpoint.accept().await {
        let inner = inner.clone();
        tokio::spawn(async move {
            let remote = incoming.remote_address();
            match incoming.await {
                Ok(conn) => match peer_id(&conn) {
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

async fn dial_loop(inner: Arc<Inner>, peer: Peer, config: quinn::ClientConfig) {
    let mut backoff = MIN_BACKOFF;
    let mut reported = false;
    loop {
        if inner.is_connected(peer.id) {
            backoff = MIN_BACKOFF;
            tokio::time::sleep(CONNECTED_RECHECK).await;
            continue;
        }
        match dial(&inner, &peer, config.clone()).await {
            Ok(conn) => {
                backoff = MIN_BACKOFF;
                reported = false;
                serve(inner.clone(), peer.id, conn, true).await;
                continue;
            }
            Err(e) if !reported => {
                info!(peer = %peer.name, "waiting for peer ({e:#})");
                reported = true;
            }
            Err(e) => debug!(peer = %peer.name, "connect failed: {e:#}"),
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

async fn dial(inner: &Inner, peer: &Peer, config: quinn::ClientConfig) -> Result<Connection> {
    let addr = resolve(&peer.address).await?;
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
                "{addr} refused the connection; is this device added as a peer on the other side?"
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

fn peer_id(conn: &Connection) -> Option<DeviceId> {
    let certs = conn
        .peer_identity()?
        .downcast::<Vec<CertificateDer<'static>>>()
        .ok()?;
    certs.first().map(|cert| DeviceId::from_cert(cert))
}

/// Runs one connection until it closes: registers it for sending and reads
/// incoming messages in stream order.
async fn serve(inner: Arc<Inner>, id: DeviceId, conn: Connection, dialed: bool) {
    let name = inner.name(id);
    let (outbox, outbox_rx) = mpsc::channel(SEND_QUEUE);
    tokio::spawn(write_loop(conn.clone(), outbox_rx, name.clone()));
    let dialer = if dialed { inner.local_id } else { id };
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
                if inner.inbox.send(Incoming { from: id, message }).await.is_err() {
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
