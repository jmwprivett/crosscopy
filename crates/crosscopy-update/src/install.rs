//! Applying a downloaded (and already verified) update. On success the
//! caller must exit promptly; the new version starts once this process ends.

use anyhow::Result;
use std::path::Path;

/// Windows: run the Inno Setup installer silently. `/update=1` makes it
/// relaunch the app when done; it waits for this process to close first.
#[cfg(windows)]
pub fn install(installer: &Path) -> Result<()> {
    use anyhow::Context;
    std::process::Command::new(installer)
        .args(["/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART", "/update=1"])
        .spawn()
        .context("starting the installer")?;
    Ok(())
}

/// macOS: unpack the signed `.app`, check it's signed by the same team as
/// the running app, swap the bundles, and relaunch once we exit.
#[cfg(target_os = "macos")]
pub fn install(zip: &Path) -> Result<()> {
    use anyhow::{Context, bail, ensure};
    use std::fs;
    use std::process::Command;

    let exe = std::env::current_exe()?;
    // .../CrossCopy.app/Contents/MacOS/crosscopy-tray
    let app = exe
        .ancestors()
        .nth(3)
        .filter(|p| p.extension().is_some_and(|e| e == "app"))
        .context("updates only work for the installed CrossCopy.app")?
        .to_path_buf();
    let parent = app.parent().context("app has no parent folder")?;
    let staging = parent.join(format!(".crosscopy-update-{}", std::process::id()));
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging).with_context(|| format!("can't write to {}", parent.display()))?;

    let result = (|| -> Result<()> {
        let status = Command::new("ditto").arg("-x").arg("-k").arg(zip).arg(&staging).status()?;
        ensure!(status.success(), "couldn't unpack the update");
        let new_app = staging.join(app.file_name().context("app has no name")?);
        ensure!(new_app.exists(), "update doesn't contain {}", app.display());

        let status = Command::new("codesign").args(["--verify", "--deep", "--strict"]).arg(&new_app).status()?;
        ensure!(status.success(), "update's code signature is invalid");
        let (current_team, new_team) = (team_id(&app)?, team_id(&new_app)?);
        if current_team != new_team {
            bail!("update is signed by a different developer ({new_team:?}, expected {current_team:?})");
        }

        let backup = staging.join("previous.app");
        fs::rename(&app, &backup).context("moving the current app aside")?;
        if let Err(e) = fs::rename(&new_app, &app) {
            let _ = fs::rename(&backup, &app);
            return Err(e).context("moving the new app into place");
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&staging);
        return result;
    }
    // The previous version stays in `staging` until the new one launches.
    relaunch_after_exit("open", &app, Some(&staging))
}

#[cfg(target_os = "macos")]
fn team_id(app: &Path) -> Result<Option<String>> {
    let output = std::process::Command::new("codesign").args(["-dv", "--verbose=2"]).arg(app).output()?;
    // codesign prints details to stderr.
    let text = String::from_utf8_lossy(&output.stderr);
    Ok(text
        .lines()
        .find_map(|l| l.strip_prefix("TeamIdentifier="))
        .filter(|t| *t != "not set")
        .map(str::to_owned))
}

/// Linux: unpack the tarball next to the running binaries, atomically
/// replace them, and relaunch once we exit.
#[cfg(target_os = "linux")]
pub fn install(tarball: &Path) -> Result<()> {
    use anyhow::{Context, ensure};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    let exe = std::env::current_exe()?;
    let dir = exe.parent().context("executable has no folder")?;
    let staging = dir.join(format!(".crosscopy-update-{}", std::process::id()));
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging).with_context(|| format!("can't write to {}", dir.display()))?;

    let result = (|| -> Result<()> {
        let file = fs::File::open(tarball)?;
        tar::Archive::new(flate2::read::GzDecoder::new(file))
            .unpack(&staging)
            .context("couldn't unpack the update")?;
        let exe_name = exe.file_name().context("executable has no name")?;
        ensure!(staging.join(exe_name).exists(), "update doesn't contain {exe_name:?}");
        for name in ["crosscopy-tray", "crosscopy"] {
            let new = staging.join(name);
            let target = dir.join(name);
            // Replace the binaries that are installed here; don't add others.
            if new.exists() && (target.exists() || exe_name == name) {
                fs::set_permissions(&new, fs::Permissions::from_mode(0o755))?;
                fs::rename(&new, &target).with_context(|| format!("replacing {}", target.display()))?;
            }
        }
        Ok(())
    })();
    let _ = fs::remove_dir_all(&staging);
    result?;
    relaunch_after_exit(&exe.to_string_lossy(), Path::new(""), None)
}

/// Starts `program [arg]` after this process exits, then removes `cleanup`.
#[cfg(unix)]
fn relaunch_after_exit(program: &str, arg: &Path, cleanup: Option<&Path>) -> Result<()> {
    use std::process::{Command, Stdio};
    let script = r#"while kill -0 "$1" 2>/dev/null; do sleep 0.2; done; [ -n "$4" ] && rm -rf "$4"; if [ -n "$3" ]; then exec "$2" "$3"; else exec "$2"; fi"#;
    Command::new("/bin/sh")
        .arg("-c")
        .arg(script)
        .arg("sh")
        .arg(std::process::id().to_string())
        .arg(program)
        .arg(arg)
        .arg(cleanup.unwrap_or(Path::new("")))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(())
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
pub fn install(_: &Path) -> Result<()> {
    anyhow::bail!("updates aren't supported on this platform")
}
