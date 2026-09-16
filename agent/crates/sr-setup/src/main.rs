//! The installer.
//!
//! **It never asks for administrator.** That is not a shortcut, it is the product:
//! everything Session Restore touches is per-user by design. Its data directory, its
//! registry keys under HKCU, its logon task, its Start Menu shortcut (ADR-0001). An
//! installer that prompted for elevation would be asking for a privilege the
//! application never uses, and teaching the user that this program needs one.
//!
//! What it does:
//!
//! 1. Copies the payload next to itself into `%LOCALAPPDATA%\Programs\SessionRestore`.
//! 2. Runs `sr-agent --install`, which owns registration. The installer deliberately
//!    does not know how to write a native messaging manifest: there is one
//!    implementation of that and it lives in the agent, where the tests are.
//! 3. Registers an Add/Remove Programs entry, so uninstalling works the way a user
//!    expects rather than by finding a folder.
//! 4. Starts the agent, which shows the welcome flow on first run.
//!
//! Uninstall reverses it, and deliberately leaves the captured sessions unless asked:
//! `--purge` is how you say you meant it.

#![cfg_attr(windows, windows_subsystem = "windows")]

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Matches the key the agent installs under, so the two agree on what "installed" is.
const APP_NAME: &str = "Session Restore";
const UNINSTALL_KEY: &str =
    r"Software\Microsoft\Windows\CurrentVersion\Uninstall\SessionRestore";
const PUBLISHER: &str = "AliouneBG";

/// The files the installed copy needs.
///
/// `WebView2Loader.dll` is not optional on the GNU toolchain: without it the agent
/// dies before `main` with 0xC0000135 and no log, which is a failure mode worth never
/// meeting again (ADR-0006).
//
// The installer copies itself too: `UninstallString` has to point at something that
// still exists after the staging folder or the zip is thrown away.
const PAYLOAD: &[&str] = &[
    "sr-agent.exe",
    "sr-relay.exe",
    "sr-setup.exe",
    "WebView2Loader.dll",
];

/// Copied whole when present, so the unpacked extension travels with the install and
/// the user can point their browser at a stable path.
const PAYLOAD_DIRS: &[&str] = &["extension"];

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Unattended installs, and the test that drives this. A dialog waiting for a click
    // is correct for a double click and wrong for everything else.
    let silent = args.iter().any(|a| a == "--silent" || a == "/S");

    let result = if args.iter().any(|a| a == "--uninstall") {
        uninstall(args.iter().any(|a| a == "--purge"), silent)
    } else {
        install(silent)
    };

    if let Err(e) = result {
        // Always reported somewhere: a dialog when a person is watching, the exit code
        // and stderr when a script is.
        if silent {
            eprintln!("Session Restore setup failed: {e:#}");
            std::process::exit(1);
        }
        report(&format!("Session Restore setup could not finish.\n\n{e:#}"), true);
        std::process::exit(1);
    }
}

pub fn install_dir() -> Result<PathBuf> {
    let local = std::env::var("LOCALAPPDATA").context("LOCALAPPDATA is not set")?;
    Ok(PathBuf::from(local).join("Programs").join("SessionRestore"))
}

fn source_dir() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("locating the installer")?;
    Ok(exe
        .parent()
        .context("the installer has no parent directory")?
        .to_path_buf())
}

fn install(silent: bool) -> Result<()> {
    let src = source_dir()?;
    let dst = install_dir()?;

    // Installing over a running copy is the normal upgrade path, and Windows will not
    // let a running executable be replaced. Stopping them first is the difference
    // between an upgrade and an error the user cannot act on.
    //
    // The relays matter as much as the agent and are easier to forget: they are
    // children of the *browsers*, not of the agent, so they keep sr-relay.exe open
    // long after the agent is gone. The browsers respawn them on the next connect.
    stop_running(&dst, "sr-agent.exe");
    stop_running(&dst, "sr-relay.exe");

    std::fs::create_dir_all(&dst).with_context(|| format!("creating {}", dst.display()))?;

    let mut copied = 0usize;
    for name in PAYLOAD {
        let from = src.join(name);
        if !from.is_file() {
            // Only the agent is truly required; the loader is absent on MSVC builds,
            // where it is linked statically.
            if *name == "sr-agent.exe" {
                anyhow::bail!("{} is missing from {}", name, src.display());
            }
            continue;
        }
        let to = dst.join(name);
        // Re-running the installed copy would otherwise try to copy a file onto
        // itself, which fails rather than being the no-op it should be.
        if same_file(&from, &to) {
            copied += 1;
            continue;
        }
        replace_file(&from, &to).with_context(|| format!("copying {name}"))?;
        copied += 1;
    }
    for name in PAYLOAD_DIRS {
        let from = src.join(name);
        if from.is_dir() {
            copy_tree(&from, &dst.join(name))?;
        }
    }

    let agent = dst.join("sr-agent.exe");

    // Registration lives in the agent. There is one implementation of it, with tests,
    // and a second one here would be a second thing to keep correct.
    let status = std::process::Command::new(&agent)
        .arg("--install")
        .status()
        .context("running the agent's registration step")?;
    if !status.success() {
        anyhow::bail!("the agent's registration step failed");
    }

    register_uninstaller(&dst)?;

    // Start it, detached. The agent shows its welcome flow on first run, which is the
    // rest of the install as far as the user is concerned.
    //
    // Detached matters more than it looks. A plain spawn hands the agent this
    // process's stdout, and the agent runs until logout, so anything that pipes the
    // installer's output waits forever for a handle that never closes. An unattended
    // install would hang rather than finish.
    spawn_detached(&agent).context("starting the agent")?;

    if silent {
        return Ok(());
    }
    report(
        &format!(
            "Session Restore is installed and running.\n\n\
             {copied} file(s) in {}\n\n\
             Look for it in the system tray, next to the clock.",
            dst.display()
        ),
        false,
    );
    Ok(())
}

