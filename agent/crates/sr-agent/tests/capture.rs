//! Desktop capture against the real machine.
//!
//! These run the actual enumeration rather than a fixture, because the things most
//! likely to be wrong are exactly the things a fixture would paper over: cloaked
//! windows, Store apps hosted by a frame process, and which titles are safe to keep.
//!
//! They are written to assert *properties* rather than specific applications, so they
//! hold on any machine and in CI where the desktop is nearly empty.

#![cfg(windows)]

use sr_agent::store::db::{Db, LIVE};
use sr_agent::watcher;
use std::path::PathBuf;

struct Tmp(PathBuf);

impl Tmp {
    fn new() -> Tmp {
        let d = std::env::temp_dir().join(format!("sr-capture-{}", sr_proto::new_id()));
        std::fs::create_dir_all(&d).unwrap();
        Tmp(d)
    }
    fn db(&self) -> Db {
        Db::open(&self.0.join("sessions.db")).unwrap()
    }
}

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn a_capture_pass_finds_the_desktop() {
    let t = Tmp::new();
    let db = t.db();
    let stats = watcher::capture_into_live(&db).unwrap();

    // Any interactive Windows session has at least one display.
    assert!(stats.displays >= 1, "no displays enumerated");

    let apps: i64 = db
        .conn
        .query_row("SELECT COUNT(*) FROM apps WHERE snapshot_id = ?1", [LIVE], |r| r.get(0))
        .unwrap();
    assert_eq!(apps as usize, stats.apps);
}

