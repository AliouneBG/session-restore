//! Application, window, and display capture.
//!
//! The browser half of the session comes from the extension; this is everything else.
//! See docs/03-capture.md.

pub mod displays;
pub mod documents;
pub mod events;
pub mod identity;
pub mod processes;
pub mod windows;

use crate::store::db::{Db, LIVE};
use anyhow::Result;
use identity::{assign_tier, TierInput};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Default, Clone, Copy)]
pub struct CaptureStats {
    pub apps: usize,
    pub windows: usize,
    pub displays: usize,
}

/// Enumerates the desktop and writes it into the live state.
///
/// Authoritative: apps and windows absent from this pass are removed, because unlike
/// the browser there is no event stream telling us a window closed. Browser windows
/// are deliberately left alone - the extension owns those rows.
/// The documents each application has open, keyed by `app_key`.
///
/// Collected across ALL of an application's windows. Taking them from whichever window
/// happened to come first meant an app with three windows recorded documents only if
/// the first one had a resolvable title - Notepad with three notes open would record
/// none.
pub fn documents_by_app(
    found: &[windows::CapturedWindow],
    recent: &HashMap<String, std::path::PathBuf>,
) -> HashMap<String, Vec<String>> {
    let mut docs_by_app: HashMap<String, Vec<String>> = HashMap::new();
    for w in found {
        if w.is_browser {
            continue;
        }
        let p = &w.process;
        let key = identity::app_key(p.kind, p.exe_path.as_deref(), p.aumid.as_deref());
        let entry = docs_by_app.entry(key).or_default();

        for d in &p.documents {
            if !entry.contains(d) {
                entry.push(d.clone());
            }
        }
        if let Some(title) = w.title.as_deref() {
            if let Some(path) = documents::resolve_from_title(title, recent) {
                let folded = identity::fold_env(&path.display().to_string());
                if !entry.contains(&folded) {
                    entry.push(folded);
                }
            }
        }
    }
    docs_by_app
}

/// What is open right now: every application with a window, and the documents it has.
///
/// The restore path asks this to avoid starting something that is already running, and
/// to tell which of an application's documents are *missing* rather than assuming all
/// or none of them are. Derived the same way a capture derives them, deliberately: if
/// the two disagreed, a restore would reopen a document the user already has open.
pub fn open_now() -> HashMap<String, Vec<String>> {
    let Ok(found) = windows::enumerate() else {
        return HashMap::new();
    };
    let recent = documents::recent_index();
    let mut out = documents_by_app(&found, &recent);

    // An application with no documents still counts as open.
    for w in &found {
        if w.is_browser {
            continue;
        }
        let p = &w.process;
        out.entry(identity::app_key(
            p.kind,
            p.exe_path.as_deref(),
            p.aumid.as_deref(),
        ))
        .or_default();
    }
    out
}

