//! Start at login: a registry Run entry on Windows, a LaunchAgent on macOS.

use anyhow::{Context, Result};
use auto_launch::{AutoLaunch, AutoLaunchBuilder, MacOSLaunchMode};

fn launcher() -> Result<AutoLaunch> {
    let exe = std::env::current_exe().context("locating the app executable")?;
    AutoLaunchBuilder::new()
        .set_app_name("CrossCopy")
        .set_app_path(&exe.to_string_lossy())
        .set_macos_launch_mode(MacOSLaunchMode::LaunchAgent)
        .build()
        .context("configuring start at login")
}

pub fn is_enabled() -> bool {
    launcher().and_then(|l| Ok(l.is_enabled()?)).unwrap_or(false)
}

pub fn set(enabled: bool) -> Result<()> {
    let launcher = launcher()?;
    if enabled {
        launcher.enable().context("enabling start at login")?;
    } else {
        launcher.disable().context("disabling start at login")?;
    }
    Ok(())
}
