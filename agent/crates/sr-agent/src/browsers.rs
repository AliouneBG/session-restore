//! What each supported browser looks like on this machine, right now.
//!
//! Both the onboarding flow and the settings window need the same facts about a
//! browser, and none of them can be guessed from one source alone:
//!
//! - **Installed?** from the filesystem, so onboarding never tells someone to add an
//!   extension to a browser they do not have.
//! - **Connected?** from the agent's live connection list, because a registered native
//!   messaging host proves nothing about whether the extension is loaded and running.
//! - **Private windows allowed?** only the extension can see this, so it is whatever it
//!   last told us and was recorded in `browser_status`.
//! - **Where to send the user** to change any of the above.

use crate::store::db::Db;
use serde::Serialize;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize)]
pub struct BrowserInfo {
    /// The id the extension reports, e.g. `edge`.
    pub id: String,
    /// What to call it in the interface.
    pub label: String,
    pub installed: bool,
    /// The extension is connected to this agent right now.
    pub connected: bool,
    /// The browser has granted the extension access to private windows.
    pub private_allowed: bool,
    pub ext_version: Option<String>,
    /// The page the user has to visit to manage the extension.
    pub extensions_page: String,
    /// Plain-language instruction for granting private-window access.
    pub private_hint: String,
}

struct Known {
    id: &'static str,
    label: &'static str,
    /// Relative to the standard program directories, tried in order.
    exe_candidates: &'static [&'static str],
    extensions_page: &'static str,
    private_hint: &'static str,
}

const KNOWN: &[Known] = &[
    Known {
        id: "chrome",
        label: "Chrome",
        exe_candidates: &[r"Google\Chrome\Application\chrome.exe"],
        extensions_page: "chrome://extensions",
        private_hint: "Details, then turn on Allow in Incognito",
    },
    Known {
        id: "edge",
        label: "Edge",
        exe_candidates: &[r"Microsoft\Edge\Application\msedge.exe"],
        extensions_page: "edge://extensions",
        private_hint: "Details, then turn on Allow in InPrivate",
    },
    Known {
        id: "firefox",
        label: "Firefox",
        exe_candidates: &[r"Mozilla Firefox\firefox.exe"],
        extensions_page: "about:addons",
        private_hint: "Session Restore, then set Run in Private Windows to Allow",
    },
];

/// The directories a browser is normally installed under, most likely first.
fn program_roots() -> Vec<PathBuf> {
    ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"]
        .iter()
        .filter_map(|k| std::env::var(k).ok())
        .map(PathBuf::from)
        .collect()
}

/// The executable for a browser, if it is installed.
pub fn executable(id: &str) -> Option<PathBuf> {
    let known = KNOWN.iter().find(|k| k.id == id)?;
    for root in program_roots() {
        for rel in known.exe_candidates {
            let p = root.join(rel);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

/// Every supported browser, with its current state on this machine.
pub fn detect(db: &Db, connected: &[String]) -> Vec<BrowserInfo> {
    KNOWN
        .iter()
        .map(|k| {
            // Any profile having been granted access is enough to say the browser is
            // set up. The per-profile detail is not something to put in front of
            // someone who has one profile, which is almost everyone.
            let (private_allowed, ext_version) = db
                .conn
                .query_row(
                    "SELECT MAX(incognito_access), MAX(ext_version) FROM browser_status
                     WHERE browser = ?1",
                    [k.id],
                    |r| {
                        Ok((
                            r.get::<_, Option<i64>>(0)?.unwrap_or(0) != 0,
                            r.get::<_, Option<String>>(1)?,
                        ))
                    },
                )
                .unwrap_or((false, None));

            BrowserInfo {
                id: k.id.to_string(),
                label: k.label.to_string(),
                installed: executable(k.id).is_some(),
                connected: connected.iter().any(|c| c == k.id),
                private_allowed,
                ext_version,
                extensions_page: k.extensions_page.to_string(),
                private_hint: k.private_hint.to_string(),
            }
        })
        .collect()
}

/// Opens a browser on its own extensions page.
///
/// The page is a privileged URL, so it cannot be opened through the shell like a normal
/// link: it has to be handed to that specific browser as an argument. That is also what
/// makes it correct, since `chrome://extensions` means nothing anywhere but Chrome.
pub fn open_extensions_page(id: &str) -> anyhow::Result<()> {
    let Some(exe) = executable(id) else {
        anyhow::bail!("{id} does not appear to be installed");
    };
    let Some(known) = KNOWN.iter().find(|k| k.id == id) else {
        anyhow::bail!("unknown browser {id}");
    };

    std::process::Command::new(exe)
        .arg(known.extensions_page)
        .spawn()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_known_browser_has_somewhere_to_send_the_user() {
        for k in KNOWN {
            assert!(!k.extensions_page.is_empty(), "{} has no page", k.id);
            assert!(!k.private_hint.is_empty(), "{} has no hint", k.id);
            assert!(!k.exe_candidates.is_empty(), "{} has no executable", k.id);
        }
    }

    #[test]
    fn the_ids_match_what_the_extension_reports() {
        // These strings are the join key between this table, `browser_status`, and the
        // `browser` field of every message. A typo here is a silently empty interface.
        let ids: Vec<&str> = KNOWN.iter().map(|k| k.id).collect();
        assert_eq!(ids, vec!["chrome", "edge", "firefox"]);
    }

    #[test]
    fn an_unknown_browser_has_no_executable() {
        assert!(executable("netscape").is_none());
    }
}
