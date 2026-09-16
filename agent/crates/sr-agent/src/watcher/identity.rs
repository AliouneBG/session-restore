//! Application identity, path folding, command-line redaction, and restore tiers.
//!
//! Deliberately free of Win32 so it can be tested directly: these are the rules that
//! decide what gets stored and what gets thrown away, and they are worth pinning down
//! independently of the enumeration that feeds them.

use std::path::Path;

/// How faithfully an application can be brought back (docs/03-capture.md).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// Exact: known exe, usable command line, not elevated.
    A,
    /// Launch only: packaged app, or arguments unavailable or redacted.
    B,
    /// Document reopen: launch the document and let the shell pick the handler.
    C,
    /// Not restorable: elevated, installer, unresolvable.
    D,
}

impl Tier {
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::A => "A",
            Tier::B => "B",
            Tier::C => "C",
            Tier::D => "D",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppKind {
    Win32,
    Uwp,
    Unknown,
}

impl AppKind {
    pub fn as_str(self) -> &'static str {
        match self {
            AppKind::Win32 => "win32",
            AppKind::Uwp => "uwp",
            AppKind::Unknown => "unknown",
        }
    }
}

/// Replaces well-known absolute prefixes with their environment variable.
///
/// This is what lets a stored profile survive a drive-letter change, and it is the
/// groundwork for a future cross-device sync where `C:\Program Files` on one machine
/// is `D:\Program Files` on another. Longest prefix wins, so `%LOCALAPPDATA%` is not
/// shadowed by the `%APPDATA%`-shaped path that contains it.
pub fn fold_env(path: &str) -> String {
    let mut candidates: Vec<(String, &str)> = Vec::new();
    for (var, token) in [
        ("LOCALAPPDATA", "%LOCALAPPDATA%"),
        ("APPDATA", "%APPDATA%"),
        ("ProgramFiles(x86)", "%ProgramFiles(x86)%"),
        ("ProgramFiles", "%ProgramFiles%"),
        ("ProgramData", "%ProgramData%"),
        ("SystemRoot", "%SystemRoot%"),
        ("USERPROFILE", "%USERPROFILE%"),
    ] {
        if let Ok(value) = std::env::var(var) {
            if !value.is_empty() {
                candidates.push((value, token));
            }
        }
    }
    candidates.sort_by_key(|(v, _)| std::cmp::Reverse(v.len()));

    for (value, token) in candidates {
        if path.len() >= value.len() && path[..value.len()].eq_ignore_ascii_case(&value) {
            return format!("{token}{}", &path[value.len()..]);
        }
    }
    path.to_string()
}

/// Stable identity for an application.
///
/// A hash rather than the path itself, so the key stays a fixed shape and does not
/// leak the path into tables and logs that do not need it.
pub fn app_key(kind: AppKind, exe_path: Option<&str>, aumid: Option<&str>) -> String {
    use sha2::{Digest, Sha256};

    match kind {
        // A packaged app has no launchable exe path; the AUMID *is* its identity, and
        // it is already stable and human-meaningful, so it is used verbatim.
        AppKind::Uwp => format!("uwp:{}", aumid.unwrap_or("unknown")),
        AppKind::Win32 => {
            let folded = fold_env(exe_path.unwrap_or_default()).to_lowercase();
            let d = Sha256::digest(folded.as_bytes());
            format!("win32:{}", hex16(&d))
        }
        AppKind::Unknown => {
            let name = exe_path
                .and_then(|p| Path::new(p).file_name().map(|s| s.to_string_lossy().to_string()))
                .unwrap_or_else(|| "unknown".into())
                .to_lowercase();
            let d = Sha256::digest(name.as_bytes());
            format!("unk:{}", hex16(&d))
        }
    }
}

fn hex16(bytes: &[u8]) -> String {
    bytes.iter().take(16).map(|b| format!("{b:02x}")).collect()
}

