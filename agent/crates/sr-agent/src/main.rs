//! Session Restore agent entry point.
//!
//! A per-user process started at logon by a Scheduled Task, not a Windows Service.
//! See ADR-0001 for why that distinction is load-bearing.

use anyhow::{Context, Result};
use sr_agent::ingest::sweep_expired_private;
use sr_agent::server::{serve, PendingRestore, Shared};
use sr_agent::store::db::Db;
use sr_agent::store::keys::KeyManager;
use sr_agent::store::snapshot;
use sr_agent::{data_dir, AGENT_VERSION};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How often expired private rows are swept. Cheap, and it bounds the window in which
/// an expired row still exists on disk to five minutes rather than until next startup.
const SWEEP_INTERVAL: Duration = Duration::from_secs(5 * 60);

const USAGE: &str = "\
sr-agent - Session Restore agent

USAGE:
  sr-agent                      Run the agent (default)
  sr-agent --install [IDS]      Register native messaging hosts for all browsers
  sr-agent --uninstall          Remove registrations and manifests
  sr-agent --status             Show registration and database status
  sr-agent --capture            Run one application/window capture pass and print it
  sr-agent --restore-apps       Launch and place the applications from the newest snapshot
                                  --dry-run   decide everything, start nothing
                                  --snapshot=<ID>  restore a specific snapshot

INSTALL OPTIONS:
  --chrome-id=<ID>    Chrome/Edge extension ID. Repeatable.
  --unpacked=<DIR>    Compute the Chromium ID for an unpacked extension directory and
                      allow it. Chromium derives the ID from the path, so this avoids
                      having to load the extension first just to read its ID.
  --firefox-id=<ID>   Firefox addon ID. Defaults to the one in manifest.firefox.json.

ENVIRONMENT:
  SR_LOG              Log filter, e.g. SR_LOG=debug
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let result = if args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{USAGE}");
        Ok(())
    } else if args.iter().any(|a| a == "--install") {
        cmd_install(&args)
    } else if args.iter().any(|a| a == "--uninstall") {
        cmd_uninstall()
    } else if args.iter().any(|a| a == "--status") {
        cmd_status()
    } else if args.iter().any(|a| a == "--capture") {
        cmd_capture()
    } else if args.iter().any(|a| a == "--restore-apps") {
        cmd_restore_apps(&args)
    } else {
        run()
    };

    if let Err(e) = result {
        eprintln!("sr-agent: {e:#}");
        std::process::exit(1);
    }
}

fn flag_values<'a>(args: &'a [String], prefix: &str) -> Vec<String> {
    args.iter()
        .filter_map(|a| a.strip_prefix(prefix).map(str::to_string))
        .filter(|s| !s.is_empty())
        .collect()
}

fn cmd_install(args: &[String]) -> Result<()> {
    let dir = data_dir()?;
    let mut ids = sr_agent::setup::Ids::default();

    let mut chromium = flag_values(args, "--chrome-id=");
    for dir in flag_values(args, "--unpacked=") {
        let id = sr_agent::setup::chromium_unpacked_id(std::path::Path::new(&dir))?;
        println!("Unpacked extension {dir}");
        println!("  computed id: {id}");
        chromium.push(id);
    }
    if !chromium.is_empty() {
        ids.chromium = chromium;
    }
    let firefox = flag_values(args, "--firefox-id=");
    if !firefox.is_empty() {
        ids.firefox = firefox;
    }

    let done = sr_agent::setup::install(&dir, &ids)?;
    println!("Registered native messaging host for: {}", done.join(", "));
    println!("Manifests in {}", dir.display());

    if ids.chromium.is_empty() {
        // Not an error: registration is still correct, it just admits no extension
        // yet. Silently writing an empty allowlist and letting the user discover the
        // connection failing later would be worse.
        println!();
        println!("No Chrome/Edge extension ID given, so no Chromium extension is allowed yet.");
        println!("Load the extension, copy its ID from chrome://extensions, then re-run:");
        println!("  sr-agent --install --chrome-id=<ID>");
    }
    Ok(())
}

fn cmd_uninstall() -> Result<()> {
    let dir = data_dir()?;
    sr_agent::setup::uninstall(&dir)?;
    println!("Removed native messaging registrations and manifests.");
    println!(
        "Captured session data in {} was left in place; delete it to remove everything.",
        dir.display()
    );
    Ok(())
}

