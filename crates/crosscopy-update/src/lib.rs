//! Signed self-updates from GitHub Releases.
//!
//! Each release carries `latest.json` (version, notes, and per-platform
//! artifact URL + SHA-256 + size) and `latest.json.sig`, an Ed25519
//! signature over the exact manifest bytes. The private key exists only as
//! a CI secret; the public key is compiled in below. An update is accepted
//! only if the signature verifies, the version is strictly newer than the
//! running one (no downgrades), and the download matches the signed hash.

mod install;

pub use install::install;

use anyhow::{Context, Result, ensure};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

pub const MANIFEST_URL: &str = "https://github.com/jmwprivett/crosscopy/releases/latest/download/latest.json";

/// Public half of the release signing key (`cargo xtask keygen`).
pub const PUBLIC_KEY: [u8; 32] = [
    0xe0, 0xf4, 0x1e, 0xc3, 0x86, 0x50, 0xed, 0x88, 0xab, 0xdb, 0xbe, 0x76, 0x9b, 0x82, 0xce, 0xb0,
    0xfb, 0x75, 0x21, 0xaf, 0x62, 0x7b, 0xae, 0x53, 0x5d, 0x64, 0xaf, 0x29, 0x2b, 0xa7, 0x69, 0xbe,
];

const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
const MAX_ARTIFACT_BYTES: u64 = 300 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub version: String,
    #[serde(default)]
    pub notes: String,
    pub platforms: BTreeMap<String, Artifact>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Artifact {
    pub url: String,
    /// Lowercase hex.
    pub sha256: String,
    pub size: u64,
}

#[derive(Debug, Clone)]
pub struct Update {
    pub version: Version,
    pub notes: String,
    pub artifact: Artifact,
}

/// Whether this build may self-update. Only release CI sets this, so local
/// `cargo run` builds never try to replace themselves.
pub fn enabled() -> bool {
    option_env!("CROSSCOPY_UPDATES") == Some("1")
}

/// The manifest key for this build's platform.
pub fn platform() -> Option<&'static str> {
    if cfg!(all(windows, target_arch = "x86_64")) {
        Some("windows-x86_64")
    } else if cfg!(target_os = "macos") {
        Some("macos-universal")
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some("linux-x86_64")
    } else {
        None
    }
}

/// Fetches and verifies the latest manifest; returns an update if it is
/// newer than `current`.
pub fn check(current: &str) -> Result<Option<Update>> {
    let manifest = fetch(MANIFEST_URL, MAX_MANIFEST_BYTES).context("downloading update info")?;
    let signature = fetch(&format!("{MANIFEST_URL}.sig"), 1024).context("downloading update signature")?;
    let manifest = verify_manifest(&manifest, &signature, &PUBLIC_KEY)?;
    select(manifest, current, platform())
}

/// Checks the signature, then parses. Nothing in an unverified manifest is
/// ever looked at.
pub fn verify_manifest(bytes: &[u8], signature_b64: &[u8], public_key: &[u8; 32]) -> Result<Manifest> {
    let key = VerifyingKey::from_bytes(public_key).context("invalid update public key")?;
    let signature = data_encoding::BASE64
        .decode(signature_b64.trim_ascii())
        .context("update signature is not base64")?;
    let signature: [u8; 64] = signature
        .try_into()
        .map_err(|_| anyhow::anyhow!("update signature has the wrong length"))?;
    key.verify_strict(bytes, &Signature::from_bytes(&signature))
        .context("update signature is invalid; refusing to update")?;
    serde_json::from_slice(bytes).context("update info is malformed")
}

fn select(manifest: Manifest, current: &str, platform: Option<&str>) -> Result<Option<Update>> {
    let latest = Version::parse(&manifest.version).context("update has an invalid version")?;
    let current = Version::parse(current).context("this build has an invalid version")?;
    if latest <= current {
        return Ok(None);
    }
    let platform = platform.context("updates aren't available for this platform")?;
    let artifact = manifest
        .platforms
        .get(platform)
        .cloned()
        .with_context(|| format!("version {latest} has no download for {platform}"))?;
    ensure!(artifact.url.starts_with("https://"), "update URL is not HTTPS");
    Ok(Some(Update { version: latest, notes: manifest.notes, artifact }))
}

