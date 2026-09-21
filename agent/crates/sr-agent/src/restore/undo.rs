//! Reversing the last restore.
//!
//! Every restore writes a `pre_restore` snapshot before it touches anything, and this
//! is what consumes it. Without a caller that snapshot was an undo point nothing could
//! reach.
//!
//! **Undo re-places windows; it never closes what the restore opened.** Closing
//! applications to undo would risk destroying work done in the meantime, which is a far
//! worse outcome than a few extra windows being open. The outcome says so out loud
//! rather than leaving the user to notice.

use crate::store::db::Db;
use anyhow::Result;

/// What an undo did, in terms a person can be told.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UndoOutcome {
    /// No restore has run, so there is nothing to reverse.
    NothingToUndo,
    /// A restore ran, but nothing was open when it did.
    NoUndoPoint { run_id: i64 },
    Restored {
        run_id: i64,
        /// Applications started again because they had been closed since.
        launched: usize,
        /// Windows moved back to where they were before the restore.
        placed: usize,
        /// Applications that were already open and so were only re-placed.
        skipped: usize,
    },
}

impl UndoOutcome {
    /// A single sentence, for a dialog, a log line or a terminal.
    pub fn message(&self) -> String {
        match self {
            UndoOutcome::NothingToUndo => "There is no restore to undo.".to_string(),
            UndoOutcome::NoUndoPoint { .. } => {
                "That restore has no undo point, because nothing was open when it ran."
                    .to_string()
            }
            UndoOutcome::Restored {
                launched,
                placed,
                skipped,
                ..
            } => {
                let mut parts = Vec::new();
                if *placed > 0 {
                    parts.push(format!(
                        "{placed} window{} moved back",
                        if *placed == 1 { "" } else { "s" }
                    ));
                }
                if *launched > 0 {
                    parts.push(format!(
                        "{launched} application{} reopened",
                        if *launched == 1 { "" } else { "s" }
                    ));
                }
                if *skipped > 0 {
                    parts.push(format!("{skipped} already open"));
                }
                if parts.is_empty() {
                    return "Nothing needed changing.".to_string();
                }
                format!(
                    "{}. Windows the restore opened were left alone rather than closed.",
                    parts.join(", ")
                )
            }
        }
    }
}

/// Whether there is anything to undo, without doing it.
///
/// Used to decide whether the interface should offer the option at all, because a menu
/// item that reports "nothing to undo" is worse than one that is not there.
pub fn can_undo(db: &Db) -> bool {
    db.conn
        .query_row(
            "SELECT COUNT(*) FROM restore_runs WHERE undo_snapshot_id IS NOT NULL",
            [],
            |r| r.get::<_, i64>(0),
        )
        .map(|n| n > 0)
        .unwrap_or(false)
}

/// Reverses the most recent restore that has an undo point.
pub fn undo_last(db: &Db) -> Result<UndoOutcome> {
    let run: Option<(i64, Option<i64>)> = db
        .conn
        .query_row(
            "SELECT id, undo_snapshot_id FROM restore_runs
             ORDER BY started_at DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok();

    let Some((run_id, undo)) = run else {
        return Ok(UndoOutcome::NothingToUndo);
    };
    let Some(undo_snapshot) = undo else {
        return Ok(UndoOutcome::NoUndoPoint { run_id });
    };

    let apps = crate::restore::apps::plan_from_snapshot(db, undo_snapshot)?;
    let displays = crate::watcher::displays::enumerate().unwrap_or_default();
    let report = crate::restore::apps::restore_apps(&apps, &displays, false);

    tracing::info!(
        run_id,
        launched = report.launched.len(),
        placed = report.placed,
        "undid a restore"
    );

    Ok(UndoOutcome::Restored {
        run_id,
        launched: report.launched.len(),
        placed: report.placed,
        skipped: report.skipped.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Dir(std::path::PathBuf);
    impl Dir {
        fn new() -> Dir {
            let p = std::env::temp_dir().join(format!("sr-undo-{}", sr_proto::new_id()));
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
    fn a_fresh_install_has_nothing_to_undo() {
        let (_d, db) = db();
        assert!(!can_undo(&db));
        assert_eq!(undo_last(&db).unwrap(), UndoOutcome::NothingToUndo);
    }

    /// A run with no undo point is not offered, because the menu item would only be
    /// able to report its own uselessness.
    #[test]
    fn a_run_without_an_undo_point_is_not_offered() {
        let (_d, db) = db();
        db.conn
            .execute(
                "INSERT INTO restore_runs (snapshot_id, started_at, mode, undo_snapshot_id)
                 VALUES (1, 1, 'ask', NULL)",
                [],
            )
            .unwrap();
        assert!(!can_undo(&db), "offered an undo that cannot do anything");
        assert!(matches!(
            undo_last(&db).unwrap(),
            UndoOutcome::NoUndoPoint { .. }
        ));
    }

    #[test]
    fn a_run_with_an_undo_point_is_offered() {
        let (_d, db) = db();
        db.conn
            .execute(
                "INSERT INTO snapshots (id, captured_at, kind, machine_id, app_count, tab_count)
                 VALUES (7, 1, 'pre_restore', 'm', 0, 0)",
                [],
            )
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO restore_runs (snapshot_id, started_at, mode, undo_snapshot_id)
                 VALUES (1, 1, 'ask', 7)",
                [],
            )
            .unwrap();
        assert!(can_undo(&db));
    }

    /// The message has to say that nothing was closed. Undoing a restore and finding
    /// the extra windows still there is confusing unless it was stated.
    #[test]
    fn the_message_says_what_was_deliberately_not_done() {
        let outcome = UndoOutcome::Restored {
            run_id: 1,
            launched: 2,
            placed: 8,
            skipped: 1,
        };
        let m = outcome.message();
        assert!(m.contains("8 windows moved back"), "{m}");
        assert!(m.contains("2 applications reopened"), "{m}");
        assert!(m.contains("left alone rather than closed"), "{m}");
    }

    #[test]
    fn the_message_reads_correctly_for_a_single_item() {
        let m = UndoOutcome::Restored {
            run_id: 1,
            launched: 1,
            placed: 1,
            skipped: 0,
        }
        .message();
        assert!(m.contains("1 window moved back"), "{m}");
        assert!(m.contains("1 application reopened"), "{m}");
    }
}
