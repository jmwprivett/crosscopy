//! Finding a device to pair with, shared by the CLI and the tray app.

use crate::config::{Config, Paths};
use anyhow::{Result, bail};
use crosscopy_core::DeviceId;
use crosscopy_net::Identity;
use crosscopy_net::pairing::{Pairing, PairingEvent, PendingPair};
use std::collections::HashSet;
use std::net::IpAddr;
use std::time::Duration;

pub const PAIR_TIMEOUT: Duration = Duration::from_secs(120);

/// Enters pairing mode and waits until a session with another device is
/// ready for confirmation. `only` restricts pairing to a device name.
/// `on_found(name, wanted)` is called once per device seen.
pub async fn find(
    identity: &Identity,
    device_name: &str,
    only: Option<&str>,
    mut on_found: impl FnMut(&str, bool),
) -> Result<(Pairing, PendingPair)> {
    let mut pairing = Pairing::start(identity, device_name)?;
    let wanted = |name: &str| only.is_none_or(|n| n.eq_ignore_ascii_case(name));
    // mDNS re-announces per interface; only report each device once.
    let mut seen = HashSet::new();
    let deadline = tokio::time::sleep(PAIR_TIMEOUT);
    tokio::pin!(deadline);

    loop {
        tokio::select! {
            event = pairing.next() => match event {
                Some(PairingEvent::Found(candidate)) => {
                    let is_wanted = wanted(&candidate.name);
                    if seen.insert(candidate.id) {
                        on_found(&candidate.name, is_wanted);
                    }
                    // Exactly one side starts the session, so both devices
                    // end up confirming the same one.
                    if is_wanted && identity.id < candidate.id {
                        match pairing.connect(&candidate).await {
                            Ok(pending) => return Ok((pairing, pending)),
                            Err(e) => tracing::warn!("couldn't reach {}: {e:#}", candidate.name),
                        }
                    }
                }
                Some(PairingEvent::Incoming(pending)) if wanted(&pending.peer_name) => {
                    return Ok((pairing, pending));
                }
                Some(PairingEvent::Incoming(pending)) => {
                    tokio::spawn(pending.finish(false));
                }
                Some(PairingEvent::Lost(_)) => {}
                None => bail!("pairing stopped unexpectedly"),
            },
            _ = &mut deadline => bail!(
                "No device found. Start pairing on the other device too, on the same network."
            ),
        }
    }
}

/// Saves a confirmed pairing; returns the name the device was saved under.
pub fn save(paths: &Paths, id: DeviceId, name: &str, ip: IpAddr) -> Result<String> {
    // Reload in case the config changed while the user was confirming.
    let mut config = Config::load(&paths.config_file)?;
    let saved_as = config.upsert_peer(id, name, Some(ip.to_string()));
    config.save(&paths.config_file)?;
    Ok(saved_as)
}
