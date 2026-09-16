//! The native messaging relay.
//!
//! The browser spawns this as a child process when the extension calls
//! `connectNative`, and kills it when the browser exits. It does exactly two things:
//!
//! 1. Frame-translate between native messaging's stdio framing and the agent's named
//!    pipe. Both use the same 4-byte-LE-length format, so this is close to a memcpy.
//! 2. Stamp `src` on every inbound message with which browser it came from, because
//!    the extension cannot be trusted to report its own host (docs/05-ipc-protocol.md).
//!
//! It deliberately holds no state and touches no disk. Native messaging spawns a child
//! of the browser and cannot connect to an already-running process, which is why this
//! exists at all rather than the extension talking to the agent directly. Making it
//! the least-privileged component is the consolation prize, and a real one: a bug in
//! message parsing here reaches a pipe, not a database (ADR-0002).

use sr_proto::frame::{read_frame, write_frame, FrameError, MAX_INBOUND_BYTES, MAX_OUTBOUND_BYTES};
use std::io::{self};

const RELAY_VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    // Never write anything but frames to stdout: the browser is parsing it. Diagnostics
    // go to stderr, which the browser logs.
    let code = match run() {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("sr-relay: {e:#}");
            1
        }
    };
    std::process::exit(code);
}

fn run() -> anyhow::Result<()> {
    let browser = detect_browser();
    eprintln!("sr-relay {RELAY_VERSION} starting (browser={browser})");

    let pipe_name = sr_ipc::pipe_name()?;

    let mut pipe = match sr_ipc::connect(&pipe_name) {
        Ok(p) => {
            eprintln!("sr-relay: connected to {pipe_name}");
            p
        }
        Err(e) => {
            // The agent is not running. Tell the extension so it can show a "finish
            // setup" affordance and back off, rather than retrying in a tight loop.
            eprintln!("sr-relay: agent unavailable: {e:#}");
            let msg = serde_json::json!({
                "v": sr_proto::PROTOCOL_VERSION,
                "id": sr_proto::new_id(),
                "type": "agent_unavailable",
                "ts": sr_proto::now_millis(),
                "body": { "reason": "not_running", "detail": e.to_string() }
            });
            let mut out = io::stdout().lock();
            let _ = write_frame(&mut out, &msg.to_string(), MAX_OUTBOUND_BYTES);
            return Ok(());
        }
    };

    // Pipe -> stdout, on its own thread. The agent can push (restore_session,
    // settings_changed) at any time, so this direction cannot be driven by the
    // request loop.
    let mut pipe_reader = pipe.try_clone()?;
    let pump = std::thread::spawn(move || {
        let mut out = io::stdout().lock();
        loop {
            match read_frame(&mut pipe_reader, MAX_OUTBOUND_BYTES) {
                Ok(msg) => {
                    eprintln!("sr-relay: <- agent {} bytes", msg.len());
                    if write_frame(&mut out, &msg, MAX_OUTBOUND_BYTES).is_err() {
                        break; // browser closed stdout
                    }
                }
                Err(FrameError::Closed) => break,
                Err(e) => {
                    eprintln!("sr-relay: pipe read: {e}");
                    break;
                }
            }
        }
    });

    // stdin -> pipe, on the main thread.
    let mut stdin = io::stdin().lock();
    loop {
        match read_frame(&mut stdin, MAX_INBOUND_BYTES) {
            Ok(raw) => {
                let stamped = stamp_source(&raw, browser);
                // Byte counts only, never payloads - a relay that logged message
                // bodies would put private URLs in the browser's log.
                eprintln!("sr-relay: -> agent {} bytes", stamped.len());
                if let Err(e) = write_frame(&mut pipe, &stamped, MAX_OUTBOUND_BYTES) {
                    eprintln!("sr-relay: pipe write: {e}");
                    break;
                }
            }
            // The browser exiting closes our stdin. Expected, not an error.
            Err(FrameError::Closed) => break,
            Err(e) => {
                eprintln!("sr-relay: stdin read: {e}");
                break;
            }
        }
    }

    drop(pipe);
    let _ = pump.join();
    Ok(())
}

/// Overwrites `src` on an inbound message with values the relay determined itself.
///
/// Any `src` the extension supplied is discarded rather than merged - a field the
/// extension controls must never influence how the agent attributes its data.
fn stamp_source(raw: &str, browser: &'static str) -> String {
    match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(mut v) => {
            if let Some(obj) = v.as_object_mut() {
                let ext_version = obj
                    .get("body")
                    .and_then(|b| b.get("ext_version"))
                    .and_then(|s| s.as_str())
                    .unwrap_or("unknown")
                    .to_string();
                obj.insert(
                    "src".into(),
                    serde_json::json!({
                        "browser": browser,
                        "profile_key": profile_key(),
                        "ext_version": ext_version,
                    }),
                );
            }
            v.to_string()
        }
        // Not JSON we can parse. Forward unchanged and let the agent reject it, rather
        // than silently dropping a message we merely failed to understand.
        Err(_) => raw.to_string(),
    }
}

