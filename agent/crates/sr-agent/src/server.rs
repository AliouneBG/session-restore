//! The agent's pipe server: accepts relay connections and applies what they send.
//!
//! One thread per connection. The database sits behind a mutex because every write is
//! serialized by SQLite anyway, so a lock costs nothing that WAL was not already
//! costing, and it keeps the ingest path straightforwardly synchronous.

use crate::ingest::{ingest_tab, IngestCtx};
use crate::store::db::{Db, LIVE};
use crate::store::keys::KeyManager;
use crate::AGENT_VERSION;
use anyhow::Result;
use sr_proto::frame::{read_frame, write_frame, FrameError, MAX_INBOUND_BYTES, MAX_OUTBOUND_BYTES};
use sr_proto::{Envelope, HelloAckBody, Op, StateBody};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

pub struct Shared {
    /// Shared with the UI thread, which must see the same database instance - two
    /// connections to one SQLite file would give the tray a stale view.
    pub db: Arc<Mutex<Db>>,
    pub keys: Arc<KeyManager>,
    /// The snapshot captured at startup, and who has already been offered it.
    pub pending_restore: Mutex<PendingRestore>,
}

/// Tracks the one restore offer this agent run may make per browser and profile.
///
/// The guard matters more than it looks: MV3 service workers reconnect constantly, and
/// every reconnect sends a fresh `hello`. Offering on each one would re-send the same
/// session repeatedly. The extension's diff would suppress most of the damage, but
/// "mostly idempotent" is not a property to lean on when the failure mode is duplicate
/// tabs.
#[derive(Default)]
pub struct PendingRestore {
    pub snapshot_id: Option<i64>,
    offered: std::collections::HashSet<String>,
}

impl PendingRestore {
    pub fn new(snapshot_id: Option<i64>) -> Self {
        PendingRestore {
            snapshot_id,
            offered: std::collections::HashSet::new(),
        }
    }

    /// Claims the offer for one browser/profile, yielding the snapshot to restore.
    /// Returns `None` when nothing is pending or it has already been offered.
    fn claim(&mut self, browser: &str, profile: &str) -> Option<i64> {
        let id = self.snapshot_id?;
        if !self.offered.insert(format!("{browser}\u{1}{profile}")) {
            return None;
        }
        Some(id)
    }
}

/// Accepts connections forever on the current user's pipe. Each gets its own thread.
pub fn serve(shared: Arc<Shared>) -> Result<()> {
    serve_on(&sr_ipc::pipe_name()?, shared)
}

/// Accepts connections forever on an explicit pipe name.
///
/// Exposed so integration tests can run a server on a unique name instead of racing
/// a real agent for the user's pipe.
pub fn serve_on(name: &str, shared: Arc<Shared>) -> Result<()> {
    tracing::info!(pipe = %name, "listening");

    let mut first = true;
    loop {
        // `first` claims the name with FILE_FLAG_FIRST_PIPE_INSTANCE, locking out
        // squatters for the lifetime of the process (ADR-0002).
        let conn = match sr_ipc::accept_one(name, first) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(error = %e, "accept failed");
                std::thread::sleep(std::time::Duration::from_secs(1));
                continue;
            }
        };
        first = false;

        let shared = Arc::clone(&shared);
        std::thread::spawn(move || {
            if let Err(e) = handle_connection(conn, shared) {
                tracing::warn!(error = %e, "connection ended");
            }
        });
    }
}

