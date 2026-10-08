use anyhow::{Context, Result};
use crosscopy_core::DeviceId;
use crosscopy_net::Peer;
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

pub const DEFAULT_PORT: u16 = 47800;

pub struct Paths {
    pub config_file: PathBuf,
    pub identity_dir: PathBuf,
}

impl Paths {
    pub fn new() -> Result<Self> {
        let dirs = ProjectDirs::from("", "", "crosscopy").context("could not locate home directory")?;
        Ok(Self {
            config_file: dirs.config_dir().join("config.toml"),
            identity_dir: dirs.config_dir().join("identity"),
        })
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default, rename = "peer")]
    pub peers: Vec<PeerConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerConfig {
    pub name: String,
    /// `host` or `host:port`; hostnames are re-resolved on every connect.
    pub address: String,
    /// The peer's device code, as printed by `crosscopy id` on that device.
    pub code: String,
}

fn default_port() -> u16 {
    DEFAULT_PORT
}

impl Default for Config {
    fn default() -> Self {
        Self { port: DEFAULT_PORT, peers: Vec::new() }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        match fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).with_context(|| format!("parsing {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let text = toml::to_string_pretty(self)?;
        fs::write(path, text).with_context(|| format!("writing {}", path.display()))
    }
}

impl PeerConfig {
    pub fn device_id(&self) -> Result<DeviceId> {
        self.code
            .parse()
            .with_context(|| format!("peer {:?} has an invalid code", self.name))
    }

    pub fn to_peer(&self) -> Result<Peer> {
        Ok(Peer {
            id: self.device_id()?,
            name: self.name.clone(),
            address: with_default_port(&self.address),
        })
    }
}

fn with_default_port(address: &str) -> String {
    let has_port = address
        .rsplit_once(':')
        .is_some_and(|(_, port)| port.parse::<u16>().is_ok());
    if has_port {
        address.to_owned()
    } else {
        format!("{address}:{DEFAULT_PORT}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_port_is_appended_only_when_missing() {
        assert_eq!(with_default_port("mac.local"), "mac.local:47800");
        assert_eq!(with_default_port("10.0.0.2:9000"), "10.0.0.2:9000");
    }

    #[test]
    fn config_round_trips() {
        let config = Config {
            port: 1234,
            peers: vec![PeerConfig {
                name: "mac".into(),
                address: "mac.local".into(),
                code: DeviceId([3; 20]).to_string(),
            }],
        };
        let parsed: Config = toml::from_str(&toml::to_string_pretty(&config).unwrap()).unwrap();
        assert_eq!(parsed.port, 1234);
        assert_eq!(parsed.peers[0].device_id().unwrap(), DeviceId([3; 20]));
    }
}
