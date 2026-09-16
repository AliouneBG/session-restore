//! The restore review window.
//!
//! `restore_mode = ask` is the default and should stay the default, which means there
//! has to be something to ask with. This is it: a WebView2 window listing what would
//! be restored, with per-item checkboxes and the honest fidelity tier.
//!
//! **Private windows are handled differently from everything else here, on purpose.**
//! Their URLs are *not* in the payload the page receives. The page is told only how
//! many private tabs exist and when they expire; the URLs are decrypted and sent only
//! after the user explicitly asks to see them. Someone screen-sharing after a reboot
//! should not have their private tabs rendered on screen by default, and this is the
//! layer where that is actually prevented (ADR-0004).

use crate::store::db::Db;
use crate::store::keys::KeyManager;
use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize)]
pub struct ReviewApp {
    pub app_key: String,
    pub name: String,
    pub tier: String,
    /// Plain-language explanation of what the tier means for this app.
    pub tier_note: String,
    pub windows: usize,
    pub restorable: bool,
    /// File names this app had open, so the row says *which* documents come back
    /// rather than just "3 windows".
    pub documents: Vec<String>,
    /// Windows with nothing identifiable open - an unsaved note, a blank editor.
    ///
    /// Counted rather than named. The title of an unsaved document is its *content*
    /// (Windows 11 Notepad puts the first line there), so there is nothing safe to
    /// show, and "2 unsaved" is the honest description.
    pub untitled_windows: usize,
}

#[derive(Debug, Serialize)]
pub struct ReviewTab {
    pub url: String,
    pub title: String,
    /// Stable within this review, so the page can name exactly which tabs to keep.
    pub tab_key: String,
}

#[derive(Debug, Serialize)]
pub struct ReviewBrowserWindow {
    pub window_id: String,
    pub browser: String,
    pub profile: String,
    pub tabs: Vec<ReviewTab>,
}

/// What the page is allowed to know about private windows before the user asks.
///
/// Counts and an expiry, never a URL or a title.
#[derive(Debug, Serialize)]
pub struct PrivateSummary {
    pub windows: usize,
    pub tabs: usize,
    pub expires_in_hours: i64,
    pub available: bool,
}

#[derive(Debug, Serialize)]
pub struct ReviewData {
    pub snapshot_id: i64,
    pub captured_at: i64,
    pub captured_label: String,
    pub apps: Vec<ReviewApp>,
    pub browser_windows: Vec<ReviewBrowserWindow>,
    pub private: PrivateSummary,
}

/// What the user chose.
#[derive(Debug, Default, Deserialize)]
pub struct ReviewChoice {
    #[serde(default)]
    pub apps: Vec<String>,
    #[serde(default)]
    pub browser_windows: Vec<String>,
    /// Individual tabs the user kept ticked. Empty means "every tab in the selected
    /// windows", which is what an untouched review means.
    #[serde(default)]
    pub tabs: Vec<String>,
    #[serde(default)]
    pub restore_private: bool,
    #[serde(default)]
    pub never_ask_again: bool,
    #[serde(default)]
    pub confirmed: bool,
}

fn tier_note(tier: &str, kind: &str) -> String {
    match tier {
        "A" => "exact".into(),
        "B" => {
            if kind == "uwp" {
                "launch only".into()
            } else {
                "launch only - no saved arguments".into()
            }
        }
        "C" => "reopens the document".into(),
        _ => "needs admin - cannot restore".into(),
    }
}

