//! T2: the best-effort flush when Windows is shutting down.
//!
//! This is the *third* tier of capture and deliberately the least important one
//! (docs/01-architecture.md). T1's 60-second reconcile already bounds loss, so this is
//! pure upside: it turns "up to a minute stale" into "current" for an orderly
//! shutdown, and contributes nothing on a power cut, which is exactly why the design
//! does not depend on it.
//!
//! The budget is hard. Windows gives an application a limited window before it is
//! force-terminated, and an app that overstays earns the user a "this app is
//! preventing shutdown" screen - a reliable way to get uninstalled. Two seconds is
//! generous for a snapshot and nowhere near long enough to annoy anyone.

use crate::store::db::Db;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const FLUSH_BUDGET: Duration = Duration::from_secs(2);

#[cfg(windows)]
static STATE: Mutex<Option<Arc<Mutex<Db>>>> = Mutex::new(None);

/// Installs a hidden message-only window that listens for end-of-session messages.
///
/// Message-only (`HWND_MESSAGE`) so it never appears anywhere, and created on the
/// thread that runs the message pump so its messages are actually dispatched.
#[cfg(windows)]
pub fn install(db: Arc<Mutex<Db>>) -> anyhow::Result<()> {
    use windows::core::{w, PCWSTR};
    use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::Shutdown::{ShutdownBlockReasonCreate, ShutdownBlockReasonDestroy};
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, RegisterClassW, CW_USEDEFAULT, HWND_MESSAGE,
        WINDOW_EX_STYLE, WINDOW_STYLE, WM_ENDSESSION, WM_QUERYENDSESSION, WNDCLASSW,
    };

    *STATE.lock().unwrap() = Some(db);

    unsafe extern "system" fn wndproc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match msg {
            WM_QUERYENDSESSION => {
                // Tell Windows why we are briefly holding things up. Without a reason
                // the user sees an unexplained stall; with one they see our name.
                let _ = ShutdownBlockReasonCreate(hwnd, w!("Saving your session..."));
                flush_now();
                let _ = ShutdownBlockReasonDestroy(hwnd);
                // Always agree to shut down. Returning FALSE would block the shutdown,
                // which is not a decision a session-restore tool gets to make.
                LRESULT(1)
            }
            WM_ENDSESSION => {
                if wparam.0 != 0 {
                    flush_now();
                }
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }

    unsafe {
        let instance = GetModuleHandleW(None)?;
        let class_name = w!("SessionRestoreShutdownSink");

        let class = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: HINSTANCE(instance.0),
            lpszClassName: class_name,
            ..Default::default()
        };
        // A duplicate registration is fine; only the first one matters.
        RegisterClassW(&class);

        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class_name,
            PCWSTR::null(),
            WINDOW_STYLE(0),
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            0,
            0,
            HWND_MESSAGE,
            None,
            HINSTANCE(instance.0),
            None,
        )?;

        if hwnd.0.is_null() {
            anyhow::bail!("could not create the shutdown sink window");
        }
    }

    Ok(())
}

/// Snapshots the live session, bounded by [`FLUSH_BUDGET`].
///
/// Runs on a worker thread so a stuck database cannot hold the message pump - and
/// therefore the whole shutdown - open past the budget.
#[cfg(windows)]
fn flush_now() {
    let Some(db) = STATE.lock().unwrap().clone() else {
        return;
    };

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let result = (|| -> anyhow::Result<()> {
            let db = db.lock().map_err(|_| anyhow::anyhow!("database lock poisoned"))?;
            // Capture applications first: the browser half is already being journaled
            // continuously, while the desktop is only polled.
            let _ = crate::watcher::capture_into_live(&db);
            crate::store::snapshot::create_from_live(&db, "shutdown", Some("at shutdown"))?;
            Ok(())
        })();
        let _ = tx.send(result.is_ok());
    });

    match rx.recv_timeout(FLUSH_BUDGET) {
        Ok(true) => tracing::info!("session saved at shutdown"),
        Ok(false) => tracing::warn!("shutdown flush failed"),
        // Abandoned rather than waited on. T1 already bounds the loss, so overstaying
        // would trade a guarantee we do not need for a shutdown stall we cannot afford.
        Err(_) => tracing::warn!("shutdown flush exceeded its budget; abandoned"),
    }
}

#[cfg(not(windows))]
pub fn install(_db: Arc<Mutex<Db>>) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_budget_is_short_enough_not_to_stall_shutdown() {
        // Windows force-terminates after a few seconds and shows a blaming screen
        // before that. This must stay well under it.
        assert!(FLUSH_BUDGET <= Duration::from_secs(3));
    }
}
