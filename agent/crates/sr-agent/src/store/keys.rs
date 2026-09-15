//! Data-key lifecycle: creation, wrapping, rotation.
//!
//! The wrapped key lives in the `crypto_keys` table; the DPAPI entropy lives in a
//! separate file outside the database. Splitting them means a copy of `sessions.db`
//! alone is insufficient even on the same account (docs/06-privacy-security.md).

use super::crypto::{unwrap_key, wrap_key, Dek, WRAP_METHOD};
use super::db::Db;
use anyhow::{Context, Result};
use rand::RngCore;
use rusqlite::OptionalExtension;
use std::path::{Path, PathBuf};

pub const PURPOSE_PRIVATE_TABS: &str = "private_tabs";

/// Rotate monthly. Rows reference `key_id`, so old rows stay readable until their TTL
/// kills them; once no row references a retired key it is deleted.
pub const ROTATE_AFTER_MS: i64 = 30 * 24 * 60 * 60 * 1000;

pub struct KeyManager {
    entropy_path: PathBuf,
}

impl KeyManager {
    pub fn new(data_dir: &Path) -> Self {
        KeyManager {
            entropy_path: data_dir.join("keys.bin"),
        }
    }

    /// Loads the DPAPI entropy, creating it on first use.
    fn entropy(&self) -> Result<Vec<u8>> {
        if let Ok(b) = std::fs::read(&self.entropy_path) {
            if b.len() == 32 {
                return Ok(b);
            }
            // Wrong length means a truncated or corrupt file. Regenerating would
            // silently orphan every existing key, so refuse loudly instead.
            anyhow::bail!(
                "{} is {} bytes, expected 32 - refusing to regenerate and orphan existing keys",
                self.entropy_path.display(),
                b.len()
            );
        }
        if let Some(dir) = self.entropy_path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut buf = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut buf);
        std::fs::write(&self.entropy_path, buf)
            .with_context(|| format!("writing {}", self.entropy_path.display()))?;
        Ok(buf.to_vec())
    }

    /// Returns the active (key_id, DEK), creating one if none exists or the current
    /// one is due for rotation.
    pub fn active_dek(&self, db: &Db) -> Result<(i64, Dek)> {
        let entropy = self.entropy()?;
        let now = sr_proto::now_millis();

        let row: Option<(i64, Vec<u8>, i64)> = db
            .conn
            .query_row(
                "SELECT id, wrapped_key, created_at FROM crypto_keys
                 WHERE purpose = ?1 AND retired_at IS NULL
                 ORDER BY id DESC LIMIT 1",
                [PURPOSE_PRIVATE_TABS],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;

        if let Some((id, wrapped, created_at)) = row {
            if now - created_at < ROTATE_AFTER_MS {
                let dek = unwrap_key(&wrapped, &entropy)
                    .context("unwrapping the private-tab key (wrong user account?)")?;
                return Ok((id, dek));
            }
            db.conn.execute(
                "UPDATE crypto_keys SET retired_at = ?1 WHERE id = ?2",
                rusqlite::params![now, id],
            )?;
        }

        self.create_key(db, &entropy, now)
    }

    fn create_key(&self, db: &Db, entropy: &[u8], now: i64) -> Result<(i64, Dek)> {
        let dek = Dek::generate();
        let wrapped = wrap_key(&dek, entropy)?;
        db.conn.execute(
            "INSERT INTO crypto_keys (purpose, wrapped_key, wrap_method, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![PURPOSE_PRIVATE_TABS, wrapped, WRAP_METHOD, now],
        )?;
        Ok((db.conn.last_insert_rowid(), dek))
    }

    /// Loads a specific key by id, including retired ones, so rows sealed under an
    /// older key remain readable until they expire.
    pub fn dek_by_id(&self, db: &Db, key_id: i64) -> Result<Dek> {
        let entropy = self.entropy()?;
        let wrapped: Vec<u8> = db.conn.query_row(
            "SELECT wrapped_key FROM crypto_keys WHERE id = ?1",
            [key_id],
            |r| r.get(0),
        )?;
        unwrap_key(&wrapped, &entropy)
    }

    /// Deletes retired keys no row references any more.
    pub fn gc(&self, db: &Db) -> Result<usize> {
        Ok(db.conn.execute(
            "DELETE FROM crypto_keys
             WHERE retired_at IS NOT NULL
               AND id NOT IN (SELECT DISTINCT key_id FROM tabs_private)",
            [],
        )?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("sr-keys-{}", sr_proto::new_id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn creates_then_reuses_the_same_key() {
        let dir = tmpdir();
        let db = Db::open_in_memory().unwrap();
        let km = KeyManager::new(&dir);
        let (id1, dek1) = km.active_dek(&db).unwrap();
        let (id2, dek2) = km.active_dek(&db).unwrap();
        assert_eq!(id1, id2);
        assert_eq!(dek1.expose(), dek2.expose());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stored_key_is_wrapped_not_raw() {
        let dir = tmpdir();
        let db = Db::open_in_memory().unwrap();
        let km = KeyManager::new(&dir);
        let (id, dek) = km.active_dek(&db).unwrap();
        let wrapped: Vec<u8> = db
            .conn
            .query_row("SELECT wrapped_key FROM crypto_keys WHERE id = ?1", [id], |r| r.get(0))
            .unwrap();
        assert!(
            !wrapped
                .windows(32)
                .any(|w| w == dek.expose().as_slice()),
            "raw key material found in the database"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rotates_after_the_interval_and_keeps_the_old_key_readable() {
        let dir = tmpdir();
        let db = Db::open_in_memory().unwrap();
        let km = KeyManager::new(&dir);
        let (old_id, old_dek) = km.active_dek(&db).unwrap();

        // Age the key past the rotation window.
        db.conn
            .execute(
                "UPDATE crypto_keys SET created_at = ?1 WHERE id = ?2",
                rusqlite::params![sr_proto::now_millis() - ROTATE_AFTER_MS - 1, old_id],
            )
            .unwrap();

        let (new_id, new_dek) = km.active_dek(&db).unwrap();
        assert_ne!(old_id, new_id);
        assert_ne!(old_dek.expose(), new_dek.expose());

        // Rows sealed under the retired key must still be decryptable.
        let recovered = km.dek_by_id(&db, old_id).unwrap();
        assert_eq!(recovered.expose(), old_dek.expose());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn gc_removes_retired_keys_with_no_rows() {
        let dir = tmpdir();
        let db = Db::open_in_memory().unwrap();
        let km = KeyManager::new(&dir);
        let (id, _) = km.active_dek(&db).unwrap();
        db.conn
            .execute("UPDATE crypto_keys SET retired_at = 1 WHERE id = ?1", [id])
            .unwrap();
        assert_eq!(km.gc(&db).unwrap(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn refuses_to_regenerate_corrupt_entropy() {
        let dir = tmpdir();
        std::fs::write(dir.join("keys.bin"), b"too short").unwrap();
        let db = Db::open_in_memory().unwrap();
        // Silently regenerating would orphan every existing key.
        assert!(KeyManager::new(&dir).active_dek(&db).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