/// Applications never worth capturing or restoring.
///
/// Seeded as ignore rules rather than hardcoded so the user can override them, but
/// having sensible defaults matters: without them the store fills with installers,
/// UAC prompts, and our own binaries.
pub const DEFAULT_IGNORED_EXES: &[&str] = &[
    // NOTE: explorer.exe is deliberately absent. It hosts both the shell (desktop,
    // taskbar) and File Explorer folder windows, and folder windows are exactly the
    // kind of thing people expect back. The shell's own windows are excluded by
    // window class instead - see IGNORED_WINDOW_CLASSES.
    "applicationframehost.exe", // UWP host; the real app is found via its AUMID
    "searchhost.exe",
    "shellexperiencehost.exe",
    "startmenuexperiencehost.exe",
    "textinputhost.exe",
    "systemsettings.exe",
    "lockapp.exe",
    "consent.exe",           // UAC prompt
    "runtimebroker.exe",
    "dwm.exe",
    "sihost.exe",
    "taskmgr.exe",
    "msiexec.exe",           // installers
    "setup.exe",
    "install.exe",
    "sr-agent.exe",          // ourselves
    "sr-relay.exe",
];

/// Window classes belonging to the shell itself rather than to anything the user
/// opened: the desktop, the wallpaper host, the taskbar and its flyouts.
///
/// Filtering these by class rather than by process is what lets File Explorer folder
/// windows be captured while the desktop is not, since both are `explorer.exe`.
pub const IGNORED_WINDOW_CLASSES: &[&str] = &[
    "Progman",                      // the desktop
    "WorkerW",                      // wallpaper / desktop host
    "Shell_TrayWnd",                // taskbar
    "Shell_SecondaryTrayWnd",
    "NotifyIconOverflowWindow",
    "Windows.UI.Core.CoreWindow",   // system flyouts and overlays
    "ApplicationManager_DesktopShellWindow",
    "ForegroundStaging",
    "XamlExplorerHostIslandWindow", // task view / alt-tab surfaces
    "MultitaskingViewFrame",
];

pub fn is_ignored_class(class: &str) -> bool {
    IGNORED_WINDOW_CLASSES.iter().any(|c| *c == class)
}

pub fn is_ignored_exe(exe_path: &str) -> bool {
    let name = Path::new(exe_path)
        .file_name()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    DEFAULT_IGNORED_EXES.iter().any(|d| *d == name)
}

/// Known browsers. Their window titles are page titles, so they are never stored
/// (docs/03-capture.md), and their tabs come from the extension instead.
pub const BROWSER_EXES: &[&str] = &[
    "chrome.exe",
    "msedge.exe",
    "firefox.exe",
    "brave.exe",
    "vivaldi.exe",
    "opera.exe",
    "arc.exe",
];

pub fn is_browser_exe(exe_path: &str) -> bool {
    let name = Path::new(exe_path)
        .file_name()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    BROWSER_EXES.iter().any(|b| *b == name)
}

/// What a window title may be stored as.
///
/// Returns `None` for any browser process. A browser window's title is the current
/// page title, so storing it would put private-browsing page titles into the plaintext
/// `windows` table, routing straight around the encrypted path the extension feeds.
///
/// The decision is made from the *process*, never by pattern-matching the title for
/// markers like "Private Browsing": those are localized, differ per browser, and any
/// page can set `document.title` to whatever it likes.
pub fn storable_title(exe_path: &str, title: &str) -> Option<String> {
    if is_browser_exe(exe_path) {
        return None;
    }
    if title.is_empty() {
        return None;
    }
    Some(title.to_string())
}

/// Redacts secrets from a command line before it is stored.
///
/// Command lines routinely carry access tokens, database passwords, and signed URLs,
/// and they are visible to anything that can list processes. A redacted argument is
/// replaced with a length-preserving sentinel so restore can tell the difference
/// between "no arguments" and "arguments we refused to keep", and drop to tier B
/// rather than replaying a secret.
pub fn redact_command_line(cmd: &str) -> (String, bool) {
    let mut redacted = false;
    let mut out: Vec<String> = Vec::new();

    for arg in split_args(cmd) {
        match redact_arg(&arg) {
            Some(r) => {
                redacted = true;
                out.push(r);
            }
            None => out.push(arg),
        }
    }
    (out.join(" "), redacted)
}

const SECRET_KEYS: &[&str] = &[
    "password", "passwd", "pwd", "token", "access_token", "refresh_token", "api_key",
    "apikey", "secret", "client_secret", "client-secret", "auth", "authorization",
    "session", "sessionid", "session_id", "key",
];

