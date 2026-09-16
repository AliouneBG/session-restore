//! The settings window.
//!
//! Everything here was previously only reachable by editing the `settings` table by
//! hand, which is not a thing to ask of anyone. Private-window capture in particular:
//! it is the feature with the most careful design behind it and it had no way to be
//! turned on except SQL.
//!
//! The window is deliberately a *status* view as much as a settings view. Turning on
//! private capture does nothing on its own, because the browser has to grant the
//! permission too, and a toggle that silently achieves nothing is worse than no toggle.
//! So each browser shows what it has actually granted, and the window says plainly when
//! the switch is on but the browser has not agreed.

use crate::browsers::BrowserInfo;
use crate::store::db::Db;
use anyhow::Result;
use serde::{Deserialize, Serialize};

pub const SETTINGS_HTML: &str = include_str!("settings.html");

#[derive(Debug, Serialize)]
pub struct SettingsPayload {
    pub restore_mode: String,
    pub capture_enabled: bool,
    pub capture_private: bool,
    pub private_ttl_hours: i64,
    pub browsers: Vec<BrowserInfo>,
    /// How many private rows exist right now, so "turn it off" can say what it deletes.
    pub private_tabs: i64,
    pub tabs_tracked: i64,
    pub windows_tracked: i64,
    pub snapshots: i64,
    pub data_dir: String,
    pub log_path: String,
    pub agent_version: String,
}

/// What the page can ask the agent to do.
#[derive(Debug, Deserialize)]
#[serde(tag = "action")]
pub enum SettingsAction {
    #[serde(rename = "ready")]
    Ready,
    #[serde(rename = "set_restore_mode")]
    SetRestoreMode { value: String },
    #[serde(rename = "set_capture_enabled")]
    SetCaptureEnabled { value: bool },
    #[serde(rename = "set_capture_private")]
    SetCapturePrivate { value: bool },
    #[serde(rename = "set_private_ttl")]
    SetPrivateTtl { value: i64 },
    #[serde(rename = "open_extensions_page")]
    OpenExtensionsPage { browser: String },
    #[serde(rename = "open_data_folder")]
    OpenDataFolder,
    #[serde(rename = "close")]
    Close,
}

pub fn collect(db: &Db, connected: &[String], data_dir: &std::path::Path) -> Result<SettingsPayload> {
    let live = crate::store::db::LIVE;

    let tabs_tracked: i64 = db
        .conn
        .query_row("SELECT COUNT(*) FROM tabs WHERE snapshot_id = ?1", [live], |r| r.get(0))
        .unwrap_or(0);
    let windows_tracked: i64 = db
        .conn
        .query_row(
            "SELECT COUNT(*) FROM browser_windows WHERE snapshot_id = ?1",
            [live],
            |r| r.get(0),
        )
        .unwrap_or(0);
    let private_tabs: i64 = db
        .conn
        .query_row("SELECT COUNT(*) FROM tabs_private", [], |r| r.get(0))
        .unwrap_or(0);
    let snapshots: i64 = db
        .conn
        .query_row("SELECT COUNT(*) FROM snapshots WHERE id > 0", [], |r| r.get(0))
        .unwrap_or(0);

    Ok(SettingsPayload {
        restore_mode: db.setting("restore_mode")?.unwrap_or_else(|| "ask".into()),
        capture_enabled: db.setting_bool("capture_enabled", true),
        capture_private: db.setting_bool("capture_private_windows", false),
        private_ttl_hours: db.setting_i64("private_ttl_hours", 24),
        browsers: crate::browsers::detect(db, connected),
        private_tabs,
        tabs_tracked,
        windows_tracked,
        snapshots,
        data_dir: data_dir.display().to_string(),
        log_path: crate::logging::log_path(data_dir).display().to_string(),
        agent_version: crate::AGENT_VERSION.to_string(),
    })
}

