//! Which browser profile a session belongs to, and how to reopen it.
//!
//! This is harder than it looks and the obvious approach does not work.
//!
//! Chromium runs **one browser process for every profile**. Opening a second profile
//! does not start a second `chrome.exe`, it adds windows to the one that is already
//! running. So the browser's command line describes how the *first* window happened to
//! be opened, and on a normal launch from the taskbar it carries no
//! `--profile-directory` at all. Deriving a profile from it collapses every profile the
//! user has into one bucket, which is what this module exists to stop.
//!
//! There are two separate questions and they need different answers:
//!
//! - **Which profile do these tabs belong to?** Only the extension can answer, because
//!   only the extension runs inside the profile. It reports a stable per-profile id it
//!   generated in `storage.local`, which is per-profile by definition. See
//!   `profile_id` in the `hello` message.
//! - **How do I reopen that profile?** That needs the profile *directory* name, which
//!   the extension cannot see. It comes from the command line when a launch specified
//!   one, and otherwise from Chromium's own `Local State`.
//!
//! Only the directory name is ever stored. Chromium's `Local State` also holds display
//! names and the signed-in account's real name, and none of that is recorded here.

use std::path::{Path, PathBuf};

/// A profile directory name that is safe to store and replay.
///
/// Chromium names them `Default`, `Profile 1`, `Profile 2`, and so on, plus a couple of
/// fixed special cases. None of those contain anything personal. Anything else is
/// treated as unknown rather than stored, because a profile path can be arbitrary and
/// arbitrary means it can contain the user's name.
pub fn is_safe_profile_dir(value: &str) -> bool {
    if value == "Default" || value == "Guest Profile" || value == "System Profile" {
        return true;
    }
    match value.strip_prefix("Profile ") {
        Some(n) => !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()),
        None => false,
    }
}

/// The `--profile-directory` value on a Chromium command line, if it has one.
pub fn profile_dir_from_command_line(cmd: &str) -> Option<String> {
    for arg in super::super::watcher::processes::split_quoted_public(cmd) {
        let trimmed = arg.trim_matches('"');
        if let Some(v) = trimmed.strip_prefix("--profile-directory=") {
            let v = v.trim_matches('"');
            if is_safe_profile_dir(v) {
                return Some(v.to_string());
            }
            return None;
        }
    }
    None
}

/// Where Chromium keeps its `Local State` for a given browser.
fn user_data_dir(browser: &str) -> Option<PathBuf> {
    let local = std::env::var("LOCALAPPDATA").ok()?;
    let rel = match browser {
        "chrome" => r"Google\Chrome\User Data",
        "edge" => r"Microsoft\Edge\User Data",
        _ => return None,
    };
    let p = PathBuf::from(local).join(rel);
    p.is_dir().then_some(p)
}

/// The profile Chromium would open if launched with no arguments.
///
/// `Local State` records the last profile the user actually used, which is exactly what
/// a bare launch reopens. Reading it is how a capture can attribute a session that was
/// started from the taskbar, where there is no flag to read.
pub fn last_used_profile_dir(browser: &str) -> Option<String> {
    let state = user_data_dir(browser)?.join("Local State");
    let text = std::fs::read_to_string(state).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let last = value.get("profile")?.get("last_used")?.as_str()?;
    is_safe_profile_dir(last).then(|| last.to_string())
}

