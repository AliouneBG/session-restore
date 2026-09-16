//! Restore orchestration.
//!
//! Two halves that deliberately work differently:
//!
//! - **Browsers** ([`build_payload`]): the agent says what the session *was* and the
//!   extension decides what is missing, because only it can see what the browser
//!   already reopened by itself.
//! - **Applications** ([`apps`]): the agent does the work itself, launching processes
//!   and placing windows, because nothing else can.

pub mod apps;
pub mod launch;
pub mod place;

use crate::store::crypto::{aad, open as unseal};
use crate::store::db::Db;
use crate::store::keys::KeyManager;
use anyhow::Result;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct RestoreTab {
    /// Not sent to the extension - used only to match a review selection.
    #[serde(skip)]
    pub tab_key: String,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub index: i64,
    pub pinned: bool,
    pub active: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group_key: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RestoreWindow {
    pub window_id: String,
    pub private: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub x: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub y: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub w: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub h: Option<i64>,
    pub tabs: Vec<RestoreTab>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RestoreSessionBody {
    pub run_id: i64,
    pub lazy: bool,
    pub windows: Vec<RestoreWindow>,
}

/// Builds the payload for one browser and profile from a snapshot.
///
/// Private windows are **excluded** here and offered only through
/// [`build_private_payload`], which has its own gates. Bundling them would make them
/// part of any automatic restore, which ADR-0004 forbids outright.
pub fn build_payload(
    db: &Db,
    snapshot_id: i64,
    browser: &str,
    profile: &str,
    run_id: i64,
) -> Result<RestoreSessionBody> {
    let mut stmt = db.conn.prepare(
        "SELECT browser_window_id, window_state, x, y, w, h
         FROM browser_windows
         WHERE snapshot_id = ?1 AND browser = ?2 AND profile_key = ?3 AND is_private = 0",
    )?;
    let windows: Vec<(String, Option<String>, Option<i64>, Option<i64>, Option<i64>, Option<i64>)> =
        stmt.query_map(rusqlite::params![snapshot_id, browser, profile], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
        })?
        .filter_map(Result::ok)
        .collect();
    drop(stmt);

    let mut out = Vec::new();
    for (window_id, state, x, y, w, h) in windows {
        let mut ts = db.conn.prepare(
            "SELECT url, title, tab_index, pinned, active, group_key, tab_key
             FROM tabs
             WHERE snapshot_id = ?1 AND browser_window_id = ?2 AND restorable = 1
             ORDER BY tab_index",
        )?;
        let tabs: Vec<RestoreTab> = ts
            .query_map(rusqlite::params![snapshot_id, window_id], |r| {
                Ok(RestoreTab {
                    url: r.get(0)?,
                    title: r.get(1)?,
                    index: r.get(2)?,
                    pinned: r.get::<_, i64>(3)? != 0,
                    active: r.get::<_, i64>(4)? != 0,
                    group_key: r.get(5)?,
                    tab_key: r.get(6)?,
                })
            })?
            .filter_map(Result::ok)
            .collect();
        drop(ts);

        if tabs.is_empty() {
            continue;
        }
        out.push(RestoreWindow {
            window_id,
            private: false,
            state,
            x,
            y,
            w,
            h,
            tabs,
        });
    }

    Ok(RestoreSessionBody {
        run_id,
        lazy: true,
        windows: out,
    })
}

/// Builds the private-window payload, decrypting immediately before sending.
///
/// Never called as part of an automatic restore. The caller must have an explicit,
/// current user confirmation; the plaintext URLs then exist only in the resulting
/// message, for the duration of the restore (ADR-0004).
pub fn build_private_payload(
    db: &Db,
    keys: &KeyManager,
    snapshot_id: i64,
    browser: &str,
    profile: &str,
    run_id: i64,
) -> Result<RestoreSessionBody> {
    if !db.setting_bool("capture_private_windows", false) {
        anyhow::bail!("private capture is disabled");
    }

    let mut stmt = db.conn.prepare(
        "SELECT browser_window_id, window_state, x, y, w, h
         FROM browser_windows
         WHERE snapshot_id = ?1 AND browser = ?2 AND profile_key = ?3 AND is_private = 1",
    )?;
    let windows: Vec<(String, Option<String>, Option<i64>, Option<i64>, Option<i64>, Option<i64>)> =
        stmt.query_map(rusqlite::params![snapshot_id, browser, profile], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
        })?
        .filter_map(Result::ok)
        .collect();
    drop(stmt);

    let now = sr_proto::now_millis();
    let mut out = Vec::new();

    for (window_id, state, x, y, w, h) in windows {
        let mut ts = db.conn.prepare(
            "SELECT tab_key, tab_index, nonce, ciphertext, key_id, expires_at
             FROM tabs_private
             WHERE snapshot_id = ?1 AND browser_window_id = ?2
             ORDER BY tab_index",
        )?;
        let rows: Vec<(String, i64, Vec<u8>, Vec<u8>, i64, i64)> = ts
            .query_map(rusqlite::params![snapshot_id, window_id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
            })?
            .filter_map(Result::ok)
            .collect();
        drop(ts);

        let mut tabs = Vec::new();
        for (tab_key, index, nonce, ciphertext, key_id, expires_at) in rows {
            // An expired row is not restorable, whatever the sweeper has got round to.
            if expires_at < now {
                continue;
            }
            let dek = keys.dek_by_id(db, key_id)?;
            let plain = unseal(&dek, &aad(&tab_key, key_id), &nonce, &ciphertext)?;
            let payload: crate::ingest::PrivatePayload = serde_json::from_slice(&plain)?;
            tabs.push(RestoreTab {
                url: payload.url,
                title: payload.title,
                index,
                pinned: payload.pinned,
                active: payload.active,
                group_key: None,
                tab_key: tab_key.clone(),
            });
        }

        if tabs.is_empty() {
            continue;
        }
        out.push(RestoreWindow {
            window_id,
            private: true,
            state,
            x,
            y,
            w,
            h,
            tabs,
        });
    }

    Ok(RestoreSessionBody {
        run_id,
        lazy: true,
        windows: out,
    })
}