fn handle_connection(conn: sr_ipc::PipeConnection, shared: Arc<Shared>) -> Result<()> {
    tracing::debug!("connection accepted");
    let mut writer = conn.try_clone()?;
    let mut reader = conn;
    tracing::debug!("connection split for read/write");

    loop {
        let raw = match read_frame(&mut reader, MAX_INBOUND_BYTES) {
            Ok(r) => {
                tracing::debug!(bytes = r.len(), "frame read");
                r
            }
            Err(FrameError::Closed) => {
                tracing::debug!("relay disconnected");
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        };

        let env: Envelope = match serde_json::from_str(&raw) {
            Ok(e) => e,
            Err(e) => {
                // One malformed message must not kill the connection.
                tracing::warn!(error = %e, "undecodable message, ignoring");
                continue;
            }
        };

        if env.is_too_new() {
            let reply = Envelope::new(
                "version_too_new",
                serde_json::json!({ "agent_protocol": sr_proto::PROTOCOL_VERSION }),
            );
            write_frame(&mut writer, &serde_json::to_string(&reply)?, MAX_OUTBOUND_BYTES)?;
            continue;
        }

        // A message we cannot handle must never end the session. Before this was
        // separated out, one unexpected field type in one tab (Chrome reports
        // `lastAccessed` as a fractional number) propagated out of dispatch and closed
        // the connection, so the extension silently stopped syncing.
        match dispatch(&env, &shared) {
            Ok(replies) => {
                for reply in replies {
                    write_frame(&mut writer, &serde_json::to_string(&reply)?, MAX_OUTBOUND_BYTES)?;
                }
            }
            Err(e) => {
                tracing::warn!(kind = %env.kind, error = %e, "message could not be handled");
            }
        }
    }
}

fn dispatch(env: &Envelope, shared: &Shared) -> Result<Vec<Envelope>> {
    let browser = env
        .src
        .as_ref()
        .map(|s| s.browser.as_str())
        .unwrap_or("unknown");
    let profile = env
        .src
        .as_ref()
        .map(|s| s.profile_key.as_str())
        .unwrap_or("default");

    // Message kind and counts only - never URLs or titles (docs/06).
    tracing::debug!(kind = %env.kind, browser, "message received");

    match env.kind.as_str() {
        "hello" => {
            let db = shared.db.lock().unwrap();
            let capture_private = db.setting_bool("capture_private_windows", false);
            let incognito_access = env
                .body
                .get("incognito_access")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            tracing::info!(
                browser,
                incognito_access,
                capture_private,
                "extension connected"
            );

            let ack = HelloAckBody {
                agent_version: AGENT_VERSION.to_string(),
                protocol_version: sr_proto::PROTOCOL_VERSION,
                capture_enabled: db.setting_bool("capture_enabled", true),
                // Tells the extension to drop private events at the source. The
                // cheapest enforcement point is the earliest one (docs/06).
                capture_private,
                reconcile_interval_s: db.setting_i64("reconcile_interval_seconds", 60) as u32,
            };
            let mut out = vec![Envelope::new("hello_ack", serde_json::to_value(ack)?)];
            drop(db);

            match maybe_offer_restore(shared, browser, profile) {
                Ok(Some(offer)) => out.push(offer),
                Ok(None) => {}
                // A restore we cannot build must not stop capture from working.
                Err(e) => tracing::warn!(error = %e, "could not build a restore offer"),
            }
            Ok(out)
        }

        "tab_delta" => {
            let body: StateBody = serde_json::from_value(env.body.clone())?;
            tracing::debug!(
                tabs = body.tabs.len(),
                windows = body.windows.len(),
                "tab_delta"
            );
            apply_state(&body, shared, false, browser, profile)?;
            Ok(vec![])
        }

        "full_state" => {
            let body: StateBody = serde_json::from_value(env.body.clone())?;
            tracing::info!(
                tabs = body.tabs.len(),
                windows = body.windows.len(),
                browser,
                "reconcile"
            );
            apply_state(&body, shared, true, browser, profile)?;
            Ok(vec![])
        }

        "restore_result" => {
            let run_id = env.body.get("run_id").and_then(|v| v.as_i64()).unwrap_or(0);
            let empty = serde_json::json!([]);
            let items = env.body.get("items").unwrap_or(&empty);
            let db = shared.db.lock().unwrap();
            let (ok, failed) = crate::restore::apply_result(&db, run_id, items)?;
            tracing::info!(browser, run_id, restored = ok, failed, "restore finished");
            Ok(vec![])
        }

        other => {
            // Unknown types are ignored, not fatal: the extension may be newer.
            tracing::debug!(kind = other, "ignoring unknown message type");
            Ok(vec![])
        }
    }
}

/// Applies a batch of deltas.
///
/// When `authoritative` (a `full_state` reconcile), anything this browser previously
/// reported that is absent from the payload is deleted. That is what heals a service
/// worker eviction that swallowed `tabs.onRemoved` - without it, closed tabs would
/// linger forever (docs/01-architecture.md).
fn apply_state(
    body: &StateBody,
    shared: &Shared,
    authoritative: bool,
    browser: &str,
    profile: &str,
) -> Result<()> {
    let db = shared.db.lock().unwrap();
    let ctx = IngestCtx::live(&db, &shared.keys);

    for w in &body.windows {
        match w.op {
            Op::Remove => {
                db.conn.execute(
                    "DELETE FROM browser_windows WHERE snapshot_id = ?1 AND browser_window_id = ?2",
                    rusqlite::params![LIVE, w.window_id],
                )?;
            }
            Op::Upsert => {
                db.conn.execute(
                    "INSERT INTO browser_windows (snapshot_id, browser_window_id, browser,
                        profile_key, is_private, window_state, x, y, w, h, focused, updated_at)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)
                     ON CONFLICT(snapshot_id, browser_window_id) DO UPDATE SET
                       is_private = excluded.is_private, window_state = excluded.window_state,
                       x = excluded.x, y = excluded.y, w = excluded.w, h = excluded.h,
                       focused = excluded.focused, updated_at = excluded.updated_at",
                    rusqlite::params![
                        LIVE,
                        w.window_id,
                        browser,
                        profile,
                        w.private,
                        w.state.map(|s| format!("{s:?}").to_lowercase()),
                        w.x,
                        w.y,
                        w.w,
                        w.h,
                        w.focused,
                        sr_proto::now_millis(),
                    ],
                )?;
            }
        }
    }

    for t in &body.tabs {
        ingest_tab(t, &ctx)?;
    }

    for g in &body.groups {
        match g.op {
            Op::Remove => {
                db.conn.execute(
                    "DELETE FROM tab_groups WHERE snapshot_id = ?1 AND group_key = ?2",
                    rusqlite::params![LIVE, g.group_key],
                )?;
            }
            Op::Upsert => {
                db.conn.execute(
                    "INSERT INTO tab_groups (snapshot_id, group_key, browser_window_id,
                        title, color, collapsed)
                     VALUES (?1,?2,?3,?4,?5,?6)
                     ON CONFLICT(snapshot_id, group_key) DO UPDATE SET
                       browser_window_id = excluded.browser_window_id,
                       title = excluded.title, color = excluded.color,
                       collapsed = excluded.collapsed",
                    rusqlite::params![
                        LIVE,
                        g.group_key,
                        g.window_id,
                        g.title,
                        g.color,
                        g.collapsed
                    ],
                )?;
            }
        }
    }

    if authoritative {
        reap_missing_for_browser(&db, body, browser, profile)?;
    }

    Ok(())
}

