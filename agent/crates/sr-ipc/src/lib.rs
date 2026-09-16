//! Named pipe transport, shared by the agent (server) and the relay (client).
//!
//! This is its own crate so the relay can use it without pulling in SQLite, crypto, or
//! anything else the agent links. The relay is the only component the browser can
//! start, so it stays the component with the least authority and the smallest
//! dependency surface (ADR-0002).
//!
//! Hardening, per ADR-0002:
//!
//! - The pipe name includes a hash of the user's SID, so two users on one machine
//!   cannot collide or probe each other's pipe.
//! - The DACL grants the owning user only. Not `Everyone`, not `Authenticated Users`.
//! - The first instance is created with `FILE_FLAG_FIRST_PIPE_INSTANCE`, so a hostile
//!   local process that starts before the agent cannot squat the name and receive the
//!   relay's connections instead. That is a standard named-pipe hijack, trivial to
//!   prevent and easy to forget.
//!
//! Threading: one thread per connection, not async. There are at most a handful of
//! connections (one per browser profile) and every message ends in a blocking SQLite
//! write, so an async runtime would add machinery and `spawn_blocking` hops without
//! buying concurrency that matters here.

use anyhow::{anyhow, Context, Result};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};

#[cfg(windows)]
use windows::core::{HSTRING, PWSTR};
#[cfg(windows)]
use windows::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, LocalFree, HLOCAL};
#[cfg(windows)]
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
    SDDL_REVISION_1,
};
#[cfg(windows)]
use windows::Win32::Security::{GetTokenInformation, TokenUser, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, PSECURITY_DESCRIPTOR};
#[cfg(windows)]
use windows::Win32::Storage::FileSystem::{
    FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, PIPE_ACCESS_DUPLEX,
};
#[cfg(windows)]
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE,
    PIPE_WAIT,
};
#[cfg(windows)]
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
#[cfg(windows)]
use windows::Win32::System::IO::{GetOverlappedResult, OVERLAPPED};

const PIPE_BUF: u32 = 64 * 1024;

/// Max simultaneous pipe instances. One per browser profile, plus headroom.
const MAX_INSTANCES: u32 = 16;

/// Returns `\\.\pipe\SessionRestore.<sid-hash>` for the current user.
pub fn pipe_name() -> Result<String> {
    Ok(format!("\\\\.\\pipe\\SessionRestore.{}", current_user_sid_hash()?))
}

/// Short hash of the user's SID. The SID itself is not used in the name: it would
/// expose the account identifier to anything that can enumerate pipe names, and the
/// hash distinguishes users just as well.
pub fn current_user_sid_hash() -> Result<String> {
    let sid = current_user_sid()?;
    let digest = Sha256::digest(sid.as_bytes());
    Ok(digest.iter().take(8).map(|b| format!("{b:02x}")).collect())
}

#[cfg(windows)]
pub fn current_user_sid() -> Result<String> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)
            .context("OpenProcessToken")?;

        let mut needed = 0u32;
        // First call fails with ERROR_INSUFFICIENT_BUFFER; it is how we learn the size.
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut needed);
        let mut buf = vec![0u8; needed as usize];
        let r = GetTokenInformation(
            token,
            TokenUser,
            Some(buf.as_mut_ptr() as *mut _),
            needed,
            &mut needed,
        );
        let _ = CloseHandle(token);
        r.context("GetTokenInformation(TokenUser)")?;

        let token_user = &*(buf.as_ptr() as *const TOKEN_USER);
        let mut sid_str = PWSTR::null();
        ConvertSidToStringSidW(token_user.User.Sid, &mut sid_str).context("ConvertSidToStringSid")?;
        let s = sid_str.to_string()?;
        let _ = LocalFree(HLOCAL(sid_str.0 as *mut _));
        Ok(s)
    }
}

#[cfg(not(windows))]
pub fn current_user_sid() -> Result<String> {
    Ok("non-windows-test-sid".into())
}

