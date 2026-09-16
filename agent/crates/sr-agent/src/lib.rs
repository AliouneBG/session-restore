//! Session Restore agent.
//!
//! Library half so integration tests can exercise the storage, ingest, and server
//! layers directly. See docs/08-agent.md for the module map.

pub mod ingest;
pub mod restore;
pub mod server;
pub mod setup;
pub mod store;
pub mod ui;
pub mod watcher;

pub const AGENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// `%LOCALAPPDATA%\SessionRestore`, created if absent.
///
/// Per-user by design: the OS gives us data isolation between accounts for free, which
/// is one of the reasons this is a logon agent rather than a service (ADR-0001).
pub fn data_dir() -> anyhow::Result<std::path::PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(std::path::PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("LOCALAPPDATA is not set"))?;
    let dir = base.join("SessionRestore");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}
