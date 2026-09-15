//! Wire types shared by the agent and the relay.
//!
//! These mirror `schema/protocol.schema.json`, which is the source of truth and from
//! which the extension's TypeScript types are generated. Keep the two in step; a CI
//! check should eventually assert it (see docs/09-roadmap.md).
//!
//! Deliberately tolerant when deserializing: unknown fields are ignored rather than
//! rejected, because the agent and extension ship through different stores and are
//! routinely different versions (docs/05-ipc-protocol.md).

pub mod frame;

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BrowserKind {
    Chrome,
    Edge,
    Firefox,
}

impl BrowserKind {
    pub fn as_str(self) -> &'static str {
        match self {
            BrowserKind::Chrome => "chrome",
            BrowserKind::Edge => "edge",
            BrowserKind::Firefox => "firefox",
        }
    }
}

/// Stamped by the relay, never trusted from the extension.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageSource {
    pub browser: BrowserKind,
    pub profile_key: String,
    pub ext_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    pub v: u32,
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub ts: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub src: Option<MessageSource>,
    pub body: serde_json::Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WindowState {
    Normal,
    Maximized,
    Minimized,
    Fullscreen,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Op {
    Upsert,
    Remove,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowserWindowDelta {
    pub op: Op,
    pub window_id: String,
    #[serde(default)]
    pub private: bool,
    #[serde(default)]
    pub state: Option<WindowState>,
    #[serde(default)]
    pub x: Option<i32>,
    #[serde(default)]
    pub y: Option<i32>,
    #[serde(default)]
    pub w: Option<i32>,
    #[serde(default)]
    pub h: Option<i32>,
    #[serde(default)]
    pub focused: bool,
}

/// A single tab change.
///
/// `private` is the only routing signal the agent needs: see
/// `sr_agent::ingest::ingest_tab`, the one function permitted to act on it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TabDelta {
    pub op: Op,
    pub tab_key: String,
    pub window_id: String,
    #[serde(default)]
    pub group_key: Option<String>,
    #[serde(default)]
    pub index: Option<i32>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub favicon_hash: Option<String>,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub active: bool,
    #[serde(default)]
    pub muted: bool,
    #[serde(default)]
    pub last_accessed: Option<i64>,
    #[serde(default)]
    pub private: bool,
    #[serde(default = "default_true")]
    pub restorable: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TabGroupDelta {
    pub op: Op,
    pub group_key: String,
    pub window_id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub color: Option<String>,
    #[serde(default)]
    pub collapsed: bool,
}

/// Body shared by `tab_delta` (incremental) and `full_state` (authoritative).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StateBody {
    #[serde(default)]
    pub windows: Vec<BrowserWindowDelta>,
    #[serde(default)]
    pub tabs: Vec<TabDelta>,
    #[serde(default)]
    pub groups: Vec<TabGroupDelta>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelloBody {
    pub ext_version: String,
    pub browser_version: String,
    pub incognito_access: bool,
    #[serde(default)]
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelloAckBody {
    pub agent_version: String,
    pub protocol_version: u32,
    pub capture_enabled: bool,
    /// When false the extension drops private events at the source rather than
    /// sending them. The cheapest enforcement point is the earliest one.
    pub capture_private: bool,
    pub reconcile_interval_s: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentUnavailableBody {
    pub reason: String,
    #[serde(default)]
    pub detail: Option<String>,
}

impl Envelope {
    pub fn new(kind: &str, body: serde_json::Value) -> Self {
        Envelope {
            v: PROTOCOL_VERSION,
            id: new_id(),
            kind: kind.to_string(),
            ts: now_millis(),
            src: None,
            body,
        }
    }

    /// True if this message is from a protocol version whose field meanings we cannot
    /// rely on. Older versions are accepted; newer are refused.
    pub fn is_too_new(&self) -> bool {
        self.v > PROTOCOL_VERSION
    }
}

pub fn now_millis() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Unique enough for correlation and log tracing. Not sortable, not cryptographic.
pub fn new_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("{:x}{:x}", now_millis(), n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tab_delta_defaults_are_safe() {
        // A minimal upsert must default to non-private and restorable. Defaulting
        // `private` to true would be safer in isolation but would silently encrypt
        // and expire every ordinary tab.
        let t: TabDelta =
            serde_json::from_str(r#"{"op":"upsert","tab_key":"w1:t1","window_id":"w1"}"#).unwrap();
        assert!(!t.private);
        assert!(t.restorable);
        assert!(!t.pinned);
    }

    #[test]
    fn unknown_fields_are_ignored_not_fatal() {
        // The extension may ship a newer additive field than this agent knows.
        let t: TabDelta = serde_json::from_str(
            r#"{"op":"upsert","tab_key":"w1:t1","window_id":"w1","future_field":42}"#,
        )
        .unwrap();
        assert_eq!(t.tab_key, "w1:t1");
    }

    #[test]
    fn private_flag_survives_roundtrip() {
        let json = r#"{"op":"upsert","tab_key":"w9:t2","window_id":"w9","private":true,"url":"https://x.test/"}"#;
        let t: TabDelta = serde_json::from_str(json).unwrap();
        assert!(t.private);
        let back = serde_json::to_string(&t).unwrap();
        assert!(back.contains("\"private\":true"));
    }

    #[test]
    fn envelope_rejects_newer_protocol_versions() {
        let mut e = Envelope::new("hello", serde_json::json!({}));
        assert!(!e.is_too_new());
        e.v = PROTOCOL_VERSION + 1;
        assert!(e.is_too_new());
    }

    #[test]
    fn envelope_type_field_is_named_type_on_the_wire() {
        let e = Envelope::new("tab_delta", serde_json::json!({}));
        let s = serde_json::to_string(&e).unwrap();
        assert!(s.contains(r#""type":"tab_delta""#), "{s}");
        assert!(!s.contains("\"kind\""));
    }

    #[test]
    fn src_is_omitted_when_absent_rather_than_null() {
        let e = Envelope::new("hello", serde_json::json!({}));
        assert!(!serde_json::to_string(&e).unwrap().contains("src"));
    }

    #[test]
    fn ids_are_unique() {
        let a = new_id();
        let b = new_id();
        assert_ne!(a, b);
    }

    #[test]
    fn browser_kind_matches_schema_spelling() {
        assert_eq!(
            serde_json::to_string(&BrowserKind::Firefox).unwrap(),
            r#""firefox""#
        );
    }
}
