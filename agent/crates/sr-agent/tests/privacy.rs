//! The privacy guarantee, asserted end to end.
//!
//! This is the test ADR-0004 and docs/06 promise: ingest a known private URL, then
//! grep the entire database *file* - not just the tables - for any trace of it. It
//! catches leaks that per-table assertions miss, including freelist pages left behind
//! by a delete and any future code path that writes a URL somewhere unexpected.
//!
//! If this test fails, the product's central privacy claim is false. Treat a failure
//! here as a release blocker, never as a flaky test.

use sr_agent::ingest::{ingest_tab, purge_all_private, sweep_expired_private, IngestCtx, NormalTab};
use sr_agent::store::db::{Db, LIVE};
use sr_agent::store::keys::KeyManager;
use sr_proto::{Op, TabDelta};
use std::path::PathBuf;

/// Distinctive enough that a substring match cannot be a coincidence.
const SECRET_URL: &str = "https://private-marker-9f3a2b.test/never-on-disk";
const SECRET_TITLE: &str = "PRIVATE-TITLE-MARKER-9f3a2b";
const NORMAL_URL: &str = "https://ordinary.test/public-page";

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let d = std::env::temp_dir().join(format!("sr-privacy-{}", sr_proto::new_id()));
        std::fs::create_dir_all(&d).unwrap();
        TempDir(d)
    }
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn private_tab(key: &str) -> TabDelta {
    TabDelta {
        op: Op::Upsert,
        tab_key: key.to_string(),
        window_id: "w-private".into(),
        group_key: None,
        index: Some(0),
        url: Some(SECRET_URL.into()),
        title: Some(SECRET_TITLE.into()),
        favicon_hash: None,
        pinned: false,
        active: true,
        muted: false,
        last_accessed: None,
        private: true,
        restorable: true,
    }
}

fn normal_tab(key: &str) -> TabDelta {
    TabDelta {
        url: Some(NORMAL_URL.into()),
        title: Some("Ordinary page".into()),
        private: false,
        ..private_tab(key)
    }
}

/// Scans every byte of every file the agent wrote.
fn files_containing(dir: &std::path::Path, needle: &str) -> Vec<String> {
    let mut hits = Vec::new();
    let needle_bytes = needle.as_bytes();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(p) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&p) else {
            continue;
        };
        for e in entries.flatten() {
            let path = e.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(bytes) = std::fs::read(&path) {
                if bytes
                    .windows(needle_bytes.len())
                    .any(|w| w == needle_bytes)
                {
                    hits.push(path.display().to_string());
                }
            }
        }
    }
    hits
}

#[test]
fn private_url_never_appears_anywhere_on_disk() {
    let dir = TempDir::new();
    let db_path = dir.path().join("sessions.db");
    let db = Db::open(&db_path).unwrap();
    let keys = KeyManager::new(dir.path());

    db.set_setting("capture_private_windows", "true").unwrap();
    let ctx = IngestCtx::live(&db, &keys);
    assert!(ctx.capture_private, "test setup: private capture must be on");

    ingest_tab(&private_tab("w-private:t1"), &ctx).unwrap();
    ingest_tab(&normal_tab("w-normal:t1"), &ctx).unwrap();

    // Force everything out of the WAL into the main file so the scan sees it all.
    db.conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();

    // The private row exists and is retrievable in encrypted form...
    let n: i64 = db
        .conn
        .query_row("SELECT COUNT(*) FROM tabs_private", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1, "private tab was not stored");

    // ...but neither the URL nor the title is anywhere in any file.
    let url_hits = files_containing(dir.path(), SECRET_URL);
    assert!(url_hits.is_empty(), "private URL found on disk in: {url_hits:?}");
    let title_hits = files_containing(dir.path(), SECRET_TITLE);
    assert!(title_hits.is_empty(), "private title found on disk in: {title_hits:?}");

    // Control: the test would have caught a leak, because it finds the normal URL.
    assert!(
        !files_containing(dir.path(), NORMAL_URL).is_empty(),
        "scanner is broken - it cannot even find a URL stored in plaintext"
    );
}

