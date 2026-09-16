//! Snapshots: immutable point-in-time copies of the live session.
//!
//! Taking one is `INSERT INTO ... SELECT ... WHERE snapshot_id = 0` with a new id, so
//! "restore what was open" and "restore last Tuesday" are the same query against the
//! same tables (docs/02-data-model.md).
//!
//! **Why this is load-bearing for restore, not just for history.**
//!
//! On the next logon the live rows still describe the previous session - that is the
//! whole point. But the browser comes back with new window ids, and its first
//! reconcile is authoritative, so it reaps every row belonging to windows that no
//! longer exist. That reap is correct, and it would delete the session we are about to
//! restore, moments before we restore it.
//!
//! So the agent snapshots the live state at startup, *before any extension can
//! connect*, and restores from the snapshot. The live rows are then free to track the
//! new session.

use super::db::{Db, LIVE};
use anyhow::Result;
use rusqlite::OptionalExtension;

/// Tables that carry a `snapshot_id` and are copied wholesale.
const COPIED_TABLES: &[(&str, &str)] = &[
    (
        "browser_windows",
        "browser_window_id, browser, profile_key, is_private, window_state, x, y, w, h, focused, updated_at",
    ),
    (
        "tabs",
        "tab_key, browser_window_id, group_key, tab_index, url, title, favicon_hash, pinned, active, muted, restorable, last_accessed, updated_at",
    ),
    (
        "tabs_private",
        "tab_key, browser_window_id, tab_index, nonce, ciphertext, key_id, expires_at, updated_at",
    ),
    (
        "tab_groups",
        "group_key, browser_window_id, title, color, collapsed",
    ),
    (
        "displays",
        "display_key, friendly_name, is_primary, bounds_x, bounds_y, bounds_w, bounds_h, work_x, work_y, work_w, work_h, dpi",
    ),
    (
        "apps",
        "app_key, kind, exe_path, aumid, display_name, icon_hash, command_line, working_dir, is_browser, restore_tier",
    ),
    (
        "windows",
        "window_key, app_key, title, display_key, norm_x, norm_y, norm_w, norm_h, show_cmd, z_order, virtual_desktop_id, is_browser, browser_window_id",
    ),
];

/// Copies the live state into a new immutable snapshot. Returns its id.
///
/// Returns `None` when there is nothing to copy, so callers do not accumulate empty
/// snapshots on every start.
pub fn create_from_live(db: &Db, kind: &str, label: Option<&str>) -> Result<Option<i64>> {
    let tab_count: i64 = db.conn.query_row(
        "SELECT (SELECT COUNT(*) FROM tabs WHERE snapshot_id = ?1)
              + (SELECT COUNT(*) FROM tabs_private WHERE snapshot_id = ?1)",
        [LIVE],
        |r| r.get(0),
    )?;
    let app_count: i64 = db
        .conn
        .query_row("SELECT COUNT(*) FROM apps WHERE snapshot_id = ?1", [LIVE], |r| {
            r.get(0)
        })?;

    if tab_count == 0 && app_count == 0 {
        return Ok(None);
    }

    let machine_id = db.meta("machine_id")?.unwrap_or_else(|| "unknown".into());
    db.conn.execute(
        "INSERT INTO snapshots (captured_at, kind, label, machine_id, app_count, tab_count)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            sr_proto::now_millis(),
            kind,
            label,
            machine_id,
            app_count,
            tab_count
        ],
    )?;
    let id = db.conn.last_insert_rowid();

    for (table, cols) in COPIED_TABLES {
        db.conn.execute(
            &format!(
                "INSERT INTO {table} (snapshot_id, {cols})
                 SELECT ?1, {cols} FROM {table} WHERE snapshot_id = ?2"
            ),
            rusqlite::params![id, LIVE],
        )?;
    }

    Ok(Some(id))
}