#[test]
fn a_browser_window_never_stores_its_title() {
    // The privacy rule from docs/03-capture.md, asserted against whatever browsers
    // happen to be running. A browser window's title is the page title, and for a
    // private window that is C4 data.
    let t = Tmp::new();
    let db = t.db();
    watcher::capture_into_live(&db).unwrap();

    let leaked: i64 = db
        .conn
        .query_row(
            "SELECT COUNT(*) FROM windows
             WHERE snapshot_id = ?1 AND is_browser = 1 AND title IS NOT NULL",
            [LIVE],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(leaked, 0, "a browser window stored a page title");
}

#[test]
fn every_window_points_at_an_app_that_exists() {
    let t = Tmp::new();
    let db = t.db();
    watcher::capture_into_live(&db).unwrap();

    let orphans: i64 = db
        .conn
        .query_row(
            "SELECT COUNT(*) FROM windows w
             WHERE w.snapshot_id = ?1
               AND NOT EXISTS (SELECT 1 FROM apps a
                               WHERE a.snapshot_id = w.snapshot_id AND a.app_key = w.app_key)",
            [LIVE],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(orphans, 0, "windows referencing a missing app");
}

#[test]
fn every_app_has_a_usable_way_to_be_launched_or_is_tier_d() {
    // A tier that promises a restore we cannot perform is worse than an honest D.
    let t = Tmp::new();
    let db = t.db();
    watcher::capture_into_live(&db).unwrap();

    let mut stmt = db
        .conn
        .prepare("SELECT restore_tier, kind, exe_path, aumid FROM apps WHERE snapshot_id = ?1")
        .unwrap();
    let rows: Vec<(String, String, Option<String>, Option<String>)> = stmt
        .query_map([LIVE], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .filter_map(Result::ok)
        .collect();

    for (tier, kind, exe, aumid) in rows {
        if tier == "D" {
            continue;
        }
        match kind.as_str() {
            "uwp" => assert!(aumid.is_some(), "a restorable Store app without an AUMID"),
            _ => assert!(exe.is_some(), "a restorable Win32 app without an exe path"),
        }
    }
}

#[test]
fn store_apps_are_attributed_to_themselves_not_the_frame_host() {
    // A Store app's window belongs to ApplicationFrameHost.exe, so without resolving
    // the child window every Store app is dropped or recorded as the host. If any UWP
    // app was captured at all, none of them may be the host.
    let t = Tmp::new();
    let db = t.db();
    watcher::capture_into_live(&db).unwrap();

    let hosts: i64 = db
        .conn
        .query_row(
            "SELECT COUNT(*) FROM apps
             WHERE snapshot_id = ?1 AND lower(exe_path) LIKE '%applicationframehost.exe'",
            [LIVE],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(hosts, 0, "the Store app frame host was captured as an application");
}

#[test]
fn stored_paths_are_env_folded() {
    // What lets a profile survive a drive-letter change, and the groundwork for
    // cross-device sync.
    let t = Tmp::new();
    let db = t.db();
    watcher::capture_into_live(&db).unwrap();

    let mut stmt = db
        .conn
        .prepare(
            "SELECT exe_path FROM apps
             WHERE snapshot_id = ?1 AND kind = 'win32' AND exe_path IS NOT NULL",
        )
        .unwrap();
    let paths: Vec<String> = stmt
        .query_map([LIVE], |r| r.get(0))
        .unwrap()
        .filter_map(Result::ok)
        .collect();

    // Anything under a well-known root should have been folded. Paths elsewhere
    // (a game on D:, say) legitimately stay absolute.
    let system_root = std::env::var("SystemRoot").unwrap_or_default().to_lowercase();
    for p in paths {
        if !system_root.is_empty() && p.to_lowercase().starts_with(&system_root) {
            panic!("an unfolded system path was stored: {p}");
        }
    }
}

#[test]
fn a_second_pass_reaps_what_closed_and_keeps_what_did_not() {
    let t = Tmp::new();
    let db = t.db();
    watcher::capture_into_live(&db).unwrap();

    // Inject a window belonging to an app that is not running.
    db.conn
        .execute(
            "INSERT INTO apps (snapshot_id, app_key, kind, exe_path, restore_tier)
             VALUES (?1, 'win32:ghost', 'win32', 'C:\\ghost.exe', 'A')",
            [LIVE],
        )
        .unwrap();
    db.conn
        .execute(
            "INSERT INTO windows (snapshot_id, window_key, app_key, show_cmd)
             VALUES (?1, 'hwnd:999999999', 'win32:ghost', 'normal')",
            [LIVE],
        )
        .unwrap();

    let before: i64 = db
        .conn
        .query_row("SELECT COUNT(*) FROM windows WHERE snapshot_id = ?1", [LIVE], |r| r.get(0))
        .unwrap();

    watcher::capture_into_live(&db).unwrap();

    let ghosts: i64 = db
        .conn
        .query_row(
            "SELECT COUNT(*) FROM windows WHERE snapshot_id = ?1 AND window_key = 'hwnd:999999999'",
            [LIVE],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(ghosts, 0, "a closed window survived a capture pass");

    let after: i64 = db
        .conn
        .query_row("SELECT COUNT(*) FROM windows WHERE snapshot_id = ?1", [LIVE], |r| r.get(0))
        .unwrap();
    assert!(after >= before - 1, "the reap removed live windows too");
}

#[test]
fn capture_is_idempotent() {
    let t = Tmp::new();
    let db = t.db();
    let a = watcher::capture_into_live(&db).unwrap();
    let b = watcher::capture_into_live(&db).unwrap();

    // The desktop can legitimately change between passes, so this checks that a
    // repeated pass does not *accumulate* rows rather than demanding equality.
    let rows: i64 = db
        .conn
        .query_row("SELECT COUNT(*) FROM windows WHERE snapshot_id = ?1", [LIVE], |r| r.get(0))
        .unwrap();
    assert!(
        rows as usize <= a.windows.max(b.windows) + 2,
        "rows accumulated across passes: {rows} after {} then {}",
        a.windows,
        b.windows
    );
}

#[test]
fn a_maximized_window_keeps_its_restored_size() {
    // GetWindowPlacement rather than GetWindowRect: a maximized window must record the
    // size it returns to, not the monitor bounds.
    let t = Tmp::new();
    let db = t.db();
    watcher::capture_into_live(&db).unwrap();

    let mut stmt = db
        .conn
        .prepare(
            "SELECT norm_w, norm_h FROM windows
             WHERE snapshot_id = ?1 AND show_cmd = 'maximized'",
        )
        .unwrap();
    let sizes: Vec<(i64, i64)> = stmt
        .query_map([LIVE], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .filter_map(Result::ok)
        .collect();

    for (w, h) in sizes {
        assert!(w > 0 && h > 0, "a maximized window stored an empty restored rect");
    }
}
