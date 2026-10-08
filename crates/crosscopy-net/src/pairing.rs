//! Interactive pairing with a short confirmation code.
//!
//! Both devices enter pairing mode, which advertises a separate mDNS
//! service on its own ephemeral port. One side connects with TLS that
//! accepts any certificate, then the two run a commit/reveal exchange:
//!
//! ```text
//! initiator                         responder
//!     Hello{name}            ──────►
//!                            ◄──────  Hello{name}, Commit(H(nR))
//!     Nonce(nI)              ──────►
//!                            ◄──────  Reveal(nR)          (initiator checks H(nR))
//!     code = H(idI, idR, nI, nR) shown on both; user compares
//!     Decision(bool)         ◄─────►  Decision(bool)
//! ```
//!
//! The responder commits to its nonce before seeing the initiator's, and the
//! initiator reveals its nonce before seeing the responder's. So a
//! man-in-the-middle, who must run separate sessions with each side using
//! its own certificate, can't steer the two codes to match: its chance is
//! one in a million per attempt.

use crate::discovery::{Discovery, DiscoveryEvent, PAIR_SERVICE};
use crate::identity::Identity;
use crate::node::transport;
use crate::tls::{self, Trust};
use anyhow::{Context, Result, bail, ensure};
use crosscopy_core::DeviceId;
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::{Connection, Endpoint, RecvStream, SendStream};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tracing::debug;

const PAIR_ALPN: &[u8] = b"crosscopy-pair/1";
const MAX_FRAME: usize = 4096;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);
/// How long to wait for the user on the other device to answer.
const DECISION_TIMEOUT: Duration = Duration::from_secs(180);

/// A device in pairing mode, seen over mDNS.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub id: DeviceId,
    pub name: String,
    pub addrs: Vec<SocketAddr>,
}

pub enum PairingEvent {
    Found(Candidate),
    Lost(DeviceId),
    /// Another device connected to us and the code is ready to confirm.
    Incoming(PendingPair),
}

/// A pairing session with its confirmation code, awaiting the user.
pub struct PendingPair {
    pub peer_id: DeviceId,
    pub peer_name: String,
    pub peer_addr: SocketAddr,
    /// Six digits, formatted `123 456`.
    pub code: String,
    conn: Connection,
    send: SendStream,
    recv: RecvStream,
}

#[derive(Debug, Serialize, Deserialize)]
enum PairMsg {
    Hello { name: String },
    Commit([u8; 32]),
    Nonce([u8; 32]),
    Reveal([u8; 32]),
    Decision(bool),
}

pub struct Pairing {
    identity: Identity,
    name: String,
    endpoint: Endpoint,
    client: quinn::ClientConfig,
    events: mpsc::Receiver<PairingEvent>,
    _discovery: Option<Discovery>,
}

impl Pairing {
    /// Enters pairing mode: listens on an ephemeral port and advertises it.
    pub fn start(identity: &Identity, name: &str) -> Result<Self> {
        Self::start_with(identity, name, true)
    }

