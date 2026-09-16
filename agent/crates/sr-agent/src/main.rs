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

fn main() {
    if let Err(e) = run() {
        eprintln!("sr-agent: fatal: {e:#}");
        std::process::exit(1);
    }
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
