//! mDNS / DNS-SD advertisement and browsing.
//!
//! Each device advertises its [`DeviceId`] and display name in TXT records.
//! Advertisements are untrusted hints: they only tell us where to dial, and
//! the TLS pin check decides whether the device on the other end is real.

use crate::addr;
use anyhow::{Context, Result};
use crosscopy_core::DeviceId;
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use std::collections::HashMap;
use std::net::SocketAddr;
use tokio::sync::mpsc;

/// Advertised by running daemons.
pub const SYNC_SERVICE: &str = "_crosscopy._udp.local.";
/// Advertised by devices currently in pairing mode.
pub const PAIR_SERVICE: &str = "_crosscopy-pair._udp.local.";

#[derive(Debug, Clone)]
pub enum DiscoveryEvent {
    Found { id: DeviceId, name: String, addrs: Vec<SocketAddr> },
    Lost { id: DeviceId },
}

/// Advertises this device while alive; stops (sending goodbyes) on drop.
pub struct Discovery {
    daemon: ServiceDaemon,
    fullname: String,
}

impl Discovery {
    pub fn start(
        service: &str,
        id: DeviceId,
        name: &str,
        port: u16,
    ) -> Result<(Self, mpsc::Receiver<DiscoveryEvent>)> {
        let daemon = ServiceDaemon::new().context("starting mDNS")?;
        let instance = id.to_string().replace('-', "");
        let host = format!("crosscopy-{}.local.", instance[..8].to_lowercase());
        let code = id.to_string();
        let properties = [("id", code.as_str()), ("name", name)];
        let info = ServiceInfo::new(service, &instance, &host, "", port, &properties[..])
            .context("building mDNS advertisement")?
            .enable_addr_auto();
        let fullname = info.get_fullname().to_owned();
        daemon.register(info).context("registering mDNS service")?;
        let browse = daemon.browse(service).context("browsing mDNS")?;

        let (tx, rx) = mpsc::channel(32);
        tokio::spawn(async move {
            // ServiceRemoved only carries the instance name.
            let mut by_fullname: HashMap<String, DeviceId> = HashMap::new();
            while let Ok(event) = browse.recv_async().await {
                let out = match event {
                    ServiceEvent::ServiceResolved(info) => {
                        let Some(peer) = info
                            .txt_properties
                            .get_property_val_str("id")
                            .and_then(|s| s.parse::<DeviceId>().ok())
                        else {
                            continue;
                        };
                        if peer == id {
                            continue;
                        }
                        let addrs = addr::rank_peer_addresses(
                            info.addresses.iter().map(|a| a.to_ip_addr()),
                            info.port,
                        );
                        if addrs.is_empty() {
                            continue;
                        }
                        let name = info
                            .txt_properties
                            .get_property_val_str("name")
                            .unwrap_or("unnamed device")
                            .to_owned();
                        by_fullname.insert(info.fullname.clone(), peer);
                        DiscoveryEvent::Found { id: peer, name, addrs }
                    }
                    ServiceEvent::ServiceRemoved(_, fullname) => match by_fullname.remove(&fullname) {
                        Some(id) => DiscoveryEvent::Lost { id },
                        None => continue,
                    },
                    _ => continue,
                };
                if tx.send(out).await.is_err() {
                    break;
                }
            }
        });

        Ok((Self { daemon, fullname }, rx))
    }
}

impl Drop for Discovery {
    fn drop(&mut self) {
        let _ = self.daemon.unregister(&self.fullname);
        let _ = self.daemon.shutdown();
    }
}