    /// Like [`Pairing::start`], optionally without mDNS (for tests).
    pub fn start_with(identity: &Identity, name: &str, discovery: bool) -> Result<Self> {
        let server_tls = tls::server_config(identity, Trust::Any, PAIR_ALPN)?;
        let mut server = quinn::ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(server_tls)?));
        server.transport_config(transport());
        let endpoint = Endpoint::server(server, SocketAddr::from(([0, 0, 0, 0], 0)))
            .context("binding pairing endpoint")?;

        let client_tls = tls::client_config(identity, Trust::Any, PAIR_ALPN)?;
        let mut client = quinn::ClientConfig::new(Arc::new(QuicClientConfig::try_from(client_tls)?));
        client.transport_config(transport());

        let (tx, events) = mpsc::channel(16);
        tokio::spawn(accept_loop(endpoint.clone(), identity.id, name.to_owned(), tx.clone()));

        let discovery = if discovery {
            let port = endpoint.local_addr()?.port();
            let (discovery, mut found) = Discovery::start(PAIR_SERVICE, identity.id, name, port)?;
            tokio::spawn(async move {
                while let Some(event) = found.recv().await {
                    let event = match event {
                        DiscoveryEvent::Found { id, name, addrs } => {
                            PairingEvent::Found(Candidate { id, name, addrs })
                        }
                        DiscoveryEvent::Lost { id } => PairingEvent::Lost(id),
                    };
                    if tx.send(event).await.is_err() {
                        break;
                    }
                }
            });
            Some(discovery)
        } else {
            None
        };

        Ok(Self {
            identity: identity.clone(),
            name: name.to_owned(),
            endpoint,
            client,
            events,
            _discovery: discovery,
        })
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.endpoint.local_addr()
    }

    pub async fn next(&mut self) -> Option<PairingEvent> {
        self.events.recv().await
    }

    /// Starts pairing with `candidate`, trying its addresses in order.
    pub async fn connect(&self, candidate: &Candidate) -> Result<PendingPair> {
        let mut last_error = None;
        for &addr in &candidate.addrs {
            match timeout(HANDSHAKE_TIMEOUT, self.initiate(addr, candidate.id)).await {
                Ok(Ok(pending)) => return Ok(pending),
                Ok(Err(e)) => last_error = Some(e),
                Err(_) => last_error = Some(anyhow::anyhow!("timed out talking to {addr}")),
            }
        }
        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("{} has no addresses", candidate.name)))
    }

    async fn initiate(&self, addr: SocketAddr, expected: DeviceId) -> Result<PendingPair> {
        let conn = self
            .endpoint
            .connect_with(self.client.clone(), addr, tls::SERVER_NAME)?
            .await
            .with_context(|| format!("connecting to {addr}"))?;
        let peer_id = tls::peer_id(&conn).context("peer sent no certificate")?;
        ensure!(peer_id == expected, "device at {addr} is not the one that was advertised");

        let (mut send, mut recv) = conn.open_bi().await?;
        write_frame(&mut send, &PairMsg::Hello { name: self.name.clone() }).await?;
        let PairMsg::Hello { name: peer_name } = read_frame(&mut recv).await? else {
            bail!("protocol error: expected hello");
        };
        let PairMsg::Commit(commit) = read_frame(&mut recv).await? else {
            bail!("protocol error: expected commitment");
        };
        let nonce = random_nonce()?;
        write_frame(&mut send, &PairMsg::Nonce(nonce)).await?;
        let PairMsg::Reveal(their_nonce) = read_frame(&mut recv).await? else {
            bail!("protocol error: expected reveal");
        };
        ensure!(
            commitment(&their_nonce) == commit,
            "the other device's commitment didn't match; pairing aborted"
        );

        Ok(PendingPair {
            peer_id,
            peer_name: sanitize_name(&peer_name),
            peer_addr: addr,
            code: confirmation_code(self.identity.id, peer_id, &nonce, &their_nonce),
            conn,
            send,
            recv,
        })
    }
}

impl Drop for Pairing {
    fn drop(&mut self) {
        self.endpoint.close(0u32.into(), b"pairing ended");
    }
}

impl PendingPair {
    /// Sends the local user's decision and waits for the other side's.
    /// Returns `true` only if both accepted.
    pub async fn finish(mut self, accept: bool) -> Result<bool> {
        write_frame(&mut self.send, &PairMsg::Decision(accept)).await?;
        let theirs = timeout(DECISION_TIMEOUT, read_frame(&mut self.recv))
            .await
            .context("timed out waiting for the other device")?
            .context("the other device left")?;
        let PairMsg::Decision(theirs) = theirs else {
            bail!("protocol error: expected decision");
        };
        // Make sure our decision reached them before closing.
        self.send.finish()?;
        let _ = timeout(Duration::from_secs(2), self.send.stopped()).await;
        self.conn.close(0u32.into(), b"done");
        Ok(accept && theirs)
    }
}