/// Builds a security descriptor granting full access to this user and nobody else.
///
/// SDDL rather than a hand-built DACL: `D:P` marks the DACL protected so it does not
/// inherit permissive ACEs, and the single ACE grants `GA` (generic all) to the user's
/// SID. Anything not listed gets nothing. Hand-assembling ACLs for this is more code
/// and more ways to be subtly wrong.
#[cfg(windows)]
fn user_only_security_descriptor(sid: &str) -> Result<(PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES)> {
    let sddl = format!("D:P(A;;GA;;;{sid})");
    unsafe {
        let mut psd = PSECURITY_DESCRIPTOR::default();
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            &HSTRING::from(sddl.as_str()),
            SDDL_REVISION_1,
            &mut psd,
            None,
        )
        .context("ConvertStringSecurityDescriptorToSecurityDescriptor")?;

        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: psd.0,
            bInheritHandle: false.into(),
        };
        Ok((psd, sa))
    }
}

/// A connected pipe client. Implements Read/Write so the framing code in `sr_proto`
/// works against it unchanged.
#[cfg(windows)]
pub struct PipeConnection {
    handle: HANDLE,
    /// Only the half that created the server end may disconnect it. A duplicated
    /// handle closes itself but must not tear down the connection underneath the
    /// other half.
    owns_disconnect: bool,
}

#[cfg(windows)]
impl PipeConnection {
    fn new(handle: HANDLE, owns_disconnect: bool) -> Self {
        PipeConnection {
            handle,
            owns_disconnect,
        }
    }

    /// Duplicates the handle so one thread can read while another writes.
    ///
    /// Both halves are usable independently; only the original tears the connection
    /// down on drop.
    pub fn try_clone(&self) -> Result<PipeConnection> {
        use windows::Win32::Foundation::DUPLICATE_SAME_ACCESS;
        use windows::Win32::System::Threading::GetCurrentProcess;
        unsafe {
            let proc = GetCurrentProcess();
            let mut dup = HANDLE::default();
            windows::Win32::Foundation::DuplicateHandle(
                proc,
                self.handle,
                proc,
                &mut dup,
                0,
                false,
                DUPLICATE_SAME_ACCESS,
            )
            .context("DuplicateHandle on pipe")?;
            Ok(PipeConnection::new(dup, false))
        }
    }
}

/// SAFETY: a Windows `HANDLE` is a process-wide kernel object reference, not a
/// thread-affine resource, so moving one to another thread is sound. `PipeConnection`
/// is deliberately **not** `Sync`: two threads sharing one handle could interleave
/// partial reads or writes and corrupt the frame stream. The supported pattern is
/// `try_clone()`, which hands each thread its own duplicated handle.
#[cfg(windows)]
unsafe impl Send for PipeConnection {}

/// Runs one overlapped operation to completion.
///
/// **Why overlapped I/O is mandatory here, not a refinement.**
///
/// A handle opened for synchronous I/O serializes every operation on the underlying
/// *file object*. `DuplicateHandle` does not create a new file object - it adds a
/// reference to the same one. So a reader thread parked in a blocking `ReadFile` also
/// blocks a writer thread on a duplicated handle, and the two deadlock: the relay
/// waiting to write four bytes the agent is waiting to read.
///
/// With `FILE_FLAG_OVERLAPPED`, each call carries its own `OVERLAPPED` and event, so
/// reads and writes proceed independently. Each call still *waits* for its own
/// completion, which keeps the blocking `Read`/`Write` interface the framing code
/// expects.
#[cfg(windows)]
unsafe fn await_overlapped(
    handle: HANDLE,
    start: impl FnOnce(*mut OVERLAPPED) -> windows::core::Result<()>,
) -> std::io::Result<u32> {
    use windows::Win32::Foundation::{ERROR_BROKEN_PIPE, ERROR_IO_PENDING, ERROR_PIPE_NOT_CONNECTED};
    use windows::Win32::System::Threading::CreateEventW;

    let event = CreateEventW(None, true, false, None)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;

    let mut ov = OVERLAPPED {
        hEvent: event,
        ..Default::default()
    };

    let started = start(&mut ov as *mut OVERLAPPED);
    if let Err(e) = started {
        let code = e.code().0 as u32 & 0xFFFF;
        if code != ERROR_IO_PENDING.0 {
            let _ = CloseHandle(event);
            return Err(map_pipe_error(code, &e));
        }
    }

    let mut transferred = 0u32;
    let r = GetOverlappedResult(handle, &ov, &mut transferred, true);
    let _ = CloseHandle(event);

    match r {
        Ok(()) => Ok(transferred),
        Err(e) => {
            let code = e.code().0 as u32 & 0xFFFF;
            // A peer that closed cleanly surfaces as broken/not-connected; report it
            // as EOF so the framing layer treats it as a clean close rather than a
            // failure (docs/05-ipc-protocol.md).
            if code == ERROR_BROKEN_PIPE.0 || code == ERROR_PIPE_NOT_CONNECTED.0 {
                return Ok(0);
            }
            Err(map_pipe_error(code, &e))
        }
    }
}