fn cmd_status() -> Result<()> {
    let dir = data_dir()?;
    println!("Data directory:  {}", dir.display());
    println!(
        "Relay binary:    {}",
        sr_agent::setup::relay_path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|e| format!("NOT FOUND ({e})"))
    );
    println!(
        "Registered:      {}",
        if sr_agent::setup::is_installed(&dir) {
            "yes"
        } else {
            "no - run: sr-agent --install"
        }
    );

    let db_path = dir.join("sessions.db");
    if db_path.exists() {
        let db = Db::open(&db_path)?;
        // Live state only. Counting every snapshot as well reported "9 tabs in 3
        // windows" for a browser showing three, which reads as a capture bug.
        let tabs: i64 = db.conn.query_row(
            "SELECT COUNT(*) FROM tabs WHERE snapshot_id = ?1",
            [sr_agent::store::db::LIVE],
            |r| r.get(0),
        )?;
        let private: i64 = db.conn.query_row(
            "SELECT COUNT(*) FROM tabs_private WHERE snapshot_id = ?1",
            [sr_agent::store::db::LIVE],
            |r| r.get(0),
        )?;
        let windows: i64 = db.conn.query_row(
            "SELECT COUNT(*) FROM browser_windows WHERE snapshot_id = ?1",
            [sr_agent::store::db::LIVE],
            |r| r.get(0),
        )?;
        println!("Tabs tracked:    {tabs} in {windows} windows");
        println!("Private tabs:    {private} (encrypted)");
        println!(
            "Private capture: {}",
            if db.setting_bool("capture_private_windows", false) {
                "on"
            } else {
                "off"
            }
        );
        println!("Restore mode:    {}", db.setting("restore_mode")?.unwrap_or_default());

        let snaps = snapshot::list(&db, 5)?;
        if snaps.is_empty() {
            println!("Snapshots:       none");
        } else {
            println!("Snapshots:");
            for s in snaps {
                let age_s = (sr_proto::now_millis() - s.captured_at) / 1000;
                println!(
                    "  #{:<4} {:<10} {:>3} tabs, {:>2} apps   {}s ago",
                    s.id, s.kind, s.tab_count, s.app_count, age_s
                );
            }
        }
    } else {
        println!("Database:        not created yet");
    }
    Ok(())
}

fn cmd_capture() -> Result<()> {
    let dir = data_dir()?;
    let db = Db::open(&dir.join("sessions.db"))?;
    let stats = sr_agent::watcher::capture_into_live(&db)?;
    println!(
        "Captured {} apps, {} windows, {} displays",
        stats.apps, stats.windows, stats.displays
    );

    println!();
    println!("{:<6} {:<26} {:<7} {}", "TIER", "APP", "WINDOWS", "PATH");
    let mut stmt = db.conn.prepare(
        "SELECT a.restore_tier, a.display_name, a.exe_path, a.aumid, a.is_browser,
                (SELECT COUNT(*) FROM windows w
                 WHERE w.snapshot_id = a.snapshot_id AND w.app_key = a.app_key)
         FROM apps a WHERE a.snapshot_id = ?1
         ORDER BY a.restore_tier, a.display_name",
    )?;
    let rows = stmt.query_map([sr_agent::store::db::LIVE], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, Option<String>>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, i64>(4)?,
            r.get::<_, i64>(5)?,
        ))
    })?;
    for row in rows.flatten() {
        let (tier, name, exe, aumid, is_browser, windows) = row;
        let label = name.unwrap_or_else(|| "?".into());
        let path = exe.or(aumid).unwrap_or_else(|| "-".into());
        let marker = if is_browser != 0 { " [browser]" } else { "" };
        println!("{tier:<6} {label:<26}{marker} {windows:<7} {path}");
    }
    drop(stmt);

    println!();
    let mut stmt = db.conn.prepare(
        "SELECT friendly_name, is_primary, bounds_w, bounds_h, dpi
         FROM displays WHERE snapshot_id = ?1",
    )?;
    for row in stmt
        .query_map([sr_agent::store::db::LIVE], |r| {
            Ok((
                r.get::<_, Option<String>>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?,
            ))
        })?
        .flatten()
    {
        println!(
            "display {} {}x{} @ {}dpi{}",
            row.0.unwrap_or_default(),
            row.2,
            row.3,
            row.4,
            if row.1 != 0 { " (primary)" } else { "" }
        );
    }
    Ok(())
}

