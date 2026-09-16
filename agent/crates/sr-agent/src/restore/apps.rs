//! Application restore: the phased pipeline from docs/04-restore.md.
//!
//! Restoring a dozen applications at once is the worst thing to do to a machine that
//! is still finishing logon, so launches are staggered and placement happens in a
//! separate phase - applications do not create their windows synchronously with
//! process start, and waiting for each one in turn would serialize the whole restore
//! behind the slowest app.

use super::{launch, place};
use crate::store::db::Db;
use crate::watcher::displays::Display;
use crate::watcher::identity::Tier;
use anyhow::Result;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Gap between launches. Enough to avoid a thundering herd without making a ten-app
/// restore feel slow.
pub const STAGGER: Duration = Duration::from_millis(400);

/// How long to wait for a launched application to show a window before giving up on
/// placing it. It is never waited on serially - the budget applies to the whole
/// placement phase.
pub const WINDOW_BUDGET: Duration = Duration::from_secs(20);

#[derive(Debug, Clone)]
pub struct PlannedApp {
    pub app_key: String,
    pub display_name: String,
    pub kind: String,
    pub tier: Tier,
    pub exe_path: Option<String>,
    pub aumid: Option<String>,
    pub command_line: Option<String>,
    pub working_dir: Option<String>,
    pub is_browser: bool,
    pub windows: Vec<PlannedWindow>,
}

#[derive(Debug, Clone)]
pub struct PlannedWindow {
    pub window_key: String,
    pub rect: place::Rect,
    pub display_key: Option<String>,
    pub show_cmd: String,
    pub dpi: u32,
}

#[derive(Debug, Default)]
pub struct AppRestoreReport {
    pub launched: Vec<String>,
    pub skipped: Vec<(String, String)>,
    pub failed: Vec<(String, String)>,
    pub placed: usize,
}

/// Reads the applications to restore from a snapshot.
///
/// Browsers are excluded: their windows come back through the extension, which knows
/// what is already open. Launching the browser here as well would race that path and
/// could open a second window.
pub fn plan_from_snapshot(db: &Db, snapshot_id: i64) -> Result<Vec<PlannedApp>> {
    let mut stmt = db.conn.prepare(
        "SELECT app_key, display_name, kind, restore_tier, exe_path, aumid, command_line,
                working_dir, is_browser
         FROM apps WHERE snapshot_id = ?1 AND is_browser = 0
         ORDER BY restore_tier, display_name",
    )?;

    let apps: Vec<PlannedApp> = stmt
        .query_map([snapshot_id], |r| {
            let tier = match r.get::<_, String>(3)?.as_str() {
                "A" => Tier::A,
                "B" => Tier::B,
                "C" => Tier::C,
                _ => Tier::D,
            };
            Ok(PlannedApp {
                app_key: r.get(0)?,
                display_name: r.get::<_, Option<String>>(1)?.unwrap_or_else(|| "?".into()),
                kind: r.get(2)?,
                tier,
                exe_path: r.get(4)?,
                aumid: r.get(5)?,
                command_line: r.get(6)?,
                working_dir: r.get(7)?,
                is_browser: r.get::<_, i64>(8)? != 0,
                windows: Vec::new(),
            })
        })?
        .filter_map(Result::ok)
        .collect();
    drop(stmt);

    let mut out = Vec::new();
    for mut app in apps {
        let mut ws = db.conn.prepare(
            "SELECT w.window_key, w.norm_x, w.norm_y, w.norm_w, w.norm_h, w.show_cmd,
                    w.display_key, COALESCE(d.dpi, 96)
             FROM windows w
             LEFT JOIN displays d
               ON d.snapshot_id = w.snapshot_id AND d.display_key = w.display_key
             WHERE w.snapshot_id = ?1 AND w.app_key = ?2",
        )?;
        app.windows = ws
            .query_map(rusqlite::params![snapshot_id, app.app_key], |r| {
                Ok(PlannedWindow {
                    window_key: r.get(0)?,
                    rect: place::Rect {
                        x: r.get(1)?,
                        y: r.get(2)?,
                        w: r.get(3)?,
                        h: r.get(4)?,
                    },
                    show_cmd: r.get(5)?,
                    display_key: r.get(6)?,
                    dpi: r.get::<_, i64>(7)? as u32,
                })
            })?
            .filter_map(Result::ok)
            .collect();
        drop(ws);
        out.push(app);
    }

    Ok(out)
}