/// Downloads the update into `dir`, verifying size and SHA-256 as it goes.
pub fn download(update: &Update, dir: &Path) -> Result<PathBuf> {
    let name = update
        .artifact
        .url
        .rsplit('/')
        .next()
        .filter(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)))
        .context("update URL has no usable file name")?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(name);

    let result = (|| -> Result<()> {
        let response = ureq::get(&update.artifact.url).call().context("downloading update")?;
        let mut reader = response.into_body().into_reader().take(MAX_ARTIFACT_BYTES + 1);
        let mut file = File::create(&path).with_context(|| format!("creating {}", path.display()))?;
        let mut hasher = Sha256::new();
        let mut size = 0u64;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = reader.read(&mut buf).context("downloading update")?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            file.write_all(&buf[..n])?;
            size += n as u64;
        }
        file.sync_all()?;
        ensure!(size == update.artifact.size, "update download is the wrong size");
        let digest = hex::encode(hasher.finalize());
        ensure!(digest.eq_ignore_ascii_case(&update.artifact.sha256), "update download doesn't match its signed hash");
        Ok(())
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_file(&path);
        return Err(e);
    }
    Ok(path)
}

fn fetch(url: &str, limit: u64) -> Result<Vec<u8>> {
    let mut response = ureq::get(url).call()?;
    let bytes = response.body_mut().with_config().limit(limit).read_to_vec()?;
    Ok(bytes)
}

/// Signs manifest bytes; used by release tooling, not the app.
pub fn sign_manifest(bytes: &[u8], secret_key: &[u8; 32]) -> String {
    let signature = SigningKey::from_bytes(secret_key).sign(bytes);
    data_encoding::BASE64.encode(&signature.to_bytes())
}

pub fn public_key_for(secret_key: &[u8; 32]) -> [u8; 32] {
    SigningKey::from_bytes(secret_key).verifying_key().to_bytes()
}

/// Lowercase hex SHA-256 of a file, for building manifests.
pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: [u8; 32] = [7; 32];

    fn manifest(version: &str) -> Vec<u8> {
        serde_json::to_vec(&Manifest {
            version: version.into(),
            notes: "notes".into(),
            platforms: BTreeMap::from([(
                "test-os".to_owned(),
                Artifact { url: "https://example.com/a.zip".into(), sha256: "00".into(), size: 1 },
            )]),
        })
        .unwrap()
    }

    #[test]
    fn valid_signature_verifies() {
        let bytes = manifest("1.2.3");
        let sig = sign_manifest(&bytes, &SECRET);
        let parsed = verify_manifest(&bytes, sig.as_bytes(), &public_key_for(&SECRET)).unwrap();
        assert_eq!(parsed.version, "1.2.3");
    }

    #[test]
    fn tampered_manifest_is_rejected() {
        let bytes = manifest("1.2.3");
        let sig = sign_manifest(&bytes, &SECRET);
        let tampered = manifest("9.9.9");
        assert!(verify_manifest(&tampered, sig.as_bytes(), &public_key_for(&SECRET)).is_err());
    }

    #[test]
    fn wrong_key_is_rejected() {
        let bytes = manifest("1.2.3");
        let sig = sign_manifest(&bytes, &[8; 32]);
        assert!(verify_manifest(&bytes, sig.as_bytes(), &public_key_for(&SECRET)).is_err());
    }

    #[test]
    fn only_newer_versions_are_offered() {
        let m = || serde_json::from_slice::<Manifest>(&manifest("1.2.3")).unwrap();
        assert!(select(m(), "1.2.3", Some("test-os")).unwrap().is_none());
        assert!(select(m(), "2.0.0", Some("test-os")).unwrap().is_none());
        let update = select(m(), "1.2.2", Some("test-os")).unwrap().unwrap();
        assert_eq!(update.version, Version::new(1, 2, 3));
        assert!(select(m(), "1.0.0", Some("other-os")).is_err());
    }
}