pub fn capture_into_live(db: &Db) -> Result<CaptureStats> {
    let found = windows::enumerate()?;
    let monitors = displays::enumerate()?;

    // Built once per pass rather than per window: it reads a few hundred shortcuts,
    // which is cheap once and wasteful forty times.
    let recent = documents::recent_index();
    let now = sr_proto::now_millis();

    let never_restore: HashSet<String> = {
        let mut stmt = db
            .conn
            .prepare("SELECT pattern FROM app_rules WHERE action = 'never_restore'")?;
        let rows: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .filter_map(Result::ok)
            .collect();
        rows.into_iter().collect()
    };

    // Displays first: windows reference them.
    db.conn
        .execute("DELETE FROM displays WHERE snapshot_id = ?1", [LIVE])?;
    for d in &monitors {
        db.conn.execute(
            "INSERT INTO displays (snapshot_id, display_key, friendly_name, is_primary,
                bounds_x, bounds_y, bounds_w, bounds_h, work_x, work_y, work_w, work_h, dpi)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
            rusqlite::params![
                LIVE,
                d.key,
                d.friendly_name,
                d.is_primary,
                d.bounds.0,
                d.bounds.1,
                d.bounds.2,
                d.bounds.3,
                d.work.0,
                d.work.1,
                d.work.2,
                d.work.3,
                d.dpi
            ],
        )?;
    }

    let docs_by_app = documents_by_app(&found, &recent);

    let mut seen_apps: HashSet<String> = HashSet::new();
    let mut seen_windows: HashSet<String> = HashSet::new();
    let mut stats = CaptureStats {
        displays: monitors.len(),
        ..Default::default()
    };

    for w in &found {
        let p = &w.process;
        let exe = p.exe_path.as_deref();
        let key = identity::app_key(p.kind, exe, p.aumid.as_deref());

        // Gathered in the pass above, across every window this app owns.
        let empty: Vec<String> = Vec::new();
        let documents = docs_by_app.get(&key).unwrap_or(&empty);

        let tier = assign_tier(&TierInput {
            kind: p.kind,
            exe_path: exe,
            aumid: p.aumid.as_deref(),
            has_command_line: p.command_line.is_some(),
            command_line_redacted: p.command_line_redacted,
            elevated: p.elevated,
            never_restore: exe.map(|e| never_restore.contains(e)).unwrap_or(false),
            has_documents: !documents.is_empty(),
        });

        if seen_apps.insert(key.clone()) {
            db.conn.execute(
                "INSERT INTO apps (snapshot_id, app_key, kind, exe_path, aumid, display_name,
                    command_line, documents, is_browser, restore_tier)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
                 ON CONFLICT(snapshot_id, app_key) DO UPDATE SET
                   exe_path = excluded.exe_path, aumid = excluded.aumid,
                   display_name = excluded.display_name,
                   command_line = excluded.command_line,
                   documents = excluded.documents,
                   is_browser = excluded.is_browser,
                   restore_tier = excluded.restore_tier",
                rusqlite::params![
                    LIVE,
                    key,
                    p.kind.as_str(),
                    // Stored env-folded so the row survives a drive-letter change.
                    exe.map(identity::fold_env),
                    p.aumid,
                    exe.and_then(|e| std::path::Path::new(e)
                        .file_stem()
                        .map(|s| s.to_string_lossy().to_string())),
                    p.command_line,
                    if documents.is_empty() {
                        None
                    } else {
                        serde_json::to_string(documents).ok()
                    },
                    w.is_browser,
                    tier.as_str(),
                ],
            )?;
            stats.apps += 1;
        }

        // hwnd is not stable across restarts, but within a capture it uniquely
        // identifies the window, and restore matches on app identity rather than this.
        let window_key = format!("hwnd:{}", w.hwnd);
        seen_windows.insert(window_key.clone());

        db.conn.execute(
            "INSERT INTO windows (snapshot_id, window_key, app_key, title, display_key,
                norm_x, norm_y, norm_w, norm_h, show_cmd, z_order, is_browser)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)
             ON CONFLICT(snapshot_id, window_key) DO UPDATE SET
               app_key = excluded.app_key, title = excluded.title,
               display_key = excluded.display_key,
               norm_x = excluded.norm_x, norm_y = excluded.norm_y,
               norm_w = excluded.norm_w, norm_h = excluded.norm_h,
               show_cmd = excluded.show_cmd, z_order = excluded.z_order,
               is_browser = excluded.is_browser",
            rusqlite::params![
                LIVE,
                window_key,
                key,
                // NULL for browsers, enforced in identity::storable_title.
                w.title,
                displays::display_for_rect(&monitors, w.norm),
                w.norm.0,
                w.norm.1,
                w.norm.2,
                w.norm.3,
                w.show_cmd,
                w.z_order,
                w.is_browser,
            ],
        )?;
        stats.windows += 1;
    }

    reap(db, &seen_apps, &seen_windows)?;
    let _ = now;
    Ok(stats)
}

/// Removes apps and windows that are no longer open.
fn reap(db: &Db, seen_apps: &HashSet<String>, seen_windows: &HashSet<String>) -> Result<()> {
    let mut stmt = db
        .conn
        .prepare("SELECT window_key FROM windows WHERE snapshot_id = ?1")?;
    let stored: Vec<String> = stmt
        .query_map([LIVE], |r| r.get(0))?
        .filter_map(Result::ok)
        .collect();
    drop(stmt);
    for k in stored {
        if !seen_windows.contains(&k) {
            db.conn.execute(
                "DELETE FROM windows WHERE snapshot_id = ?1 AND window_key = ?2",
                rusqlite::params![LIVE, k],
            )?;
        }
    }

    let mut stmt = db
        .conn
        .prepare("SELECT app_key FROM apps WHERE snapshot_id = ?1")?;
    let stored: Vec<String> = stmt
        .query_map([LIVE], |r| r.get(0))?
        .filter_map(Result::ok)
        .collect();
    drop(stmt);
    for k in stored {
        if !seen_apps.contains(&k) {
            db.conn.execute(
                "DELETE FROM apps WHERE snapshot_id = ?1 AND app_key = ?2",
                rusqlite::params![LIVE, k],
            )?;
        }
    }
    Ok(())
}
