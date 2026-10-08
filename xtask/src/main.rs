//! Release tooling: `cargo xtask <command>`.

use anyhow::{Context, Result, bail, ensure};
use clap::{Parser, Subcommand};
use crosscopy_update::{Artifact, Manifest};
use image::{Rgba, RgbaImage, imageops};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

/// Environment variable holding the base64 release signing key in CI.
const SIGNING_KEY_ENV: &str = "UPDATE_SIGNING_KEY";

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Generate a new release signing keypair.
    Keygen {
        /// Where to write the secret key (base64). Must not exist.
        #[arg(long)]
        secret_out: PathBuf,
    },
    /// Write `latest.json` for a release.
    Manifest {
        #[arg(long)]
        version: String,
        /// File with release notes (optional).
        #[arg(long)]
        notes: Option<PathBuf>,
        /// URL the artifacts will be downloadable from (no trailing slash).
        #[arg(long)]
        base_url: String,
        #[arg(long)]
        out: PathBuf,
        /// `platform=path` pairs, e.g. `windows-x86_64=dist/CrossCopy-setup.exe`.
        artifacts: Vec<String>,
    },
    /// Sign a manifest with the key in $UPDATE_SIGNING_KEY, writing `<manifest>.sig`.
    Sign { manifest: PathBuf },
    /// Verify a manifest's signature against the public key built into the app.
    Verify { manifest: PathBuf },
    /// Render the app icon (glyph on a rounded tile) from `assets/crosscopy.png`.
    AppIcon {
        #[arg(long, default_value = "assets/app-icon.png")]
        out: PathBuf,
        #[arg(long, default_value_t = 1024)]
        size: u32,
    },
    /// Write a multi-size `.ico` from `assets/app-icon.png` (for the installer).
    WindowsIco { out: PathBuf },
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Keygen { secret_out } => keygen(secret_out),
        Command::Manifest { version, notes, base_url, out, artifacts } => {
            manifest(version, notes, base_url, out, artifacts)
        }
        Command::Sign { manifest } => sign(manifest),
        Command::Verify { manifest } => verify(manifest),
        Command::AppIcon { out, size } => app_icon(out, size),
        Command::WindowsIco { out } => windows_ico(out),
    }
}

fn windows_ico(out: PathBuf) -> Result<()> {
    let source = image::open("assets/app-icon.png").context("run from the repo root")?.to_rgba8();
    let mut dir = ico::IconDir::new(ico::ResourceType::Icon);
    for size in [16, 20, 24, 32, 40, 48, 64, 128, 256] {
        let scaled = imageops::resize(&source, size, size, imageops::FilterType::Lanczos3);
        dir.add_entry(ico::IconDirEntry::encode(&ico::IconImage::from_rgba_data(size, size, scaled.into_raw()))?);
    }
    if let Some(parent) = out.parent() {
        fs::create_dir_all(parent)?;
    }
    dir.write(fs::File::create(&out)?)?;
    println!("Wrote {}", out.display());
    Ok(())
}

fn keygen(secret_out: PathBuf) -> Result<()> {
    ensure!(!secret_out.exists(), "{} already exists; refusing to overwrite a key", secret_out.display());
    let mut secret = [0u8; 32];
    getrandom::fill(&mut secret).map_err(|e| anyhow::anyhow!("no system randomness: {e}"))?;
    fs::write(&secret_out, data_encoding::BASE64.encode(&secret))?;
    let public = crosscopy_update::public_key_for(&secret);
    println!("Secret key written to {}", secret_out.display());
    println!("Public key for crates/crosscopy-update/src/lib.rs:");
    println!("pub const PUBLIC_KEY: [u8; 32] = [");
    for row in public.chunks(16) {
        let bytes: Vec<String> = row.iter().map(|b| format!("0x{b:02x}")).collect();
        println!("    {},", bytes.join(", "));
    }
    println!("];");
    Ok(())
}