fn uninstall(purge: bool, silent: bool) -> Result<()> {
    let dst = install_dir()?;
    let agent = dst.join("sr-agent.exe");

    if agent.is_file() {
        let mut cmd = std::process::Command::new(&agent);
        cmd.arg("--uninstall");
        // Captured sessions survive unless the user said otherwise. Deleting someone's
        // last month of sessions because they uninstalled an application is not a
        // decision to make on their behalf.
        if !purge {
            cmd.arg("--keep-data");
        }
        let _ = cmd.status();
    }

    stop_running(&dst, "sr-agent.exe");
    stop_running(&dst, "sr-relay.exe");
    let _ = remove_uninstaller();

    // The installer cannot delete itself while it is running, so a leftover is
    // expected and harmless. Everything else goes.
    if dst.is_dir() {
        for entry in std::fs::read_dir(&dst)?.flatten() {
            let p = entry.path();
            if p == std::env::current_exe().unwrap_or_default() {
                continue;
            }
            let _ = if p.is_dir() {
                std::fs::remove_dir_all(&p)
            } else {
                std::fs::remove_file(&p)
            };
        }
    }

    if !silent {
        report("Session Restore has been removed.", false);
    }
    Ok(())
}

/// Starts a program with no console and no inherited handles.
#[cfg(windows)]
fn spawn_detached(exe: &Path) -> Result<()> {
    use std::os::windows::process::CommandExt;
    // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP
    const FLAGS: u32 = 0x0000_0008 | 0x0000_0200;
    std::process::Command::new(exe)
        .creation_flags(FLAGS)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    Ok(())
}

#[cfg(not(windows))]
fn spawn_detached(exe: &Path) -> Result<()> {
    std::process::Command::new(exe).spawn()?;
    Ok(())
}

/// True when both paths name the same file on disk.
fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)?.flatten() {
        let src = entry.path();
        let dst = to.join(entry.file_name());
        if src.is_dir() {
            copy_tree(&src, &dst)?;
        } else {
            std::fs::copy(&src, &dst)
                .with_context(|| format!("copying {}", src.display()))?;
        }
    }
    Ok(())
}

/// Stops an installed agent so its executable can be replaced.
///
/// Matched by path, not by name: a developer running the agent out of a build
/// directory should not have it killed by an installer touching a different copy.
/// Copies over a file that may still be locked.
///
/// Windows refuses to overwrite a running executable but is happy to *rename* one, and
/// a renamed file keeps running until its last handle closes. So the old copy is moved
/// aside and deleted on the next install, which turns an upgrade that fails into one
/// that leaves a stale file nobody will notice.
fn replace_file(from: &Path, to: &Path) -> Result<()> {
    match std::fs::copy(from, to) {
        Ok(_) => return Ok(()),
        Err(e) if e.kind() != std::io::ErrorKind::PermissionDenied => return Err(e.into()),
        Err(_) => {}
    }

    let aside = to.with_extension("old");
    let _ = std::fs::remove_file(&aside);
    std::fs::rename(to, &aside)
        .with_context(|| format!("moving the running {} aside", to.display()))?;
    std::fs::copy(from, to)?;
    Ok(())
}

fn stop_running(install_dir: &Path, exe_name: &str) {
    let target = install_dir.join(exe_name);
    if !target.is_file() {
        return;
    }
    // Matched by full path, not by image name: a developer running the agent out of a
    // build directory should not have it killed by an installer touching a different
    // copy.
    let script = format!(
        "Get-CimInstance Win32_Process -Filter {} |          Where-Object {{ $_.ExecutablePath -eq {} }} |          ForEach-Object {{ Stop-Process -Id $_.ProcessId -Force }}",
        ps_literal(&format!("Name='{exe_name}'")),
        ps_literal(&target.display().to_string())
    );
    let _ = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output();
    // Give Windows a moment to release the file handle before it is overwritten.
    std::thread::sleep(std::time::Duration::from_millis(800));
}