/// The most recent snapshot that is a plausible restore candidate.
pub fn newest_restorable(db: &Db) -> Result<Option<i64>> {
    Ok(db
        .conn
        .query_row(
            "SELECT id FROM snapshots
             WHERE id != ?1 AND kind IN ('shutdown','periodic','manual')
             ORDER BY captured_at DESC LIMIT 1",
            [LIVE],
            |r| r.get(0),
        )
        .optional()?)
}

pub struct SnapshotInfo {
    pub id: i64,
    pub captured_at: i64,
    pub kind: String,
    pub tab_count: i64,
    pub app_count: i64,
}

pub fn list(db: &Db, limit: i64) -> Result<Vec<SnapshotInfo>> {
    let mut stmt = db.conn.prepare(
        "SELECT id, captured_at, kind, tab_count, app_count FROM snapshots
         WHERE id != ?1 ORDER BY captured_at DESC LIMIT ?2",
    )?;
    let rows = stmt
        .query_map(rusqlite::params![LIVE, limit], |r| {
            Ok(SnapshotInfo {
                id: r.get(0)?,
                captured_at: r.get(1)?,
                kind: r.get(2)?,
                tab_count: r.get(3)?,
                app_count: r.get(4)?,
            })
        })?
        .filter_map(Result::ok)
        .collect();
    Ok(rows)
}

/// Deletes a snapshot and everything belonging to it.
pub fn delete(db: &Db, id: i64) -> Result<()> {
    if id == LIVE {
        anyhow::bail!("refusing to delete the live state");
    }
    for (table, _) in COPIED_TABLES {
        db.conn.execute(
            &format!("DELETE FROM {table} WHERE snapshot_id = ?1"),
            [id],
        )?;
    }
    db.conn.execute("DELETE FROM snapshots WHERE id = ?1", [id])?;
    Ok(())
}