async fn accept_loop(endpoint: Endpoint, local: DeviceId, name: String, events: mpsc::Sender<PairingEvent>) {
    while let Some(incoming) = endpoint.accept().await {
        let (name, events) = (name.clone(), events.clone());
        tokio::spawn(async move {
            match timeout(HANDSHAKE_TIMEOUT, respond(incoming, local, name)).await {
                Ok(Ok(pending)) => {
                    let _ = events.send(PairingEvent::Incoming(pending)).await;
                }
                Ok(Err(e)) => debug!("incoming pairing attempt failed: {e:#}"),
                Err(_) => debug!("incoming pairing attempt timed out"),
            }
        });
    }
}

async fn respond(incoming: quinn::Incoming, local: DeviceId, name: String) -> Result<PendingPair> {
    let peer_addr = incoming.remote_address();
    let conn = incoming.await?;
    let peer_id = tls::peer_id(&conn).context("peer sent no certificate")?;

    let (mut send, mut recv) = conn.accept_bi().await?;
    let PairMsg::Hello { name: peer_name } = read_frame(&mut recv).await? else {
        bail!("protocol error: expected hello");
    };
    let nonce = random_nonce()?;
    write_frame(&mut send, &PairMsg::Hello { name }).await?;
    write_frame(&mut send, &PairMsg::Commit(commitment(&nonce))).await?;
    let PairMsg::Nonce(their_nonce) = read_frame(&mut recv).await? else {
        bail!("protocol error: expected nonce");
    };
    write_frame(&mut send, &PairMsg::Reveal(nonce)).await?;

    Ok(PendingPair {
        peer_id,
        peer_name: sanitize_name(&peer_name),
        peer_addr,
        code: confirmation_code(peer_id, local, &their_nonce, &nonce),
        conn,
        send,
        recv,
    })
}

async fn write_frame(send: &mut SendStream, msg: &PairMsg) -> Result<()> {
    let bytes = postcard::to_allocvec(msg)?;
    send.write_all(&(bytes.len() as u32).to_be_bytes()).await?;
    send.write_all(&bytes).await?;
    Ok(())
}

async fn read_frame(recv: &mut RecvStream) -> Result<PairMsg> {
    let mut len = [0u8; 4];
    recv.read_exact(&mut len).await?;
    let len = u32::from_be_bytes(len) as usize;
    ensure!(len <= MAX_FRAME, "pairing message too large");
    let mut buf = vec![0u8; len];
    recv.read_exact(&mut buf).await?;
    Ok(postcard::from_bytes(&buf)?)
}

fn random_nonce() -> Result<[u8; 32]> {
    let mut nonce = [0u8; 32];
    getrandom::fill(&mut nonce).map_err(|e| anyhow::anyhow!("no system randomness: {e}"))?;
    Ok(nonce)
}

fn commitment(nonce: &[u8; 32]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new_derive_key("crosscopy pairing commitment v1");
    hasher.update(nonce);
    *hasher.finalize().as_bytes()
}

fn confirmation_code(
    initiator: DeviceId,
    responder: DeviceId,
    initiator_nonce: &[u8; 32],
    responder_nonce: &[u8; 32],
) -> String {
    let mut hasher = blake3::Hasher::new_derive_key("crosscopy pairing code v1");
    hasher.update(&initiator.0);
    hasher.update(&responder.0);
    hasher.update(initiator_nonce);
    hasher.update(responder_nonce);
    let bytes: [u8; 8] = hasher.finalize().as_bytes()[..8].try_into().expect("8 bytes");
    let n = u64::from_le_bytes(bytes) % 1_000_000;
    format!("{:03} {:03}", n / 1000, n % 1000)
}

/// Names come from the network; keep them short and printable.
fn sanitize_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .filter(|c| !c.is_control())
        .take(64)
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() { "device".to_owned() } else { trimmed.to_owned() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_depends_on_every_input() {
        let (a, b) = (DeviceId([1; 20]), DeviceId([2; 20]));
        let (x, y) = ([3; 32], [4; 32]);
        let base = confirmation_code(a, b, &x, &y);
        assert_eq!(base.len(), 7);
        assert_ne!(base, confirmation_code(b, a, &x, &y));
        assert_ne!(base, confirmation_code(a, b, &y, &x));
        assert_ne!(base, confirmation_code(a, b, &x, &[5; 32]));
    }
}
