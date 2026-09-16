//! Process identity: image path, packaged-app AUMID, elevation, and command line.

use super::identity::{self, AppKind};
use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct ProcessInfo {
    pub pid: u32,
    pub kind: AppKind,
    pub exe_path: Option<String>,
    pub aumid: Option<String>,
    /// Redacted, and only for applications whose arguments change the outcome.
    pub command_line: Option<String>,
    pub command_line_redacted: bool,
    pub elevated: bool,
}

#[cfg(windows)]
pub fn info_for_pid(pid: u32) -> Result<ProcessInfo> {
    use windows::core::PWSTR;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::Storage::Packaging::Appx::GetApplicationUserModelId;
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_FORMAT,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };

    unsafe {
        // PROCESS_QUERY_LIMITED_INFORMATION is deliberate: it is the least privilege
        // that returns the image name, and unlike PROCESS_QUERY_INFORMATION it
        // succeeds against protected and higher-integrity processes.
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)
            .with_context(|| format!("OpenProcess({pid})"))?;

        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let exe_path = if QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_FORMAT(0),
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
        .is_ok()
        {
            Some(String::from_utf16_lossy(&buf[..len as usize]))
        } else {
            None
        };

        // A packaged app has no launchable exe path, so its AUMID is its identity and
        // the only way to start it again.
        let mut aumid_len = 0u32;
        let _ = GetApplicationUserModelId(handle, &mut aumid_len, PWSTR::null());
        let aumid = if aumid_len > 0 {
            let mut abuf = vec![0u16; aumid_len as usize];
            if GetApplicationUserModelId(handle, &mut aumid_len, PWSTR(abuf.as_mut_ptr())).is_ok() {
                let s = String::from_utf16_lossy(&abuf[..aumid_len as usize]);
                Some(s.trim_end_matches('\0').to_string())
            } else {
                None
            }
        } else {
            None
        };

        let elevated = is_elevated(handle);
        let _ = CloseHandle(handle);

        let kind = if aumid.is_some() {
            AppKind::Uwp
        } else if exe_path.is_some() {
            AppKind::Win32
        } else {
            AppKind::Unknown
        };

        // Collect arguments only where they change what comes back. Collecting less is
        // stronger than redacting more (docs/06-privacy-security.md).
        let (command_line, command_line_redacted) = match &exe_path {
            Some(p) if identity::arguments_matter(p) => match read_command_line(pid) {
                Ok(raw) => {
                    let (red, was) = identity::redact_command_line(&raw);
                    (Some(red), was)
                }
                // Unreadable is recorded as absent, never guessed at; the app simply
                // drops to tier B.
                Err(_) => (None, false),
            },
            _ => (None, false),
        };

        Ok(ProcessInfo {
            pid,
            kind,
            exe_path,
            aumid,
            command_line,
            command_line_redacted,
            elevated,
        })
    }
}

#[cfg(windows)]
unsafe fn is_elevated(handle: windows::Win32::Foundation::HANDLE) -> bool {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
    use windows::Win32::System::Threading::OpenProcessToken;

    let mut token = windows::Win32::Foundation::HANDLE::default();
    if OpenProcessToken(handle, TOKEN_QUERY, &mut token).is_err() {
        // Cannot tell. Assume elevated, which makes the app tier D: refusing to
        // restore something we might not be allowed to is the safe direction.
        return true;
    }
    let mut elevation = TOKEN_ELEVATION::default();
    let mut size = std::mem::size_of::<TOKEN_ELEVATION>() as u32;
    let ok = GetTokenInformation(
        token,
        TokenElevation,
        Some(&mut elevation as *mut _ as *mut _),
        size,
        &mut size,
    )
    .is_ok();
    let _ = CloseHandle(token);
    if !ok {
        return true;
    }
    elevation.TokenIsElevated != 0
}

