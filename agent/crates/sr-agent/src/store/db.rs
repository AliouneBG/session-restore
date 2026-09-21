//! Database open, pragmas, migration, and settings.
//!
//! The agent is the only process that writes this file (docs/08-agent.md), so WAL is
//! here for crash-safety rather than concurrency: a hard power cut must leave a
//! consistent database, since the whole design accepts losing up to one reconcile
//! interval but never accepts corruption.

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension};
use std::path::Path;

/// Reserved snapshot id meaning "live current state". See docs/02-data-model.md.
pub const LIVE: i64 = 0;

pub const SCHEMA_VERSION: i64 = 1;

const SCHEMA_SQL: &str = include_str!("schema.sql");

pub struct Db {
    pub conn: Connection,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("creating data directory {}", dir.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening database {}", path.display()))?;
        Self::init(conn)
    }

    /// In-memory database for tests.
    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        // WAL survives a power cut consistently. NORMAL can lose the last few commits
        // but cannot corrupt, which is the right trade when we already accept up to a
        // 60s loss by design; FULL would mean an fsync every two seconds forever.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        // Without this, deleted private rows linger in freelist pages where a raw file
        // scan finds them, which would make the TTL cosmetic (docs/06).
        conn.pragma_update(None, "secure_delete", "ON")?;

        let db = Db { conn };
        db.migrate()?;
        Ok(db)
    }

    fn migrate(&self) -> Result<()> {
        self.conn
            .execute_batch(SCHEMA_SQL)
            .context("applying schema")?;

        // CREATE TABLE IF NOT EXISTS does nothing for a table that already exists, so
        // columns added after the first release need an explicit ALTER. Adding one
        // that is already there is an error, not a no-op, hence the check.
        self.add_column_if_missing("apps", "documents", "TEXT")?;
        self.add_column_if_missing("browser_status", "profile_dir", "TEXT")?;

        let existing: Option<String> = self
            .conn
            .query_row("SELECT value FROM meta WHERE key = 'schema_version'", [], |r| {
                r.get(0)
            })
            .optional()?;

        match existing {
            None => {
                self.conn.execute(
                    "INSERT INTO meta (key, value) VALUES ('schema_version', ?1)",
                    [SCHEMA_VERSION.to_string()],
                )?;
                // A random per-install id. Deliberately NOT derived from hardware:
                // it exists to distinguish machines in a future sync, not to
                // fingerprint one.
                self.conn.execute(
                    "INSERT OR IGNORE INTO meta (key, value) VALUES ('machine_id', ?1)",
                    [random_hex(16)],
                )?;
                self.conn.execute(
                    "INSERT OR IGNORE INTO meta (key, value) VALUES ('created_at', ?1)",
                    [sr_proto::now_millis().to_string()],
                )?;
                self.seed_settings()?;
                self.ensure_live_snapshot()?;
            }
            Some(v) => {
                let found: i64 = v.parse().unwrap_or(0);
                if found > SCHEMA_VERSION {
                    anyhow::bail!(
                        "database schema v{found} is newer than this agent supports (v{SCHEMA_VERSION})"
                    );
                }
                self.seed_settings()?;
                self.ensure_live_snapshot()?;
            }
        }
        Ok(())
    }

    /// Adds a column when an older database predates it.
    fn add_column_if_missing(&self, table: &str, column: &str, ty: &str) -> Result<()> {
        let mut stmt = self.conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let existing: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(1))?
            .filter_map(Result::ok)
            .collect();
        drop(stmt);
        if !existing.iter().any(|c| c == column) {
            self.conn
                .execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {ty}"))?;
        }
        Ok(())
    }

    /// Defaults. `INSERT OR IGNORE` so a user's existing choice is never overwritten
    /// by a later agent version adding a new default.
    fn seed_settings(&self) -> Result<()> {
        let now = sr_proto::now_millis();
        let defaults: &[(&str, &str)] = &[
            // Off by default. Turning this on is a deliberate act; see ADR-0004.
            ("capture_private_windows", "false"),
            ("private_ttl_hours", "24"),
            ("reconcile_interval_seconds", "60"),
            ("snapshot_retention_count", "20"),
            ("restore_mode", "ask"),
            ("cloud_backup_enabled", "false"),
            ("capture_enabled", "true"),
        ];
        let mut stmt = self.conn.prepare(
            "INSERT OR IGNORE INTO settings (key, value, updated_at) VALUES (?1, ?2, ?3)",
        )?;
        for (k, v) in defaults {
            stmt.execute(rusqlite::params![k, v, now])?;
        }
        Ok(())
    }

    fn ensure_live_snapshot(&self) -> Result<()> {
        let machine_id = self.meta("machine_id")?.unwrap_or_else(|| "unknown".into());
        self.conn.execute(
            "INSERT OR IGNORE INTO snapshots (id, captured_at, kind, machine_id)
             VALUES (?1, ?2, 'live', ?3)",
            rusqlite::params![LIVE, sr_proto::now_millis(), machine_id],
        )?;
        Ok(())
    }

    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .optional()?)
    }

    pub fn setting(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| {
                r.get(0)
            })
            .optional()?)
    }

    pub fn setting_bool(&self, key: &str, default: bool) -> bool {
        match self.setting(key) {
            Ok(Some(v)) => v == "true" || v == "1",
            _ => default,
        }
    }

    pub fn setting_i64(&self, key: &str, default: i64) -> i64 {
        match self.setting(key) {
            Ok(Some(v)) => v.parse().unwrap_or(default),
            _ => default,
        }
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO settings (key, value, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(key) DO UPDATE SET value = ?2, updated_at = ?3",
            rusqlite::params![key, value, sr_proto::now_millis()],
        )?;
        Ok(())
    }

    /// `PRAGMA integrity_check`. A failure means the file is untrustworthy by
    /// definition, so the caller renames it aside and starts fresh rather than
    /// attempting repair (docs/08-agent.md).
    pub fn integrity_ok(&self) -> bool {
        self.conn
            .query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))
            .map(|s| s == "ok")
            .unwrap_or(false)
    }
}