#[cfg(windows)]
fn map_pipe_error(code: u32, e: &windows::core::Error) -> std::io::Error {
    use windows::Win32::Foundation::{ERROR_BROKEN_PIPE, ERROR_PIPE_NOT_CONNECTED};
    let kind = if code == ERROR_BROKEN_PIPE.0 || code == ERROR_PIPE_NOT_CONNECTED.0 {
        std::io::ErrorKind::BrokenPipe
    } else {
        std::io::ErrorKind::Other
    };
    std::io::Error::new(kind, e.to_string())
}

#[cfg(windows)]
impl Read for PipeConnection {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        use windows::Win32::Storage::FileSystem::ReadFile;
        let handle = self.handle;
        let n = unsafe {
            await_overlapped(handle, |ov| ReadFile(handle, Some(buf), None, Some(ov)))?
        };
        // Zero bytes means the peer closed.
        Ok(n as usize)
    }
}

#[cfg(windows)]
impl Write for PipeConnection {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        use windows::Win32::Storage::FileSystem::WriteFile;
        let handle = self.handle;
        let n = unsafe {
            await_overlapped(handle, |ov| WriteFile(handle, Some(buf), None, Some(ov)))?
        };
        if n == 0 && !buf.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "peer closed the pipe",
            ));
        }
        Ok(n as usize)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        // Deliberately a no-op.
        //
        // `WriteFile` on a named pipe hands the bytes to the kernel immediately - there
        // is no userspace buffer to flush. `FlushFileBuffers` looks like the right call
        // and is a trap: on the write end of a pipe it "does not return until the
        // reading process has read all the data", so it blocks on the peer's read
        // cadence. That turned every frame write into a rendezvous and deadlocked the
        // relay against the agent.
        Ok(())
    }
}

#[cfg(windows)]
impl Drop for PipeConnection {
    fn drop(&mut self) {
        unsafe {
            if self.owns_disconnect {
                let _ = DisconnectNamedPipe(self.handle);
            }
            let _ = CloseHandle(self.handle);
        }
    }
}

/// Blocks until a client connects, then hands back the connection.
///
/// `first` must be true only for the very first instance created at startup - that is
/// the one that claims the name and locks out squatters.
#[cfg(windows)]
pub fn accept_one(name: &str, first: bool) -> Result<PipeConnection> {
    let sid = current_user_sid()?;
    let (psd, sa) = user_only_security_descriptor(&sid)?;

    // FILE_FLAG_OVERLAPPED on the server end as well: the agent writes replies from
    // the same handler that may have a read pending, and synchronous handles would
    // serialize the two.
    let mut open_mode = PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED;
    if first {
        open_mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
    }

    unsafe {
        let handle = CreateNamedPipeW(
            &HSTRING::from(name),
            open_mode,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
            MAX_INSTANCES,
            PIPE_BUF,
            PIPE_BUF,
            0,
            Some(&sa),
        );
        let _ = LocalFree(HLOCAL(psd.0 as *mut _));

        if handle == INVALID_HANDLE_VALUE {
            return Err(anyhow!(
                "CreateNamedPipe({name}) failed: {}",
                std::io::Error::last_os_error()
            ));
        }

        // On an overlapped pipe, ConnectNamedPipe needs its own OVERLAPPED and returns
        // immediately with ERROR_IO_PENDING; the wait happens in GetOverlappedResult.
        //
        // ERROR_PIPE_CONNECTED (535) means a client raced in between CreateNamedPipe
        // and ConnectNamedPipe. That is a successful connection, not a failure, and
        // treating it as an error would drop every connection that arrives fast.
        const ERROR_PIPE_CONNECTED: u32 = 535;
        const ERROR_IO_PENDING: u32 = 997;

        use windows::Win32::System::Threading::CreateEventW;
        let event = match CreateEventW(None, true, false, None) {
            Ok(e) => e,
            Err(e) => {
                let _ = CloseHandle(handle);
                return Err(anyhow!("CreateEvent failed: {e}"));
            }
        };
        let mut ov = OVERLAPPED {
            hEvent: event,
            ..Default::default()
        };

        let mut connected = false;
        match ConnectNamedPipe(handle, Some(&mut ov)) {
            Ok(()) => connected = true,
            Err(e) => {
                let code = e.code().0 as u32 & 0xFFFF;
                if code == ERROR_PIPE_CONNECTED {
                    connected = true;
                } else if code != ERROR_IO_PENDING {
                    let _ = CloseHandle(event);
                    let _ = CloseHandle(handle);
                    return Err(anyhow!("ConnectNamedPipe failed: {e}"));
                }
            }
        }

        if !connected {
            let mut transferred = 0u32;
            if let Err(e) = GetOverlappedResult(handle, &ov, &mut transferred, true) {
                let _ = CloseHandle(event);
                let _ = CloseHandle(handle);
                return Err(anyhow!("waiting for a client failed: {e}"));
            }
        }
        let _ = CloseHandle(event);

        Ok(PipeConnection::new(handle, true))
    }
}