/// Every profile directory this browser has, most useful first.
///
/// Used to tell "this user has one profile, so an unmatched session is obviously
/// theirs" from "this user has four, so guessing would be wrong".
pub fn known_profile_dirs(browser: &str) -> Vec<String> {
    let Some(dir) = user_data_dir(browser) else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(dir.join("Local State")) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };

    // `profiles_order` is the order the user sees. Fall back to the keys of
    // `info_cache`, which is the same set without the ordering.
    let ordered = value
        .get("profile")
        .and_then(|p| p.get("profiles_order"))
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str())
                .filter(|s| is_safe_profile_dir(s))
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if !ordered.is_empty() {
        return ordered;
    }

    value
        .get("profile")
        .and_then(|p| p.get("info_cache"))
        .and_then(|v| v.as_object())
        .map(|o| {
            o.keys()
                .filter(|s| is_safe_profile_dir(s))
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

/// The profile directory an extension instance actually lives in.
///
/// This is the deterministic answer, and it replaces a guess.
///
/// Chromium stores an extension's `storage.local` inside the profile directory, at
/// `<User Data>\<Profile>\Local Extension Settings\<extension id>\`. The extension
/// writes its own profile id there, so the directory containing that id *is* the
/// profile the extension is running in. Nothing is inferred: the browser filed it there
/// itself.
///
/// The alternative, reading `Local State`'s `last_used`, answers "which profile did the
/// user look at most recently", which is only the same thing by coincidence. With two
/// profiles open at once it is wrong half the time, and being wrong here means
/// restoring one profile's tabs into another.
///
/// Returns `None` when the id is not on disk yet. `storage.local` is written
/// asynchronously, so the very first handshake after an extension generates its id can
/// race the write. The caller keeps the previous answer and tries again on the next
/// handshake.
pub fn directory_for_profile_id(
    browser: &str,
    extension_ids: &[String],
    profile_id: &str,
) -> Option<String> {
    // A UUID is specific enough that a substring match cannot collide, and reading the
    // LevelDB properly would mean vendoring a LevelDB reader to answer one question.
    if profile_id.len() < 8 {
        return None;
    }
    let base = user_data_dir(browser)?;
    scan_for_profile_id(&base, &known_profile_dirs(browser), extension_ids, profile_id)
}

/// The search itself, against an explicit user-data root so it can be tested.
fn scan_for_profile_id(
    base: &Path,
    profile_dirs: &[String],
    extension_ids: &[String],
    profile_id: &str,
) -> Option<String> {
    let needle = profile_id.as_bytes();
    for dir in profile_dirs {
        for ext in extension_ids {
            let store = base.join(dir).join("Local Extension Settings").join(ext);
            if !store.is_dir() {
                continue;
            }
            let Ok(entries) = std::fs::read_dir(&store) else {
                continue;
            };
            for entry in entries.flatten() {
                if contains_bytes(&entry.path(), needle) {
                    return Some(dir.clone());
                }
            }
        }
    }
    None
}

/// Whether a file contains a byte sequence.
///
/// Opened with sharing, because Chromium holds these files open for writing the whole
/// time it is running and an exclusive open simply fails.
fn contains_bytes(path: &Path, needle: &[u8]) -> bool {
    /// Skip anything implausibly large. Extension storage for this extension is a few
    /// kilobytes; a huge file here means something unexpected and is not worth reading
    /// into memory during a handshake.
    const MAX: u64 = 8 * 1024 * 1024;

    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() || meta.len() > MAX {
        return false;
    }

    let Ok(bytes) = read_shared(path) else {
        return false;
    };
    bytes.windows(needle.len()).any(|w| w == needle)
}

#[cfg(windows)]
fn read_shared(path: &Path) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    use std::os::windows::fs::OpenOptionsExt;
    // FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE
    const SHARE_ALL: u32 = 0x0000_0001 | 0x0000_0002 | 0x0000_0004;

    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(SHARE_ALL)
        .open(path)?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    Ok(buf)
}

#[cfg(not(windows))]
fn read_shared(path: &Path) -> std::io::Result<Vec<u8>> {
    std::fs::read(path)
}

/// The Chromium extension ids the agent has registered, read back from the manifest.
///
/// Read from disk rather than kept in memory because the manifest is the thing that is
/// actually true: it is what the browser consults, and it survives a restart.
pub fn registered_extension_ids(data_dir: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(data_dir.join("relay-manifest.chrome.json")) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    value
        .get("allowed_origins")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str())
                .filter_map(|o| {
                    o.strip_prefix("chrome-extension://")
                        .map(|rest| rest.trim_end_matches('/').to_string())
                })
                .filter(|id| !id.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// The best guess at which profile directory a connected browser is showing.
///
/// The command line wins when it has one, because it is a fact rather than a guess.
/// `Local State` is the fallback and is right for the overwhelmingly common case of a
/// browser opened normally.
pub fn resolve_profile_dir(browser: &str, command_line: Option<&str>) -> Option<String> {
    if let Some(cmd) = command_line {
        if let Some(dir) = profile_dir_from_command_line(cmd) {
            return Some(dir);
        }
    }
    last_used_profile_dir(browser)
}

/// Whether a stored directory name is worth passing to a launch.
pub fn launch_args_for(browser: &str, profile_dir: Option<&str>) -> Vec<String> {
    let Some(dir) = profile_dir else {
        return Vec::new();
    };
    if !is_safe_profile_dir(dir) {
        return Vec::new();
    }
    match browser {
        // Quoted by the launcher, not here: this is an argument, not a command line.
        "chrome" | "edge" => vec![format!("--profile-directory={dir}")],
        // Firefox profiles are named, not directories, and the name is user-chosen and
        // may contain anything. Deliberately not replayed.
        _ => Vec::new(),
    }
}

/// Where a browser's profile data lives, for callers that need to check it exists.
pub fn profile_path(browser: &str, profile_dir: &str) -> Option<PathBuf> {
    let base = user_data_dir(browser)?;
    let p: &Path = &base.join(profile_dir);
    p.is_dir().then(|| p.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The mapping is deterministic because the browser filed the data itself.
    /// Proven against a real profile tree rather than a mocked one.
    #[test]
    fn a_profile_id_is_found_in_the_directory_that_holds_it() {
        let root = std::env::temp_dir().join(format!("sr-prof-{}", sr_proto::new_id()));
        let ext = "abcdefghijklmnopabcdefghijklmnop";

        for dir in ["Default", "Profile 1"] {
            let store = root.join(dir).join("Local Extension Settings").join(ext);
            std::fs::create_dir_all(&store).unwrap();
        }
        // Only one profile holds this id, the way Chromium would have written it.
        std::fs::write(
            root.join("Profile 1")
                .join("Local Extension Settings")
                .join(ext)
                .join("000003.log"),
            b" garbage sr_profile_id the-real-id-9f2c more",
        )
        .unwrap();

        let found = scan_for_profile_id(
            &root,
            &["Default".into(), "Profile 1".into()],
            &[ext.to_string()],
            "the-real-id-9f2c",
        );
        assert_eq!(found.as_deref(), Some("Profile 1"));

        let missing = scan_for_profile_id(
            &root,
            &["Default".into(), "Profile 1".into()],
            &[ext.to_string()],
            "an-id-nobody-has",
        );
        assert_eq!(missing, None, "matched a profile that does not hold the id");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A short id could collide by accident, and a wrong answer here restores one
    /// profile's tabs into another.
    #[test]
    fn an_implausibly_short_id_is_refused() {
        assert_eq!(directory_for_profile_id("chrome", &["x".into()], "abc"), None);
    }

    #[test]
    fn extension_ids_are_read_back_out_of_the_manifest() {
        let dir = std::env::temp_dir().join(format!("sr-manifest-{}", sr_proto::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("relay-manifest.chrome.json"),
            r#"{"allowed_origins":["chrome-extension://aaaabbbbccccddddeeeeffffgggghhhh/"]}"#,
        )
        .unwrap();

        assert_eq!(
            registered_extension_ids(&dir),
            vec!["aaaabbbbccccddddeeeeffffgggghhhh".to_string()]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn chromium_profile_directory_names_are_safe_to_store() {
        assert!(is_safe_profile_dir("Default"));
        assert!(is_safe_profile_dir("Profile 1"));
        assert!(is_safe_profile_dir("Profile 42"));
        assert!(is_safe_profile_dir("Guest Profile"));
    }

    /// A profile path can be arbitrary, and arbitrary means it can carry the user's
    /// name. Anything that is not a known-shaped directory is treated as unknown.
    #[test]
    fn anything_that_could_carry_a_name_is_refused() {
        assert!(!is_safe_profile_dir(r"C:\Users\Alioune\custom-profile"));
        assert!(!is_safe_profile_dir("Alioune"));
        assert!(!is_safe_profile_dir("Profile "));
        assert!(!is_safe_profile_dir("Profile abc"));
        assert!(!is_safe_profile_dir(""));
    }

    #[test]
    fn the_flag_is_read_off_a_command_line() {
        let cmd = r#""C:\chrome.exe" --profile-directory="Profile 1" --other"#;
        assert_eq!(profile_dir_from_command_line(cmd).as_deref(), Some("Profile 1"));

        let unquoted = r#""C:\chrome.exe" --profile-directory=Default"#;
        assert_eq!(profile_dir_from_command_line(unquoted).as_deref(), Some("Default"));
    }

    #[test]
    fn a_bare_launch_has_no_flag_to_read() {
        // This is the normal case and the reason the module exists: Chromium runs one
        // browser process for every profile, and a taskbar launch names none of them.
        assert_eq!(profile_dir_from_command_line(r#""C:\chrome.exe""#), None);
    }

    #[test]
    fn an_unsafe_flag_value_is_refused_rather_than_stored() {
        let cmd = r#""C:\chrome.exe" --profile-directory="C:\Users\Alioune\secret""#;
        assert_eq!(profile_dir_from_command_line(cmd), None);
    }

    #[test]
    fn launch_arguments_are_built_only_for_chromium() {
        assert_eq!(
            launch_args_for("chrome", Some("Profile 1")),
            vec!["--profile-directory=Profile 1".to_string()]
        );
        assert_eq!(
            launch_args_for("edge", Some("Default")),
            vec!["--profile-directory=Default".to_string()]
        );
        // Firefox profile names are user-chosen free text, so they are never replayed.
        assert!(launch_args_for("firefox", Some("Default")).is_empty());
        assert!(launch_args_for("chrome", None).is_empty());
        assert!(launch_args_for("chrome", Some("../../etc")).is_empty());
    }
}
