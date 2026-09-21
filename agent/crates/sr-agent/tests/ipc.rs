//! End-to-end IPC over a real named pipe.
//!
//! This is the M0 walking-skeleton acceptance test: a client connects over the actual
//! Windows named pipe transport, speaks the real wire protocol, and the agent's real
//! dispatch and ingest paths run. No mocks of the transport - the whole point is to
//! exercise the part most likely to be subtly wrong.

#![cfg(windows)]

use sr_agent::server::{serve_on, PendingRestore, Shared};
use sr_agent::store::db::Db;
use sr_agent::store::keys::KeyManager;
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
    fn start() -> Harness {
        let dir = std::env::temp_dir().join(format!("sr-ipc-{}", sr_proto::new_id()));
        std::fs::create_dir_all(&dir).unwrap();

        let db = Db::open(&dir.join("sessions.db")).unwrap();
        let keys = KeyManager::new(&dir);
        let shared = Arc::new(Shared::new(
            Arc::new(Mutex::new(db)),
            Arc::new(keys),
            PendingRestore::new(None),
        ));

        // Unique name per test so a real agent (or a parallel test) is never contended.
        let pipe = format!("\\\\.\\pipe\\SessionRestoreTest.{}", sr_proto::new_id());

        let server_shared = Arc::clone(&shared);
        let server_pipe = pipe.clone();
        std::thread::spawn(move || {
            let _ = serve_on(&server_pipe, server_shared);
        });

        // Give the server a moment to create the first pipe instance.
        std::thread::sleep(Duration::from_millis(300));

        Harness { dir, pipe, shared }
    }

    fn connect(&self) -> sr_ipc::PipeConnection {
        let mut last = None;
        for _ in 0..40 {
            match sr_ipc::connect(&self.pipe) {
                Ok(c) => return c,
                Err(e) => {
                    last = Some(e);
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
        panic!("could not connect to {}: {:?}", self.pipe, last);
    }

    fn tab_count(&self) -> i64 {
        self.shared
            .db
            .lock()
            .unwrap()
            .conn
            .query_row("SELECT COUNT(*) FROM tabs", [], |r| r.get(0))
            .unwrap()
    }

    fn has_tab_url(&self, url: &str) -> bool {
        let n: i64 = self
            .shared
            .db
            .lock()
            .unwrap()
            .conn
            .query_row("SELECT COUNT(*) FROM tabs WHERE url = ?1", [url], |r| r.get(0))
            .unwrap();
        n > 0
    }

    fn window_count(&self) -> i64 {
        self.shared
            .db
            .lock()
            .unwrap()
            .conn
            .query_row("SELECT COUNT(*) FROM browser_windows", [], |r| r.get(0))
            .unwrap()
    }

    fn private_count(&self) -> i64 {
        self.shared
            .db
            .lock()
            .unwrap()
            .conn
            .query_row("SELECT COUNT(*) FROM tabs_private", [], |r| r.get(0))
            .unwrap()
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn send(conn: &mut sr_ipc::PipeConnection, msg: serde_json::Value) {
    write_frame(conn, &msg.to_string(), MAX_OUTBOUND_BYTES).unwrap();
}

fn recv(conn: &mut sr_ipc::PipeConnection) -> Envelope {
    let raw = read_frame(conn, MAX_INBOUND_BYTES).unwrap();
    serde_json::from_str(&raw).unwrap()
}

fn hello() -> serde_json::Value {
    serde_json::json!({
        "v": 1, "id": "m1", "type": "hello", "ts": 0,
        "src": { "browser": "chrome", "profile_key": "default", "ext_version": "0.1.0" },
        "body": {
            "ext_version": "0.1.0",
            "browser_version": "141.0",
            "incognito_access": true,
            "capabilities": ["tab_groups"]
        }
    })
}

#[test]
fn hello_gets_an_ack_over_a_real_pipe() {
    let h = Harness::start();
    let mut c = h.connect();

    send(&mut c, hello());
    let ack = recv(&mut c);

    assert_eq!(ack.kind, "hello_ack");
    assert_eq!(ack.v, sr_proto::PROTOCOL_VERSION);
    assert_eq!(ack.body["protocol_version"], 1);
    assert_eq!(ack.body["capture_enabled"], true);
    // Private capture is off by default, and the ack is how the extension learns to
    // drop private events at the source.
    assert_eq!(ack.body["capture_private"], false);
    assert_eq!(ack.body["reconcile_interval_s"], 60);
}

#[test]
fn ack_reports_private_capture_once_enabled() {
    let h = Harness::start();
    h.shared
        .db
        .lock()
        .unwrap()
        .set_setting("capture_private_windows", "true")
        .unwrap();

    let mut c = h.connect();
    send(&mut c, hello());
    assert_eq!(recv(&mut c).body["capture_private"], true);
}

#[test]
fn tab_delta_reaches_the_database() {
    let h = Harness::start();
    let mut c = h.connect();
    send(&mut c, hello());
    let _ = recv(&mut c);

    send(
        &mut c,
        serde_json::json!({
            "v": 1, "id": "m2", "type": "tab_delta", "ts": 0,
            "src": { "browser": "chrome", "profile_key": "default", "ext_version": "0.1.0" },
            "body": {
                "windows": [{ "op": "upsert", "window_id": "w1", "state": "maximized",
                              "x": 0, "y": 0, "w": 2560, "h": 1440, "focused": true }],
                "tabs": [
                    { "op": "upsert", "tab_key": "w1:t1", "window_id": "w1", "index": 0,
                      "url": "https://example.test/a", "title": "A", "active": true },
                    { "op": "upsert", "tab_key": "w1:t2", "window_id": "w1", "index": 1,
                      "url": "https://example.test/b", "title": "B" }
                ],
                "groups": []
            }
        }),
    );

    // tab_delta draws no reply, so poll until the write lands.
    for _ in 0..40 {
        if h.tab_count() == 2 {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(h.tab_count(), 2);

    let url: String = h
        .shared
        .db
        .lock()
        .unwrap()
        .conn
        .query_row("SELECT url FROM tabs WHERE tab_key = 'w1:t1'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(url, "https://example.test/a");
}

#[test]
fn private_tabs_are_dropped_over_the_wire_when_capture_is_off() {
    let h = Harness::start();
    let mut c = h.connect();
    send(&mut c, hello());
    let _ = recv(&mut c);

    send(
        &mut c,
        serde_json::json!({
            "v": 1, "id": "m3", "type": "tab_delta", "ts": 0,
            "src": { "browser": "chrome", "profile_key": "default", "ext_version": "0.1.0" },
            "body": {
                "windows": [{ "op": "upsert", "window_id": "wp", "private": true }],
                "tabs": [{ "op": "upsert", "tab_key": "wp:t1", "window_id": "wp", "index": 0,
                           "url": "https://should-not-persist.test/x", "private": true }],
                "groups": []
            }
        }),
    );
    std::thread::sleep(Duration::from_millis(400));

    assert_eq!(h.private_count(), 0, "stored a private tab while capture was off");
    assert_eq!(h.tab_count(), 0, "private tab leaked into the plaintext table");
}

#[test]
fn full_state_reaps_tabs_that_closed_while_we_were_not_looking() {
    // The reconcile guarantee: a service worker evicted mid-session misses
    // tabs.onRemoved, so without this the closed tab would linger forever.
    let h = Harness::start();
    let mut c = h.connect();
    send(&mut c, hello());
    let _ = recv(&mut c);

    let two_tabs = serde_json::json!({
        "v": 1, "id": "m4", "type": "tab_delta", "ts": 0,
        "src": { "browser": "chrome", "profile_key": "default", "ext_version": "0.1.0" },
        "body": {
            "windows": [{ "op": "upsert", "window_id": "w1" }],
            "tabs": [
                { "op": "upsert", "tab_key": "w1:t1", "window_id": "w1", "index": 0,
                  "url": "https://example.test/keep" },
                { "op": "upsert", "tab_key": "w1:t2", "window_id": "w1", "index": 1,
                  "url": "https://example.test/closed-behind-our-back" }
            ],
            "groups": []
        }
    });
    send(&mut c, two_tabs);
    for _ in 0..40 {
        if h.tab_count() == 2 {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(h.tab_count(), 2);

    // Reconcile reports only one tab still open. No explicit removal was ever sent.
    send(
        &mut c,
        serde_json::json!({
            "v": 1, "id": "m5", "type": "full_state", "ts": 0,
            "src": { "browser": "chrome", "profile_key": "default", "ext_version": "0.1.0" },
            "body": {
                "windows": [{ "op": "upsert", "window_id": "w1" }],
                "tabs": [{ "op": "upsert", "tab_key": "w1:t1", "window_id": "w1", "index": 0,
                           "url": "https://example.test/keep" }],
                "groups": []
            }
        }),
    );
    for _ in 0..40 {
        if h.tab_count() == 1 {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(h.tab_count(), 1, "reconcile did not reap the closed tab");

    let survivor: String = h
        .shared
        .db
        .lock()
        .unwrap()
        .conn
        .query_row("SELECT tab_key FROM tabs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(survivor, "w1:t1");
}

#[test]
fn a_message_from_a_newer_protocol_is_refused_not_guessed_at() {
    let h = Harness::start();
    let mut c = h.connect();

    send(
        &mut c,
        serde_json::json!({
            "v": 99, "id": "m6", "type": "hello", "ts": 0,
            "body": { "ext_version": "9", "browser_version": "9",
                      "incognito_access": false, "capabilities": [] }
        }),
    );

    assert_eq!(recv(&mut c).kind, "version_too_new");
}

#[test]
fn a_malformed_message_does_not_kill_the_connection() {
    let h = Harness::start();
    let mut c = h.connect();

    // Valid framing, unusable payload.
    send(&mut c, serde_json::json!({ "nonsense": true }));
    // The connection must still serve the next message.
    send(&mut c, hello());
    assert_eq!(recv(&mut c).kind, "hello_ack");
}

#[test]
fn unknown_message_types_are_ignored_rather_than_fatal() {
    let h = Harness::start();
    let mut c = h.connect();

    send(
        &mut c,
        serde_json::json!({
            "v": 1, "id": "m7", "type": "some_future_message", "ts": 0, "body": { "x": 1 }
        }),
    );
    send(&mut c, hello());
    assert_eq!(recv(&mut c).kind, "hello_ack");
}

#[test]
fn a_tab_with_a_fractional_timestamp_is_stored() {
    // Regression, found only by running a real browser: Chrome reports
    // tab.lastAccessed as fractional milliseconds.
    let h = Harness::start();
    let mut c = h.connect();
    send(&mut c, hello());
    let _ = recv(&mut c);

    send(
        &mut c,
        serde_json::json!({
            "v": 1, "id": "mf", "type": "full_state", "ts": 0,
            "src": { "browser": "chrome", "profile_key": "default", "ext_version": "0.1.0" },
            "body": {
                "windows": [{ "op": "upsert", "window_id": "w1" }],
                "tabs": [{ "op": "upsert", "tab_key": "w1:t1", "window_id": "w1", "index": 0,
                           "url": "https://example.test/a",
                           "last_accessed": 1789521076302.926 }],
                "groups": []
            }
        }),
    );

    for _ in 0..40 {
        if h.tab_count() == 1 { break; }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(h.tab_count(), 1, "a fractional timestamp lost the whole batch");
}

#[test]
fn an_undecodable_body_does_not_end_the_session() {
    // The severe half of the same bug: a body that fails to decode used to propagate
    // out and close the connection, so the extension silently stopped syncing.
    let h = Harness::start();
    let mut c = h.connect();
    send(&mut c, hello());
    let _ = recv(&mut c);

    send(
        &mut c,
        serde_json::json!({
            "v": 1, "id": "mbad", "type": "full_state", "ts": 0,
            "src": { "browser": "chrome", "profile_key": "default", "ext_version": "0.1.0" },
            "body": { "windows": "not-an-array", "tabs": 42, "groups": null }
        }),
    );

    // The connection must still be usable afterwards.
    send(&mut c, hello());
    assert_eq!(recv(&mut c).kind, "hello_ack");

    // And real data must still flow.
    send(
        &mut c,
        serde_json::json!({
            "v": 1, "id": "mok", "type": "full_state", "ts": 0,
            "src": { "browser": "chrome", "profile_key": "default", "ext_version": "0.1.0" },
            "body": {
                "windows": [{ "op": "upsert", "window_id": "w1" }],
                "tabs": [{ "op": "upsert", "tab_key": "w1:t1", "window_id": "w1",
                           "index": 0, "url": "https://example.test/after" }],
                "groups": []
            }
        }),
    );
    for _ in 0..40 {
        if h.tab_count() == 1 { break; }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(h.tab_count(), 1, "session did not survive an undecodable body");
}

#[test]
fn a_client_can_read_and_write_concurrently() {
    // Regression for a deadlock no other test could catch, because every other test
    // reads and writes from one thread.
    //
    // The relay genuinely needs both at once: a pump thread reads pushes from the
    // agent while the main thread forwards messages to it. On a handle opened for
    // SYNCHRONOUS i/o, all operations on the underlying file object are serialized -
    // and DuplicateHandle shares that file object rather than making a new one. So a
    // parked ReadFile blocked the WriteFile behind it and the two processes waited on
    // each other forever, over four bytes. The fix is FILE_FLAG_OVERLAPPED.
    use std::sync::mpsc;

    let h = Harness::start();
    let mut writer = h.connect();
    let mut reader = writer.try_clone().unwrap();

    // Park a reader before writing anything, which is what the relay does.
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let got = read_frame(&mut reader, MAX_INBOUND_BYTES);
        let _ = tx.send(got.map(|s| s.len()).unwrap_or(0));
    });
    std::thread::sleep(Duration::from_millis(200));

    // With synchronous handles this write never completes.
    send(&mut writer, hello());

    let len = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("deadlocked: the reply never arrived while a read was pending");
    assert!(len > 0, "expected a hello_ack frame, got an empty read");
}

fn state_msg(id: &str, browser: &str, windows: serde_json::Value, tabs: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "v": 1, "id": id, "type": "full_state", "ts": 0,
        "src": { "browser": browser, "profile_key": "default", "ext_version": "0.1.0" },
        "body": { "windows": windows, "tabs": tabs, "groups": [] }
    })
}

#[test]
fn a_browser_restart_does_not_leave_a_phantom_window() {
    // Window ids are assigned fresh on every browser start, so a restart reports an
    // entirely new set. Reaping scoped to "windows the payload mentioned" could never
    // catch the old ones, and each restart leaked one phantom window forever.
    let h = Harness::start();
    let mut c = h.connect();
    send(&mut c, hello());
    let _ = recv(&mut c);

    send(&mut c, state_msg("r1", "chrome",
        serde_json::json!([{ "op": "upsert", "window_id": "w-old" }]),
        serde_json::json!([{ "op": "upsert", "tab_key": "w-old:t1", "window_id": "w-old",
                             "index": 0, "url": "https://before-restart.test/" }])));
    for _ in 0..40 {
        if h.tab_count() == 1 { break; }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(h.tab_count(), 1);

    // Browser restarts: brand new window id, nothing in common with the old one.
    send(&mut c, state_msg("r2", "chrome",
        serde_json::json!([{ "op": "upsert", "window_id": "w-new" }]),
        serde_json::json!([{ "op": "upsert", "tab_key": "w-new:t1", "window_id": "w-new",
                             "index": 0, "url": "https://after-restart.test/" }])));
    // Poll on the expected *content*: counts alone were already satisfied by the
    // pre-restart state, so waiting on them raced past the message under test.
    for _ in 0..60 {
        if h.has_tab_url("https://after-restart.test/") && h.tab_count() == 1 { break; }
        std::thread::sleep(Duration::from_millis(50));
    }

    assert_eq!(h.window_count(), 1, "the pre-restart window was never reaped");
    assert_eq!(h.tab_count(), 1, "the pre-restart tab was never reaped");
    let url: String = h.shared.db.lock().unwrap().conn
        .query_row("SELECT url FROM tabs", [], |r| r.get(0)).unwrap();
    assert_eq!(url, "https://after-restart.test/");
}

#[test]
fn one_browser_reconcile_does_not_reap_another_browsers_tabs() {
    // The reason reaping is scoped at all: each browser is authoritative only for
    // itself. A Chrome reconcile must not delete what Firefox reported.
    let h = Harness::start();
    let mut c = h.connect();
    send(&mut c, hello());
    let _ = recv(&mut c);

    send(&mut c, state_msg("f1", "firefox",
        serde_json::json!([{ "op": "upsert", "window_id": "ff-w1" }]),
        serde_json::json!([{ "op": "upsert", "tab_key": "ff-w1:t1", "window_id": "ff-w1",
                             "index": 0, "url": "https://firefox-only.test/" }])));
    for _ in 0..40 {
        if h.tab_count() == 1 { break; }
        std::thread::sleep(Duration::from_millis(50));
    }

    // Chrome reconciles with a completely different window set.
    send(&mut c, state_msg("c1", "chrome",
        serde_json::json!([{ "op": "upsert", "window_id": "ch-w1" }]),
        serde_json::json!([{ "op": "upsert", "tab_key": "ch-w1:t1", "window_id": "ch-w1",
                             "index": 0, "url": "https://chrome-only.test/" }])));
    for _ in 0..40 {
        if h.tab_count() == 2 { break; }
        std::thread::sleep(Duration::from_millis(50));
    }

    assert_eq!(h.tab_count(), 2, "a reconcile from one browser reaped another's tabs");
}

#[test]
fn several_browsers_can_be_connected_at_once() {
    let h = Harness::start();
    let mut chrome = h.connect();
    let mut firefox = h.connect();

    send(&mut chrome, hello());
    let mut ff_hello = hello();
    ff_hello["src"]["browser"] = serde_json::json!("firefox");
    send(&mut firefox, ff_hello);

    assert_eq!(recv(&mut chrome).kind, "hello_ack");
    assert_eq!(recv(&mut firefox).kind, "hello_ack");
}

/// A window that already existed must adopt the profile its extension now reports.
///
/// The upsert used to leave `profile_key` alone on conflict. When the key improved,
/// from a guess derived from the browser process to the profile's own id, existing
/// windows kept the stale one. One profile's session then sat under two keys, and both
/// the restore offer and the reconcile reap are scoped by profile, so half of it became
/// unreachable and could never be cleaned up.
#[test]
fn an_existing_window_adopts_the_profile_its_extension_reports() {
    let h = Harness::start();

    // First contact, before the extension reported a profile of its own.
    let mut c = h.connect();
    send(&mut c, window_state("w1", "old-key-from-the-process"));
    wait_for_window(&h, "w1", "old-key-from-the-process");

    // The same window, now reported by an extension that knows its profile.
    send(&mut c, window_state("w1", "ext:a-real-profile-id"));
    wait_for_window(&h, "w1", "ext:a-real-profile-id");
}

fn window_state(window_id: &str, profile: &str) -> serde_json::Value {
    serde_json::json!({
        "v": 1, "id": sr_proto::new_id(), "type": "full_state", "ts": 0,
        "src": { "browser": "chrome", "profile_key": profile, "ext_version": "0.1.0" },
        "body": {
            "windows": [{ "op": "upsert", "window_id": window_id, "focused": true,
                          "private": false }],
            "tabs": []
        }
    })
}

fn wait_for_window(h: &Harness, window_id: &str, expected_profile: &str) {
    for _ in 0..60 {
        {
            let db = h.shared.db.lock().unwrap();
            let got: Option<String> = db
                .conn
                .query_row(
                    "SELECT profile_key FROM browser_windows
                     WHERE snapshot_id = 0 AND browser_window_id = ?1",
                    [window_id],
                    |r| r.get(0),
                )
                .ok();
            if got.as_deref() == Some(expected_profile) {
                return;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let db = h.shared.db.lock().unwrap();
    let got: Option<String> = db
        .conn
        .query_row(
            "SELECT profile_key FROM browser_windows WHERE snapshot_id = 0 AND browser_window_id = ?1",
            [window_id],
            |r| r.get(0),
        )
        .ok();
    panic!("window {window_id} is under {got:?}, expected {expected_profile}");
}