/// Deletes stored tabs and windows the authoritative payload did not mention.
///
/// Scoped by **browser and profile**, not by window. Window ids are assigned fresh
/// every time a browser starts, so a restart produces an entirely new set of them and
/// the previous session's windows would otherwise linger forever - one phantom window
/// per browser restart, accumulating without limit.
///
/// Scoping to the reporting browser is what keeps Chrome's reconcile from deleting
/// Firefox's rows: each browser is authoritative only for itself.
fn reap_missing_for_browser(db: &Db, body: &StateBody, browser: &str, profile: &str) -> Result<()> {
    let live_windows: HashSet<&str> = body
        .windows
        .iter()
        .filter(|w| matches!(w.op, Op::Upsert))
        .map(|w| w.window_id.as_str())
        .collect();
    let live_tabs: HashSet<&str> = body
        .tabs
        .iter()
        .filter(|t| matches!(t.op, Op::Upsert))
        .map(|t| t.tab_key.as_str())
        .collect();

    // Every window this browser/profile currently has on record.
    let mut stmt = db.conn.prepare(
        "SELECT browser_window_id FROM browser_windows
         WHERE snapshot_id = ?1 AND browser = ?2 AND profile_key = ?3",
    )?;
    let owned: Vec<String> = stmt
        .query_map(rusqlite::params![LIVE, browser, profile], |r| r.get(0))?
        .filter_map(Result::ok)
        .collect();
    drop(stmt);

    for window_id in &owned {
        if live_windows.contains(window_id.as_str()) {
            // Window still exists: drop only the tabs it no longer reports.
            for table in ["tabs", "tabs_private"] {
                let sql = format!(
                    "SELECT tab_key FROM {table} WHERE snapshot_id = ?1 AND browser_window_id = ?2"
                );
                let mut s = db.conn.prepare(&sql)?;
                let keys: Vec<String> = s
                    .query_map(rusqlite::params![LIVE, window_id], |r| r.get(0))?
                    .filter_map(Result::ok)
                    .collect();
                drop(s);
                for k in keys {
                    if !live_tabs.contains(k.as_str()) {
                        db.conn.execute(
                            &format!("DELETE FROM {table} WHERE snapshot_id = ?1 AND tab_key = ?2"),
                            rusqlite::params![LIVE, k],
                        )?;
                    }
                }
            }
        } else {
            // Window is gone. Take its tabs and groups with it.
            for table in ["tabs", "tabs_private", "tab_groups"] {
                db.conn.execute(
                    &format!(
                        "DELETE FROM {table} WHERE snapshot_id = ?1 AND browser_window_id = ?2"
                    ),
                    rusqlite::params![LIVE, window_id],
                )?;
            }
            db.conn.execute(
                "DELETE FROM browser_windows WHERE snapshot_id = ?1 AND browser_window_id = ?2",
                rusqlite::params![LIVE, window_id],
            )?;
        }
    }

    Ok(())
}


