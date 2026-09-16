//! Top-level window enumeration and filtering.

use super::identity::{self, AppKind};
use super::processes::{self, ProcessInfo};
use anyhow::Result;

#[derive(Debug, Clone)]
pub struct CapturedWindow {
    pub hwnd: isize,
    pub pid: u32,
    /// `None` for browser processes - see `identity::storable_title`.
    pub title: Option<String>,
    pub show_cmd: &'static str,
    /// Restored ("normal") rectangle, independent of the current show state.
    pub norm: (i32, i32, i32, i32),
    pub z_order: i32,
    pub process: ProcessInfo,
    pub is_browser: bool,
}

/// Enumerates capturable top-level windows, nearest-to-front first.
#[cfg(windows)]
pub fn enumerate() -> Result<Vec<CapturedWindow>> {
    use windows::Win32::Foundation::{BOOL, HWND, LPARAM};
    use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetClassNameW, GetWindow, GetWindowLongPtrW, GetWindowPlacement,
        GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible,
        GWL_EXSTYLE, GW_OWNER, SW_SHOWMAXIMIZED, SW_SHOWMINIMIZED, WINDOWPLACEMENT,
        WS_EX_TOOLWINDOW,
    };

    struct Collector {
        out: Vec<(isize, u32, String, &'static str, (i32, i32, i32, i32))>,
        z: i32,
    }

    unsafe extern "system" fn cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let c = &mut *(lparam.0 as *mut Collector);
        c.z += 1;

        if !IsWindowVisible(hwnd).as_bool() {
            return true.into();
        }

        // Tool windows are palettes and tooltips, never something a user "had open".
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        if ex & WS_EX_TOOLWINDOW.0 != 0 {
            return true.into();
        }

        // Owned windows are dialogs belonging to a window we will already capture.
        if !GetWindow(hwnd, GW_OWNER).unwrap_or_default().0.is_null() {
            return true.into();
        }

        // The shell's own windows (desktop, taskbar, flyouts) live in explorer.exe
        // alongside File Explorer folder windows, so they are separated by class.
        let mut class_buf = [0u16; 256];
        let class_len = GetClassNameW(hwnd, &mut class_buf);
        if class_len > 0 {
            let class = String::from_utf16_lossy(&class_buf[..class_len as usize]);
            if identity::is_ignored_class(&class) {
                return true.into();
            }
        }

        let len = GetWindowTextLengthW(hwnd);
        if len <= 0 {
            return true.into();
        }

        // THE check that is easy to miss. Every suspended Store app, and every window
        // on another virtual desktop, stays IsWindowVisible == TRUE while being
        // invisible to the user. Without this the store fills with phantom Calculator
        // and Mail windows the user never opened, and restore cheerfully reopens them.
        let mut cloaked: u32 = 0;
        if DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloaked as *mut _ as *mut core::ffi::c_void,
            std::mem::size_of::<u32>() as u32,
        )
        .is_ok()
            && cloaked != 0
        {
            return true.into();
        }

        let mut buf = vec![0u16; (len + 1) as usize];
        let n = GetWindowTextW(hwnd, &mut buf);
        let title = String::from_utf16_lossy(&buf[..n as usize]);

        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 {
            return true.into();
        }

        // GetWindowPlacement, not GetWindowRect: it reports the *restored* rectangle
        // plus a show state, so a maximized window records both "maximized" and the
        // size it returns to. GetWindowRect would give the monitor bounds and lose the
        // original size for good.
        let mut wp = WINDOWPLACEMENT {
            length: std::mem::size_of::<WINDOWPLACEMENT>() as u32,
            ..Default::default()
        };
        let show = if GetWindowPlacement(hwnd, &mut wp).is_ok() {
            match wp.showCmd {
                x if x == SW_SHOWMAXIMIZED.0 as u32 => "maximized",
                x if x == SW_SHOWMINIMIZED.0 as u32 => "minimized",
                _ => "normal",
            }
        } else {
            "normal"
        };
        let r = wp.rcNormalPosition;

        c.out.push((
            hwnd.0 as isize,
            pid,
            title,
            show,
            (r.left, r.top, r.right - r.left, r.bottom - r.top),
        ));
        true.into()
    }

    let mut collector = Collector {
        out: Vec::new(),
        z: 0,
    };
    unsafe {
        EnumWindows(Some(cb), LPARAM(&mut collector as *mut _ as isize))?;
    }

    let mut out = Vec::new();
    for (i, (hwnd, pid, title, show, norm)) in collector.out.into_iter().enumerate() {
        // A Store app's visible window belongs to ApplicationFrameHost.exe, not to the
        // app: the host owns the frame and the real app renders into a child window in
        // its own process. Treating the host as the application means every Store app
        // is either dropped or recorded as "ApplicationFrameHost", so the real process
        // is resolved through the child window before anything else is decided.
        let pid = resolve_frame_host(hwnd, pid);

        let process = match processes::info_for_pid(pid) {
            Ok(p) => p,
            // A process we cannot open (protected, or exited between enumeration and
            // the query) is skipped rather than stored as a mystery row.
            Err(_) => continue,
        };

        let exe = process.exe_path.clone().unwrap_or_default();
        if !exe.is_empty() && identity::is_ignored_exe(&exe) {
            continue;
        }
        if process.kind == AppKind::Unknown && process.aumid.is_none() {
            continue;
        }

        let is_browser = identity::is_browser_exe(&exe);
        out.push(CapturedWindow {
            hwnd,
            pid,
            title: identity::storable_title(&exe, &title),
            show_cmd: show,
            norm,
            z_order: i as i32,
            process,
            is_browser,
        });
    }

    Ok(out)
}

/// Returns the PID that actually owns a window's content.
///
/// For an `ApplicationFrameHost.exe` frame, that is the child window's process - the
/// Store app itself. For anything else the input PID is already correct.
#[cfg(windows)]
fn resolve_frame_host(hwnd: isize, pid: u32) -> u32 {
    use windows::Win32::Foundation::{BOOL, HWND, LPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{EnumChildWindows, GetWindowThreadProcessId};

    let Ok(info) = processes::info_for_pid(pid) else {
        return pid;
    };
    let is_host = info
        .exe_path
        .as_deref()
        .map(|p| {
            std::path::Path::new(p)
                .file_name()
                .map(|n| n.to_string_lossy().eq_ignore_ascii_case("applicationframehost.exe"))
                .unwrap_or(false)
        })
        .unwrap_or(false);
    if !is_host {
        return pid;
    }

    struct Search {
        host_pid: u32,
        found: u32,
    }

    unsafe extern "system" fn child_cb(child: HWND, lparam: LPARAM) -> BOOL {
        let s = &mut *(lparam.0 as *mut Search);
        let mut cpid = 0u32;
        GetWindowThreadProcessId(child, Some(&mut cpid));
        if cpid != 0 && cpid != s.host_pid {
            s.found = cpid;
            return false.into(); // stop at the first child in another process
        }
        true.into()
    }

    let mut search = Search {
        host_pid: pid,
        found: 0,
    };
    unsafe {
        let _ = EnumChildWindows(
            HWND(hwnd as *mut core::ffi::c_void),
            Some(child_cb),
            LPARAM(&mut search as *mut _ as isize),
        );
    }

    if search.found != 0 {
        search.found
    } else {
        pid
    }
}

#[cfg(not(windows))]
fn resolve_frame_host(_hwnd: isize, pid: u32) -> u32 {
    pid
}

#[cfg(not(windows))]
pub fn enumerate() -> Result<Vec<CapturedWindow>> {
    Ok(Vec::new())
}