/// Reads another process's command line from its PEB.
///
/// Windows has no cheap supported way to do this. The documented alternative, WMI's
/// `Win32_Process.CommandLine`, costs 100ms+ per query and seconds to initialise,
/// which is far too slow for a capture pass over dozens of processes.
///
/// This is the most dangerous code in the project: `NtQueryInformationProcess` is
/// undocumented and the PEB layout can shift between Windows builds. It is therefore
/// confined to one function, allowed to fail, and its failure is not fatal - the app
/// just drops to tier B (docs/03-capture.md).
///
/// TODO(M4): subscribe to the `Microsoft-Windows-Kernel-Process` ETW provider and
/// cache command lines as processes start, using this only for processes that predate
/// the agent.
#[cfg(windows)]
fn read_command_line(pid: u32) -> Result<String> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ,
    };

    unsafe {
        let handle = OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ,
            false,
            pid,
        )
        .context("OpenProcess for VM read")?;

        let result = (|| -> Result<String> {
            let mut pbi = ProcessBasicInformation::default();
            let mut ret_len = 0u32;
            // windows-rs does not expose NtQueryInformationProcess (it is not part of
            // the documented Win32 surface), so it is declared by hand below.
            let status = NtQueryInformationProcess(
                handle.0,
                0, // ProcessBasicInformation
                &mut pbi as *mut _ as *mut core::ffi::c_void,
                std::mem::size_of::<ProcessBasicInformation>() as u32,
                &mut ret_len,
            );
            if status < 0 {
                anyhow::bail!("NtQueryInformationProcess failed: {status:#x}");
            }

            if pbi.peb_base_address == 0 {
                anyhow::bail!("no PEB (likely a 32/64-bit mismatch)");
            }

            // PEB -> ProcessParameters is at offset 0x20 on x64.
            let mut params_ptr: usize = 0;
            read_mem(
                handle,
                (pbi.peb_base_address + 0x20) as *const _,
                &mut params_ptr as *mut _ as *mut _,
                std::mem::size_of::<usize>(),
            )?;
            if params_ptr == 0 {
                anyhow::bail!("no process parameters");
            }

            // RTL_USER_PROCESS_PARAMETERS -> CommandLine (UNICODE_STRING) at 0x70 on x64.
            #[repr(C)]
            #[derive(Default, Clone, Copy)]
            struct UnicodeString {
                length: u16,
                maximum_length: u16,
                _pad: u32,
                buffer: usize,
            }

            let mut us = UnicodeString::default();
            read_mem(
                handle,
                (params_ptr + 0x70) as *const _,
                &mut us as *mut _ as *mut _,
                std::mem::size_of::<UnicodeString>(),
            )?;

            if us.length == 0 || us.buffer == 0 {
                anyhow::bail!("empty command line");
            }
            // Bound the allocation: a corrupt read must not turn into a huge alloc.
            if us.length as usize > 64 * 1024 {
                anyhow::bail!("implausible command line length");
            }

            let mut wide = vec![0u16; us.length as usize / 2];
            read_mem(
                handle,
                us.buffer as *const _,
                wide.as_mut_ptr() as *mut _,
                us.length as usize,
            )?;
            Ok(String::from_utf16_lossy(&wide))
        })();

        let _ = CloseHandle(handle);
        result
    }
}

/// The undocumented surface this module depends on, gathered in one place.
///
/// `NtQueryInformationProcess` is not part of the documented Win32 API and windows-rs
/// does not bind it. `PROCESS_BASIC_INFORMATION` is only reachable there behind an
/// extra feature, and only its `PebBaseAddress` field is used, so it is declared here
/// too - keeping every unsupported dependency visible together rather than spread
/// between a feature flag and an extern block.
///
/// Everything below can break on a Windows update. That is why it is isolated, failing
/// soft, and only ever costs an application its tier-A restore.
#[cfg(windows)]
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct ProcessBasicInformation {
    exit_status: i32,
    peb_base_address: usize,
    affinity_mask: usize,
    base_priority: i32,
    unique_process_id: usize,
    inherited_from_unique_process_id: usize,
}

#[cfg(windows)]
extern "system" {
    fn NtQueryInformationProcess(
        process_handle: *mut core::ffi::c_void,
        process_information_class: i32,
        process_information: *mut core::ffi::c_void,
        process_information_length: u32,
        return_length: *mut u32,
    ) -> i32;
}

#[cfg(windows)]
unsafe fn read_mem(
    handle: windows::Win32::Foundation::HANDLE,
    addr: *const core::ffi::c_void,
    out: *mut core::ffi::c_void,
    len: usize,
) -> Result<()> {
    use windows::Win32::System::Diagnostics::Debug::ReadProcessMemory;
    let mut read = 0usize;
    ReadProcessMemory(handle, addr, out, len, Some(&mut read)).context("ReadProcessMemory")?;
    if read != len {
        anyhow::bail!("short read: {read} of {len}");
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn info_for_pid(pid: u32) -> Result<ProcessInfo> {
    Ok(ProcessInfo {
        pid,
        kind: AppKind::Unknown,
        exe_path: None,
        aumid: None,
        command_line: None,
        command_line_redacted: false,
        elevated: false,
    })
}
