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
