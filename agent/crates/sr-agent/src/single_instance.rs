//! One agent per logon session, and a way to poke the one that is already running.
//!
//! Two problems, one mechanism.
//!
//! The first is a bug: launching the agent twice started a second tray icon and a
//! second capture loop, and the newcomer then spun forever failing to claim the pipe,
//! because `FILE_FLAG_FIRST_PIPE_INSTANCE` had already been taken (ADR-0002). Nothing
//! told the user, they just had two icons.
//!
//! The second is what a person reasonably expects. Session Restore has no main window,
//! so clicking its Start Menu entry while it is already running looks like it did
//! nothing. Opening settings is the honest answer to "I clicked the app".
//!
//! Both are the same handshake: the second process discovers it is second, asks the
//! first to show itself, and exits.
//!
//! Everything here is in the `Local\` namespace, which Windows scopes to the logon
//! session. That is the isolation this needs: two users signed in at once each get
//! their own agent, which is the whole premise of a per-user agent (ADR-0001).

#[cfg(windows)]
use anyhow::Result;

/// Held for the process lifetime. Dropping it releases the claim.
#[cfg(windows)]
pub struct InstanceLock {
    handle: windows::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
impl Drop for InstanceLock {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.handle);
        }
    }
}

/// SAFETY: a Windows `HANDLE` is a process-wide kernel object reference, not
/// thread-affine. Moving it between threads is exactly what handles are for.
#[cfg(windows)]
unsafe impl Send for InstanceLock {}

#[cfg(windows)]
const MUTEX_NAME: windows::core::PCWSTR = windows::core::w!(r"Local\SessionRestore.Agent");
#[cfg(windows)]
const EVENT_NAME: windows::core::PCWSTR = windows::core::w!(r"Local\SessionRestore.ShowUi");

/// Claims the single-instance slot.
///
/// `Ok(Some(lock))` means this process is the agent. `Ok(None)` means another one
/// already is.
#[cfg(windows)]
pub fn acquire() -> Result<Option<InstanceLock>> {
    use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows::Win32::System::Threading::CreateMutexW;

    unsafe {
        let handle = CreateMutexW(None, true, MUTEX_NAME)?;
        // CreateMutexW succeeds either way; the error code is what distinguishes
        // "created it" from "opened someone else's".
        if GetLastError() == ERROR_ALREADY_EXISTS {
            let _ = windows::Win32::Foundation::CloseHandle(handle);
            return Ok(None);
        }
        Ok(Some(InstanceLock { handle }))
    }
}

/// Asks the running agent to show its settings window.
///
/// Best effort by design: if the other process is mid-shutdown there is nothing useful
/// to do about it, and the user can click again.
#[cfg(windows)]
pub fn signal_show_ui() -> Result<()> {
    use windows::Win32::System::Threading::{OpenEventW, SetEvent, EVENT_MODIFY_STATE};

    unsafe {
        let handle = OpenEventW(EVENT_MODIFY_STATE, false, EVENT_NAME)?;
        let r = SetEvent(handle);
        let _ = windows::Win32::Foundation::CloseHandle(handle);
        r?;
    }
    Ok(())
}

/// Runs `on_signal` every time another launch asks for the interface.
///
/// Spawns a thread that blocks on the event. A blocking wait rather than a poll:
/// this fires a few times in a process's life, and a timer checking a flag forever
/// would cost more than the thread does.
#[cfg(windows)]
pub fn listen_for_show_ui<F>(on_signal: F) -> Result<()>
where
    F: Fn() + Send + 'static,
{
    use windows::Win32::Foundation::WAIT_OBJECT_0;
    use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject, INFINITE};

    let handle = unsafe { CreateEventW(None, false, false, EVENT_NAME)? };
    let lock = InstanceLock { handle };

    std::thread::spawn(move || {
        // Moved in so the handle lives exactly as long as the thread that waits on it.
        let lock = lock;
        loop {
            let r = unsafe { WaitForSingleObject(lock.handle, INFINITE) };
            if r != WAIT_OBJECT_0 {
                tracing::warn!(result = ?r, "stopped listening for launches");
                return;
            }
            tracing::info!("another launch asked for the interface");
            on_signal();
        }
    });

    Ok(())
}

#[cfg(not(windows))]
pub struct InstanceLock;

#[cfg(not(windows))]
pub fn acquire() -> anyhow::Result<Option<InstanceLock>> {
    Ok(Some(InstanceLock))
}

#[cfg(not(windows))]
pub fn signal_show_ui() -> anyhow::Result<()> {
    Ok(())
}

#[cfg(not(windows))]
pub fn listen_for_show_ui<F>(_f: F) -> anyhow::Result<()>
where
    F: Fn() + Send + 'static,
{
    Ok(())
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    /// The whole point: the second process must be able to tell that it is second.
    #[test]
    fn only_one_process_holds_the_slot_at_a_time() {
        let first = acquire().unwrap();
        assert!(first.is_some(), "a free slot must be claimable");

        let second = acquire().unwrap();
        assert!(second.is_none(), "the slot was claimed twice");

        drop(first);
        let again = acquire().unwrap();
        assert!(again.is_some(), "the slot must free up when the agent exits");
    }

    #[test]
    fn signalling_nobody_is_an_error_not_a_hang() {
        // No agent is listening in this test process, so opening the event fails
        // rather than blocking. A launcher that hung here would be worse than one
        // that reported nothing to talk to.
        let _ = signal_show_ui();
    }
}