/// Gathers everything the review window shows.
pub fn collect(db: &Db, snapshot_id: i64) -> Result<ReviewData> {
    let captured_at: i64 = db
        .conn
        .query_row(
            "SELECT captured_at FROM snapshots WHERE id = ?1",
            [snapshot_id],
            |r| r.get(0),
        )
        .unwrap_or(0);

    let mut stmt = db.conn.prepare(
        "SELECT a.app_key, COALESCE(a.display_name, '?'), a.restore_tier, a.kind,
                (SELECT COUNT(*) FROM windows w
                 WHERE w.snapshot_id = a.snapshot_id AND w.app_key = a.app_key),
                a.documents,
                (SELECT COUNT(*) FROM windows w
                 WHERE w.snapshot_id = a.snapshot_id AND w.app_key = a.app_key
                   AND w.title IS NULL)
         FROM apps a
         WHERE a.snapshot_id = ?1 AND a.is_browser = 0
         ORDER BY a.restore_tier, a.display_name",
    )?;
    let apps: Vec<ReviewApp> = stmt
        .query_map([snapshot_id], |r| {
            let tier: String = r.get(2)?;
            let kind: String = r.get(3)?;
            let documents: Vec<String> = r
                .get::<_, Option<String>>(5)?
                .and_then(|j| serde_json::from_str::<Vec<String>>(&j).ok())
                .unwrap_or_default()
                .iter()
                .filter_map(|p| {
                    std::path::Path::new(p)
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                })
                .collect();

            Ok(ReviewApp {
                app_key: r.get(0)?,
                name: r.get(1)?,
                tier_note: tier_note(&tier, &kind),
                restorable: tier != "D",
                tier,
                windows: r.get::<_, i64>(4)? as usize,
                documents,
                untitled_windows: r.get::<_, i64>(6)? as usize,
            })
        })?
        .filter_map(Result::ok)
        .collect();
    drop(stmt);

    let mut stmt = db.conn.prepare(
        "SELECT browser_window_id, browser, profile_key
         FROM browser_windows
         WHERE snapshot_id = ?1 AND is_private = 0",
    )?;
    let windows: Vec<(String, String, String)> = stmt
        .query_map([snapshot_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .filter_map(Result::ok)
        .collect();
    drop(stmt);

    let mut browser_windows = Vec::new();
    for (window_id, browser, profile) in windows {
        let mut ts = db.conn.prepare(
            "SELECT url, COALESCE(title, ''), tab_key FROM tabs
             WHERE snapshot_id = ?1 AND browser_window_id = ?2 AND restorable = 1
             ORDER BY tab_index",
        )?;
        let tabs: Vec<ReviewTab> = ts
            .query_map(rusqlite::params![snapshot_id, window_id], |r| {
                Ok(ReviewTab {
                    url: r.get(0)?,
                    title: r.get(1)?,
                    tab_key: r.get(2)?,
                })
            })?
            .filter_map(Result::ok)
            .collect();
        drop(ts);
        if tabs.is_empty() {
            continue;
        }
        browser_windows.push(ReviewBrowserWindow {
            window_id,
            browser,
            profile,
            tabs,
        });
    }

    // Counts only. No URL, no title, no favicon - nothing that could be read off a
    // shared screen.
    let now = sr_proto::now_millis();
    let (private_tabs, private_windows, soonest_expiry): (i64, i64, i64) = db
        .conn
        .query_row(
            "SELECT COUNT(*), COUNT(DISTINCT browser_window_id), COALESCE(MIN(expires_at), 0)
             FROM tabs_private WHERE snapshot_id = ?1 AND expires_at > ?2",
            rusqlite::params![snapshot_id, now],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap_or((0, 0, 0));

    Ok(ReviewData {
        snapshot_id,
        captured_at,
        captured_label: humanize_age(now - captured_at),
        apps,
        browser_windows,
        private: PrivateSummary {
            windows: private_windows as usize,
            tabs: private_tabs as usize,
            expires_in_hours: if soonest_expiry > now {
                (soonest_expiry - now) / 3_600_000
            } else {
                0
            },
            available: private_tabs > 0 && db.setting_bool("capture_private_windows", false),
        },
    })
}

fn humanize_age(ms: i64) -> String {
    let mins = ms / 60_000;
    if mins < 2 {
        "just now".into()
    } else if mins < 60 {
        format!("{mins} minutes ago")
    } else if mins < 60 * 24 {
        let h = mins / 60;
        format!("{h} hour{} ago", if h == 1 { "" } else { "s" })
    } else {
        let d = mins / (60 * 24);
        format!("{d} day{} ago", if d == 1 { "" } else { "s" })
    }
}

/// Decrypts private tabs for display, only ever in response to an explicit request.
pub fn reveal_private(
    db: &Db,
    keys: &KeyManager,
    snapshot_id: i64,
) -> Result<Vec<ReviewBrowserWindow>> {
    use crate::store::crypto::{aad, open as unseal};

    if !db.setting_bool("capture_private_windows", false) {
        anyhow::bail!("private capture is disabled");
    }

    let now = sr_proto::now_millis();
    let mut stmt = db.conn.prepare(
        "SELECT browser_window_id, tab_key, nonce, ciphertext, key_id
         FROM tabs_private
         WHERE snapshot_id = ?1 AND expires_at > ?2
         ORDER BY browser_window_id, tab_index",
    )?;
    let rows: Vec<(String, String, Vec<u8>, Vec<u8>, i64)> = stmt
        .query_map(rusqlite::params![snapshot_id, now], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?
        .filter_map(Result::ok)
        .collect();
    drop(stmt);

    let mut by_window: std::collections::BTreeMap<String, Vec<ReviewTab>> = Default::default();
    for (window_id, tab_key, nonce, ciphertext, key_id) in rows {
        // One unreadable row must not hide the rest. A row sealed under a key that has
        // since been rotated out, or written by an older build, should cost its own
        // line and nothing more - previously any single failure aborted the whole
        // reveal and the window showed an empty list.
        let decoded = keys
            .dek_by_id(db, key_id)
            .and_then(|dek| unseal(&dek, &aad(&tab_key, key_id), &nonce, &ciphertext))
            .and_then(|plain| {
                serde_json::from_slice::<crate::ingest::PrivatePayload>(&plain)
                    .map_err(anyhow::Error::from)
            });

        match decoded {
            Ok(payload) => by_window.entry(window_id).or_default().push(ReviewTab {
                title: payload.title.unwrap_or_default(),
                url: payload.url,
                tab_key,
            }),
            Err(e) => tracing::warn!(error = %e, "a private tab could not be decrypted"),
        }
    }

    Ok(by_window
        .into_iter()
        .map(|(window_id, tabs)| ReviewBrowserWindow {
            window_id,
            browser: "private".into(),
            profile: "private".into(),
            tabs,
        })
        .collect())
}

pub const REVIEW_HTML: &str = include_str!("review.html");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_notes_are_plain_language() {
        // The review window says what a tier means before the user clicks, rather than
        // producing a confusing result afterwards.
        assert_eq!(tier_note("A", "win32"), "exact");
        assert!(tier_note("B", "win32").contains("launch only"));
        assert!(tier_note("D", "win32").contains("admin"));
    }

    #[test]
    fn packaged_apps_do_not_mention_missing_arguments() {
        // A Store app never had arguments to lose, so saying so would be noise.
        assert_eq!(tier_note("B", "uwp"), "launch only");
    }

    #[test]
    fn ages_read_naturally() {
        assert_eq!(humanize_age(30_000), "just now");
        assert_eq!(humanize_age(5 * 60_000), "5 minutes ago");
        assert_eq!(humanize_age(60 * 60_000), "1 hour ago");
        assert_eq!(humanize_age(3 * 60 * 60_000), "3 hours ago");
        assert_eq!(humanize_age(25 * 60 * 60_000), "1 day ago");
    }

    #[test]
    fn the_payload_carries_no_private_urls() {
        // The property this module exists to guarantee: PrivateSummary is counts only,
        // so there is no field a private URL could travel in.
        let s = PrivateSummary {
            windows: 2,
            tabs: 11,
            expires_in_hours: 4,
            available: true,
        };
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"tabs\":11"));
        assert!(!json.contains("url"));
        assert!(!json.contains("title"));
    }

    #[test]
    fn a_choice_defaults_to_restoring_nothing() {
        // An empty or malformed message from the page must not be read as consent,
        // least of all consent to restore private windows.
        let c: ReviewChoice = serde_json::from_str("{}").unwrap();
        assert!(c.apps.is_empty());
        assert!(c.browser_windows.is_empty());
        assert!(!c.restore_private);
        assert!(!c.confirmed);
        assert!(!c.never_ask_again);
    }

    #[test]
    fn private_restore_must_be_asked_for_explicitly() {
        let c: ReviewChoice =
            serde_json::from_str(r#"{"apps":["a"],"confirmed":true}"#).unwrap();
        assert!(c.confirmed);
        assert!(!c.restore_private, "private restore defaulted to on");
    }
}