fn redact_arg(arg: &str) -> Option<String> {
    let lower = arg.to_lowercase();

    // --password=..., /token:..., api_key=...
    if let Some(pos) = arg.find(['=', ':']) {
        let (name, value) = arg.split_at(pos);
        let value = &value[1..];
        let stem = name.trim_start_matches(['-', '/']).to_lowercase();
        // A drive letter is not a key/value pair.
        if !value.is_empty() && stem.len() > 1 && SECRET_KEYS.iter().any(|k| stem.ends_with(k)) {
            return Some(format!("{name}{}<redacted:{}>", &arg[pos..pos + 1], value.len()));
        }
    }

    // Recognisable credential shapes, wherever they appear.
    if lower.starts_with("bearer ")
        || arg.starts_with("eyJ")                       // JWT
        || (arg.starts_with("ghp_") && arg.len() > 20)  // GitHub PAT
        || (arg.starts_with("AKIA") && arg.len() == 20) // AWS access key id
        || (arg.starts_with("xox") && arg.len() > 20)   // Slack token
    {
        return Some(format!("<redacted:{}>", arg.len()));
    }

    // Any URL carrying a query string: signed URLs and magic links live there.
    if (lower.starts_with("http://") || lower.starts_with("https://")) && arg.contains('?') {
        let cut = arg.find('?').unwrap();
        return Some(format!("{}?<redacted:{}>", &arg[..cut], arg.len() - cut - 1));
    }

    None
}

/// Splits a Windows command line, honouring double quotes.
fn split_args(cmd: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;

    for c in cmd.chars() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                cur.push(c);
            }
            c if c.is_whitespace() && !in_quotes => {
                if !cur.is_empty() {
                    args.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        args.push(cur);
    }
    args
}

/// Whether an application's arguments are worth keeping at all.
///
/// Collecting less is stronger than redacting more, so the command line is stored only
/// for applications whose arguments genuinely change what comes back: browsers with a
/// profile, editors with a folder, terminals with a working directory. Everything else
/// restores fine from the exe path alone (docs/06-privacy-security.md).
pub fn arguments_matter(exe_path: &str) -> bool {
    let name = Path::new(exe_path)
        .file_name()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    matches!(
        name.as_str(),
        "chrome.exe"
            | "msedge.exe"
            | "firefox.exe"
            | "brave.exe"
            | "vivaldi.exe"
            | "code.exe"
            | "devenv.exe"
            | "idea64.exe"
            | "pycharm64.exe"
            | "rider64.exe"
            | "sublime_text.exe"
            | "windowsterminal.exe"
            | "wt.exe"
            | "powershell.exe"
            | "pwsh.exe"
            | "cmd.exe"
            | "explorer.exe"
    )
}

pub struct TierInput<'a> {
    pub kind: AppKind,
    pub exe_path: Option<&'a str>,
    pub aumid: Option<&'a str>,
    pub has_command_line: bool,
    pub command_line_redacted: bool,
    pub elevated: bool,
    pub never_restore: bool,
}