fn manifest(version: String, notes: Option<PathBuf>, base_url: String, out: PathBuf, artifacts: Vec<String>) -> Result<()> {
    semver::Version::parse(&version).context("version must be semver, e.g. 0.2.0")?;
    let notes = match notes {
        Some(path) => fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?,
        None => String::new(),
    };
    let mut platforms = BTreeMap::new();
    for spec in artifacts {
        let (platform, path) = spec.split_once('=').context("artifacts must be platform=path")?;
        let path = PathBuf::from(path);
        let name = path.file_name().context("artifact has no file name")?.to_string_lossy();
        platforms.insert(
            platform.to_owned(),
            Artifact {
                url: format!("{}/{name}", base_url.trim_end_matches('/')),
                sha256: crosscopy_update::sha256_file(&path)?,
                size: fs::metadata(&path)?.len(),
            },
        );
    }
    if platforms.is_empty() {
        bail!("no artifacts given");
    }
    let manifest = Manifest { version, notes: notes.trim().to_owned(), platforms };
    fs::write(&out, serde_json::to_vec_pretty(&manifest)?)?;
    println!("Wrote {}", out.display());
    Ok(())
}

fn sign(manifest: PathBuf) -> Result<()> {
    let encoded = std::env::var(SIGNING_KEY_ENV).with_context(|| format!("${SIGNING_KEY_ENV} is not set"))?;
    let secret: [u8; 32] = data_encoding::BASE64
        .decode(encoded.trim().as_bytes())
        .context("signing key is not base64")?
        .try_into()
        .map_err(|_| anyhow::anyhow!("signing key must be 32 bytes"))?;
    ensure!(
        crosscopy_update::public_key_for(&secret) == crosscopy_update::PUBLIC_KEY,
        "signing key doesn't match the public key built into the app; installed apps would reject this release"
    );
    let bytes = fs::read(&manifest)?;
    let sig_path = sig_path(&manifest);
    fs::write(&sig_path, crosscopy_update::sign_manifest(&bytes, &secret))?;
    println!("Wrote {}", sig_path.display());
    Ok(())
}

fn verify(manifest: PathBuf) -> Result<()> {
    let bytes = fs::read(&manifest)?;
    let signature = fs::read(sig_path(&manifest))?;
    let parsed = crosscopy_update::verify_manifest(&bytes, &signature, &crosscopy_update::PUBLIC_KEY)?;
    println!("Signature OK: version {} for {:?}", parsed.version, parsed.platforms.keys().collect::<Vec<_>>());
    Ok(())
}

fn sig_path(manifest: &std::path::Path) -> PathBuf {
    let mut name = manifest.as_os_str().to_owned();
    name.push(".sig");
    PathBuf::from(name)
}

/// The white glyph on a dark rounded tile, so the icon reads on light and
/// dark backgrounds (Explorer, Finder, installers).
fn app_icon(out: PathBuf, size: u32) -> Result<()> {
    const BACKGROUND: [u8; 3] = [0x1f, 0x29, 0x37];
    let glyph = image::open("assets/crosscopy.png").context("run from the repo root")?.to_rgba8();
    let s = size as f32;
    let radius = s * 0.22;
    let mut tile = RgbaImage::from_fn(size, size, |x, y| {
        // Distance outside the rounded rectangle, for an anti-aliased edge.
        let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
        let dx = (radius - fx).max(fx - (s - radius)).max(0.0);
        let dy = (radius - fy).max(fy - (s - radius)).max(0.0);
        let outside = (dx * dx + dy * dy).sqrt() - radius;
        let coverage = (0.5 - outside).clamp(0.0, 1.0);
        let [r, g, b] = BACKGROUND;
        Rgba([r, g, b, (coverage * 255.0).round() as u8])
    });
    let inner = (s * 0.72) as u32;
    let scaled = imageops::resize(&glyph, inner, inner, imageops::FilterType::Lanczos3);
    let offset = i64::from((size - inner) / 2);
    imageops::overlay(&mut tile, &scaled, offset, offset);
    tile.save(&out)?;
    println!("Wrote {}", out.display());
    Ok(())
}
