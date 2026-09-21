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

/// A tab a restore opened, offered back so the user can close it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct OpenedTab {
    pub url: String,
    /// What to show in a list. The URL is the identity; this is for reading.
    pub label: String,
    pub browser: String,
}

/// The most recent restore, and the tabs it actually created.
///
/// Only rows recorded as `launched`. A `placed` row is a tab that was *already open*
/// when the restore ran, which the restore deliberately left alone. Offering to close
/// those would be offering to close the user's own tabs and calling it an undo.
pub fn tabs_opened_by_last_restore(db: &Db) -> Result<(Option<i64>, Vec<OpenedTab>)> {
    let run: Option<i64> = db
        .conn
        .query_row(
            "SELECT id FROM restore_runs ORDER BY started_at DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .ok();
    let Some(run_id) = run else {
        return Ok((None, Vec::new()));
    };

    let mut stmt = db.conn.prepare(
        "SELECT item_key FROM restore_items
         WHERE run_id = ?1 AND item_kind = 'tab' AND status = 'launched'
         ORDER BY item_key",
    )?;
    let urls: Vec<String> = stmt
        .query_map([run_id], |r| r.get(0))?
        .filter_map(Result::ok)
        .collect();
    drop(stmt);

    // Which browser to ask. A run belongs to one browser, and the snapshot's windows
    // say which; falling back to chrome would send the request to the wrong place.
    let browser: String = db
        .conn
        .query_row(
            "SELECT DISTINCT w.browser FROM browser_windows w
             JOIN restore_runs r ON r.snapshot_id = w.snapshot_id
             WHERE r.id = ?1 LIMIT 1",
            [run_id],
            |r| r.get(0),
        )
        .unwrap_or_else(|_| "chrome".to_string());

    let tabs = urls
        .into_iter()
        .map(|url| OpenedTab {
            label: readable(&url),
            browser: browser.clone(),
            url,
        })
        .collect();

    Ok((Some(run_id), tabs))
}

/// A URL shortened to something worth reading in a list.
fn readable(url: &str) -> String {
    let trimmed = url
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_start_matches("www.");
    let cut = trimmed.find('?').unwrap_or(trimmed.len());
    let short = &trimmed[..cut];
    if short.len() > 70 {
        format!("{}...", &short[..67])
    } else {
        short.to_string()
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
    /// Only what the restore created. A `placed` row was a tab the user already had
    /// open, and offering to close it would be offering to close their own work.
    #[test]
    fn only_tabs_the_restore_created_are_offered_for_closing() {
        let (_d, db) = db();
        db.conn
            .execute(
                "INSERT INTO restore_runs (id, snapshot_id, started_at, mode)
                 VALUES (5, 1, 100, 'ask')",
                [],
            )
            .unwrap();
        for (url, status) in [
            ("https://created.test/", "launched"),
            ("https://already-open.test/", "placed"),
            ("https://failed.test/", "failed"),
        ] {
            db.conn
                .execute(
                    "INSERT INTO restore_items (run_id, item_kind, item_key, status)
                     VALUES (5, 'tab', ?1, ?2)",
                    rusqlite::params![url, status],
                )
                .unwrap();
        }

        let (run, tabs) = tabs_opened_by_last_restore(&db).unwrap();
        assert_eq!(run, Some(5));
        let urls: Vec<&str> = tabs.iter().map(|t| t.url.as_str()).collect();
        assert_eq!(urls, vec!["https://created.test/"]);
    }

    #[test]
    fn nothing_is_offered_when_no_restore_has_run() {
        let (_d, db) = db();
        let (run, tabs) = tabs_opened_by_last_restore(&db).unwrap();
        assert_eq!(run, None);
        assert!(tabs.is_empty());
    }

    #[test]
    fn a_label_is_readable_without_losing_the_url() {
        let t = readable("https://www.example.com/a/b?utm_source=x&very=long");
        assert_eq!(t, "example.com/a/b");
        assert!(readable(&format!("https://x.test/{}", "a".repeat(200))).len() <= 70);
    }

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
