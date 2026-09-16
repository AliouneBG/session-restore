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
    FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX,
};
#[cfg(windows)]
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE,
    PIPE_WAIT,
};
#[cfg(windows)]
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

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

#[cfg(windows)]
impl Read for PipeConnection {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        use windows::Win32::Storage::FileSystem::ReadFile;
        let mut read = 0u32;
        unsafe {
            ReadFile(self.handle, Some(buf), Some(&mut read), None)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
        }
        if read == 0 {
            // Zero bytes on a blocking pipe read means the peer closed.
            return Ok(0);
        }
        Ok(read as usize)
    }
}

#[cfg(windows)]
impl Write for PipeConnection {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        use windows::Win32::Storage::FileSystem::WriteFile;
        let mut written = 0u32;
        unsafe {
            WriteFile(self.handle, Some(buf), Some(&mut written), None)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
        }
        Ok(written as usize)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        use windows::Win32::Storage::FileSystem::FlushFileBuffers;
        unsafe {
            FlushFileBuffers(self.handle)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))
        }
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

    let mut open_mode = PIPE_ACCESS_DUPLEX;
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

        // ERROR_PIPE_CONNECTED (535) means a client raced in between CreateNamedPipe
        // and ConnectNamedPipe. That is a successful connection, not a failure, and
        // treating it as an error would drop every connection that arrives fast.
        const ERROR_PIPE_CONNECTED: u32 = 535;
        if let Err(e) = ConnectNamedPipe(handle, None) {
            if (e.code().0 as u32 & 0xFFFF) != ERROR_PIPE_CONNECTED {
                let _ = CloseHandle(handle);
                return Err(anyhow!("ConnectNamedPipe failed: {e}"));
            }
        }

        Ok(PipeConnection::new(handle, true))
    }
}

/// Connects to the agent's pipe as a client. Used by the relay.
#[cfg(windows)]
pub fn connect(name: &str) -> Result<PipeConnection> {
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_MODE, OPEN_EXISTING,
    };
    unsafe {
        let handle = CreateFileW(
            &HSTRING::from(name),
            (FILE_GENERIC_READ | FILE_GENERIC_WRITE).0,
            FILE_SHARE_MODE(0),
            None,
            OPEN_EXISTING,
            Default::default(),
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
