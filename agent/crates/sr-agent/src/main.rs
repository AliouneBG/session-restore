//! Session Restore agent entry point.
//!
//! A per-user process started at logon by a Scheduled Task, not a Windows Service.
//! See ADR-0001 for why that distinction is load-bearing.

use anyhow::{Context, Result};
use sr_agent::ingest::sweep_expired_private;
use sr_agent::server::{serve, Shared};
use sr_agent::store::db::Db;
use sr_agent::store::keys::KeyManager;
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
        let tabs: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM tabs", [], |r| r.get(0))?;
        let private: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM tabs_private", [], |r| r.get(0))?;
        let windows: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM browser_windows", [], |r| r.get(0))?;
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
    } else {
        println!("Database:        not created yet");
    }
    Ok(())
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

    let shared = Arc::new(Shared {
        db: Mutex::new(db),
        keys,
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