/// Launches applications and places their windows.
///
/// `dry_run` performs every decision and reports what it would do without starting
/// anything, which is what makes this safe to exercise on a working machine.
pub fn restore_apps(
    apps: &[PlannedApp],
    current_displays: &[Display],
    dry_run: bool,
) -> AppRestoreReport {
    let mut report = AppRestoreReport::default();
    let mut launched_pids: HashMap<String, Option<u32>> = HashMap::new();

    // Phase 1-3: launch, cheapest and most exact first, staggered.
    for app in apps {
        match app.tier {
            Tier::D => {
                report.skipped.push((
                    app.display_name.clone(),
                    if app.kind == "uwp" {
                        "packaged app without an identity".into()
                    } else {
                        "needs admin, or cannot be launched safely".into()
                    },
                ));
                continue;
            }
            _ => {}
        }

        if dry_run {
            report.launched.push(app.display_name.clone());
            continue;
        }

        let result = match (app.tier, app.kind.as_str()) {
            (Tier::A, _) => match &app.command_line {
                Some(cmd) => launch::launch_with_command_line(cmd, app.working_dir.as_deref()),
                // Tier A without a command line should not happen, but falling back is
                // better than failing the app outright.
                None => match &app.exe_path {
                    Some(p) => launch::launch_via_shell(p),
                    None => Err(anyhow::anyhow!("no exe path")),
                },
            },
            (_, "uwp") => match &app.aumid {
                Some(a) => launch::launch_uwp(a),
                None => Err(anyhow::anyhow!("no AUMID")),
            },
            _ => match &app.exe_path {
                Some(p) => launch::launch_via_shell(p),
                None => Err(anyhow::anyhow!("no exe path")),
            },
        };

        match result {
            Ok(l) => {
                launched_pids.insert(app.app_key.clone(), l.pid);
                report.launched.push(app.display_name.clone());
            }
            Err(e) => {
                // One application failing never stops the rest. "7 of 9 restored,
                // Figma is not installed" is a good outcome; "some stuff came back"
                // is not.
                report
                    .failed
                    .push((app.display_name.clone(), format!("{e:#}")));
            }
        }

        std::thread::sleep(STAGGER);
    }

    if dry_run {
        report.placed = apps.iter().map(|a| a.windows.len()).sum();
        return report;
    }

    // Phase 5: placement, once windows have had a chance to appear.
    report.placed = place_windows(apps, &launched_pids, current_displays);
    report
}

/// Waits for launched applications' windows to appear, then places them.
///
/// Matching is by app identity rather than by the pid we launched, because many
/// applications relaunch through a stub and the window ends up owned by a different
/// process than the one `CreateProcess` returned.
#[cfg(windows)]
fn place_windows(
    apps: &[PlannedApp],
    _launched_pids: &HashMap<String, Option<u32>>,
    current_displays: &[Display],
) -> usize {
    use crate::watcher::{identity, windows as winwatch};

    let deadline = Instant::now() + WINDOW_BUDGET;
    let mut placed = 0usize;
    let mut done: HashMap<String, usize> = HashMap::new();

    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(500));

        let Ok(live) = winwatch::enumerate() else {
            continue;
        };

        // Group live windows by the app they belong to.
        let mut by_app: HashMap<String, Vec<&winwatch::CapturedWindow>> = HashMap::new();
        for w in &live {
            let key = identity::app_key(
                w.process.kind,
                w.process.exe_path.as_deref(),
                w.process.aumid.as_deref(),
            );
            by_app.entry(key).or_default().push(w);
        }

        let mut all_done = true;
        for app in apps {
            if app.windows.is_empty() || app.tier == Tier::D {
                continue;
            }
            let already = *done.get(&app.app_key).unwrap_or(&0);
            if already >= app.windows.len() {
                continue;
            }

            let Some(live_windows) = by_app.get(&app.app_key) else {
                all_done = false;
                continue;
            };

            for (i, want) in app.windows.iter().enumerate().skip(already) {
                let Some(target) = live_windows.get(i) else {
                    all_done = false;
                    break;
                };
                let mapped = place::map_to_current(
                    &place::StoredPlacement {
                        rect: want.rect,
                        display_key: want.display_key.clone(),
                        dpi: want.dpi,
                    },
                    current_displays,
                );
                if place::apply(target.hwnd, mapped, &want.show_cmd).is_ok() {
                    placed += 1;
                    *done.entry(app.app_key.clone()).or_insert(0) += 1;
                }
            }
        }

        if all_done {
            break;
        }
    }

    placed
}

#[cfg(not(windows))]
fn place_windows(
    _apps: &[PlannedApp],
    _launched_pids: &HashMap<String, Option<u32>>,
    _current_displays: &[Display],
) -> usize {
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(name: &str, tier: Tier, kind: &str) -> PlannedApp {
        PlannedApp {
            app_key: format!("k:{name}"),
            display_name: name.into(),
            kind: kind.into(),
            tier,
            exe_path: Some(format!(r"C:\Apps\{name}.exe")),
            aumid: if kind == "uwp" { Some("X!App".into()) } else { None },
            command_line: Some(format!(r"C:\Apps\{name}.exe")),
            working_dir: None,
            is_browser: false,
            windows: vec![],
        }
    }

    #[test]
    fn a_dry_run_launches_nothing_but_accounts_for_everything() {
        let apps = vec![
            app("Editor", Tier::A, "win32"),
            app("Calc", Tier::B, "uwp"),
        ];
        let r = restore_apps(&apps, &[], true);
        assert_eq!(r.launched.len(), 2);
        assert!(r.failed.is_empty());
    }

    #[test]
    fn tier_d_apps_are_skipped_with_a_reason() {
        // An honest "needs admin" beats a silent failure or a UAC prompt at logon.
        let apps = vec![app("AdminThing", Tier::D, "win32")];
        let r = restore_apps(&apps, &[], true);
        assert!(r.launched.is_empty());
        assert_eq!(r.skipped.len(), 1);
        assert!(!r.skipped[0].1.is_empty(), "skipped without a reason");
    }

    #[test]
    fn a_packaged_app_without_an_identity_says_so() {
        let mut a = app("Broken", Tier::D, "uwp");
        a.aumid = None;
        let r = restore_apps(&[a], &[], true);
        assert!(r.skipped[0].1.contains("packaged"));
    }

    #[test]
    fn mixed_tiers_are_all_accounted_for() {
        let apps = vec![
            app("A1", Tier::A, "win32"),
            app("B1", Tier::B, "win32"),
            app("D1", Tier::D, "win32"),
        ];
        let r = restore_apps(&apps, &[], true);
        assert_eq!(r.launched.len() + r.skipped.len() + r.failed.len(), apps.len());
    }
}