/// Connects to the agent's pipe as a client. Used by the relay.
#[cfg(windows)]
pub fn connect(name: &str) -> Result<PipeConnection> {
    use windows::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
    use windows::Win32::Storage::FileSystem::{CreateFileW, FILE_SHARE_MODE, OPEN_EXISTING};
    unsafe {
        // GENERIC_READ | GENERIC_WRITE, not FILE_GENERIC_*.
        //
        // This distinction is not cosmetic. FILE_GENERIC_WRITE contains
        // FILE_APPEND_DATA (0x0004), and on a named pipe that same bit means
        // FILE_CREATE_PIPE_INSTANCE. Asking for it yields a handle that opens
        // successfully and looks connected, but is not bound to the instance the
        // server is waiting on - so both sides sit forever, the client blocked in
        // WriteFile and the server in ReadFile, over four bytes.
        let handle = CreateFileW(
            &HSTRING::from(name),
            GENERIC_READ.0 | GENERIC_WRITE.0,
            FILE_SHARE_MODE(0),
            None,
            OPEN_EXISTING,
            windows::Win32::Storage::FileSystem::FILE_FLAG_OVERLAPPED,
            None,
        )
        .context("the agent does not appear to be running")?;

        if handle == INVALID_HANDLE_VALUE {
            return Err(anyhow!("could not open {name}"));
        }
        // Client side: closing the handle is correct, disconnecting is the server's job.
        Ok(PipeConnection::new(handle, false))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipe_name_is_scoped_to_the_user_and_hides_the_sid() {
        let name = pipe_name().unwrap();
        assert!(name.starts_with("\\\\.\\pipe\\SessionRestore."));
        let sid = current_user_sid().unwrap();
        assert!(
            !name.contains(&sid),
            "the raw SID must not appear in the pipe name"
        );
    }

    #[test]
    fn sid_hash_is_stable_across_calls() {
        assert_eq!(current_user_sid_hash().unwrap(), current_user_sid_hash().unwrap());
    }

    #[test]
    fn sid_hash_is_short_and_hex() {
        let h = current_user_sid_hash().unwrap();
        assert_eq!(h.len(), 16);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[cfg(windows)]
    #[test]
    fn current_user_sid_looks_like_a_sid() {
        let sid = current_user_sid().unwrap();
        assert!(sid.starts_with("S-1-"), "got {sid}");
    }

    #[cfg(windows)]
    #[test]
    fn security_descriptor_builds_for_this_user() {
        let sid = current_user_sid().unwrap();
        let r = user_only_security_descriptor(&sid);
        assert!(r.is_ok(), "SDDL rejected: {:?}", r.err());
        unsafe {
            let _ = LocalFree(HLOCAL(r.unwrap().0 .0 as *mut _));
        }
    }
}
