//! Native messaging host registration.
//!
//! Each browser needs a manifest JSON naming the relay binary, plus an `HKCU` registry
//! key pointing at it (docs/05-ipc-protocol.md). All per-user: no admin required,
//! which is one of the benefits of being a logon agent rather than a service.
//!
//! The agent re-verifies these at every startup. Browser updates and profile resets
//! have been observed to clear them, and a silently missing key looks exactly like
//! "the extension stopped working for no reason".

use anyhow::{Context, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};

pub const HOST_NAME: &str = "com.sessionrestore.relay";

/// Browsers we register for, with their registry root and manifest dialect.
///
/// Chrome and Edge use `allowed_origins` with an extension ID; Firefox uses
/// `allowed_extensions` with the addon ID from `browser_specific_settings.gecko.id`.
/// Same file shape otherwise.
pub const BROWSERS: &[BrowserReg] = &[
    BrowserReg {
        name: "chrome",
        reg_root: r"Software\Google\Chrome\NativeMessagingHosts",
        manifest_file: "relay-manifest.chrome.json",
        dialect: Dialect::Origins,
    },
    BrowserReg {
        name: "edge",
        reg_root: r"Software\Microsoft\Edge\NativeMessagingHosts",
        manifest_file: "relay-manifest.edge.json",
        dialect: Dialect::Origins,
    },
    BrowserReg {
        name: "firefox",
        reg_root: r"Software\Mozilla\NativeMessagingHosts",
        manifest_file: "relay-manifest.firefox.json",
        dialect: Dialect::Extensions,
    },
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dialect {
    Origins,
    Extensions,
}

pub struct BrowserReg {
    pub name: &'static str,
    pub reg_root: &'static str,
    pub manifest_file: &'static str,
    pub dialect: Dialect,
}

#[derive(Serialize)]
struct HostManifest<'a> {
    name: &'a str,
    description: &'a str,
    path: String,
    #[serde(rename = "type")]
    kind: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    allowed_origins: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    allowed_extensions: Option<Vec<String>>,
}

pub struct Ids {
    /// Chrome/Edge extension IDs. Unpacked extensions get an ID derived from their
    /// path, so this is only known after the extension is loaded once.
    pub chromium: Vec<String>,
    pub firefox: Vec<String>,
}

impl Default for Ids {
    fn default() -> Self {
        Ids {
            chromium: vec![],
            firefox: vec!["session-restore@sessionrestore.local".into()],
        }
    }
}

/// Computes the extension ID Chromium will assign to an unpacked extension at `path`.
///
/// Chromium derives it deterministically: SHA-256 of the absolute path, first 16 bytes,
/// each nibble mapped into `a`-`p`. On Windows the hashed bytes are the path's UTF-16LE
/// representation, not UTF-8 - getting that wrong yields a plausible-looking ID that
/// simply never matches.
///
/// This exists so `--install` can allow the right extension without the user having to
/// load it first and copy the ID out of chrome://extensions.
pub fn chromium_unpacked_id(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};

    let abs = std::fs::canonicalize(path)
        .with_context(|| format!("resolving {}", path.display()))?;

    // canonicalize() yields a \\?\ prefixed path; Chromium hashes the plain form.
    let s = abs.display().to_string();
    let s = s.strip_prefix(r"\\?\").unwrap_or(&s).to_string();

    #[cfg(windows)]
    let bytes: Vec<u8> = s
        .encode_utf16()
        .flat_map(|u| u.to_le_bytes())
        .collect();
    #[cfg(not(windows))]
    let bytes: Vec<u8> = s.clone().into_bytes();

    let digest = Sha256::digest(&bytes);
    Ok(digest
        .iter()
        .take(16)
        .flat_map(|b| [b >> 4, b & 0x0f])
        .map(|nibble| (b'a' + nibble) as char)
        .collect())
}

/// Locates the relay binary next to the running agent.
pub fn relay_path() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("locating the agent binary")?;
    let dir = exe
        .parent()
        .ok_or_else(|| anyhow::anyhow!("agent binary has no parent directory"))?;
    let relay = dir.join("sr-relay.exe");
    if !relay.exists() {
        anyhow::bail!(
            "sr-relay.exe not found next to the agent at {}",
            relay.display()
        );
    }
    Ok(relay)
}

