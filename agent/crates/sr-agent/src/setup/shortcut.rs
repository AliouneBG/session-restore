//! The Start Menu entry.
//!
//! Session Restore has no main window: it is a tray icon and a review window that
//! appears on its own. That is the right shape for it, but it left a hole. Quit from
//! the tray and there was no way back except finding the executable by hand or signing
//! out and in again, because the only other thing that starts it is the logon task.
//!
//! A Start Menu shortcut costs one file and makes the app findable the way every other
//! app on the machine is findable.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// The shortcut's display name, and therefore what the user types into Start.
pub const LINK_NAME: &str = "Session Restore.lnk";

/// Per-user, not all-users: the agent is per-user (ADR-0001), it runs from wherever
/// this build lives, and writing to the machine-wide Start Menu needs admin.
pub fn start_menu_dir() -> Result<PathBuf> {
    let appdata = std::env::var("APPDATA").context("APPDATA is not set")?;
    Ok(PathBuf::from(appdata)
        .join("Microsoft")
        .join("Windows")
        .join("Start Menu")
        .join("Programs"))
}

pub fn link_path() -> Result<PathBuf> {
    Ok(start_menu_dir()?.join(LINK_NAME))
}

/// Creates or replaces the Start Menu shortcut pointing at `exe`.
#[cfg(windows)]
pub fn install(exe: &Path) -> Result<PathBuf> {
    use windows::core::{Interface, HSTRING};
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, IPersistFile, CLSCTX_INPROC_SERVER,
        COINIT_APARTMENTTHREADED,
    };
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};

    let dir = start_menu_dir()?;
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let link = dir.join(LINK_NAME);

    unsafe {
        // Ignore the result: the thread may already be initialized, and
        // RPC_E_CHANGED_MODE is not a failure for our purposes. Omitting this entirely
        // is what made an earlier shell call silently return nothing.
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

        let shell_link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)
            .context("creating a ShellLink instance")?;

        shell_link
            .SetPath(&HSTRING::from(exe.as_os_str()))
            .context("setting the shortcut target")?;
        shell_link
            .SetDescription(&HSTRING::from("Restore your last session"))
            .context("setting the shortcut description")?;

        // Working directory matters: the agent resolves the relay binary relative to
        // its own location, but a shortcut with no working directory inherits
        // whatever Explorer happened to have.
        if let Some(parent) = exe.parent() {
            let _ = shell_link.SetWorkingDirectory(&HSTRING::from(parent.as_os_str()));
        }
        let _ = shell_link.SetIconLocation(&HSTRING::from(exe.as_os_str()), 0);

        let persist: IPersistFile = shell_link.cast().context("QI for IPersistFile")?;
        persist
            .Save(&HSTRING::from(link.as_os_str()), true)
            .with_context(|| format!("writing {}", link.display()))?;
    }

    Ok(link)
}

/// Removes the shortcut. Absent is success: uninstall must be idempotent.
pub fn uninstall() -> Result<()> {
    if let Ok(link) = link_path() {
        match std::fs::remove_file(&link) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("removing {}", link.display())),
        }
    }
    Ok(())
}

pub fn is_installed() -> bool {
    link_path().map(|p| p.exists()).unwrap_or(false)
}

#[cfg(not(windows))]
pub fn install(_exe: &Path) -> Result<PathBuf> {
    anyhow::bail!("Start Menu shortcuts are Windows only")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shortcut_lands_in_the_per_user_start_menu() {
        let Ok(dir) = start_menu_dir() else {
            return; // APPDATA unset, nothing to assert
        };
        let s = dir.to_string_lossy();
        assert!(s.contains("Start Menu"), "not a Start Menu path: {s}");
        assert!(s.ends_with("Programs"), "shortcuts must land in Programs: {s}");
    }

    #[test]
    fn the_link_is_named_so_start_search_finds_it() {
        // Users type "session restore", so the file name is the search key.
        assert!(LINK_NAME.starts_with("Session Restore"));
        assert!(LINK_NAME.ends_with(".lnk"));
    }
}
