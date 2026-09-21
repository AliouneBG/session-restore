//! The undo window.
//!
//! Undo used to be one menu item that did one thing: put the windows back. That is
//! half an undo, and the half people notice is missing, because the tabs the restore
//! opened are still sitting there.
//!
//! This offers both halves separately, because they are different decisions with
//! different risks. Moving windows back changes nothing you cannot see. Closing tabs
//! destroys something, so it is per-tab, opt-in per item, and restricted to tabs the
//! restore actually created.

use crate::restore::undo::OpenedTab;
use crate::store::db::Db;
use anyhow::Result;
use serde::{Deserialize, Serialize};

pub const UNDO_HTML: &str = include_str!("undo.html");

#[derive(Debug, Serialize)]
pub struct UndoPayload {
    pub can_undo: bool,
    pub run_id: Option<i64>,
    pub tabs: Vec<OpenedTab>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "action")]
pub enum UndoAction {
    #[serde(rename = "ready")]
    Ready,
    /// Put applications and windows back where they were. Closes nothing.
    #[serde(rename = "undo_windows")]
    UndoWindows,
    /// Close specific tabs this restore opened.
    #[serde(rename = "close_tabs")]
    CloseTabs { urls: Vec<String> },
    #[serde(rename = "close")]
    Close,
}

pub fn collect(db: &Db) -> Result<UndoPayload> {
    let (run_id, tabs) = crate::restore::undo::tabs_opened_by_last_restore(db)?;
    Ok(UndoPayload {
        can_undo: crate::restore::undo::can_undo(db),
        run_id,
        tabs,
    })
}

/// The browser to send a close request to, for the run being undone.
pub fn browser_for(tabs: &[OpenedTab]) -> Option<String> {
    tabs.first().map(|t| t.browser.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Dir(std::path::PathBuf);
    impl Dir {
        fn new() -> Dir {
            let p = std::env::temp_dir().join(format!("sr-undowin-{}", sr_proto::new_id()));
            std::fs::create_dir_all(&p).unwrap();
            Dir(p)
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn db() -> (Dir, Db) {
        let d = Dir::new();
        let db = Db::open(&d.0.join("sessions.db")).unwrap();
        (d, db)
    }

    #[test]
    fn a_fresh_install_offers_nothing() {
        let (_d, db) = db();
        let p = collect(&db).unwrap();
        assert!(!p.can_undo);
        assert!(p.tabs.is_empty());
        assert_eq!(p.run_id, None);
    }

    #[test]
    fn the_payload_lists_only_what_the_restore_opened() {
        let (_d, db) = db();
        db.conn
            .execute(
                "INSERT INTO snapshots (id, captured_at, kind, machine_id, app_count, tab_count)
                 VALUES (9, 1, 'pre_restore', 'm', 0, 0)",
                [],
            )
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO restore_runs (id, snapshot_id, started_at, mode, undo_snapshot_id)
                 VALUES (3, 1, 100, 'ask', 9)",
                [],
            )
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO restore_items (run_id, item_kind, item_key, status)
                 VALUES (3,'tab','https://opened.test/','launched'),
                        (3,'tab','https://was-already-open.test/','placed')",
                [],
            )
            .unwrap();

        let p = collect(&db).unwrap();
        assert!(p.can_undo);
        assert_eq!(p.run_id, Some(3));
        assert_eq!(p.tabs.len(), 1, "offered a tab the restore did not open");
        assert_eq!(p.tabs[0].url, "https://opened.test/");
    }

    #[test]
    fn the_action_names_match_what_the_page_sends() {
        let close: UndoAction =
            serde_json::from_str(r#"{"action":"close_tabs","urls":["https://a.test/"]}"#).unwrap();
        assert!(matches!(close, UndoAction::CloseTabs { urls } if urls.len() == 1));

        let windows: UndoAction = serde_json::from_str(r#"{"action":"undo_windows"}"#).unwrap();
        assert!(matches!(windows, UndoAction::UndoWindows));
    }
}
