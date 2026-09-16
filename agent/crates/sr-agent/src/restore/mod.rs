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
            "SELECT url, title, tab_index, pinned, active, group_key
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
            let plain = unseal(&dek, &aad(snapshot_id, &tab_key, key_id), &nonce, &ciphertext)?;
            let payload: crate::ingest::PrivatePayload = serde_json::from_slice(&plain)?;
            tabs.push(RestoreTab {
                url: payload.url,
                title: payload.title,
                index,
                pinned: payload.pinned,
                active: payload.active,
                group_key: None,
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

/// Opens a restore run, recording the undo point.
pub fn begin_run(db: &Db, snapshot_id: i64, mode: &str) -> Result<i64> {
    // Snapshot the current session first, so a restore is always undoable
    // (docs/04-restore.md). Failing to create one is not fatal - there may be nothing
    // open yet - but it is recorded as absent rather than silently assumed.
    let undo = crate::store::snapshot::create_from_live(db, "pre_restore", Some("before restore"))?;

    db.conn.execute(
        "INSERT INTO restore_runs (snapshot_id, started_at, mode, undo_snapshot_id)
         VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![snapshot_id, sr_proto::now_millis(), mode, undo],
    )?;
    Ok(db.conn.last_insert_rowid())
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
