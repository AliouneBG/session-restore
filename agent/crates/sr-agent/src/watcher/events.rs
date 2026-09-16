//! T0 for the desktop: window events instead of waiting for the next poll.
//!
//! The 60-second reconcile stays exactly as it is - it is the floor on correctness and
//! the thing that makes a power cut cost at most one interval (docs/01-architecture.md).
//! This sits on top so that moving a window, opening an app, or switching a document
//! is reflected in seconds rather than up to a minute.
//!
//! Two details matter more than they look:
//!
//! - **Out-of-context hooks only.** An in-context hook injects a DLL into every process
//!   on the machine. That is an enormous amount of ambient authority for a convenience
//!   tool, and it is not a trade worth making for slightly lower latency.
//! - **`EVENT_OBJECT_LOCATIONCHANGE` fires continuously during a drag**, dozens of times
//!   a second. Without coalescing, dragging one window across the screen would trigger
//!   hundreds of full desktop captures.

use std::sync::mpsc::{Receiver, Sender};
use std::sync::OnceLock;
use std::time::Duration;

/// Quiet period before a burst of window events turns into one capture.
pub const DEBOUNCE: Duration = Duration::from_secs(2);

/// Upper bound on how long a continuous stream of events can defer a capture.
///
/// Without this, dragging a window around for a minute would keep resetting the
/// debounce and never capture at all.
pub const MAX_DEFER: Duration = Duration::from_secs(10);

static SIGNAL: OnceLock<Sender<()>> = OnceLock::new();

/// Installs the hooks. Must be called on a thread that runs a message pump.
///
/// Returns a receiver that emits once per debounced burst; the caller decides what to
/// do with it (in practice: one `capture_into_live`).
#[cfg(windows)]
pub fn install() -> anyhow::Result<Receiver<()>> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::Accessibility::{SetWinEventHook, HWINEVENTHOOK};
    use windows::Win32::UI::WindowsAndMessaging::{
        EVENT_OBJECT_CREATE, EVENT_OBJECT_DESTROY, EVENT_OBJECT_LOCATIONCHANGE,
        EVENT_OBJECT_NAMECHANGE, EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_MINIMIZEEND,
        EVENT_SYSTEM_MINIMIZESTART, OBJID_WINDOW, WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS,
    };

    let (raw_tx, raw_rx) = std::sync::mpsc::channel::<()>();
    let _ = SIGNAL.set(raw_tx);

    unsafe extern "system" fn callback(
        _hook: HWINEVENTHOOK,
        _event: u32,
        _hwnd: HWND,
        id_object: i32,
        id_child: i32,
        _thread: u32,
        _time: u32,
    ) {
        // Only whole windows. Without this filter every caret move, menu highlight and
        // focus change inside an application arrives here too.
        if id_object != OBJID_WINDOW.0 || id_child != 0 {
            return;
        }
        if let Some(tx) = SIGNAL.get() {
            // Never blocks: a full channel just means a capture is already pending.
            let _ = tx.send(());
        }
    }

    unsafe {
        // Ranges rather than one hook per event: fewer hooks is less overhead in every
        // process that raises them.
        for (from, to) in [
            (EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND),
            (EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MINIMIZEEND),
            (EVENT_OBJECT_CREATE, EVENT_OBJECT_DESTROY),
            (EVENT_OBJECT_NAMECHANGE, EVENT_OBJECT_NAMECHANGE),
            (EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_LOCATIONCHANGE),
        ] {
            let hook = SetWinEventHook(
                from,
                to,
                None,
                Some(callback),
                0,
                0,
                // OUTOFCONTEXT keeps us out of other processes entirely.
                // SKIPOWNPROCESS stops our own review window from triggering captures.
                WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
            );
            if hook.is_invalid() {
                anyhow::bail!("SetWinEventHook failed for {from:#x}..{to:#x}");
            }
            // Deliberately never unhooked: the hooks live as long as the agent, and
            // Windows tears them down when the process exits.
        }
    }

    Ok(debounce(raw_rx))
}

/// Collapses a burst of events into a single signal.
///
/// Trailing debounce with a ceiling: quiet for [`DEBOUNCE`] emits, and a stream that
/// never goes quiet still emits every [`MAX_DEFER`] so a long drag is not invisible.
fn debounce(raw: Receiver<()>) -> Receiver<()> {
    let (tx, rx) = std::sync::mpsc::channel();

    std::thread::spawn(move || {
        loop {
            // Block until something happens at all; an idle desktop costs nothing.
            if raw.recv().is_err() {
                return;
            }
            let first = std::time::Instant::now();

            // Then drain until quiet, or until the ceiling.
            loop {
                match raw.recv_timeout(DEBOUNCE) {
                    Ok(()) => {
                        if first.elapsed() >= MAX_DEFER {
                            break;
                        }
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
                }
            }

            if tx.send(()).is_err() {
                return;
            }
        }
    });

    rx
}

#[cfg(not(windows))]
pub fn install() -> anyhow::Result<Receiver<()>> {
    let (_tx, rx) = std::sync::mpsc::channel();
    Ok(rx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;
    use std::time::Instant;

    #[test]
    fn a_burst_of_events_becomes_one_capture() {
        // The drag case: dozens of events, one capture.
        let (tx, raw) = channel();
        let out = debounce(raw);
        for _ in 0..50 {
            tx.send(()).unwrap();
        }
        out.recv_timeout(Duration::from_secs(5)).expect("no signal");
        assert!(
            out.recv_timeout(Duration::from_millis(500)).is_err(),
            "one burst produced more than one capture"
        );
    }

    #[test]
    fn it_waits_for_quiet_rather_than_firing_immediately() {
        let (tx, raw) = channel();
        let out = debounce(raw);
        let start = Instant::now();
        tx.send(()).unwrap();
        out.recv_timeout(Duration::from_secs(5)).expect("no signal");
        assert!(
            start.elapsed() >= DEBOUNCE - Duration::from_millis(250),
            "fired after only {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn a_continuous_stream_still_gets_captured() {
        // A long drag must not defer forever.
        let (tx, raw) = channel();
        let out = debounce(raw);
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let s = std::sync::Arc::clone(&stop);
        std::thread::spawn(move || {
            while !s.load(std::sync::atomic::Ordering::Relaxed) {
                if tx.send(()).is_err() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        });

        let got = out.recv_timeout(MAX_DEFER + Duration::from_secs(3));
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(got.is_ok(), "a continuous stream never produced a capture");
    }

    #[test]
    fn separate_bursts_produce_separate_captures() {
        let (tx, raw) = channel();
        let out = debounce(raw);

        tx.send(()).unwrap();
        out.recv_timeout(Duration::from_secs(5)).expect("first burst");

        tx.send(()).unwrap();
        out.recv_timeout(Duration::from_secs(5)).expect("second burst");
    }

    #[test]
    fn an_idle_desktop_produces_nothing() {
        let (_tx, raw) = channel::<()>();
        let out = debounce(raw);
        assert!(out.recv_timeout(Duration::from_millis(600)).is_err());
    }
}