pub fn assign_tier(i: &TierInput) -> Tier {
    if i.never_restore || i.elevated {
        // Never launch elevated. An always-running agent that elevates other processes
        // based on the contents of a writable database would be a standing privilege
        // escalation primitive (ADR-0001).
        return Tier::D;
    }
    match i.kind {
        AppKind::Uwp => {
            if i.aumid.is_some() {
                Tier::B
            } else {
                Tier::D
            }
        }
        AppKind::Unknown => Tier::D,
        AppKind::Win32 => {
            let Some(path) = i.exe_path else {
                return Tier::D;
            };
            if path.is_empty() {
                return Tier::D;
            }
            if i.has_command_line && !i.command_line_redacted {
                Tier::A
            } else {
                Tier::B
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folds_known_prefixes() {
        std::env::set_var("ProgramFiles", r"C:\Program Files");
        assert_eq!(
            fold_env(r"C:\Program Files\App\app.exe"),
            r"%ProgramFiles%\App\app.exe"
        );
    }

    #[test]
    fn folding_is_case_insensitive_like_windows_paths() {
        std::env::set_var("ProgramFiles", r"C:\Program Files");
        assert_eq!(
            fold_env(r"c:\PROGRAM FILES\App\app.exe"),
            r"%ProgramFiles%\App\app.exe"
        );
    }

    #[test]
    fn longest_prefix_wins() {
        // %LOCALAPPDATA% sits inside %USERPROFILE%; the more specific one must win or
        // two different apps could fold to the same key.
        std::env::set_var("USERPROFILE", r"C:\Users\A");
        std::env::set_var("LOCALAPPDATA", r"C:\Users\A\AppData\Local");
        assert_eq!(
            fold_env(r"C:\Users\A\AppData\Local\App\app.exe"),
            r"%LOCALAPPDATA%\App\app.exe"
        );
    }

    #[test]
    fn unknown_paths_are_left_alone() {
        assert_eq!(fold_env(r"D:\Games\thing.exe"), r"D:\Games\thing.exe");
    }

    #[test]
    fn app_key_is_stable_and_case_insensitive() {
        let a = app_key(AppKind::Win32, Some(r"C:\Apps\Thing.exe"), None);
        let b = app_key(AppKind::Win32, Some(r"c:\apps\thing.EXE"), None);
        assert_eq!(a, b);
        assert!(a.starts_with("win32:"));
    }

    #[test]
    fn app_key_differs_between_apps() {
        assert_ne!(
            app_key(AppKind::Win32, Some(r"C:\a.exe"), None),
            app_key(AppKind::Win32, Some(r"C:\b.exe"), None)
        );
    }

    #[test]
    fn packaged_apps_key_on_their_aumid() {
        let k = app_key(AppKind::Uwp, None, Some("Microsoft.WindowsCalculator_8wekyb3d8bbwe!App"));
        assert_eq!(k, "uwp:Microsoft.WindowsCalculator_8wekyb3d8bbwe!App");
    }

    #[test]
    fn browser_titles_are_never_stored() {
        // The regression this exists for: a browser window's title is the page title,
        // so a private window would leak its page title into the plaintext table.
        assert_eq!(
            storable_title(r"C:\Program Files\Mozilla Firefox\firefox.exe", "Secret - Private Browsing"),
            None
        );
        assert_eq!(
            storable_title(r"C:\...\msedge.exe", "Something - Profile 1 - Microsoft Edge"),
            None
        );
    }

    #[test]
    fn non_browser_titles_are_kept() {
        assert_eq!(
            storable_title(r"C:\Apps\Code.exe", "main.rs - project"),
            Some("main.rs - project".into())
        );
    }

    #[test]
    fn empty_titles_are_dropped() {
        assert_eq!(storable_title(r"C:\Apps\Code.exe", ""), None);
    }

    #[test]
    fn redacts_password_style_arguments() {
        let (out, redacted) = redact_command_line("app.exe --password=hunter2 --user=bob");
        assert!(redacted);
        assert!(!out.contains("hunter2"), "{out}");
        assert!(out.contains("--user=bob"), "non-secret args must survive: {out}");
    }

    #[test]
    fn redacts_several_key_spellings() {
        for arg in [
            "--token=abc123",
            "--api_key=abc123",
            "--client-secret=abc123",
            "/password:abc123",
            "access_token=abc123",
        ] {
            let (out, redacted) = redact_command_line(&format!("app.exe {arg}"));
            assert!(redacted, "not redacted: {arg}");
            assert!(!out.contains("abc123"), "leaked: {out}");
        }
    }

    #[test]
    fn redacts_credential_shaped_values() {
        for arg in [
            "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9",
            "ghp_0123456789abcdefghijklmnopqrstuvwx",
            "AKIAIOSFODNN7EXAMPLE",
        ] {
            let (out, redacted) = redact_command_line(&format!("app.exe {arg}"));
            assert!(redacted, "not redacted: {arg}");
            assert!(!out.contains(arg), "leaked: {out}");
        }
    }

    #[test]
    fn redacts_query_strings_but_keeps_the_page() {
        let (out, redacted) =
            redact_command_line("app.exe https://example.com/doc?sig=abc123&exp=999");
        assert!(redacted);
        assert!(out.contains("https://example.com/doc"));
        assert!(!out.contains("abc123"));
    }

    #[test]
    fn leaves_ordinary_command_lines_untouched() {
        let cmd = r#""C:\Apps\Code.exe" --new-window C:\src\project"#;
        let (out, redacted) = redact_command_line(cmd);
        assert!(!redacted);
        assert_eq!(out, cmd);
    }

    #[test]
    fn a_drive_letter_is_not_mistaken_for_a_secret() {
        let (out, redacted) = redact_command_line(r"app.exe C:\src\project");
        assert!(!redacted, "{out}");
    }

    #[test]
    fn quoted_arguments_stay_together() {
        let cmd = r#"app.exe "C:\Program Files\thing" --flag"#;
        let (out, _) = redact_command_line(cmd);
        assert_eq!(out, cmd);
    }

    #[test]
    fn arguments_are_kept_only_where_they_change_the_result() {
        assert!(arguments_matter(r"C:\...\chrome.exe"));
        assert!(arguments_matter(r"C:\...\Code.exe"));
        assert!(!arguments_matter(r"C:\...\spotify.exe"));
        assert!(!arguments_matter(r"C:\...\notepad.exe"));
    }

    #[test]
    fn known_noise_is_ignored() {
        assert!(is_ignored_exe(r"C:\Windows\System32\consent.exe"));
        assert!(is_ignored_exe(r"C:\x\sr-agent.exe"));
        assert!(!is_ignored_exe(r"C:\Apps\Code.exe"));
    }

    #[test]
    fn file_explorer_windows_stay_capturable() {
        // explorer.exe hosts both the desktop and folder windows. Ignoring the whole
        // process silently drops every File Explorer window, which is one of the more
        // obvious things a user expects back.
        assert!(!is_ignored_exe(r"C:\Windows\explorer.exe"));
    }

    #[test]
    fn the_shells_own_windows_are_ignored_by_class() {
        assert!(is_ignored_class("Progman"));
        assert!(is_ignored_class("Shell_TrayWnd"));
        assert!(is_ignored_class("WorkerW"));
        // A folder window is CabinetWClass, and must survive.
        assert!(!is_ignored_class("CabinetWClass"));
    }

    fn tier_of(kind: AppKind, exe: Option<&str>, cmd: bool, redacted: bool, elevated: bool) -> Tier {
        assign_tier(&TierInput {
            kind,
            exe_path: exe,
            aumid: Some("X!App"),
            has_command_line: cmd,
            command_line_redacted: redacted,
            elevated,
            never_restore: false,
        })
    }

    #[test]
    fn exact_restore_needs_a_clean_command_line() {
        assert_eq!(tier_of(AppKind::Win32, Some("a.exe"), true, false, false), Tier::A);
    }

    #[test]
    fn a_redacted_command_line_drops_to_launch_only() {
        // Restoring with a redaction sentinel on the command line would be worse than
        // launching plain.
        assert_eq!(tier_of(AppKind::Win32, Some("a.exe"), true, true, false), Tier::B);
    }

    #[test]
    fn a_missing_command_line_drops_to_launch_only() {
        assert_eq!(tier_of(AppKind::Win32, Some("a.exe"), false, false, false), Tier::B);
    }

    #[test]
    fn elevated_apps_are_never_restorable() {
        assert_eq!(tier_of(AppKind::Win32, Some("a.exe"), true, false, true), Tier::D);
    }

    #[test]
    fn packaged_apps_are_launch_only() {
        assert_eq!(tier_of(AppKind::Uwp, None, false, false, false), Tier::B);
    }

    #[test]
    fn a_packaged_app_without_an_aumid_cannot_be_restored() {
        assert_eq!(
            assign_tier(&TierInput {
                kind: AppKind::Uwp,
                exe_path: None,
                aumid: None,
                has_command_line: false,
                command_line_redacted: false,
                elevated: false,
                never_restore: false,
            }),
            Tier::D
        );
    }

    #[test]
    fn a_never_restore_rule_wins_over_everything() {
        assert_eq!(
            assign_tier(&TierInput {
                kind: AppKind::Win32,
                exe_path: Some("a.exe"),
                aumid: None,
                has_command_line: true,
                command_line_redacted: false,
                elevated: false,
                never_restore: true,
            }),
            Tier::D
        );
    }
}