/// A browser the restore needs running, and how to start it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserLaunch {
    /// The id the extension reports in `hello`, e.g. `edge`.
    pub browser: String,
    pub exe_path: String,
    /// The command line it was captured with, when one is usable.
    ///
    /// This is what restores the *profile*. A browser started from its bare executable
    /// opens whichever profile it opens by default, so a session captured in a second
    /// profile came back in the first one - and the offer, which is keyed by profile,
    /// then went unclaimed. The profile argument lives on the command line, and
    /// `profile_key` cannot stand in for it: it is a hash, deliberately, because a
    /// profile path can contain the user's name.
    pub command_line: Option<String>,
}

/// True when a redacted command line must not be replayed.
///
/// A redacted argument is stored as a length-preserving sentinel so restore can tell
/// "no arguments" from "arguments we refused to keep". Passing the sentinel to the
/// browser would be passing it a literal `<redacted:24>`.
fn is_replayable(command_line: &str) -> bool {
    !command_line.contains("<redacted:")
}

/// The browser id the extension will report for an executable we captured.
///
/// Only the three browsers with a registered native messaging host
/// ([`crate::setup::BROWSERS`]) can produce an offer to wait for, so launching
/// anything else would open a browser and restore nothing into it.
fn browser_id_for_exe(exe_path: &str) -> Option<&'static str> {
    let name = std::path::Path::new(exe_path)
        .file_name()?
        .to_string_lossy()
        .to_lowercase();
    Some(match name.as_str() {
        "chrome.exe" => "chrome",
        "msedge.exe" => "edge",
        "firefox.exe" => "firefox",
        _ => return None,
    })
}

