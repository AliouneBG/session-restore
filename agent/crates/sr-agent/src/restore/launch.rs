//! Launching applications, one mechanism per restore tier.
//!
//! **Nothing here ever elevates.** The agent runs at medium integrity and stays there.
//! An always-running process that launches other processes *as admin* based on rows in
//! a writable database would be a standing privilege-escalation primitive, which is
//! not a trade worth making for convenience (ADR-0001). Applications that were running
//! elevated are tier D and are reported as "start it yourself".

use anyhow::{Context, Result};

/// A launched process, if the mechanism gave us one.
///
/// `None` for shell and Store activations: those hand the request to another process,
/// and the window that eventually appears may belong to a pid we never saw.
#[derive(Debug, Clone, Copy)]
pub struct Launched {
    pub pid: Option<u32>,
}

/// Tier A: exact relaunch with the recorded command line.
#[cfg(windows)]
pub fn launch_with_command_line(command_line: &str, working_dir: Option<&str>) -> Result<Launched> {
    use windows::core::PWSTR;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        CreateProcessW, CREATE_UNICODE_ENVIRONMENT, PROCESS_INFORMATION, STARTUPINFOW,
    };

    let expanded = expand_env(command_line);
    let mut cmd: Vec<u16> = expanded.encode_utf16().chain(std::iter::once(0)).collect();
    // A null working directory means "inherit ours", which is the right default when
    // none was recorded.
    let dir: Vec<u16> = working_dir
        .map(|d| expand_env(d).encode_utf16().chain(std::iter::once(0)).collect())
        .unwrap_or_default();

    let si = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        ..Default::default()
    };
    let mut pi = PROCESS_INFORMATION::default();

    unsafe {
        CreateProcessW(
            None,
            // Must be mutable: CreateProcessW writes into this buffer.
            PWSTR(cmd.as_mut_ptr()),
            None,
            None,
            false,
            CREATE_UNICODE_ENVIRONMENT,
            None,
            if dir.is_empty() {
                windows::core::PCWSTR::null()
            } else {
                windows::core::PCWSTR(dir.as_ptr())
            },
            &si,
            &mut pi,
        )
        .with_context(|| format!("CreateProcess failed for {expanded}"))?;

        let pid = pi.dwProcessId;
        // We do not wait on the process; closing the handles just releases our
        // references, it does not affect the launched application.
        let _ = CloseHandle(pi.hThread);
        let _ = CloseHandle(pi.hProcess);
        Ok(Launched { pid: Some(pid) })
    }
}

/// Tier B (Win32) and tier C: hand the path to the shell.
///
/// For tier C the path is a *document*, and the shell picks the handler, which is
/// exactly what "reopen the thing they were working on" means.
#[cfg(windows)]
pub fn launch_via_shell(path: &str) -> Result<Launched> {
    use windows::core::HSTRING;
    use windows::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let expanded = expand_env(path);
    let file = HSTRING::from(expanded.as_str());
    // "open", never "runas": runas is the verb that prompts for elevation.
    let verb = HSTRING::from("open");

    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: windows::core::PCWSTR(verb.as_ptr()),
        lpFile: windows::core::PCWSTR(file.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };

    unsafe {
        ShellExecuteExW(&mut info).with_context(|| format!("ShellExecuteEx failed for {expanded}"))?;
        if !info.hProcess.is_invalid() {
            use windows::Win32::Foundation::CloseHandle;
            use windows::Win32::System::Threading::GetProcessId;
            let pid = GetProcessId(info.hProcess);
            let _ = CloseHandle(info.hProcess);
            return Ok(Launched {
                pid: if pid != 0 { Some(pid) } else { None },
            });
        }
    }
    Ok(Launched { pid: None })
}

/// Tier B (Store apps): activate by AUMID.
///
/// Packaged apps genuinely require this. They have no launchable exe path, and running
/// the binary under `WindowsApps` directly either fails outright or produces a broken
/// instance detached from its package identity.
#[cfg(windows)]
pub fn launch_uwp(aumid: &str) -> Result<Launched> {
    use windows::core::HSTRING;
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CLSCTX_LOCAL_SERVER, COINIT_APARTMENTTHREADED,
    };
    use windows::Win32::UI::Shell::{
        ApplicationActivationManager, IApplicationActivationManager, AO_NONE,
    };

    unsafe {
        // Ignore the result: the thread may already be initialized, which is fine.
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

        let manager: IApplicationActivationManager =
            CoCreateInstance(&ApplicationActivationManager, None, CLSCTX_LOCAL_SERVER)
                .context("CoCreateInstance(ApplicationActivationManager)")?;

        let pid = manager
            .ActivateApplication(&HSTRING::from(aumid), None, AO_NONE)
            .with_context(|| format!("ActivateApplication failed for {aumid}"))?;

        Ok(Launched {
            pid: if pid != 0 { Some(pid) } else { None },
        })
    }
}

/// Expands `%VAR%` tokens written by `identity::fold_env`.
///
/// The inverse of folding, and the reason folded paths are safe to store: they are
/// resolved against *this* machine at restore time.
pub fn expand_env(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;

    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('%') {
            Some(end) => {
                let name = &after[..end];
                match std::env::var(name) {
                    Ok(v) => out.push_str(&v),
                    // Unknown variable: leave it literal rather than silently producing
                    // a path with a hole in it.
                    Err(_) => {
                        out.push('%');
                        out.push_str(name);
                        out.push('%');
                    }
                }
                rest = &after[end + 1..];
            }
            None => {
                out.push('%');
                out.push_str(after);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(not(windows))]
pub fn launch_with_command_line(_c: &str, _d: Option<&str>) -> Result<Launched> {
    anyhow::bail!("not supported on this platform")
}
#[cfg(not(windows))]
pub fn launch_via_shell(_p: &str) -> Result<Launched> {
    anyhow::bail!("not supported on this platform")
}
#[cfg(not(windows))]
pub fn launch_uwp(_a: &str) -> Result<Launched> {
    anyhow::bail!("not supported on this platform")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_a_known_variable() {
        std::env::set_var("SR_TEST_ROOT", r"C:\Root");
        assert_eq!(expand_env(r"%SR_TEST_ROOT%\app.exe"), r"C:\Root\app.exe");
    }

    #[test]
    fn expansion_round_trips_folding() {
        // The property that makes storing folded paths safe.
        std::env::set_var("ProgramFiles", r"C:\Program Files");
        let original = r"C:\Program Files\App\app.exe";
        let folded = crate::watcher::identity::fold_env(original);
        assert_ne!(folded, original, "nothing was folded, test proves nothing");
        assert_eq!(expand_env(&folded), original);
    }

    #[test]
    fn an_unknown_variable_is_left_literal() {
        // Better a path that visibly fails than one silently missing a segment.
        assert_eq!(
            expand_env(r"%SR_DEFINITELY_NOT_SET%\app.exe"),
            r"%SR_DEFINITELY_NOT_SET%\app.exe"
        );
    }

    #[test]
    fn plain_paths_are_untouched() {
        assert_eq!(expand_env(r"D:\Games\thing.exe"), r"D:\Games\thing.exe");
    }

    #[test]
    fn an_unterminated_percent_does_not_eat_the_rest() {
        assert_eq!(expand_env(r"C:\100%\file.exe"), r"C:\100%\file.exe");
    }

    #[test]
    fn handles_several_variables() {
        std::env::set_var("SR_A", "one");
        std::env::set_var("SR_B", "two");
        assert_eq!(expand_env("%SR_A%-%SR_B%"), "one-two");
    }
}