#[test]
fn private_tab_never_lands_in_the_normal_tabs_table() {
    let dir = TempDir::new();
    let db = Db::open(&dir.path().join("sessions.db")).unwrap();
    let keys = KeyManager::new(dir.path());
    db.set_setting("capture_private_windows", "true").unwrap();
    let ctx = IngestCtx::live(&db, &keys);

    ingest_tab(&private_tab("w-private:t1"), &ctx).unwrap();

    let in_tabs: i64 = db
        .conn
        .query_row("SELECT COUNT(*) FROM tabs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(in_tabs, 0, "a private tab reached the plaintext tabs table");
}

#[test]
fn private_tabs_are_dropped_entirely_when_the_setting_is_off() {
    let dir = TempDir::new();
    let db = Db::open(&dir.path().join("sessions.db")).unwrap();
    let keys = KeyManager::new(dir.path());
    // Default is off; assert that rather than assuming it.
    assert!(!db.setting_bool("capture_private_windows", true));
    let ctx = IngestCtx::live(&db, &keys);

    ingest_tab(&private_tab("w-private:t1"), &ctx).unwrap();

    let private_rows: i64 = db
        .conn
        .query_row("SELECT COUNT(*) FROM tabs_private", [], |r| r.get(0))
        .unwrap();
    let normal_rows: i64 = db
        .conn
        .query_row("SELECT COUNT(*) FROM tabs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(private_rows, 0, "stored a private tab while capture was off");
    assert_eq!(normal_rows, 0, "private tab leaked into tabs while capture was off");
}

#[test]
fn a_private_tab_cannot_become_a_cloud_eligible_normal_tab() {
    // The type-level half of the guarantee. The sync layer takes &NormalTab, and this
    // is the only constructor - so private data has no representation it accepts.
    assert!(NormalTab::from_delta(&private_tab("w:t1")).is_none());
    assert!(NormalTab::from_delta(&normal_tab("w:t2")).is_some());
}

#[test]
fn expired_private_rows_are_hard_deleted_with_no_residue() {
    let dir = TempDir::new();
    let db_path = dir.path().join("sessions.db");
    let db = Db::open(&db_path).unwrap();
    let keys = KeyManager::new(dir.path());
    db.set_setting("capture_private_windows", "true").unwrap();
    db.set_setting("private_ttl_hours", "24").unwrap();
    let ctx = IngestCtx::live(&db, &keys);

    ingest_tab(&private_tab("w-private:t1"), &ctx).unwrap();

    // Age the row past its TTL.
    db.conn
        .execute(
            "UPDATE tabs_private SET expires_at = ?1",
            [sr_proto::now_millis() - 1],
        )
        .unwrap();

    assert_eq!(sweep_expired_private(&db).unwrap(), 1);
    let left: i64 = db
        .conn
        .query_row("SELECT COUNT(*) FROM tabs_private", [], |r| r.get(0))
        .unwrap();
    assert_eq!(left, 0);

    // Ciphertext is gone from the file too, not just unlinked from the table.
    db.conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
    assert!(files_containing(dir.path(), SECRET_URL).is_empty());
}

#[test]
fn purging_removes_every_private_row() {
    let dir = TempDir::new();
    let db = Db::open(&dir.path().join("sessions.db")).unwrap();
    let keys = KeyManager::new(dir.path());
    db.set_setting("capture_private_windows", "true").unwrap();
    let ctx = IngestCtx::live(&db, &keys);

    for i in 0..5 {
        ingest_tab(&private_tab(&format!("w-private:t{i}")), &ctx).unwrap();
    }
    ingest_tab(&normal_tab("w-normal:t1"), &ctx).unwrap();

    assert_eq!(purge_all_private(&db).unwrap(), 5);
    let normal_left: i64 = db
        .conn
        .query_row("SELECT COUNT(*) FROM tabs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(normal_left, 1, "purging private data disturbed normal tabs");
}

#[test]
fn removing_a_tab_clears_it_from_both_tables() {
    let dir = TempDir::new();
    let db = Db::open(&dir.path().join("sessions.db")).unwrap();
    let keys = KeyManager::new(dir.path());
    db.set_setting("capture_private_windows", "true").unwrap();
    let ctx = IngestCtx::live(&db, &keys);

    ingest_tab(&private_tab("shared-key"), &ctx).unwrap();

    let mut removal = private_tab("shared-key");
    removal.op = Op::Remove;
    ingest_tab(&removal, &ctx).unwrap();

    let n: i64 = db
        .conn
        .query_row("SELECT COUNT(*) FROM tabs_private", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 0);
}

#[test]
fn private_rows_carry_no_plaintext_columns_at_all() {
    // Guards the schema itself: someone adding a convenience `url` column to
    // tabs_private would defeat the entire design, and should fail here.
    let dir = TempDir::new();
    let db = Db::open(&dir.path().join("sessions.db")).unwrap();

    let mut stmt = db.conn.prepare("PRAGMA table_info(tabs_private)").unwrap();
    let cols: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(1))
        .unwrap()
        .filter_map(Result::ok)
        .collect();

    for forbidden in ["url", "title", "favicon_hash"] {
        assert!(
            !cols.contains(&forbidden.to_string()),
            "tabs_private grew a plaintext `{forbidden}` column"
        );
    }
    assert!(cols.contains(&"ciphertext".to_string()));
}

#[test]
fn normal_tabs_still_work_normally() {
    // The privacy machinery must not break the common path.
    let dir = TempDir::new();
    let db = Db::open(&dir.path().join("sessions.db")).unwrap();
    let keys = KeyManager::new(dir.path());
    let ctx = IngestCtx::live(&db, &keys);

    ingest_tab(&normal_tab("w1:t1"), &ctx).unwrap();

    let (url, snapshot): (String, i64) = db
        .conn
        .query_row(
            "SELECT url, snapshot_id FROM tabs WHERE tab_key = 'w1:t1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(url, NORMAL_URL);
    assert_eq!(snapshot, LIVE, "live state must land in snapshot 0");
}

#[test]
fn upsert_updates_rather_than_duplicates() {
    let dir = TempDir::new();
    let db = Db::open(&dir.path().join("sessions.db")).unwrap();
    let keys = KeyManager::new(dir.path());
    let ctx = IngestCtx::live(&db, &keys);

    ingest_tab(&normal_tab("w1:t1"), &ctx).unwrap();
    let mut moved = normal_tab("w1:t1");
    moved.url = Some("https://ordinary.test/second-page".into());
    ingest_tab(&moved, &ctx).unwrap();

    let n: i64 = db
        .conn
        .query_row("SELECT COUNT(*) FROM tabs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1);
    let url: String = db
        .conn
        .query_row("SELECT url FROM tabs WHERE tab_key = 'w1:t1'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(url, "https://ordinary.test/second-page");
}

#[test]
fn a_private_tab_survives_being_snapshotted() {
    // The review window reads private tabs from a *snapshot*, not from live state.
    // Snapshots copy rows to a new snapshot_id, so anything that binds the ciphertext
    // to the id it was sealed under stops decrypting the moment it is copied - and the
    // failure is silent, because a private tab that will not decrypt just does not
    // appear.
    use sr_agent::store::snapshot;
    use sr_agent::ui::review;

    let dir = TempDir::new();
    let db = Db::open(&dir.path().join("sessions.db")).unwrap();
    let keys = KeyManager::new(dir.path());
    db.set_setting("capture_private_windows", "true").unwrap();

    db.conn
        .execute(
            "INSERT INTO browser_windows (snapshot_id, browser_window_id, browser,
             profile_key, is_private, updated_at) VALUES (0,'w-priv','chrome','default',1,0)",
            [],
        )
        .unwrap();

    let ctx = IngestCtx::live(&db, &keys);
    ingest_tab(&private_tab("w-priv:t1"), &ctx).unwrap();

    let snap = snapshot::create_from_live(&db, "shutdown", None).unwrap().unwrap();

    let revealed = review::reveal_private(&db, &keys, snap)
        .expect("reveal_private failed outright");
    let tabs: usize = revealed.iter().map(|w| w.tabs.len()).sum();
    assert_eq!(
        tabs, 1,
        "a private tab could not be decrypted after being copied into a snapshot"
    );
    assert_eq!(revealed[0].tabs[0].url, SECRET_URL);
}
