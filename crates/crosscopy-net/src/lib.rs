//! Encrypted peer-to-peer transport.
//!
//! Every device owns a self-signed certificate; its [`DeviceId`] is a hash of
//! that certificate. Connections are QUIC with mutual TLS 1.3, and each side
//! only accepts certificates whose hash is in its pinned peer list, so no CA
//! or network trust is involved. Pinning happens through [`pairing`].
//!
//! [`DeviceId`]: crosscopy_core::DeviceId

mod addr;
mod discovery;
mod identity;
mod node;
pub mod pairing;
mod tls;

pub use addr::local_addresses;
pub use identity::Identity;
pub use node::{Incoming, Node, NodeOptions, Peer};
