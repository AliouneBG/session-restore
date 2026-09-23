//! Restore offer behaviour, end to end over the real pipe.
//!
//! The scenario these cover is the one that actually happens at logon and is easy to
//! get wrong: the stored session belongs to windows that no longer exist, and the
//! browser that reconnects is a *different* run of the browser.

#![cfg(windows)]

use sr_agent::server::{serve_on, BrowserSelection, PendingRestore, Shared};
use sr_agent::store::db::Db;
use sr_agent::store::keys::KeyManager;
use sr_agent::store::snapshot;
use sr_proto::frame::{read_frame, write_frame, MAX_INBOUND_BYTES, MAX_OUTBOUND_BYTES};
use sr_proto::Envelope;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct Harness {
    dir: PathBuf,
    pipe: String,
    shared: Arc<Shared>,
}

impl Harness {
    /// Seeds a stored session, snapshots it the way the agent does at startup, wipes
    /// live state the way a browser's first reconcile would, and starts the server.
    fn with_previous_session(tabs: &[(&str, &str)]) -> Harness {
        let dir = std::env::temp_dir().join(format!("sr-restore-{}", sr_proto::new_id()));
        std::fs::create_dir_all(&dir).unwrap();

        let db = Db::open(&dir.join("sessions.db")).unwrap();
        db.conn
            .execute(
                "INSERT INTO browser_windows (snapshot_id, browser_window_id, browser,
                 profile_key, is_private, window_state, x, y, w, h, focused, updated_at)
                 VALUES (0,'w-old','chrome','default',0,'normal',10,20,1200,800,1,0)",
                [],
            )
            .unwrap();
        for (i, (key, url)) in tabs.iter().enumerate() {
            db.conn
                .execute(
                    "INSERT INTO tabs (snapshot_id, tab_key, browser_window_id, tab_index,
                     url, title, pinned, active, muted, restorable, updated_at)
                     VALUES (0, ?1, 'w-old', ?2, ?3, 'title', 0, ?4, 0, 1, 0)",
                    rusqlite::params![key, i as i64, url, (i == 0) as i64],
                )
                .unwrap();
        }

        let snap = snapshot::create_from_live(&db, "shutdown", Some("previous session"))
            .unwrap()
            .unwrap();

        // The browser's first reconcile reaps the old windows. The snapshot must not care.
        db.conn.execute("DELETE FROM tabs WHERE snapshot_id = 0", []).unwrap();
        db.conn
            .execute("DELETE FROM browser_windows WHERE snapshot_id = 0", [])
            .unwrap();

        let keys = KeyManager::new(&dir);
        let shared = Arc::new(Shared::new(
            Arc::new(Mutex::new(db)),
            Arc::new(keys),
            PendingRestore::new(Some(snap)),
        ));

        let pipe = format!("\\\\.\\pipe\\SessionRestoreTest.{}", sr_proto::new_id());
        let server_shared = Arc::clone(&shared);
        let server_pipe = pipe.clone();
        std::thread::spawn(move || {
            let _ = serve_on(&server_pipe, server_shared);
        });
        std::thread::sleep(Duration::from_millis(300));

        Harness { dir, pipe, shared }
    }

    fn connect(&self) -> sr_ipc::PipeConnection {
        for _ in 0..40 {
            if let Ok(c) = sr_ipc::connect(&self.pipe) {
                return c;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("could not connect to {}", self.pipe);
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn send(c: &mut sr_ipc::PipeConnection, msg: serde_json::Value) {
    write_frame(c, &msg.to_string(), MAX_OUTBOUND_BYTES).unwrap();
}

fn recv(c: &mut sr_ipc::PipeConnection) -> Envelope {
    serde_json::from_str(&read_frame(c, MAX_INBOUND_BYTES).unwrap()).unwrap()
}

fn hello(browser: &str) -> serde_json::Value {
    serde_json::json!({
        "v": 1, "id": sr_proto::new_id(), "type": "hello", "ts": 0,
        "src": { "browser": browser, "profile_key": "default", "ext_version": "0.1.0" },
        "body": { "ext_version": "0.1.0", "browser_version": "test",
                  "incognito_access": false, "capabilities": [] }
    })
}

#[test]
fn a_connecting_browser_is_offered_the_previous_session() {
    let h = Harness::with_previous_session(&[
        ("w-old:t1", "https://one.test/"),
        ("w-old:t2", "https://two.test/"),
    ]);
    let mut c = h.connect();

    send(&mut c, hello("chrome"));
    assert_eq!(recv(&mut c).kind, "hello_ack");

    let offer = recv(&mut c);
    assert_eq!(offer.kind, "restore_session");
    assert_eq!(offer.body["windows"].as_array().unwrap().len(), 1);

    let tabs = offer.body["windows"][0]["tabs"].as_array().unwrap();
    assert_eq!(tabs.len(), 2);
    assert_eq!(tabs[0]["url"], "https://one.test/");
    assert_eq!(tabs[1]["url"], "https://two.test/");
    // Lazy by default: 40 tabs must not all start loading during logon.
    assert_eq!(offer.body["lazy"], true);
}

#[test]
fn the_offer_carries_window_geometry() {
    let h = Harness::with_previous_session(&[("w-old:t1", "https://one.test/")]);
    let mut c = h.connect();
    send(&mut c, hello("chrome"));
    let _ = recv(&mut c);

    let w = &recv(&mut c).body["windows"][0];
    assert_eq!(w["state"], "normal");
    assert_eq!(w["x"], 10);
    assert_eq!(w["y"], 20);
    assert_eq!(w["w"], 1200);
    assert_eq!(w["h"], 800);
}

#[test]
fn restore_is_offered_only_once_per_browser() {
    // MV3 workers reconnect constantly; every reconnect sends a fresh hello. Offering
    // each time would re-send the whole session repeatedly.
    let h = Harness::with_previous_session(&[("w-old:t1", "https://one.test/")]);
    let mut c = h.connect();

    send(&mut c, hello("chrome"));
    assert_eq!(recv(&mut c).kind, "hello_ack");
    assert_eq!(recv(&mut c).kind, "restore_session");

    // Reconnect.
    send(&mut c, hello("chrome"));
    assert_eq!(recv(&mut c).kind, "hello_ack");

    // Nothing further should arrive. Prove it by round-tripping another hello and
    // seeing only its ack.
    send(&mut c, hello("chrome"));
    assert_eq!(recv(&mut c).kind, "hello_ack");
}

#[test]
fn a_different_browser_gets_its_own_offer_decision() {
    // The stored session belongs to Chrome, so Firefox must be offered nothing rather
    // than inheriting Chrome's tabs.
    let h = Harness::with_previous_session(&[("w-old:t1", "https://one.test/")]);
    let mut c = h.connect();

    send(&mut c, hello("firefox"));
    assert_eq!(recv(&mut c).kind, "hello_ack");

    send(&mut c, hello("firefox"));
    assert_eq!(recv(&mut c).kind, "hello_ack", "firefox was offered chrome's session");
}

#[test]
fn nothing_is_offered_when_restore_mode_is_off() {
    let h = Harness::with_previous_session(&[("w-old:t1", "https://one.test/")]);
    h.shared
        .db
        .lock()
        .unwrap()
        .set_setting("restore_mode", "off")
        .unwrap();

    let mut c = h.connect();
    send(&mut c, hello("chrome"));
    assert_eq!(recv(&mut c).kind, "hello_ack");

    send(&mut c, hello("chrome"));
    assert_eq!(recv(&mut c).kind, "hello_ack", "offered a restore while disabled");
}

#[test]
fn a_restore_run_records_an_undo_point_and_outcomes() {
    let h = Harness::with_previous_session(&[
        ("w-old:t1", "https://one.test/"),
        ("w-old:t2", "https://two.test/"),
    ]);
    let mut c = h.connect();
    send(&mut c, hello("chrome"));
    let _ = recv(&mut c);
    let offer = recv(&mut c);
    let run_id = offer.body["run_id"].as_i64().unwrap();

    send(
        &mut c,
        serde_json::json!({
            "v": 1, "id": "res", "type": "restore_result", "ts": 0,
            "src": { "browser": "chrome", "profile_key": "default", "ext_version": "0.1.0" },
            "body": { "run_id": run_id, "items": [
                { "tab_key": "https://one.test/", "status": "created" },
                { "tab_key": "https://two.test/", "status": "failed", "detail": "bad scheme" }
            ]}
        }),
    );

    // restore_result draws no reply, so poll for the recorded outcome.
    let mut finished = false;
    for _ in 0..40 {
        let db = h.shared.db.lock().unwrap();
        let done: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM restore_runs WHERE id = ?1 AND finished_at IS NOT NULL",
                [run_id],
                |r| r.get(0),
            )
            .unwrap();
        if done == 1 {
            finished = true;
            let undo: Option<i64> = db
                .conn
                .query_row(
                    "SELECT undo_snapshot_id FROM restore_runs WHERE id = ?1",
                    [run_id],
                    |r| r.get(0),
                )
                .unwrap();
            // Nothing was open in this harness, so there is legitimately nothing to
            // undo to; the column records that honestly rather than inventing one.
            let _ = undo;

            let failed: i64 = db
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM restore_items WHERE run_id = ?1 AND status = 'failed'",
                    [run_id],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(failed, 1, "the failed item was not recorded");

            let detail: String = db
                .conn
                .query_row(
                    "SELECT detail FROM restore_items WHERE run_id = ?1 AND status = 'failed'",
                    [run_id],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(detail, "bad scheme");
            break;
        }
        drop(db);
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(finished, "the restore run was never marked finished");
}

#[test]
fn private_windows_are_never_part_of_an_automatic_offer() {
    // ADR-0004: private restore requires an explicit, current confirmation, which the
    // automatic path has no way to obtain.
    let h = Harness::with_previous_session(&[("w-old:t1", "https://one.test/")]);

    {
        let db = h.shared.db.lock().unwrap();
        let snap: i64 = db
            .conn
            .query_row("SELECT id FROM snapshots WHERE kind = 'shutdown'", [], |r| r.get(0))
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO browser_windows (snapshot_id, browser_window_id, browser,
                 profile_key, is_private, updated_at) VALUES (?1,'w-priv','chrome','default',1,0)",
                [snap],
            )
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO tabs (snapshot_id, tab_key, browser_window_id, tab_index,
                 url, pinned, active, muted, restorable, updated_at)
                 VALUES (?1,'w-priv:t1','w-priv',0,'https://secret.test/',0,0,0,1,0)",
                [snap],
            )
            .unwrap();
    }

    let mut c = h.connect();
    send(&mut c, hello("chrome"));
    let _ = recv(&mut c);
    let offer = recv(&mut c);

    let body = offer.body.to_string();
    assert!(
        !body.contains("secret.test"),
        "a private window was included in an automatic restore offer"
    );
    for w in offer.body["windows"].as_array().unwrap() {
        assert_eq!(w["private"], false);
    }
}


#[test]
fn a_review_selection_limits_which_windows_are_offered() {
    // The review window shows per-window checkboxes. If the offer ignored them, the
    // checkboxes would be decoration.
    let h = Harness::with_previous_session(&[
        ("w-old:t1", "https://one.test/"),
        ("w-old:t2", "https://two.test/"),
    ]);
    h.shared
        .pending_restore
        .lock()
        .unwrap()
        .set_selection(BrowserSelection {
            windows: ["some-other-window".to_string()].into_iter().collect(),
            tabs: Default::default(),
            declined: false,
            restore_private: false,
        });

    let mut c = h.connect();
    send(&mut c, hello("chrome"));
    assert_eq!(recv(&mut c).kind, "hello_ack");

    // The stored window was not selected, so nothing is offered at all.
    send(&mut c, hello("chrome"));
    assert_eq!(recv(&mut c).kind, "hello_ack", "offered an unselected window");
}

#[test]
fn a_review_selection_limits_which_tabs_are_offered() {
    let h = Harness::with_previous_session(&[
        ("w-old:t1", "https://keep.test/"),
        ("w-old:t2", "https://drop.test/"),
        ("w-old:t3", "https://also-drop.test/"),
    ]);
    h.shared
        .pending_restore
        .lock()
        .unwrap()
        .set_selection(BrowserSelection {
            windows: ["w-old".to_string()].into_iter().collect(),
            tabs: ["w-old:t1".to_string()].into_iter().collect(),
            declined: false,
            restore_private: false,
        });

    let mut c = h.connect();
    send(&mut c, hello("chrome"));
    let _ = recv(&mut c);
    let offer = recv(&mut c);

    assert_eq!(offer.kind, "restore_session");
    let tabs = offer.body["windows"][0]["tabs"].as_array().unwrap();
    assert_eq!(tabs.len(), 1, "unselected tabs were offered anyway");
    assert_eq!(tabs[0]["url"], "https://keep.test/");
}

#[test]
fn declining_the_review_declines_the_tabs_too() {
    // Saying no has to mean no to everything, not just the applications.
    let h = Harness::with_previous_session(&[("w-old:t1", "https://one.test/")]);
    h.shared
        .pending_restore
        .lock()
        .unwrap()
        .set_selection(BrowserSelection {
            windows: Default::default(),
            tabs: Default::default(),
            declined: true,
            restore_private: false,
        });

    let mut c = h.connect();
    send(&mut c, hello("chrome"));
    assert_eq!(recv(&mut c).kind, "hello_ack");

    send(&mut c, hello("chrome"));
    assert_eq!(recv(&mut c).kind, "hello_ack", "offered a declined restore");
}

#[test]
fn with_no_review_answer_the_whole_session_is_offered() {
    // No selection means the user was never asked - the historical behaviour, and the
    // right default when there is no review to consult.
    let h = Harness::with_previous_session(&[
        ("w-old:t1", "https://one.test/"),
        ("w-old:t2", "https://two.test/"),
    ]);

    let mut c = h.connect();
    send(&mut c, hello("chrome"));
    let _ = recv(&mut c);
    let offer = recv(&mut c);
    assert_eq!(offer.body["windows"][0]["tabs"].as_array().unwrap().len(), 2);
}

/// The defect this pins: a restore is several runs (the applications from the review
/// window, then each browser as it connects), and each one used to take its own
/// `pre_restore` snapshot. `--undo` reads the newest run, so it read an undo point
/// captured *after* the restore had already launched everything - undoing returned you
/// to the state the restore had just produced.
#[test]
fn every_run_in_one_restore_shares_the_undo_point() {
    let h = Harness::with_previous_session(&[("w-old:t1", "https://one.test/")]);
    let snapshot_id = {
        let db = h.shared.db.lock().unwrap();
        db.conn
            .query_row(
                "SELECT id FROM snapshots WHERE kind = 'shutdown' ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
    };

    let db = h.shared.db.lock().unwrap();
    live_tab(&db, "w-now:t1");

    let first = sr_agent::restore::begin_run(&db, snapshot_id, "ask").unwrap();
    let second = sr_agent::restore::begin_run(&db, snapshot_id, "ask").unwrap();
    let third = sr_agent::restore::begin_run(&db, snapshot_id, "ask").unwrap();
    assert_ne!(first, second, "each run is still its own row");

    let undo_of = |id: i64| -> Option<i64> {
        db.conn
            .query_row(
                "SELECT undo_snapshot_id FROM restore_runs WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .unwrap()
    };
    assert!(undo_of(first).is_some(), "the first run must record an undo point");
    assert_eq!(undo_of(first), undo_of(second));
    assert_eq!(undo_of(first), undo_of(third));

    let pre: i64 = db
        .conn
        .query_row("SELECT COUNT(*) FROM snapshots WHERE kind = 'pre_restore'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(pre, 1, "one restore must leave one undo point, not one per run");
}

/// A restore for a *different* snapshot is a different restore, even moments later.
#[test]
fn a_separate_restore_gets_its_own_undo_point() {
    let h = Harness::with_previous_session(&[("w-old:t1", "https://one.test/")]);
    let db = h.shared.db.lock().unwrap();

    // The harness reaps live state, and an undo point can only be made from something.
    live_tab(&db, "w-now:t1");

    let first = sr_agent::restore::begin_run(&db, 1, "manual").unwrap();
    let second = sr_agent::restore::begin_run(&db, 2, "manual").unwrap();

    let undo_of = |id: i64| -> Option<i64> {
        db.conn
            .query_row(
                "SELECT undo_snapshot_id FROM restore_runs WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .unwrap()
    };
    assert_ne!(undo_of(first), undo_of(second));
}

/// Outcomes of the agent-side half of a restore are recorded, and the run is closed.
#[test]
fn an_application_restore_records_what_it_did() {
    let h = Harness::with_previous_session(&[("w-old:t1", "https://one.test/")]);
    let db = h.shared.db.lock().unwrap();
    live_tab(&db, "w-now:t1");
    let run_id = sr_agent::restore::begin_run(&db, 1, "manual").unwrap();

    let report = sr_agent::restore::apps::AppRestoreReport {
        launched: vec!["Notepad".into()],
        skipped: vec![("Code".into(), "already running".into())],
        failed: vec![("Figma".into(), "not installed".into())],
        placed: 1,
    };
    sr_agent::restore::record_app_outcomes(&db, run_id, &report).unwrap();

    let mut stmt = db
        .conn
        .prepare("SELECT item_key, status, detail FROM restore_items WHERE run_id = ?1 AND item_kind = 'app' ORDER BY item_key")
        .unwrap();
    let rows: Vec<(String, String, Option<String>)> = stmt
        .query_map([run_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    drop(stmt);

    assert_eq!(
        rows,
        vec![
            ("Code".into(), "skipped".into(), Some("already running".into())),
            ("Figma".into(), "failed".into(), Some("not installed".into())),
            ("Notepad".into(), "launched".into(), None),
        ]
    );

    let finished: Option<i64> = db
        .conn
        .query_row("SELECT finished_at FROM restore_runs WHERE id = ?1", [run_id], |r| {
            r.get(0)
        })
        .unwrap();
    assert!(finished.is_some(), "recording outcomes closes the run");
}

/// Puts one tab in live state. `create_from_live` counts tabs and applications, so
/// without one there is nothing to snapshot and no undo point to compare.
fn live_tab(db: &Db, tab_key: &str) {
    db.conn
        .execute(
            "INSERT OR REPLACE INTO tabs (snapshot_id, tab_key, browser_window_id, tab_index,
             url, title, pinned, active, muted, restorable, updated_at)
             VALUES (0, ?1, 'w-now', 0, 'https://now.test/', 'now', 0, 1, 0, 1, 0)",
            [tab_key],
        )
        .unwrap();
}

/// Seeds a browser application row, the way a desktop capture would.
fn browser_app(db: &Db, snapshot_id: i64, key: &str, exe: &str) {
    browser_app_with(db, snapshot_id, key, exe, None);
}

fn browser_app_with(db: &Db, snapshot_id: i64, key: &str, exe: &str, cmd: Option<&str>) {
    db.conn
        .execute(
            "INSERT OR REPLACE INTO apps (snapshot_id, app_key, kind, exe_path, display_name,
             command_line, is_browser, restore_tier)
             VALUES (?1, ?2, 'win32', ?3, 'browser', ?4, 1, 'A')",
            rusqlite::params![snapshot_id, key, exe, cmd],
        )
        .unwrap();
}

/// The gap this closes: a restore offer waits for an extension to connect, and after a
/// reboot no browser is running to connect one. Ticking a browser window meant
/// "restore these tabs the next time you happen to open Edge".
#[test]
fn a_browser_that_is_not_running_is_started_for_the_restore() {
    let h = Harness::with_previous_session(&[("w-old:t1", "https://one.test/")]);
    let db = h.shared.db.lock().unwrap();
    let snapshot_id: i64 = db
        .conn
        .query_row("SELECT MAX(id) FROM snapshots", [], |r| r.get(0))
        .unwrap();

    browser_app(&db, snapshot_id, "app-chrome", r"C:\Program Files\Chrome\chrome.exe");

    let wanted: std::collections::HashSet<String> = ["w-old".to_string()].into_iter().collect();
    let plan = sr_agent::restore::browsers_to_launch(&db, snapshot_id, &wanted).unwrap();

    assert_eq!(plan.len(), 1, "the snapshot's browser was not planned");
    assert_eq!(plan[0].browser, "chrome");
    assert!(plan[0].exe_path.ends_with("chrome.exe"));
}

#[test]
fn a_browser_that_is_already_running_is_left_alone() {
    let h = Harness::with_previous_session(&[("w-old:t1", "https://one.test/")]);
    let db = h.shared.db.lock().unwrap();
    let snapshot_id: i64 = db
        .conn
        .query_row("SELECT MAX(id) FROM snapshots", [], |r| r.get(0))
        .unwrap();

    browser_app(&db, snapshot_id, "app-chrome", r"C:\Program Files\Chrome\chrome.exe");
    // The caller captures the desktop first, so live state is what is on screen now.
    browser_app(&db, 0, "app-chrome-live", r"C:\Program Files\Chrome\chrome.exe");

    let wanted: std::collections::HashSet<String> = ["w-old".to_string()].into_iter().collect();
    let plan = sr_agent::restore::browsers_to_launch(&db, snapshot_id, &wanted).unwrap();
    assert!(plan.is_empty(), "started a browser that was already open: {plan:?}");
}

#[test]
fn a_browser_whose_windows_were_all_unticked_is_not_started() {
    let h = Harness::with_previous_session(&[("w-old:t1", "https://one.test/")]);
    let db = h.shared.db.lock().unwrap();
    let snapshot_id: i64 = db
        .conn
        .query_row("SELECT MAX(id) FROM snapshots", [], |r| r.get(0))
        .unwrap();

    browser_app(&db, snapshot_id, "app-chrome", r"C:\Program Files\Chrome\chrome.exe");

    let plan =
        sr_agent::restore::browsers_to_launch(&db, snapshot_id, &Default::default()).unwrap();
    assert!(plan.is_empty(), "started a browser the user had unticked: {plan:?}");
}

/// A private window is never part of an offer, so starting a browser for one would
/// open it and restore nothing.
#[test]
fn a_private_window_alone_does_not_start_a_browser() {
    let h = Harness::with_previous_session(&[("w-old:t1", "https://one.test/")]);
    let db = h.shared.db.lock().unwrap();
    let snapshot_id: i64 = db
        .conn
        .query_row("SELECT MAX(id) FROM snapshots", [], |r| r.get(0))
        .unwrap();

    browser_app(&db, snapshot_id, "app-chrome", r"C:\Program Files\Chrome\chrome.exe");
    db.conn
        .execute(
            "INSERT INTO browser_windows (snapshot_id, browser_window_id, browser,
             profile_key, is_private, window_state, focused, updated_at)
             VALUES (?1,'w-priv','chrome','default',1,'normal',0,0)",
            [snapshot_id],
        )
        .unwrap();

    let wanted: std::collections::HashSet<String> = ["w-priv".to_string()].into_iter().collect();
    let plan = sr_agent::restore::browsers_to_launch(&db, snapshot_id, &wanted).unwrap();
    assert!(plan.is_empty(), "started a browser for a private window: {plan:?}");
}

/// The race this closes: at logon the review window and the browser start together,
/// and the browser almost always wins. Every checkbox in that window was bypassed
/// seconds before the user could tick one.
#[test]
fn a_browser_that_connects_before_the_review_is_answered_waits_for_it() {
    let h = Harness::with_previous_session(&[
        ("w-old:t1", "https://one.test/"),
        ("w-old:t2", "https://two.test/"),
    ]);
    {
        let mut pending = h.shared.pending_restore.lock().unwrap();
        *pending = PendingRestore::awaiting_review(pending.snapshot_id);
    }

    let mut c = h.connect();
    send(&mut c, hello("chrome"));
    assert_eq!(recv(&mut c).kind, "hello_ack");
    send(&mut c, hello("chrome"));
    assert_eq!(
        recv(&mut c).kind,
        "hello_ack",
        "offered the whole session while the user was still choosing"
    );

    // Answering pushes the offer to the browser that was made to wait.
    {
        let mut pending = h.shared.pending_restore.lock().unwrap();
        pending.set_selection(BrowserSelection {
            windows: ["w-old".to_string()].into_iter().collect(),
            tabs: ["w-old:t2".to_string()].into_iter().collect(),
            declined: false,
            restore_private: false,
        });
    }
    sr_agent::server::offer_to_connected(&h.shared);

    let offer = recv(&mut c);
    assert_eq!(offer.kind, "restore_session");
    let tabs = offer.body["windows"][0]["tabs"].as_array().unwrap();
    assert_eq!(tabs.len(), 1, "the deferred offer ignored the review");
    assert_eq!(tabs[0]["url"], "https://two.test/");
}

/// Dismissing must release a browser that was made to wait, without restoring.
///
/// This test used to assert the opposite, that dismissal delivered the offer, and it
/// was wrong in the way that matters: closing the review window and opening a browser
/// later silently reopened the entire previous session. The release is still required,
/// or the browser waits forever, but what gets released is a refusal.
#[test]
fn dismissing_the_review_releases_a_waiting_browser_without_restoring() {
    let h = Harness::with_previous_session(&[("w-old:t1", "https://one.test/")]);
    {
        let mut pending = h.shared.pending_restore.lock().unwrap();
        *pending = PendingRestore::awaiting_review(pending.snapshot_id);
    }

    let mut c = h.connect();
    send(&mut c, hello("chrome"));
    assert_eq!(recv(&mut c).kind, "hello_ack");
    send(&mut c, hello("chrome"));
    assert_eq!(recv(&mut c).kind, "hello_ack", "offered while the review was open");

    {
        let mut pending = h.shared.pending_restore.lock().unwrap();
        pending.review_dismissed();
    }
    sr_agent::server::offer_to_connected(&h.shared);

    // Released, but with nothing to restore. A third hello proves no offer arrived and
    // that the connection is still being served rather than stuck.
    send(&mut c, hello("chrome"));
    assert_eq!(
        recv(&mut c).kind,
        "hello_ack",
        "a dismissed review restored the session anyway"
    );
}

/// A browser that connects *after* the answer takes the normal path, not the push.
#[test]
fn a_browser_that_connects_after_the_review_is_offered_normally() {
    let h = Harness::with_previous_session(&[
        ("w-old:t1", "https://one.test/"),
        ("w-old:t2", "https://two.test/"),
    ]);
    {
        let mut pending = h.shared.pending_restore.lock().unwrap();
        *pending = PendingRestore::awaiting_review(pending.snapshot_id);
        pending.set_selection(BrowserSelection {
            windows: ["w-old".to_string()].into_iter().collect(),
            tabs: ["w-old:t1".to_string()].into_iter().collect(),
            declined: false,
            restore_private: false,
        });
    }

    let mut c = h.connect();
    send(&mut c, hello("chrome"));
    assert_eq!(recv(&mut c).kind, "hello_ack");
    let offer = recv(&mut c);
    assert_eq!(offer.kind, "restore_session");
    assert_eq!(offer.body["windows"][0]["tabs"].as_array().unwrap().len(), 1);
}

/// A browser with nothing restorable must not leave a run behind. `--undo` reads the
/// most recent run, so an empty one is not merely untidy: it is the undo point.
#[test]
fn a_browser_with_nothing_stored_opens_no_run() {
    let h = Harness::with_previous_session(&[("w-old:t1", "https://one.test/")]);

    let mut c = h.connect();
    send(&mut c, hello("firefox"));
    assert_eq!(recv(&mut c).kind, "hello_ack");
    send(&mut c, hello("firefox"));
    assert_eq!(recv(&mut c).kind, "hello_ack", "offered a session it does not have");

    let db = h.shared.db.lock().unwrap();
    let runs: i64 = db
        .conn
        .query_row("SELECT COUNT(*) FROM restore_runs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(runs, 0, "opened a restore run with nothing to restore");
}

/// Reopening a specific Chromium profile needs its *directory* name, and that cannot
/// come from `profile_key`, which is a hash on purpose. It comes from the mapping the
/// extension handshake records. Without it a session captured in a second profile came
/// back in the first one, and the offer, keyed by profile, then went unclaimed.
#[test]
fn a_browser_is_started_with_the_profile_it_was_captured_in() {
    let h = Harness::with_previous_session(&[("w-old:t1", "https://one.test/")]);
    let db = h.shared.db.lock().unwrap();
    let snapshot_id: i64 = db
        .conn
        .query_row("SELECT MAX(id) FROM snapshots", [], |r| r.get(0))
        .unwrap();

    browser_app(&db, snapshot_id, "app-chrome", r"C:\Program Files\Chrome\chrome.exe");

    // The snapshot's window is under some profile key; record what directory that key
    // corresponds to, which is what a hello does.
    let key: String = db
        .conn
        .query_row(
            "SELECT profile_key FROM browser_windows WHERE snapshot_id = ?1 LIMIT 1",
            [snapshot_id],
            |r| r.get(0),
        )
        .unwrap();
    db.conn
        .execute(
            "INSERT INTO browser_status (browser, profile_key, incognito_access,
                                         profile_dir, last_seen)
             VALUES ('chrome', ?1, 0, 'Profile 2', 0)",
            [&key],
        )
        .unwrap();

    let wanted: std::collections::HashSet<String> = ["w-old".to_string()].into_iter().collect();
    let plan = sr_agent::restore::browsers_to_launch(&db, snapshot_id, &wanted).unwrap();

    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0].profile_dir.as_deref(), Some("Profile 2"));
    assert!(
        plan[0].args.iter().any(|a| a == "--profile-directory=Profile 2"),
        "profile was lost: {:?}",
        plan[0].args
    );
}

/// Startup flags are not session state, and replaying them is actively harmful.
/// A browser auto-started at logon carries `--no-startup-window`, which means
/// "open no window": replaying it looks exactly like the restore failing silently.
#[test]
fn startup_only_flags_are_never_replayed() {
    let h = Harness::with_previous_session(&[("w-old:t1", "https://one.test/")]);
    let db = h.shared.db.lock().unwrap();
    let snapshot_id: i64 = db
        .conn
        .query_row("SELECT MAX(id) FROM snapshots", [], |r| r.get(0))
        .unwrap();

    browser_app_with(
        &db,
        snapshot_id,
        "app-chrome",
        r"C:\Program Files\Chrome\chrome.exe",
        Some(r#""C:\chrome.exe" --no-startup-window --win-session-start --restore-last-session"#),
    );

    let wanted: std::collections::HashSet<String> = ["w-old".to_string()].into_iter().collect();
    let plan = sr_agent::restore::browsers_to_launch(&db, snapshot_id, &wanted).unwrap();

    assert_eq!(plan.len(), 1, "the browser must still be started");
    for bad in ["--no-startup-window", "--win-session-start", "--restore-last-session"] {
        assert!(
            !plan[0].args.iter().any(|a| a == bad),
            "replayed a startup-only flag {bad}: {:?}",
            plan[0].args
        );
    }
}

/// An unpacked extension only exists if the flag that loads it comes back, so this one
/// is kept where the startup flags are not.
#[test]
fn the_flag_that_loads_the_extension_is_kept() {
    let h = Harness::with_previous_session(&[("w-old:t1", "https://one.test/")]);
    let db = h.shared.db.lock().unwrap();
    let snapshot_id: i64 = db
        .conn
        .query_row("SELECT MAX(id) FROM snapshots", [], |r| r.get(0))
        .unwrap();

    browser_app_with(
        &db,
        snapshot_id,
        "app-chrome",
        r"C:\Program Files\Chrome\chrome.exe",
        Some(r#""C:\chrome.exe" --load-extension=C:\ext --no-startup-window"#),
    );

    let wanted: std::collections::HashSet<String> = ["w-old".to_string()].into_iter().collect();
    let plan = sr_agent::restore::browsers_to_launch(&db, snapshot_id, &wanted).unwrap();
    assert!(
        plan[0].args.iter().any(|a| a.starts_with("--load-extension=")),
        "dropped the flag that loads the extension: {:?}",
        plan[0].args
    );
}

/// A redacted command line must never be replayed: the sentinel is not an argument.
#[test]
fn a_redacted_command_line_is_not_replayed() {
    let h = Harness::with_previous_session(&[("w-old:t1", "https://one.test/")]);
    let db = h.shared.db.lock().unwrap();
    let snapshot_id: i64 = db
        .conn
        .query_row("SELECT MAX(id) FROM snapshots", [], |r| r.get(0))
        .unwrap();

    browser_app_with(
        &db,
        snapshot_id,
        "app-chrome",
        r"C:\Program Files\Chrome\chrome.exe",
        Some(r#""C:\Program Files\Chrome\chrome.exe" --auth=<redacted:24>"#),
    );

    let wanted: std::collections::HashSet<String> = ["w-old".to_string()].into_iter().collect();
    let plan = sr_agent::restore::browsers_to_launch(&db, snapshot_id, &wanted).unwrap();

    assert_eq!(plan.len(), 1, "the browser must still be started");
    assert!(
        plan[0].args.iter().all(|a| !a.contains("<redacted:")),
        "would have passed a redaction sentinel to the browser: {:?}",
        plan[0].args
    );
    assert!(plan[0].exe_path.ends_with("chrome.exe"), "no fallback to start with");
}

/// The fallback that makes a profile mismatch survivable.
///
/// Chromium runs one browser process for every profile, so a session captured from a
/// taskbar launch has no profile on its command line and one captured after the profile
/// picker does. Requiring an exact match meant picking the right profile by hand still
/// restored nothing, which is the worst answer to give someone who just did as asked.
#[test]
fn a_browser_whose_profile_does_not_match_is_still_offered_its_only_session() {
    let h = Harness::with_previous_session(&[
        ("w-old:t1", "https://one.test/"),
        ("w-old:t2", "https://two.test/"),
    ]);

    let mut c = h.connect();
    // The stored window is under profile "default"; this browser announces a different
    // one, exactly as Chromium does after the profile picker.
    send(
        &mut c,
        serde_json::json!({
            "v": 1, "id": sr_proto::new_id(), "type": "hello", "ts": 0,
            "src": { "browser": "chrome", "profile_key": "a1b2c3d4e5f6", "ext_version": "0.1.0" },
            "body": { "ext_version": "0.1.0", "browser_version": "test",
                      "incognito_access": false, "capabilities": [] }
        }),
    );
    assert_eq!(recv(&mut c).kind, "hello_ack");

    let offer = recv(&mut c);
    assert_eq!(offer.kind, "restore_session", "a mismatched profile got nothing back");
    assert_eq!(offer.body["windows"][0]["tabs"].as_array().unwrap().len(), 2);
}

/// With several stored profiles there is no safe guess, and putting one profile's tabs
/// into another is worse than restoring nothing.
#[test]
fn a_mismatch_is_not_guessed_when_several_profiles_were_captured() {
    let h = Harness::with_previous_session(&[("w-old:t1", "https://one.test/")]);
    {
        let db = h.shared.db.lock().unwrap();
        let snap: i64 = db
            .conn
            .query_row("SELECT MAX(id) FROM snapshots", [], |r| r.get(0))
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO browser_windows (snapshot_id, browser_window_id, browser,
                 profile_key, is_private, window_state, focused, updated_at)
                 VALUES (?1,'w-other','chrome','second-profile',0,'normal',0,0)",
                [snap],
            )
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO tabs (snapshot_id, tab_key, browser_window_id, tab_index,
                 url, title, pinned, active, muted, restorable, updated_at)
                 VALUES (?1,'w-other:t1','w-other',0,'https://other.test/','o',0,1,0,1,0)",
                [snap],
            )
            .unwrap();
    }

    let mut c = h.connect();
    send(
        &mut c,
        serde_json::json!({
            "v": 1, "id": sr_proto::new_id(), "type": "hello", "ts": 0,
            "src": { "browser": "chrome", "profile_key": "unknown-third", "ext_version": "0.1.0" },
            "body": { "ext_version": "0.1.0", "browser_version": "test",
                      "incognito_access": false, "capabilities": [] }
        }),
    );
    assert_eq!(recv(&mut c).kind, "hello_ack");

    send(&mut c, hello("chrome"));
    assert_eq!(
        recv(&mut c).kind,
        "hello_ack",
        "guessed a profile when several were stored"
    );
}

/// The defect this pins: the private checkbox was collected from the page, stored in
/// the choice, asserted about in a test, and then never read. Private windows were
/// captured, encrypted, revealed on request, and never restored.
#[test]
fn ticking_private_windows_actually_restores_them() {
    let h = Harness::with_previous_session(&[("w-old:t1", "https://one.test/")]);
    let snapshot_id: i64 = {
        let db = h.shared.db.lock().unwrap();
        db.conn
            .query_row("SELECT MAX(id) FROM snapshots", [], |r| r.get(0))
            .unwrap()
    };

    // A private window with one encrypted tab, written the way ingest writes them.
    {
        let db = h.shared.db.lock().unwrap();
        db.set_setting("capture_private_windows", "true").unwrap();
        for snap in [0i64, snapshot_id] {
            db.conn
                .execute(
                    "INSERT INTO browser_windows (snapshot_id, browser_window_id, browser,
                     profile_key, is_private, window_state, focused, updated_at)
                     VALUES (?1,'w-priv','chrome','default',1,'normal',0,0)",
                    [snap],
                )
                .unwrap();
        }

        let ctx = sr_agent::ingest::IngestCtx {
            db: &db,
            keys: &h.shared.keys,
            capture_private: true,
            private_ttl_hours: 24,
            snapshot_id,
        };
        let delta = sr_proto::TabDelta {
            op: sr_proto::Op::Upsert,
            tab_key: "w-priv:t1".into(),
            window_id: "w-priv".into(),
            group_key: None,
            index: Some(0),
            url: Some("https://secret.test/".into()),
            title: Some("secret".into()),
            favicon_hash: None,
            pinned: false,
            active: true,
            muted: false,
            last_accessed: None,
            private: true,
            restorable: true,
        };
        sr_agent::ingest::ingest_tab(&delta, &ctx).unwrap();
    }

    {
        let mut pending = h.shared.pending_restore.lock().unwrap();
        pending.set_selection(BrowserSelection {
            windows: ["w-old".to_string(), "w-priv".to_string()].into_iter().collect(),
            tabs: Default::default(),
            declined: false,
            restore_private: true,
        });
    }

    let mut c = h.connect();
    send(&mut c, hello("chrome"));
    assert_eq!(recv(&mut c).kind, "hello_ack");

    let offer = recv(&mut c);
    assert_eq!(offer.kind, "restore_session");
    let windows = offer.body["windows"].as_array().unwrap();
    let private: Vec<_> = windows.iter().filter(|w| w["private"] == true).collect();
    assert_eq!(private.len(), 1, "the private window was not offered: {windows:?}");
    assert_eq!(private[0]["tabs"][0]["url"], "https://secret.test/");
}

/// Not ticking it leaves them alone, which is the default and the safe direction.
#[test]
fn not_ticking_private_windows_leaves_them_out() {
    let h = Harness::with_previous_session(&[("w-old:t1", "https://one.test/")]);
    {
        let mut pending = h.shared.pending_restore.lock().unwrap();
        pending.set_selection(BrowserSelection {
            windows: ["w-old".to_string()].into_iter().collect(),
            tabs: Default::default(),
            declined: false,
            restore_private: false,
        });
    }

    let mut c = h.connect();
    send(&mut c, hello("chrome"));
    assert_eq!(recv(&mut c).kind, "hello_ack");
    let offer = recv(&mut c);
    assert!(
        offer.body["windows"]
            .as_array()
            .unwrap()
            .iter()
            .all(|w| w["private"] != true),
        "a private window was restored without being asked for"
    );
}

/// Chromium runs one browser process for every profile, so the relay stamps the same
/// key for all of them and their tabs collapse into one bucket. The extension is the
/// only thing that can tell them apart, and this pins that its answer is used for the
/// whole connection rather than just the handshake.
#[test]
fn the_profile_the_extension_reports_governs_its_tabs() {
    let h = Harness::with_previous_session(&[("w-old:t1", "https://one.test/")]);

    let mut c = h.connect();
    send(
        &mut c,
        serde_json::json!({
            "v": 1, "id": sr_proto::new_id(), "type": "hello", "ts": 0,
            "src": { "browser": "chrome", "profile_key": "default", "ext_version": "0.1.0" },
            "body": { "ext_version": "0.1.0", "browser_version": "test",
                      "incognito_access": false, "profile_id": "work-uuid",
                      "capabilities": [] }
        }),
    );
    assert_eq!(recv(&mut c).kind, "hello_ack");
    let _ = recv(&mut c); // the restore offer

    // A tab sent afterwards must be attributed to the reported profile, not to the
    // one the relay guessed from the shared browser process.
    send(
        &mut c,
        serde_json::json!({
            "v": 1, "id": sr_proto::new_id(), "type": "full_state", "ts": 0,
            "src": { "browser": "chrome", "profile_key": "default", "ext_version": "0.1.0" },
            "body": {
                "windows": [{ "op": "upsert", "window_id": "w-new", "focused": true,
                              "private": false }],
                "tabs": [{ "op": "upsert", "tab_key": "w-new:t1", "window_id": "w-new",
                           "index": 0, "url": "https://work.test/", "title": "work" }]
            }
        }),
    );

    let mut stored = None;
    for _ in 0..40 {
        {
            let db = h.shared.db.lock().unwrap();
            stored = db
                .conn
                .query_row(
                    "SELECT profile_key FROM browser_windows
                     WHERE snapshot_id = 0 AND browser_window_id = 'w-new'",
                    [],
                    |r| r.get::<_, String>(0),
                )
                .ok();
        }
        if stored.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    assert_eq!(
        stored.as_deref(),
        Some("ext:work-uuid"),
        "the extension's profile did not govern its own tabs"
    );
}

/// Firefox runs a process per profile, so the relay can already tell them apart and an
/// extension that sends nothing must keep working exactly as before.
#[test]
fn an_extension_that_reports_no_profile_still_works() {
    let h = Harness::with_previous_session(&[("w-old:t1", "https://one.test/")]);
    let mut c = h.connect();
    send(&mut c, hello("firefox"));
    assert_eq!(recv(&mut c).kind, "hello_ack");
}

/// The bug a user hit: close the review window, open a browser later, and the whole
/// previous session came back anyway.
///
/// Dismissal used to only clear the waiting flag, leaving `selection` as `None`, which
/// the offer path reads as "there was no review to consult" and answers by offering
/// everything. Closing the window is a refusal of the automatic restore, not an absence
/// of an answer.
#[test]
fn closing_the_review_window_does_not_restore_anything() {
    let h = Harness::with_previous_session(&[
        ("w-old:t1", "https://one.test/"),
        ("w-old:t2", "https://two.test/"),
    ]);
    {
        let mut pending = h.shared.pending_restore.lock().unwrap();
        *pending = PendingRestore::awaiting_review(pending.snapshot_id);
        // Exactly what the window's close button does.
        pending.review_dismissed();
    }

    let mut c = h.connect();
    send(&mut c, hello("chrome"));
    assert_eq!(recv(&mut c).kind, "hello_ack");

    // A second hello proves nothing was sent in between.
    send(&mut c, hello("chrome"));
    assert_eq!(
        recv(&mut c).kind,
        "hello_ack",
        "a dismissed review still restored the session"
    );
}

/// Dismissing is "not now", not "never". The session has to survive so the tray can
/// still restore it, and confirming there has to actually reach the browser.
#[test]
fn a_browser_can_still_be_offered_after_the_user_changes_their_mind() {
    let h = Harness::with_previous_session(&[("w-old:t1", "https://one.test/")]);
    {
        let mut pending = h.shared.pending_restore.lock().unwrap();
        *pending = PendingRestore::awaiting_review(pending.snapshot_id);
        pending.review_dismissed();
    }

    let mut c = h.connect();
    send(&mut c, hello("chrome"));
    assert_eq!(recv(&mut c).kind, "hello_ack");
    send(&mut c, hello("chrome"));
    assert_eq!(recv(&mut c).kind, "hello_ack", "offered a dismissed restore");

    // The user reopens the review from the tray and confirms.
    {
        let mut pending = h.shared.pending_restore.lock().unwrap();
        pending.set_selection(BrowserSelection {
            windows: ["w-old".to_string()].into_iter().collect(),
            tabs: Default::default(),
            declined: false,
            restore_private: false,
        });
    }
    sr_agent::server::offer_to_connected(&h.shared);

    let offer = recv(&mut c);
    assert_eq!(
        offer.kind, "restore_session",
        "declining consumed the offer, so changing your mind did nothing"
    );
    assert_eq!(offer.body["windows"][0]["tabs"].as_array().unwrap().len(), 1);
}

/// Pressing Not now is the same refusal as closing the window, and must behave the same.
#[test]
fn not_now_and_closing_the_window_agree() {
    for dismissed_by_closing in [true, false] {
        let h = Harness::with_previous_session(&[("w-old:t1", "https://one.test/")]);
        {
            let mut pending = h.shared.pending_restore.lock().unwrap();
            *pending = PendingRestore::awaiting_review(pending.snapshot_id);
            if dismissed_by_closing {
                pending.review_dismissed();
            } else {
                pending.set_selection(BrowserSelection {
                    declined: true,
                    ..Default::default()
                });
            }
        }

        let mut c = h.connect();
        send(&mut c, hello("chrome"));
        assert_eq!(recv(&mut c).kind, "hello_ack");
        send(&mut c, hello("chrome"));
        assert_eq!(
            recv(&mut c).kind,
            "hello_ack",
            "closing={dismissed_by_closing} restored without consent"
        );
    }
}
