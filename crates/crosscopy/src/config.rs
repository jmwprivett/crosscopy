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
    /// Name shown to other devices; defaults to the hostname.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_name: Option<String>,
    #[serde(default, rename = "peer")]
    pub peers: Vec<PeerConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerConfig {
    pub name: String,
    /// Fallback `host` or `host:port` for when mDNS discovery can't find the
    /// peer. Hostnames are re-resolved on every connect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    /// The peer's device code, as printed by `crosscopy id` on that device.
    pub code: String,
}

fn default_port() -> u16 {
    DEFAULT_PORT
}

impl Default for Config {
    fn default() -> Self {
        Self { port: DEFAULT_PORT, device_name: None, peers: Vec::new() }
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
        // Write then rename so a running daemon never reads a partial file.
        let tmp = path.with_extension("toml.tmp");
        fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
        fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))
    }

    pub fn device_name(&self) -> String {
        self.device_name.clone().unwrap_or_else(|| {
            let host = gethostname::gethostname().to_string_lossy().into_owned();
            // macOS reports e.g. "pinkbook.local"; keep just the machine name.
            host.split('.').next().filter(|s| !s.is_empty()).unwrap_or("device").to_owned()
        })
    }

    pub fn peers(&self) -> Result<Vec<Peer>> {
        self.peers.iter().map(PeerConfig::to_peer).collect()
    }

    /// Adds a newly paired device, replacing any entry with the same code
    /// (re-pairing) and picking a unique name.
    pub fn upsert_peer(&mut self, id: DeviceId, name: &str, address: Option<String>) -> String {
        self.peers.retain(|p| p.device_id().ok() != Some(id));
        let base: String = name
            .chars()
            .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
            .collect();
        let mut unique = base.clone();
        let mut n = 2;
        while self.peers.iter().any(|p| p.name.eq_ignore_ascii_case(&unique)) {
            unique = format!("{base}-{n}");
            n += 1;
        }
        self.peers.push(PeerConfig { name: unique.clone(), address, code: id.to_string() });
        unique
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
            address: self.address.as_deref().map(with_default_port),
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
        let mut config = Config { port: 1234, ..Config::default() };
        config.upsert_peer(DeviceId([3; 20]), "mac", Some("mac.local".into()));
        config.upsert_peer(DeviceId([4; 20]), "pc", None);
        let parsed: Config = toml::from_str(&toml::to_string_pretty(&config).unwrap()).unwrap();
        assert_eq!(parsed.port, 1234);
        assert_eq!(parsed.peers[0].device_id().unwrap(), DeviceId([3; 20]));
        assert_eq!(parsed.peers[1].address, None);
    }

    #[test]
    fn existing_config_without_new_fields_still_loads() {
        let text = "port = 47800\n[[peer]]\nname = \"pinkbook\"\naddress = \"192.168.1.5\"\ncode = \"TFYR-UJ4U-X725-CEIQ-RRND-YGDR-XJVT-353D\"\n";
        let config: Config = toml::from_str(text).unwrap();
        assert_eq!(config.peers().unwrap()[0].address.as_deref(), Some("192.168.1.5:47800"));
    }

    #[test]
    fn upsert_replaces_same_device_and_dedupes_names() {
        let mut config = Config::default();
        assert_eq!(config.upsert_peer(DeviceId([1; 20]), "Josh's Mac", None), "Josh-s-Mac");
        assert_eq!(config.upsert_peer(DeviceId([2; 20]), "Josh's Mac", None), "Josh-s-Mac-2");
        // Re-pairing device 1 replaces its entry rather than adding one.
        assert_eq!(config.upsert_peer(DeviceId([1; 20]), "mac", None), "mac");
        assert_eq!(config.peers.len(), 2);
    }
}
