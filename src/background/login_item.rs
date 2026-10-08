//! Starting CloudBridge when the user logs in.
//!
//! The operating system's own record is the truth — a launch agent on
//! macOS, the per-user `Run` key on Windows, an XDG autostart entry on
//! Linux — so nothing is kept in `config.json`: a user who removes the item
//! in System Settings sees the switch off the next time they look.
//!
//! A login launch passes [`super::BACKGROUND_ARG`], so the app starts in the
//! menu bar without opening its window.

use anyhow::{anyhow, Result};
use auto_launch::{AutoLaunch, AutoLaunchBuilder};

fn auto_launch() -> Result<AutoLaunch> {
    let exe = std::env::current_exe()?;
    let path = exe
        .to_str()
        .ok_or_else(|| anyhow!("The app's path is not valid UTF-8: {}", exe.display()))?;

    let mut builder = AutoLaunchBuilder::new();
    builder
        // The launch agent's label and file name on macOS, the value name
        // under the `Run` key on Windows, the desktop file on Linux.
        .set_app_name(super::BUNDLE_ID)
        .set_app_path(path)
        .set_args(&[super::BACKGROUND_ARG]);
    #[cfg(target_os = "macos")]
    builder
        .set_macos_launch_mode(auto_launch::MacOSLaunchMode::LaunchAgent)
        // Lets System Settings → Login Items show the agent under
        // CloudBridge's name rather than the developer's.
        .set_bundle_identifiers(&[super::BUNDLE_ID]);
    #[cfg(target_os = "windows")]
    builder.set_windows_enable_mode(auto_launch::WindowsEnableMode::CurrentUser);
    #[cfg(target_os = "linux")]
    builder.set_linux_launch_mode(auto_launch::LinuxLaunchMode::XdgAutostart);

    Ok(builder.build()?)
}

/// Whether CloudBridge starts at login.
pub fn is_enabled() -> Result<bool> {
    Ok(auto_launch()?.is_enabled()?)
}

/// Start CloudBridge at login, or stop doing so.
///
/// Registers this executable, wherever it is now: an app moved after the
/// switch was turned on starts from the old place, or not at all, until the
/// switch is turned off and on again.
pub fn set_enabled(enabled: bool) -> Result<()> {
    let item = auto_launch()?;
    if enabled {
        item.enable()?;
    } else if item.is_enabled()? {
        item.disable()?;
    }
    Ok(())
}