/// Browsers a confirmed restore needs, that are not already running.
///
/// Without this the browser half of a restore was conditional on the user, because a
/// restore offer waits in `pending_restore` for an extension to connect and nothing
/// makes one connect. After a reboot no browser is running, which is the *only* case
/// that matters, so ticking a browser window in the review window meant "restore these
/// tabs the next time you happen to open Edge" - a promise the user had no reason to
/// read that way, and no way to tell had not been kept.
///
/// "Already running" is read from live state rather than by enumerating processes,
/// because the caller captures the desktop immediately before asking. A browser
/// running with no window does not count: it cannot show the user anything, and
/// starting it is what they asked for.
pub fn browsers_to_launch(
    db: &Db,
    snapshot_id: i64,
    wanted_windows: &std::collections::HashSet<String>,
) -> Result<Vec<BrowserLaunch>> {
    let mut stmt = db.conn.prepare(
        "SELECT DISTINCT browser, browser_window_id FROM browser_windows
         WHERE snapshot_id = ?1 AND is_private = 0",
    )?;
    let wanted: std::collections::HashSet<String> = stmt
        .query_map([snapshot_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?
        .filter_map(Result::ok)
        .filter(|(_, window_id)| wanted_windows.contains(window_id))
        .map(|(browser, _)| browser)
        .collect();
    drop(stmt);

    if wanted.is_empty() {
        return Ok(Vec::new());
    }

    // What is on screen right now, by browser id.
    let mut stmt = db.conn.prepare(
        "SELECT exe_path FROM apps WHERE snapshot_id = ?1 AND is_browser = 1
           AND exe_path IS NOT NULL",
    )?;
    let running: std::collections::HashSet<String> = stmt
        .query_map([crate::store::db::LIVE], |r| r.get::<_, String>(0))?
        .filter_map(Result::ok)
        .filter_map(|p| browser_id_for_exe(&p).map(str::to_string))
        .collect();

    drop(stmt);

    // Where each browser lives and how it was started, from the snapshot that recorded it.
    let mut stmt = db.conn.prepare(
        "SELECT exe_path, command_line FROM apps
         WHERE snapshot_id = ?1 AND is_browser = 1 AND exe_path IS NOT NULL",
    )?;
    let rows: Vec<(String, Option<String>)> = stmt
        .query_map([snapshot_id], |r| Ok((r.get(0)?, r.get(1)?)))?
        .filter_map(Result::ok)
        .collect();
    drop(stmt);

    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (exe_path, command_line) in rows {
        let Some(browser) = browser_id_for_exe(&exe_path) else {
            continue;
        };
        if !wanted.contains(browser) || running.contains(browser) {
            continue;
        }
        if !seen.insert(browser.to_string()) {
            continue;
        }
        out.push(BrowserLaunch {
            browser: browser.to_string(),
            exe_path,
            command_line: command_line.filter(|c| is_replayable(c)),
        });
    }
    Ok(out)
}

/// How long after a restore begins that further runs count as the same episode.
///
/// One restore is several runs: the applications, then each browser as it connects.
/// They share an undo point (see [`begin_run`]), and this bounds "share" to the span
/// an episode actually takes - a browser the restore itself launched, connecting a few
/// seconds later, not one the user opens twenty minutes afterwards.
const UNDO_EPISODE_MILLIS: i64 = 5 * 60 * 1000;

/// Opens a restore run, recording the undo point.
///
/// Runs that belong to the same restore **share one undo point**, and that sharing is
/// the whole reason this is not just an INSERT. A restore is not one call: the review
/// window restores the applications, then every browser calls this again as it
/// connects. Taking a fresh `pre_restore` snapshot each time meant the newest one -
/// the one `--undo` reaches for - was captured *after* the applications had already
/// been launched, so undoing returned you to the state the restore had just created.
/// Undo was a no-op in exactly the case it exists for.
pub fn begin_run(db: &Db, snapshot_id: i64, mode: &str) -> Result<i64> {
    // Snapshot the current session first, so a restore is always undoable
    // (docs/04-restore.md). Failing to create one is not fatal - there may be nothing
    // open yet - but it is recorded as absent rather than silently assumed.
    let recent: Option<i64> = db
        .conn
        .query_row(
            "SELECT undo_snapshot_id FROM restore_runs
             WHERE snapshot_id = ?1 AND undo_snapshot_id IS NOT NULL
               AND started_at >= ?2
             ORDER BY started_at DESC LIMIT 1",
            rusqlite::params![snapshot_id, sr_proto::now_millis() - UNDO_EPISODE_MILLIS],
            |r| r.get(0),
        )
        .ok();

    let undo = match recent {
        Some(id) => Some(id),
        None => {
            crate::store::snapshot::create_from_live(db, "pre_restore", Some("before restore"))?
        }
    };

    db.conn.execute(
        "INSERT INTO restore_runs (snapshot_id, started_at, mode, undo_snapshot_id)
         VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![snapshot_id, sr_proto::now_millis(), mode, undo],
    )?;
    Ok(db.conn.last_insert_rowid())
}

/// Records what an application restore actually did, and closes the run.
///
/// The counterpart to [`apply_result`] for the half of a restore the agent performs
/// itself. Without it a run that launched applications finished with no `finished_at`
/// and no items, so "what did that restore do?" had only the log to answer it.
pub fn record_app_outcomes(db: &Db, run_id: i64, report: &apps::AppRestoreReport) -> Result<()> {
    let mut rows: Vec<(&str, &str, Option<&str>)> = Vec::new();
    for name in &report.launched {
        rows.push((name.as_str(), "launched", None));
    }
    for (name, why) in &report.skipped {
        rows.push((name.as_str(), "skipped", Some(why.as_str())));
    }
    for (name, why) in &report.failed {
        rows.push((name.as_str(), "failed", Some(why.as_str())));
    }

    for (key, status, detail) in rows {
        db.conn.execute(
            "INSERT OR REPLACE INTO restore_items (run_id, item_kind, item_key, status, detail)
             VALUES (?1, 'app', ?2, ?3, ?4)",
            rusqlite::params![run_id, key, status, detail],
        )?;
    }

    db.conn.execute(
        "UPDATE restore_runs SET finished_at = ?1 WHERE id = ?2",
        rusqlite::params![sr_proto::now_millis(), run_id],
    )?;
    Ok(())
}

pub fn record_items(db: &Db, run_id: i64, body: &RestoreSessionBody) -> Result<()> {
    for w in &body.windows {
        for t in &w.tabs {
            db.conn.execute(
                "INSERT OR REPLACE INTO restore_items (run_id, item_kind, item_key, status)
                 VALUES (?1, 'tab', ?2, 'pending')",
                rusqlite::params![run_id, t.url],
            )?;
        }
    }
    Ok(())
}

/// Applies the per-item outcomes the extension reported.
///
/// Failures are recorded, never swallowed: "7 of 9 restored, Figma is not installed"
/// is a good outcome; "some stuff came back" is not.
pub fn apply_result(db: &Db, run_id: i64, items: &serde_json::Value) -> Result<(usize, usize)> {
    let mut ok = 0usize;
    let mut failed = 0usize;

    if let Some(arr) = items.as_array() {
        for item in arr {
            let key = item.get("tab_key").and_then(|v| v.as_str()).unwrap_or("");
            let status = item.get("status").and_then(|v| v.as_str()).unwrap_or("failed");
            let detail = item.get("detail").and_then(|v| v.as_str());
            match status {
                "created" | "already_open" => ok += 1,
                _ => failed += 1,
            }
            let mapped = match status {
                "created" => "launched",
                "already_open" => "placed",
                "skipped" => "skipped",
                _ => "failed",
            };
            db.conn.execute(
                "INSERT OR REPLACE INTO restore_items (run_id, item_kind, item_key, status, detail)
                 VALUES (?1, 'tab', ?2, ?3, ?4)",
                rusqlite::params![run_id, key, mapped, detail],
            )?;
        }
    }

    db.conn.execute(
        "UPDATE restore_runs SET finished_at = ?1 WHERE id = ?2",
        rusqlite::params![sr_proto::now_millis(), run_id],
    )?;
    Ok((ok, failed))
}
