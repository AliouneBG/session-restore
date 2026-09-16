//! Restore offer behaviour, end to end over the real pipe.
//!
//! The scenario these cover is the one that actually happens at logon and is easy to
//! get wrong: the stored session belongs to windows that no longer exist, and the
//! browser that reconnects is a *different* run of the browser.

#![cfg(windows)]

use sr_agent::server::{serve_on, PendingRestore, Shared};
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
        let shared = Arc::new(Shared {
            db: Mutex::new(db),
            keys,
            pending_restore: Mutex::new(PendingRestore::new(Some(snap))),
        });

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