/// Keeps the newest `keep` snapshots plus every pinned one.
pub fn prune(db: &Db, keep: i64) -> Result<usize> {
    let mut stmt = db.conn.prepare(
        "SELECT id FROM snapshots
         WHERE id != ?1 AND pinned = 0
         ORDER BY captured_at DESC
         LIMIT -1 OFFSET ?2",
    )?;
    let doomed: Vec<i64> = stmt
        .query_map(rusqlite::params![LIVE, keep], |r| r.get(0))?
        .filter_map(Result::ok)
        .collect();
    drop(stmt);

    for id in &doomed {
        delete(db, *id)?;
    }
    Ok(doomed.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::{ingest_tab, IngestCtx};
    use crate::store::keys::KeyManager;
    use sr_proto::{Op, TabDelta};

    fn tmpdir() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("sr-snap-{}", sr_proto::new_id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn tab(key: &str, url: &str) -> TabDelta {
        TabDelta {
            op: Op::Upsert,
            tab_key: key.into(),
            window_id: "w1".into(),
            group_key: None,
            index: Some(0),
            url: Some(url.into()),
            title: Some("t".into()),
            favicon_hash: None,
            pinned: false,
            active: false,
            muted: false,
            last_accessed: None,
            private: false,
            restorable: true,
        }
    }

    fn seed(db: &Db, keys: &KeyManager, n: usize) {
        db.conn
            .execute(
                "INSERT INTO browser_windows (snapshot_id, browser_window_id, browser,
                    profile_key, is_private, updated_at) VALUES (0,'w1','chrome','default',0,0)",
                [],
            )
            .unwrap();
        let ctx = IngestCtx::live(db, keys);
        for i in 0..n {
            ingest_tab(&tab(&format!("w1:t{i}"), &format!("https://x.test/{i}")), &ctx).unwrap();
        }
    }

    #[test]
    fn copies_live_state_into_a_snapshot() {
        let dir = tmpdir();
        let db = Db::open(&dir.join("s.db")).unwrap();
        let keys = KeyManager::new(&dir);
        seed(&db, &keys, 3);

        let id = create_from_live(&db, "shutdown", None).unwrap().unwrap();
        assert_ne!(id, LIVE);

        let n: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM tabs WHERE snapshot_id = ?1", [id], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_snapshot_survives_the_live_state_being_wiped() {
        // The property restore depends on: the browser's first reconcile reaps live
        // rows, and the snapshot must be untouched by that.
        let dir = tmpdir();
        let db = Db::open(&dir.join("s.db")).unwrap();
        let keys = KeyManager::new(&dir);
        seed(&db, &keys, 2);

        let id = create_from_live(&db, "shutdown", None).unwrap().unwrap();
        db.conn.execute("DELETE FROM tabs WHERE snapshot_id = 0", []).unwrap();

        let live: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM tabs WHERE snapshot_id = 0", [], |r| r.get(0))
            .unwrap();
        let snap: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM tabs WHERE snapshot_id = ?1", [id], |r| r.get(0))
            .unwrap();
        assert_eq!(live, 0);
        assert_eq!(snap, 2, "the snapshot was destroyed with the live state");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_live_state_produces_no_snapshot() {
        let dir = tmpdir();
        let db = Db::open(&dir.join("s.db")).unwrap();
        assert!(create_from_live(&db, "shutdown", None).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn newest_restorable_picks_the_latest() {
        let dir = tmpdir();
        let db = Db::open(&dir.join("s.db")).unwrap();
        let keys = KeyManager::new(&dir);
        seed(&db, &keys, 1);

        let first = create_from_live(&db, "shutdown", None).unwrap().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let second = create_from_live(&db, "shutdown", None).unwrap().unwrap();

        assert_ne!(first, second);
        assert_eq!(newest_restorable(&db).unwrap(), Some(second));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pre_restore_snapshots_are_not_offered_as_restore_candidates() {
        // Otherwise an undo point would look like something worth restoring.
        let dir = tmpdir();
        let db = Db::open(&dir.join("s.db")).unwrap();
        let keys = KeyManager::new(&dir);
        seed(&db, &keys, 1);

        create_from_live(&db, "pre_restore", None).unwrap().unwrap();
        assert_eq!(newest_restorable(&db).unwrap(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn delete_removes_rows_and_refuses_to_touch_live() {
        let dir = tmpdir();
        let db = Db::open(&dir.join("s.db")).unwrap();
        let keys = KeyManager::new(&dir);
        seed(&db, &keys, 2);

        let id = create_from_live(&db, "manual", None).unwrap().unwrap();
        delete(&db, id).unwrap();
        let n: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM tabs WHERE snapshot_id = ?1", [id], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
        assert!(delete(&db, LIVE).is_err());

        let live: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM tabs WHERE snapshot_id = 0", [], |r| r.get(0))
            .unwrap();
        assert_eq!(live, 2, "deleting a snapshot disturbed the live state");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_keeps_the_newest_and_drops_the_rest() {
        let dir = tmpdir();
        let db = Db::open(&dir.join("s.db")).unwrap();
        let keys = KeyManager::new(&dir);
        seed(&db, &keys, 1);

        for _ in 0..5 {
            create_from_live(&db, "periodic", None).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(3));
        }
        assert_eq!(prune(&db, 2).unwrap(), 3);
        assert_eq!(list(&db, 100).unwrap().len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_never_drops_a_pinned_snapshot() {
        let dir = tmpdir();
        let db = Db::open(&dir.join("s.db")).unwrap();
        let keys = KeyManager::new(&dir);
        seed(&db, &keys, 1);

        let keeper = create_from_live(&db, "manual", Some("important")).unwrap().unwrap();
        db.conn
            .execute("UPDATE snapshots SET pinned = 1 WHERE id = ?1", [keeper])
            .unwrap();
        for _ in 0..4 {
            create_from_live(&db, "periodic", None).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(3));
        }

        prune(&db, 1).unwrap();
        let ids: Vec<i64> = list(&db, 100).unwrap().iter().map(|s| s.id).collect();
        assert!(ids.contains(&keeper), "prune dropped a pinned snapshot");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