fn cmd_restore_apps(args: &[String]) -> Result<()> {
    let dry = args.iter().any(|a| a == "--dry-run");
    let dir = data_dir()?;
    let db = Db::open(&dir.join("sessions.db"))?;

    let snapshot_id = match flag_values(args, "--snapshot=").first() {
        Some(v) => v.parse::<i64>().context("--snapshot expects a number")?,
        None => match snapshot::newest_restorable(&db)? {
            Some(id) => id,
            None => {
                println!("No snapshot to restore from. Run --capture first, or restart the agent.");
                return Ok(());
            }
        },
    };

    let apps = sr_agent::restore::apps::plan_from_snapshot(&db, snapshot_id)?;
    if apps.is_empty() {
        println!("Snapshot #{snapshot_id} has no non-browser applications.");
        return Ok(());
    }

    println!(
        "{} applications from snapshot #{snapshot_id}{}",
        apps.len(),
        if dry { " (dry run - nothing will start)" } else { "" }
    );
    for a in &apps {
        println!(
            "  {:<6} {:<24} {} window(s)",
            a.tier.as_str(),
            a.display_name,
            a.windows.len()
        );
    }
    println!();

    // An undo point before touching anything, even in a dry run: the snapshot is
    // cheap and having one is the difference between a reversible mistake and a
    // permanent one.
    let run_id = sr_agent::restore::begin_run(&db, snapshot_id, "manual")?;

    let displays = sr_agent::watcher::displays::enumerate()?;
    let report = sr_agent::restore::apps::restore_apps(&apps, &displays, dry);

    println!("Launched: {}", report.launched.len());
    for name in &report.launched {
        println!("  + {name}");
    }
    if !report.skipped.is_empty() {
        println!("Skipped:");
        for (name, why) in &report.skipped {
            println!("  - {name}: {why}");
        }
    }
    if !report.failed.is_empty() {
        println!("Failed:");
        for (name, why) in &report.failed {
            println!("  ! {name}: {why}");
        }
    }
    println!("Windows placed: {}", report.placed);

    db.conn.execute(
        "UPDATE restore_runs SET finished_at = ?1 WHERE id = ?2",
        rusqlite::params![sr_proto::now_millis(), run_id],
    )?;
    Ok(())
}

/// Launches and places the applications from a snapshot, recording a restore run.
fn restore_apps_at_startup(db: &Db, snapshot_id: i64) -> Result<(usize, usize)> {
    let apps = sr_agent::restore::apps::plan_from_snapshot(db, snapshot_id)?;
    if apps.is_empty() {
        return Ok((0, 0));
    }

    let run_id = sr_agent::restore::begin_run(db, snapshot_id, "auto")?;
    let displays = sr_agent::watcher::displays::enumerate()?;
    let report = sr_agent::restore::apps::restore_apps(&apps, &displays, false);

    for (name, why) in &report.failed {
        tracing::warn!(app = %name, reason = %why, "application could not be restored");
    }
    for (name, why) in &report.skipped {
        tracing::info!(app = %name, reason = %why, "application skipped");
    }

    db.conn.execute(
        "UPDATE restore_runs SET finished_at = ?1 WHERE id = ?2",
        rusqlite::params![sr_proto::now_millis(), run_id],
    )?;
    Ok((report.launched.len(), report.placed))
}