/// Builds a `restore_session` offer for a browser that has just connected, if this
/// agent run still owes one.
///
/// Only non-private windows. Private windows are never part of an automatic offer -
/// they require an explicit, current confirmation (ADR-0004), which this code path has
/// no way to obtain.
fn maybe_offer_restore(shared: &Shared, browser: &str, profile: &str) -> Result<Option<Envelope>> {
    let snapshot_id = {
        let mut pending = shared.pending_restore.lock().unwrap();
        match pending.claim(browser, profile) {
            Some(id) => id,
            None => return Ok(None),
        }
    };

    let db = shared.db.lock().unwrap();

    let mode = db
        .setting("restore_mode")
        .ok()
        .flatten()
        .unwrap_or_else(|| "ask".to_string());
    if mode == "off" {
        tracing::info!("restore is disabled; not offering");
        return Ok(None);
    }

    let run_id = crate::restore::begin_run(&db, snapshot_id, &mode)?;
    let body = crate::restore::build_payload(&db, snapshot_id, browser, profile, run_id)?;

    if body.windows.is_empty() {
        tracing::debug!(browser, "nothing stored for this browser; no offer");
        return Ok(None);
    }

    let tabs: usize = body.windows.iter().map(|w| w.tabs.len()).sum();
    crate::restore::record_items(&db, run_id, &body)?;
    tracing::info!(
        browser,
        run_id,
        windows = body.windows.len(),
        tabs,
        snapshot_id,
        "offering restore"
    );

    Ok(Some(Envelope::new(
        "restore_session",
        serde_json::to_value(body)?,
    )))
}