/// Identifies the browser by walking up the process tree.
///
/// The immediate parent is not always the browser: Chromium spawns native messaging
/// hosts from a utility/broker process, so the direct parent can be another
/// `msedge.exe`-family process or an intermediate that matches nothing. Checking only
/// the parent silently mislabelled Edge as Chrome, which would merge two browsers'
/// tabs under one profile key on restore.
///
/// Walks a few levels and takes the first recognizable browser. Order matters: Edge's
/// image name is `msedge.exe`, and a naive `contains("edge")` check would also match
/// nothing useful, while checking `chrome` first would never reach Edge.
fn detect_browser() -> &'static str {
    for name in ancestor_process_names(6) {
        let n = name.to_ascii_lowercase();
        if n.contains("firefox") {
            return "firefox";
        }
        if n.contains("msedge") {
            return "edge";
        }
        if n.contains("chrome") {
            return "chrome";
        }
    }
    // Unknown host. Chrome is the safest default: it is the most common, and a wrong
    // guess costs attribution accuracy, not correctness.
    "chrome"
}

/// TODO(M2): derive from the parent's `--profile-directory=` argument so "Chrome Work"
/// and "Chrome Personal" stay distinct (docs/03-capture.md). Reading another process's
/// command line needs the PEB/ETW machinery the agent grows in M2; until then every
/// profile of a given browser shares one key, which merges their tabs on restore.
fn profile_key() -> &'static str {
    "default"
}

/// Image names of this process's ancestors, nearest first, up to `max` levels.
///
/// Builds a pid -> (parent pid, image name) map from one Toolhelp snapshot rather than
/// re-snapshotting per level, and stops on a cycle so a recycled PID cannot loop.
#[cfg(windows)]
fn ancestor_process_names(max: usize) -> Vec<String> {
    use std::collections::{HashMap, HashSet};
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };

    let mut out = Vec::new();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return out;
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };

        let mut table: HashMap<u32, (u32, String)> = HashMap::new();
        if Process32FirstW(snap, &mut entry).is_ok() {
            loop {
                let end = entry
                    .szExeFile
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(entry.szExeFile.len());
                table.insert(
                    entry.th32ProcessID,
                    (
                        entry.th32ParentProcessID,
                        String::from_utf16_lossy(&entry.szExeFile[..end]),
                    ),
                );
                if Process32NextW(snap, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);

        let mut pid = std::process::id();
        let mut seen = HashSet::new();
        for _ in 0..max {
            let Some((parent, _)) = table.get(&pid) else {
                break;
            };
            let parent = *parent;
            if parent == 0 || !seen.insert(parent) {
                break;
            }
            match table.get(&parent) {
                Some((_, name)) => out.push(name.clone()),
                None => break,
            }
            pid = parent;
        }
    }
    out
}

#[cfg(not(windows))]
fn ancestor_process_names(_max: usize) -> Vec<String> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamps_src_onto_a_message() {
        let out = stamp_source(
            r#"{"v":1,"id":"a","type":"hello","ts":1,"body":{"ext_version":"1.2.3"}}"#,
            "firefox",
        );
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["src"]["browser"], "firefox");
        assert_eq!(v["src"]["ext_version"], "1.2.3");
    }

    #[test]
    fn replaces_a_src_the_extension_tried_to_supply() {
        // The extension must not be able to attribute its data to another browser.
        let out = stamp_source(
            r#"{"v":1,"id":"a","type":"hello","ts":1,"src":{"browser":"chrome","profile_key":"spoofed","ext_version":"x"},"body":{}}"#,
            "firefox",
        );
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["src"]["browser"], "firefox");
        assert_ne!(v["src"]["profile_key"], "spoofed");
    }

    #[test]
    fn preserves_the_body_untouched() {
        let out = stamp_source(
            r#"{"v":1,"id":"a","type":"tab_delta","ts":1,"body":{"tabs":[{"op":"upsert","tab_key":"w1:t1","window_id":"w1","private":true}]}}"#,
            "chrome",
        );
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["body"]["tabs"][0]["private"], true);
        assert_eq!(v["body"]["tabs"][0]["tab_key"], "w1:t1");
    }

    #[test]
    fn forwards_unparseable_input_rather_than_dropping_it() {
        assert_eq!(stamp_source("not json", "chrome"), "not json");
    }
}