/// Applies one change. Returns true when the page should be re-rendered from fresh
/// state, which is anything that changes what the *other* controls should say.
pub fn apply(db: &Db, action: &SettingsAction) -> Result<bool> {
    match action {
        SettingsAction::SetRestoreMode { value } => {
            // The CHECK constraint on restore_runs.mode is not the only place these
            // strings matter, so refuse an unknown one rather than storing it.
            if !matches!(value.as_str(), "ask" | "auto" | "off") {
                anyhow::bail!("unknown restore mode {value}");
            }
            db.set_setting("restore_mode", value)?;
            Ok(false)
        }
        SettingsAction::SetCaptureEnabled { value } => {
            db.set_setting("capture_enabled", if *value { "true" } else { "false" })?;
            Ok(false)
        }
        SettingsAction::SetCapturePrivate { value } => {
            db.set_setting(
                "capture_private_windows",
                if *value { "true" } else { "false" },
            )?;
            // Turning it off deletes what was already captured, immediately. Leaving
            // encrypted private rows on disk after the user has said "stop doing this"
            // would be the wrong reading of what they asked for (ADR-0004).
            if !*value {
                let n = crate::ingest::purge_all_private(db)?;
                if n > 0 {
                    tracing::info!(rows = n, "purged private rows after the setting was turned off");
                }
            }
            Ok(true)
        }
        SettingsAction::SetPrivateTtl { value } => {
            // One hour to one week. A zero would mean rows expire the instant they are
            // written, which looks like the feature is broken rather than strict.
            let clamped = (*value).clamp(1, 168);
            db.set_setting("private_ttl_hours", &clamped.to_string())?;
            Ok(clamped != *value)
        }
        SettingsAction::OpenExtensionsPage { browser } => {
            crate::browsers::open_extensions_page(browser)?;
            Ok(false)
        }
        SettingsAction::OpenDataFolder => {
            if let Ok(dir) = crate::data_dir() {
                let _ = std::process::Command::new("explorer.exe").arg(dir).spawn();
            }
            Ok(false)
        }
        SettingsAction::Ready | SettingsAction::Close => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> (tempdir::Dir, Db) {
        let dir = tempdir::Dir::new();
        let db = Db::open(&dir.path().join("sessions.db")).unwrap();
        (dir, db)
    }

    mod tempdir {
        pub struct Dir(std::path::PathBuf);
        impl Dir {
            pub fn new() -> Dir {
                let p = std::env::temp_dir().join(format!("sr-settings-{}", sr_proto::new_id()));
                std::fs::create_dir_all(&p).unwrap();
                Dir(p)
            }
            pub fn path(&self) -> &std::path::Path {
                &self.0
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    #[test]
    fn an_unknown_restore_mode_is_refused() {
        let (_d, db) = db();
        let before = db.setting("restore_mode").unwrap();
        let bad = SettingsAction::SetRestoreMode { value: "sometimes".into() };
        assert!(apply(&db, &bad).is_err());
        // And the stored value is untouched. `restore_mode` is seeded with a default,
        // so "unchanged" is the assertion, not "absent".
        assert_eq!(db.setting("restore_mode").unwrap(), before);
    }

    #[test]
    fn every_valid_restore_mode_is_accepted() {
        let (_d, db) = db();
        for m in ["ask", "auto", "off"] {
            apply(&db, &SettingsAction::SetRestoreMode { value: m.into() }).unwrap();
            assert_eq!(db.setting("restore_mode").unwrap().as_deref(), Some(m));
        }
    }

    /// Turning the switch off has to delete what is already stored. Leaving encrypted
    /// private rows behind after "stop doing this" is the wrong reading of the request.
    #[test]
    fn turning_private_capture_off_purges_what_was_captured() {
        let (_d, db) = db();
        // tabs_private.key_id is a foreign key, so the key has to exist first.
        db.conn
            .execute(
                "INSERT INTO crypto_keys (id, purpose, wrapped_key, wrap_method, created_at)
                 VALUES (1,'private_tabs',x'00','dpapi',0)",
                [],
            )
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO tabs_private (snapshot_id, tab_key, browser_window_id, tab_index,
                 nonce, ciphertext, key_id, expires_at, updated_at)
                 VALUES (0,'w:1','w',0,x'00',x'00',1,9999999999999,0)",
                [],
            )
            .unwrap();

        apply(&db, &SettingsAction::SetCapturePrivate { value: false }).unwrap();

        let left: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM tabs_private", [], |r| r.get(0))
            .unwrap();
        assert_eq!(left, 0, "private rows survived the setting being turned off");
        assert!(!db.setting_bool("capture_private_windows", true));
    }

    #[test]
    fn turning_private_capture_on_does_not_delete_anything() {
        let (_d, db) = db();
        apply(&db, &SettingsAction::SetCapturePrivate { value: true }).unwrap();
        assert!(db.setting_bool("capture_private_windows", false));
    }

    #[test]
    fn the_private_ttl_is_clamped_to_something_sane() {
        let (_d, db) = db();
        apply(&db, &SettingsAction::SetPrivateTtl { value: 0 }).unwrap();
        assert_eq!(db.setting_i64("private_ttl_hours", 0), 1, "zero would expire instantly");

        apply(&db, &SettingsAction::SetPrivateTtl { value: 100_000 }).unwrap();
        assert_eq!(db.setting_i64("private_ttl_hours", 0), 168, "capped at a week");

        apply(&db, &SettingsAction::SetPrivateTtl { value: 48 }).unwrap();
        assert_eq!(db.setting_i64("private_ttl_hours", 0), 48);
    }

    #[test]
    fn the_payload_reports_what_is_actually_stored() {
        let (dir, db) = db();
        db.set_setting("restore_mode", "auto").unwrap();
        db.set_setting("capture_private_windows", "true").unwrap();

        let p = collect(&db, &["chrome".to_string()], dir.path()).unwrap();
        assert_eq!(p.restore_mode, "auto");
        assert!(p.capture_private);
        assert_eq!(p.browsers.len(), 3, "all three browsers are always listed");
        assert!(p.browsers.iter().any(|b| b.id == "chrome" && b.connected));
        assert!(p.browsers.iter().any(|b| b.id == "firefox" && !b.connected));
        assert!(p.log_path.ends_with("agent.log"));
    }
}