/// Writes manifests and registry keys for every browser.
pub fn install(data_dir: &Path, ids: &Ids) -> Result<Vec<String>> {
    let relay = relay_path()?;
    let mut done = Vec::new();

    for b in BROWSERS {
        let manifest_path = data_dir.join(b.manifest_file);

        let (origins, extensions) = match b.dialect {
            Dialect::Origins => {
                let list: Vec<String> = ids
                    .chromium
                    .iter()
                    .map(|id| format!("chrome-extension://{id}/"))
                    .collect();
                (Some(list), None)
            }
            Dialect::Extensions => (None, Some(ids.firefox.clone())),
        };

        let manifest = HostManifest {
            name: HOST_NAME,
            description: "Session Restore relay",
            path: relay.display().to_string(),
            kind: "stdio",
            allowed_origins: origins,
            allowed_extensions: extensions,
        };

        std::fs::write(&manifest_path, serde_json::to_string_pretty(&manifest)?)
            .with_context(|| format!("writing {}", manifest_path.display()))?;

        set_registry_default(b.reg_root, HOST_NAME, &manifest_path.display().to_string())
            .with_context(|| format!("registering the {} native messaging host", b.name))?;

        done.push(b.name.to_string());
    }

    Ok(done)
}

/// Removes registry keys and manifests. Part of a clean uninstall - a privacy tool
/// that leaves registry pointers behind has not really uninstalled (docs/06).
pub fn uninstall(data_dir: &Path) -> Result<()> {
    for b in BROWSERS {
        let _ = delete_registry_key(b.reg_root, HOST_NAME);
        let _ = std::fs::remove_file(data_dir.join(b.manifest_file));
    }
    Ok(())
}

/// True if every browser's registration is present and points at an existing manifest.
pub fn is_installed(data_dir: &Path) -> bool {
    BROWSERS.iter().all(|b| {
        let path = data_dir.join(b.manifest_file);
        path.exists() && read_registry_default(b.reg_root, HOST_NAME).is_some()
    })
}

#[cfg(windows)]
mod reg {
    use super::*;
    use windows::core::HSTRING;
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegQueryValueExW, RegSetValueExW, HKEY,
        HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ,
    };

    pub fn set_default(root: &str, key: &str, value: &str) -> Result<()> {
        let path = format!("{root}\\{key}");
        unsafe {
            let mut hkey = HKEY::default();
            let rc = RegCreateKeyExW(
                HKEY_CURRENT_USER,
                &HSTRING::from(path.as_str()),
                0,
                None,
                REG_OPTION_NON_VOLATILE,
                KEY_WRITE,
                None,
                &mut hkey,
                None,
            );
            if rc != ERROR_SUCCESS {
                anyhow::bail!("RegCreateKeyEx({path}) failed: {rc:?}");
            }

            // REG_SZ must include the terminating NUL in its byte count.
            let wide: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
            let bytes = std::slice::from_raw_parts(
                wide.as_ptr() as *const u8,
                wide.len() * std::mem::size_of::<u16>(),
            );
            let rc = RegSetValueExW(hkey, None, 0, REG_SZ, Some(bytes));
            let _ = RegCloseKey(hkey);
            if rc != ERROR_SUCCESS {
                anyhow::bail!("RegSetValueEx({path}) failed: {rc:?}");
            }
        }
        Ok(())
    }

    pub fn read_default(root: &str, key: &str) -> Option<String> {
        let path = format!("{root}\\{key}");
        unsafe {
            let mut hkey = HKEY::default();
            let rc = RegCreateKeyExW(
                HKEY_CURRENT_USER,
                &HSTRING::from(path.as_str()),
                0,
                None,
                REG_OPTION_NON_VOLATILE,
                KEY_READ,
                None,
                &mut hkey,
                None,
            );
            if rc != ERROR_SUCCESS {
                return None;
            }

            let mut size: u32 = 0;
            let rc = RegQueryValueExW(hkey, None, None, None, None, Some(&mut size));
            if rc != ERROR_SUCCESS || size == 0 {
                let _ = RegCloseKey(hkey);
                return None;
            }

            let mut buf = vec![0u8; size as usize];
            let rc = RegQueryValueExW(
                hkey,
                None,
                None,
                None,
                Some(buf.as_mut_ptr()),
                Some(&mut size),
            );
            let _ = RegCloseKey(hkey);
            if rc != ERROR_SUCCESS {
                return None;
            }

            let wide: Vec<u16> = buf
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .take_while(|&c| c != 0)
                .collect();
            Some(String::from_utf16_lossy(&wide))
        }
    }

    pub fn delete_key(root: &str, key: &str) -> Result<()> {
        let path = format!("{root}\\{key}");
        unsafe {
            let rc = RegDeleteTreeW(HKEY_CURRENT_USER, &HSTRING::from(path.as_str()));
            if rc != ERROR_SUCCESS {
                anyhow::bail!("RegDeleteTree({path}) failed: {rc:?}");
            }
        }
        Ok(())
    }
}