fn random_hex(bytes: usize) -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let mut out = String::with_capacity(bytes * 2);
    while out.len() < bytes * 2 {
        let mut h = RandomState::new().build_hasher();
        h.write_u64(sr_proto::now_millis() as u64);
        out.push_str(&format!("{:016x}", h.finish()));
    }
    out.truncate(bytes * 2);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_and_creates_schema() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(db.meta("schema_version").unwrap().as_deref(), Some("1"));
        assert!(db.integrity_ok());
    }

    #[test]
    fn live_snapshot_exists_with_reserved_id_zero() {
        let db = Db::open_in_memory().unwrap();
        let kind: String = db
            .conn
            .query_row("SELECT kind FROM snapshots WHERE id = 0", [], |r| r.get(0))
            .unwrap();
        assert_eq!(kind, "live");
    }

    #[test]
    fn private_capture_is_off_by_default() {
        // The single most important default in the product (ADR-0004).
        let db = Db::open_in_memory().unwrap();
        assert!(!db.setting_bool("capture_private_windows", true));
    }

    #[test]
    fn defaults_match_the_spec() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(db.setting_i64("private_ttl_hours", 0), 24);
        assert_eq!(db.setting_i64("reconcile_interval_seconds", 0), 60);
        assert_eq!(db.setting("restore_mode").unwrap().as_deref(), Some("ask"));
        assert!(!db.setting_bool("cloud_backup_enabled", true));
    }

    #[test]
    fn migration_is_idempotent_and_preserves_user_choices() {
        let dir = std::env::temp_dir().join(format!("sr-test-{}", sr_proto::new_id()));
        let path = dir.join("sessions.db");
        {
            let db = Db::open(&path).unwrap();
            db.set_setting("capture_private_windows", "true").unwrap();
            db.set_setting("private_ttl_hours", "6").unwrap();
        }
        {
            // Reopening must not reset what the user chose.
            let db = Db::open(&path).unwrap();
            assert!(db.setting_bool("capture_private_windows", false));
            assert_eq!(db.setting_i64("private_ttl_hours", 0), 6);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn refuses_a_database_from_a_newer_agent() {
        let db = Db::open_in_memory().unwrap();
        db.conn
            .execute("UPDATE meta SET value = '99' WHERE key = 'schema_version'", [])
            .unwrap();
        // Re-running migration against the future schema must refuse rather than
        // silently operate on fields it may not understand.
        assert!(db.migrate().is_err());
    }

    #[test]
    fn foreign_keys_and_secure_delete_are_on() {
        let db = Db::open_in_memory().unwrap();
        let fk: i64 = db
            .conn
            .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fk, 1);
        let sd: i64 = db
            .conn
            .query_row("PRAGMA secure_delete", [], |r| r.get(0))
            .unwrap();
        assert_eq!(sd, 1);
    }

    #[test]
    fn machine_id_is_present_and_not_derived_from_hardware() {
        let a = Db::open_in_memory().unwrap().meta("machine_id").unwrap().unwrap();
        let b = Db::open_in_memory().unwrap().meta("machine_id").unwrap().unwrap();
        assert_eq!(a.len(), 32);
        // Two fresh installs on the same machine must not share an id.
        assert_ne!(a, b);
    }
}
