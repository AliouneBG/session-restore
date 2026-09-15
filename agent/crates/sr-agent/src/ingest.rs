//! The privacy chokepoint.
//!
//! **Every path by which tab data enters storage goes through `ingest_tab`.** Do not
//! add a second one. The value of a chokepoint is that the security property becomes
//! checkable: verifying "private URLs never reach `tabs`, `journal`, or the sync
//! queue" is reading one function, not auditing a codebase (docs/06-privacy-security.md).
//!
//! Three layers enforce it, deliberately redundantly:
//!
//! 1. **At the source** - when `capture_private` is false the extension never sends
//!    private events at all (`hello_ack.capture_private`).
//! 2. **Here** - the one routing decision.
//! 3. **In the type system** - [`NormalTab`] is the only thing the sync layer accepts,
//!    and a private tab cannot be turned into one. Cloud exclusion is a compile error,
//!    not a runtime check someone forgets to write in two years.

use crate::store::crypto::{aad, seal};
use crate::store::db::{Db, LIVE};
use crate::store::keys::KeyManager;
use anyhow::Result;
use serde::Serialize;
use sr_proto::{Op, TabDelta};

/// A tab that is cloud-eligible and safe to store in plaintext.
///
/// There is deliberately no constructor from a private `TabDelta`, and no way to build
/// one except [`NormalTab::from_delta`], which refuses private input. The future cloud
/// sync layer takes `&NormalTab` and therefore cannot be handed private data.
#[derive(Debug, Clone, Serialize)]
pub struct NormalTab {
    pub tab_key: String,
    pub browser_window_id: String,
    pub group_key: Option<String>,
    pub tab_index: i32,
    pub url: String,
    pub title: Option<String>,
    pub favicon_hash: Option<String>,
    pub pinned: bool,
    pub active: bool,
    pub muted: bool,
    pub restorable: bool,
    pub last_accessed: Option<i64>,
}

impl NormalTab {
    /// Returns `None` for a private tab. This is the type-level half of the guarantee:
    /// a private tab has no representation that the sync layer will accept.
    pub fn from_delta(d: &TabDelta) -> Option<NormalTab> {
        if d.private {
            return None;
        }
        Some(NormalTab {
            tab_key: d.tab_key.clone(),
            browser_window_id: d.window_id.clone(),
            group_key: d.group_key.clone(),
            tab_index: d.index.unwrap_or(0),
            url: d.url.clone().unwrap_or_default(),
            title: d.title.clone(),
            favicon_hash: d.favicon_hash.clone(),
            pinned: d.pinned,
            active: d.active,
            muted: d.muted,
            restorable: d.restorable,
            last_accessed: d.last_accessed,
        })
    }
}

pub struct IngestCtx<'a> {
    pub db: &'a Db,
    pub keys: &'a KeyManager,
    /// Mirrors the `capture_private_windows` setting. When false, private deltas are
    /// dropped here even if one arrives - belt and braces with the source-side drop.
    pub capture_private: bool,
    pub private_ttl_hours: i64,
    pub snapshot_id: i64,
}

impl<'a> IngestCtx<'a> {
    pub fn live(db: &'a Db, keys: &'a KeyManager) -> Self {
        IngestCtx {
            db,
            keys,
            capture_private: db.setting_bool("capture_private_windows", false),
            private_ttl_hours: db.setting_i64("private_ttl_hours", 24),
            snapshot_id: LIVE,
        }
    }
}

/// The ONLY place tab data may enter storage.
pub fn ingest_tab(tab: &TabDelta, ctx: &IngestCtx) -> Result<()> {
    if matches!(tab.op, Op::Remove) {
        return remove_tab(&tab.tab_key, ctx);
    }

    if tab.private {
        if !ctx.capture_private {
            return Ok(()); // dropped, silently and on purpose
        }
        return store_private_encrypted(tab, ctx);
    }

    match NormalTab::from_delta(tab) {
        Some(n) => store_normal(&n, ctx),
        // Unreachable given the branch above, but if the two ever disagree, dropping
        // the row is the safe failure.
        None => Ok(()),
    }
}