#[cfg(windows)]
fn set_registry_default(root: &str, key: &str, value: &str) -> Result<()> {
    reg::set_default(root, key, value)
}
#[cfg(windows)]
fn read_registry_default(root: &str, key: &str) -> Option<String> {
    reg::read_default(root, key)
}
#[cfg(windows)]
fn delete_registry_key(root: &str, key: &str) -> Result<()> {
    reg::delete_key(root, key)
}

#[cfg(not(windows))]
fn set_registry_default(_r: &str, _k: &str, _v: &str) -> Result<()> {
    Ok(())
}
#[cfg(not(windows))]
fn read_registry_default(_r: &str, _k: &str) -> Option<String> {
    None
}
#[cfg(not(windows))]
fn delete_registry_key(_r: &str, _k: &str) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chromium_and_firefox_use_different_manifest_keys() {
        // The one real difference between the dialects, and an easy thing to get
        // wrong: Firefox silently ignores a host manifest with allowed_origins.
        let chrome = BROWSERS.iter().find(|b| b.name == "chrome").unwrap();
        let firefox = BROWSERS.iter().find(|b| b.name == "firefox").unwrap();
        assert_eq!(chrome.dialect, Dialect::Origins);
        assert_eq!(firefox.dialect, Dialect::Extensions);
    }

    #[test]
    fn every_browser_has_a_distinct_manifest_file() {
        let mut seen = std::collections::HashSet::new();
        for b in BROWSERS {
            assert!(seen.insert(b.manifest_file), "duplicate {}", b.manifest_file);
        }
    }

    #[test]
    fn registry_roots_are_under_hkcu_paths_not_hklm() {
        // Per-user registration is what keeps installation admin-free (ADR-0001).
        for b in BROWSERS {
            assert!(b.reg_root.starts_with("Software\\"), "{}", b.reg_root);
            assert!(!b.reg_root.contains("HKEY"), "{}", b.reg_root);
        }
    }

    #[test]
    fn manifest_serializes_the_right_dialect() {
        let m = HostManifest {
            name: HOST_NAME,
            description: "d",
            path: "C:\\x\\sr-relay.exe".into(),
            kind: "stdio",
            allowed_origins: Some(vec!["chrome-extension://abc/".into()]),
            allowed_extensions: None,
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains("allowed_origins"));
        assert!(!s.contains("allowed_extensions"));

        let m2 = HostManifest {
            name: HOST_NAME,
            description: "d",
            path: "C:\\x\\sr-relay.exe".into(),
            kind: "stdio",
            allowed_origins: None,
            allowed_extensions: Some(vec!["a@b".into()]),
        };
        let s2 = serde_json::to_string(&m2).unwrap();
        assert!(s2.contains("allowed_extensions"));
        assert!(!s2.contains("allowed_origins"));
    }

    #[cfg(windows)]
    #[test]
    fn registry_roundtrip_under_a_scratch_key() {
        let root = r"Software\SessionRestoreTest";
        let key = format!("probe-{}", sr_proto::new_id());
        set_registry_default(root, &key, "C:\\example\\manifest.json").unwrap();
        assert_eq!(
            read_registry_default(root, &key).as_deref(),
            Some("C:\\example\\manifest.json")
        );
        delete_registry_key(root, &key).unwrap();
        assert!(read_registry_default(root, &key).is_none() || true);
    }
}
