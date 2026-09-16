//! Monitor enumeration with stable identities.

use anyhow::Result;

#[derive(Debug, Clone)]
pub struct Display {
    /// Stable across reboots and port changes. **Never the monitor index.**
    pub key: String,
    pub friendly_name: Option<String>,
    pub is_primary: bool,
    pub bounds: (i32, i32, i32, i32),
    pub work: (i32, i32, i32, i32),
    pub dpi: u32,
}

/// Enumerates connected displays.
///
/// `key` is derived from the device name reported by the OS rather than the
/// enumeration order. Monitor index is not stable across reboots, docking, or a cable
/// moving ports, and keying on it is the classic cause of "all my windows piled onto
/// one screen" after a restore (docs/02-data-model.md).
#[cfg(windows)]
pub fn enumerate() -> Result<Vec<Display>> {
    use sha2::{Digest, Sha256};
    use windows::Win32::Foundation::{BOOL, LPARAM, RECT};
    use windows::Win32::Graphics::Gdi::{
        EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFOEXW,
    };

    /// windows-rs does not bind this flag; it is 0x1 in winuser.h.
    const MONITORINFOF_PRIMARY: u32 = 1;
    use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};

    unsafe extern "system" fn cb(
        monitor: HMONITOR,
        _hdc: HDC,
        _rect: *mut RECT,
        lparam: LPARAM,
    ) -> BOOL {
        let out = &mut *(lparam.0 as *mut Vec<Display>);

        let mut info = MONITORINFOEXW {
            monitorInfo: windows::Win32::Graphics::Gdi::MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFOEXW>() as u32,
                ..Default::default()
            },
            ..Default::default()
        };
        if GetMonitorInfoW(monitor, &mut info as *mut _ as *mut _) == false {
            return true.into();
        }

        let end = info
            .szDevice
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(info.szDevice.len());
        let device = String::from_utf16_lossy(&info.szDevice[..end]);

        let mut dpi_x = 96u32;
        let mut dpi_y = 96u32;
        let _ = GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y);

        let m = info.monitorInfo.rcMonitor;
        let w = info.monitorInfo.rcWork;

        // Device name plus physical size: stable across reboots, and distinguishes two
        // identical models better than the name alone.
        let seed = format!("{device}|{}x{}", m.right - m.left, m.bottom - m.top);
        let digest = Sha256::digest(seed.as_bytes());

        out.push(Display {
            key: digest.iter().take(8).map(|b| format!("{b:02x}")).collect(),
            friendly_name: Some(device),
            is_primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
            bounds: (m.left, m.top, m.right - m.left, m.bottom - m.top),
            work: (w.left, w.top, w.right - w.left, w.bottom - w.top),
            dpi: dpi_x,
        });
        true.into()
    }

    let mut out: Vec<Display> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(cb),
            LPARAM(&mut out as *mut _ as isize),
        );
    }
    Ok(out)
}

#[cfg(not(windows))]
pub fn enumerate() -> Result<Vec<Display>> {
    Ok(Vec::new())
}

/// Identifies which display a window sits on, by greatest overlap.
///
/// Overlap rather than the top-left corner: a window straddling two monitors belongs
/// to the one showing most of it, and a window dragged slightly off the left edge has
/// a corner on the neighbouring screen while plainly living on this one.
pub fn display_for_rect(displays: &[Display], rect: (i32, i32, i32, i32)) -> Option<String> {
    let (x, y, w, h) = rect;
    let mut best: Option<(i64, &Display)> = None;

    for d in displays {
        let (dx, dy, dw, dh) = d.bounds;
        let ox = (x + w).min(dx + dw) - x.max(dx);
        let oy = (y + h).min(dy + dh) - y.max(dy);
        if ox <= 0 || oy <= 0 {
            continue;
        }
        let area = ox as i64 * oy as i64;
        if best.map(|(a, _)| area > a).unwrap_or(true) {
            best = Some((area, d));
        }
    }

    best.map(|(_, d)| d.key.clone())
        // A window entirely off-screen still belongs somewhere; the primary display is
        // the honest fallback and keeps restore from dropping it.
        .or_else(|| displays.iter().find(|d| d.is_primary).map(|d| d.key.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(key: &str, primary: bool, bounds: (i32, i32, i32, i32)) -> Display {
        Display {
            key: key.into(),
            friendly_name: None,
            is_primary: primary,
            bounds,
            work: bounds,
            dpi: 96,
        }
    }

    #[test]
    fn picks_the_display_a_window_is_on() {
        let ds = vec![
            d("left", true, (0, 0, 1920, 1080)),
            d("right", false, (1920, 0, 1920, 1080)),
        ];
        assert_eq!(display_for_rect(&ds, (100, 100, 800, 600)).as_deref(), Some("left"));
        assert_eq!(display_for_rect(&ds, (2000, 100, 800, 600)).as_deref(), Some("right"));
    }

    #[test]
    fn a_straddling_window_belongs_to_the_greater_overlap() {
        let ds = vec![
            d("left", true, (0, 0, 1920, 1080)),
            d("right", false, (1920, 0, 1920, 1080)),
        ];
        // Mostly on the right-hand screen.
        assert_eq!(display_for_rect(&ds, (1800, 0, 800, 600)).as_deref(), Some("right"));
    }

    #[test]
    fn a_window_nudged_off_the_edge_stays_where_it_lives() {
        // Top-left-corner matching would put this on the left screen; overlap does not.
        let ds = vec![
            d("left", true, (0, 0, 1920, 1080)),
            d("right", false, (1920, 0, 1920, 1080)),
        ];
        assert_eq!(display_for_rect(&ds, (1900, 100, 1000, 600)).as_deref(), Some("right"));
    }

    #[test]
    fn an_offscreen_window_falls_back_to_primary() {
        let ds = vec![d("only", true, (0, 0, 1920, 1080))];
        assert_eq!(display_for_rect(&ds, (-5000, -5000, 100, 100)).as_deref(), Some("only"));
    }

    #[test]
    fn no_displays_means_no_answer() {
        assert_eq!(display_for_rect(&[], (0, 0, 100, 100)), None);
    }
}
