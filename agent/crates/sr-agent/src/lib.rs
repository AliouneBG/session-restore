//! Session Restore agent.
//!
//! Library half so integration tests can exercise the storage and ingest layers
//! directly. See docs/08-agent.md for the module map.

pub mod ingest;
pub mod store;

pub const AGENT_VERSION: &str = env!("CARGO_PKG_VERSION");