/// Quotes a path as a PowerShell single-quoted string.
///
/// Backslashes need no escaping inside single quotes, but an apostrophe does, and a
/// user name containing one is not exotic.
fn ps_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[cfg(windows)]
fn register_uninstaller(dir: &Path) -> Result<()> {
    use windows::core::HSTRING;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER,
        KEY_WRITE, REG_DWORD, REG_OPTION_NON_VOLATILE, REG_SZ,
    };

    let setup = dir.join("sr-setup.exe");
    let agent = dir.join("sr-agent.exe");

    unsafe {
        let mut key = HKEY::default();
        // HKCU, so no elevation. Windows shows per-user entries in Apps and Features
        // exactly like machine-wide ones.
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            &HSTRING::from(UNINSTALL_KEY),
            0,
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut key,
            None,
        )
        .ok()
        .context("creating the uninstall registry key")?;

        let set_sz = |name: &str, value: &str| {
            let wide: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
            let bytes = std::slice::from_raw_parts(
                wide.as_ptr() as *const u8,
                wide.len() * std::mem::size_of::<u16>(),
            );
            let _ = RegSetValueExW(key, &HSTRING::from(name), 0, REG_SZ, Some(bytes));
        };

        set_sz("DisplayName", APP_NAME);
        set_sz("DisplayVersion", env!("CARGO_PKG_VERSION"));
        set_sz("Publisher", PUBLISHER);
        set_sz("InstallLocation", &dir.display().to_string());
        set_sz("DisplayIcon", &agent.display().to_string());
        set_sz(
            "UninstallString",
            &format!("\"{}\" --uninstall", setup.display()),
        );

        // Windows hides the Modify and Repair buttons when these are set, which is
        // right: neither exists.
        let one: u32 = 1;
        let bytes = std::slice::from_raw_parts(
            &one as *const u32 as *const u8,
            std::mem::size_of::<u32>(),
        );
        let _ = RegSetValueExW(key, &HSTRING::from("NoModify"), 0, REG_DWORD, Some(bytes));
        let _ = RegSetValueExW(key, &HSTRING::from("NoRepair"), 0, REG_DWORD, Some(bytes));

        let _ = RegCloseKey(key);
    }
    Ok(())
}

#[cfg(windows)]
fn remove_uninstaller() -> Result<()> {
    use windows::core::HSTRING;
    use windows::Win32::System::Registry::{RegDeleteTreeW, HKEY_CURRENT_USER};
    unsafe {
        let _ = RegDeleteTreeW(HKEY_CURRENT_USER, &HSTRING::from(UNINSTALL_KEY));
    }
    Ok(())
}

/// Says something to a user who has no console, because this is a windows-subsystem
/// program launched by a double click.
#[cfg(windows)]
fn report(message: &str, error: bool) {
    use windows::core::HSTRING;
    use windows::Win32::UI::WindowsAndMessaging::{
        MessageBoxW, MB_ICONERROR, MB_ICONINFORMATION, MB_OK,
    };
    unsafe {
        MessageBoxW(
            None,
            &HSTRING::from(message),
            &HSTRING::from(APP_NAME),
            MB_OK | if error { MB_ICONERROR } else { MB_ICONINFORMATION },
        );
    }
}

#[cfg(not(windows))]
fn register_uninstaller(_dir: &Path) -> Result<()> {
    Ok(())
}
#[cfg(not(windows))]
fn remove_uninstaller() -> Result<()> {
    Ok(())
}
#[cfg(not(windows))]
fn report(message: &str, _error: bool) {
    println!("{message}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_install_goes_somewhere_that_needs_no_administrator() {
        let Ok(dir) = install_dir() else { return };
        let s = dir.to_string_lossy();
        assert!(s.contains("Programs"), "unexpected install location: {s}");
        // Program Files would need elevation, which this product never uses.
        assert!(!s.contains("Program Files"), "would require admin: {s}");
    }

    #[test]
    fn the_uninstall_key_is_per_user() {
        // Under HKCU when written, so Apps and Features lists it without elevation.
        assert!(UNINSTALL_KEY.starts_with(r"Software\Microsoft\Windows"));
        assert!(!UNINSTALL_KEY.starts_with("HKEY"));
    }

    #[test]
    fn the_installer_ships_itself_so_uninstall_survives() {
        // UninstallString points into the install directory, which has to still work
        // after the staging folder or the downloaded zip is deleted.
        assert!(PAYLOAD.contains(&"sr-setup.exe"));
    }

    #[test]
    fn the_payload_carries_what_the_gnu_build_needs() {
        // Missing WebView2Loader.dll kills the agent before main with no log at all.
        assert!(PAYLOAD.contains(&"sr-agent.exe"));
        assert!(PAYLOAD.contains(&"sr-relay.exe"));
        assert!(PAYLOAD.contains(&"WebView2Loader.dll"));
    }
}