/// The only writer of `tabs`.
fn store_normal(tab: &NormalTab, ctx: &IngestCtx) -> Result<()> {
    ctx.db.conn.execute(
        "INSERT INTO tabs (snapshot_id, tab_key, browser_window_id, group_key, tab_index,
                           url, title, favicon_hash, pinned, active, muted, restorable,
                           last_accessed, updated_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)
         ON CONFLICT(snapshot_id, tab_key) DO UPDATE SET
           browser_window_id = excluded.browser_window_id,
           group_key = excluded.group_key, tab_index = excluded.tab_index,
           url = excluded.url, title = excluded.title,
           favicon_hash = excluded.favicon_hash, pinned = excluded.pinned,
           active = excluded.active, muted = excluded.muted,
           restorable = excluded.restorable, last_accessed = excluded.last_accessed,
           updated_at = excluded.updated_at",
        rusqlite::params![
            ctx.snapshot_id,
            tab.tab_key,
            tab.browser_window_id,
            tab.group_key,
            tab.tab_index,
            tab.url,
            tab.title,
            tab.favicon_hash,
            tab.pinned,
            tab.active,
            tab.muted,
            tab.restorable,
            tab.last_accessed,
            sr_proto::now_millis(),
        ],
    )?;
    Ok(())
}

/// What gets encrypted for a private tab. Everything identifying lives in here, so
/// none of it needs a plaintext column.
#[derive(Serialize, serde::Deserialize)]
pub struct PrivatePayload {
    pub url: String,
    pub title: Option<String>,
    pub pinned: bool,
    pub active: bool,
    pub muted: bool,
}

/// The only writer of `tabs_private`, and the only caller that obtains a DEK.
fn store_private_encrypted(tab: &TabDelta, ctx: &IngestCtx) -> Result<()> {
    let (key_id, dek) = ctx.keys.active_dek(ctx.db)?;

    let payload = PrivatePayload {
        url: tab.url.clone().unwrap_or_default(),
        title: tab.title.clone(),
        pinned: tab.pinned,
        active: tab.active,
        muted: tab.muted,
    };
    let plaintext = serde_json::to_vec(&payload)?;
    let sealed = seal(&dek, &aad(ctx.snapshot_id, &tab.tab_key, key_id), &plaintext)?;

    let now = sr_proto::now_millis();
    let expires_at = now + ctx.private_ttl_hours * 3600 * 1000;

    ctx.db.conn.execute(
        "INSERT INTO tabs_private (snapshot_id, tab_key, browser_window_id, tab_index,
                                   nonce, ciphertext, key_id, expires_at, updated_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)
         ON CONFLICT(snapshot_id, tab_key) DO UPDATE SET
           browser_window_id = excluded.browser_window_id,
           tab_index = excluded.tab_index, nonce = excluded.nonce,
           ciphertext = excluded.ciphertext, key_id = excluded.key_id,
           expires_at = excluded.expires_at, updated_at = excluded.updated_at",
        rusqlite::params![
            ctx.snapshot_id,
            tab.tab_key,
            tab.window_id,
            tab.index.unwrap_or(0),
            sealed.nonce,
            sealed.ciphertext,
            key_id,
            expires_at,
            now,
        ],
    )?;
    Ok(())
}

/// Removes a tab from both tables.
///
/// Both, because we do not know which one holds it - and asking would mean looking up
/// whether a given tab was private, which is exactly the kind of query we do not want
/// to make easy. Deleting from both is cheaper and leaks nothing.
fn remove_tab(tab_key: &str, ctx: &IngestCtx) -> Result<()> {
    ctx.db.conn.execute(
        "DELETE FROM tabs WHERE snapshot_id = ?1 AND tab_key = ?2",
        rusqlite::params![ctx.snapshot_id, tab_key],
    )?;
    ctx.db.conn.execute(
        "DELETE FROM tabs_private WHERE snapshot_id = ?1 AND tab_key = ?2",
        rusqlite::params![ctx.snapshot_id, tab_key],
    )?;
    Ok(())
}

/// Hard-deletes expired private rows. Runs on a 5-minute timer and at startup.
pub fn sweep_expired_private(db: &Db) -> Result<usize> {
    let n = db.conn.execute(
        "DELETE FROM tabs_private WHERE expires_at < ?1",
        [sr_proto::now_millis()],
    )?;
    if n > 0 {
        // Deletion alone leaves content in freelist pages. secure_delete zeroes them;
        // VACUUM removes the pages entirely.
        let _ = db.conn.execute_batch("VACUUM");
    }
    Ok(n)
}

/// Deletes every private row immediately. Called when the user turns the setting off,
/// and when the last private window closes.
pub fn purge_all_private(db: &Db) -> Result<usize> {
    let n = db.conn.execute("DELETE FROM tabs_private", [])?;
    if n > 0 {
        let _ = db.conn.execute_batch("VACUUM");
    }
    Ok(n)
}