fn run() -> Result<()> {
    init_logging();
    tracing::info!(version = AGENT_VERSION, "starting");

    let dir = data_dir()?;
    let db_path = dir.join("sessions.db");

    let db = open_or_recover(&db_path)?;
    let keys = KeyManager::new(&dir);

    // Sweep at startup as well as on the timer: the agent may have been down past a
    // row's expiry, and the TTL is a promise about wall-clock time, not uptime.
    match sweep_expired_private(&db) {
        Ok(n) if n > 0 => tracing::info!(count = n, "swept expired private rows at startup"),
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "startup sweep failed"),
    }

    // Snapshot the previous session BEFORE the pipe server starts.
    //
    // The live rows still describe the last session, which is exactly what we want to
    // restore - but the browser comes back with new window ids, and its first
    // reconcile is authoritative, so it reaps every row whose window no longer exists.
    // That reap is correct and it would delete the session moments before we restore
    // it. Ordering is the whole defence: no extension can connect until this is done.
    let pending = match snapshot::create_from_live(&db, "shutdown", Some("previous session")) {
        Ok(Some(id)) => {
            let tabs: i64 = db
                .conn
                .query_row("SELECT tab_count FROM snapshots WHERE id = ?1", [id], |r| r.get(0))
                .unwrap_or(0);
            tracing::info!(snapshot_id = id, tabs, "captured the previous session");
            Some(id)
        }
        Ok(None) => {
            tracing::info!("no previous session to restore");
            None
        }
        Err(e) => {
            tracing::error!(error = %e, "could not snapshot the previous session");
            None
        }
    };

    if let Err(e) = snapshot::prune(&db, db.setting_i64("snapshot_retention_count", 20)) {
        tracing::warn!(error = %e, "snapshot prune failed");
    }

    // Application restore, if the user has asked for it.
    //
    // Gated on `restore_mode = auto` specifically, not merely "not off" as the browser
    // path is. Launching a dozen applications unprompted is materially more intrusive
    // than adding tabs to a browser the user just opened, and until there is a review
    // window to ask with, the honest default is not to do it. `ask` therefore behaves
    // as "not yet" here rather than silently meaning "yes".
    if let Some(snapshot_id) = pending {
        let mode = db.setting("restore_mode")?.unwrap_or_else(|| "ask".into());
        if mode == "auto" {
            match restore_apps_at_startup(&db, snapshot_id) {
                Ok((launched, placed)) => {
                    tracing::info!(launched, placed, "restored applications")
                }
                // A failed app restore must not stop the agent from capturing.
                Err(e) => tracing::error!(error = %e, "application restore failed"),
            }
        } else {
            let count: i64 = db
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM apps WHERE snapshot_id = ?1 AND is_browser = 0",
                    [snapshot_id],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            if count > 0 {
                tracing::info!(
                    apps = count,
                    "applications available to restore; run --restore-apps, or set restore_mode=auto"
                );
            }
        }
    }

    let shared = Arc::new(Shared {
        db: Mutex::new(db),
        keys,
        pending_restore: Mutex::new(PendingRestore::new(pending)),
    });

    let sweeper = Arc::clone(&shared);
    std::thread::spawn(move || loop {
        std::thread::sleep(SWEEP_INTERVAL);
        let db = sweeper.db.lock().unwrap();
        match sweep_expired_private(&db) {
            Ok(n) if n > 0 => tracing::info!(count = n, "swept expired private rows"),
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "sweep failed"),
        }
    });

    // Periodic application capture, the desktop equivalent of the browser's reconcile.
    //
    // Polling rather than SetWinEventHook for now: a 60s sweep bounds staleness the
    // same way the browser's does, and it is the same interval the whole design is
    // already built around. Event hooks are an optimization on top (docs/03-capture.md),
    // and adding them before the polled pass is known-correct would make failures
    // harder to attribute.
    let capturer = Arc::clone(&shared);
    let capture_interval = {
        let db = capturer.db.lock().unwrap();
        Duration::from_secs(db.setting_i64("reconcile_interval_seconds", 60).max(15) as u64)
    };
    std::thread::spawn(move || {
        loop {
            {
                let db = capturer.db.lock().unwrap();
                match sr_agent::watcher::capture_into_live(&db) {
                    Ok(s) => tracing::debug!(
                        apps = s.apps,
                        windows = s.windows,
                        displays = s.displays,
                        "captured desktop"
                    ),
                    // Enumeration touching dozens of processes will occasionally lose a
                    // race with one exiting. Never fatal.
                    Err(e) => tracing::warn!(error = %e, "desktop capture failed"),
                }
            }
            std::thread::sleep(capture_interval);
        }
    });

    serve(shared)
}

/// Opens the database, or moves a corrupt one aside and starts fresh.
///
/// No automatic repair: a file that fails `integrity_check` is untrustworthy by
/// definition, and salvaging rows from it risks carrying the corruption forward
/// (docs/08-agent.md).
fn open_or_recover(path: &std::path::Path) -> Result<Db> {
    match Db::open(path) {
        Ok(db) if db.integrity_ok() => Ok(db),
        Ok(_) => {
            let aside = path.with_extension(format!("db.corrupt-{}", sr_proto::now_millis()));
            tracing::error!(moved_to = %aside.display(), "database failed integrity check");
            std::fs::rename(path, &aside).context("moving corrupt database aside")?;
            Db::open(path)
        }
        Err(e) => Err(e),
    }
}

fn init_logging() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_env("SR_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    // Logs must never contain URLs, window titles, or command lines at default level
    // (docs/06-privacy-security.md). The ingest path logs counts and key hashes only.
    fmt().with_env_filter(filter).with_target(false).init();
}
