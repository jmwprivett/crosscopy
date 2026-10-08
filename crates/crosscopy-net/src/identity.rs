use anyhow::{Context, Result};
use crosscopy_core::DeviceId;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use std::fs;
use std::io::Write;
use std::path::Path;

const CERT_FILE: &str = "cert.der";
const KEY_FILE: &str = "key.der";

/// This device's long-lived TLS identity.
pub struct Identity {
    pub id: DeviceId,
    pub(crate) cert: CertificateDer<'static>,
    key: PrivatePkcs8KeyDer<'static>,
}

impl Identity {
    /// Loads the identity from `dir`, generating and saving one on first use.
    pub fn load_or_create(dir: &Path) -> Result<Self> {
        let cert_path = dir.join(CERT_FILE);
        let key_path = dir.join(KEY_FILE);

        if cert_path.exists() && key_path.exists() {
            let cert = fs::read(&cert_path).with_context(|| format!("reading {}", cert_path.display()))?;
            let key = fs::read(&key_path).with_context(|| format!("reading {}", key_path.display()))?;
            return Ok(Self::from_der(cert, key));
        }

        let generated = rcgen::generate_simple_self_signed(vec!["crosscopy".to_owned()])
            .context("generating device certificate")?;
        let cert = generated.cert.der().to_vec();
        let key = generated.signing_key.serialize_der();

        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        write_private(&key_path, &key)?;
        fs::write(&cert_path, &cert).with_context(|| format!("writing {}", cert_path.display()))?;
        tracing::info!("generated new device identity in {}", dir.display());
        Ok(Self::from_der(cert, key))
    }

    fn from_der(cert: Vec<u8>, key: Vec<u8>) -> Self {
        Self {
            id: DeviceId::from_cert(&cert),
            cert: CertificateDer::from(cert),
            key: PrivatePkcs8KeyDer::from(key),
        }
    }

    pub(crate) fn key(&self) -> PrivateKeyDer<'static> {
        PrivateKeyDer::Pkcs8(self.key.clone_key())
    }
}

/// Writes a file readable only by the current user (on Unix).
fn write_private(path: &Path, contents: &[u8]) -> Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).with_context(|| format!("writing {}", path.display()))?;
    file.write_all(contents)?;
    Ok(())
}
