//! Window placement: mapping a stored rectangle onto the displays that exist now.
//!
//! The geometry is pure and tested directly. The monitor layout at restore time is
//! routinely *not* the one at capture time - a laptop undocked, a monitor moved to
//! another port, a different scaling factor - and that is exactly when placement has
//! to be smart rather than literal (docs/04-restore.md).

use crate::watcher::displays::Display;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// Minimum on-screen title bar. Below this a window is effectively unreachable: it
/// cannot be grabbed with the mouse, and a user would have to know keyboard window
/// commands to recover it.
pub const MIN_VISIBLE_W: i32 = 120;
pub const MIN_VISIBLE_H: i32 = 40;

#[derive(Debug, Clone)]
pub struct StoredPlacement {
    pub rect: Rect,
    pub display_key: Option<String>,
    /// DPI of the display this window was captured on.
    pub dpi: u32,
}

/// Computes where a stored window should go on the current displays.
///
/// When its display is gone the window is moved to the primary one, scaled by the
/// ratio of work areas so a window that filled two thirds of a 4K screen does not
/// arrive larger than a 1080p laptop panel.
pub fn map_to_current(stored: &StoredPlacement, current: &[Display]) -> Rect {
    let Some(target) = pick_target(stored, current) else {
        // Nothing to map onto. Returning the rect unchanged is better than inventing
        // coordinates; the caller simply applies it as-is.
        return stored.rect;
    };

    let same_display = stored
        .display_key
        .as_deref()
        .map(|k| k == target.key)
        .unwrap_or(false);

    let (tx, ty, tw, th) = target.work;

    let mut r = if same_display {
        stored.rect
    } else {
        // Offset within the old display is unknown once that display is gone, so the
        // window is placed proportionally within the new work area instead.
        let scale_w = tw as f64 / stored.rect.w.max(1) as f64;
        let scale_h = th as f64 / stored.rect.h.max(1) as f64;
        let scale = scale_w.min(scale_h).min(1.0);
        let w = ((stored.rect.w as f64) * scale) as i32;
        let h = ((stored.rect.h as f64) * scale) as i32;
        Rect {
            x: tx + (tw - w) / 2,
            y: ty + (th - h) / 2,
            w,
            h,
        }
    };

    // Rescale for a different DPI. Without this, everything comes back visibly wrong
    // on any mixed-DPI laptop-plus-monitor setup, which is most of them.
    if stored.dpi != 0 && target.dpi != 0 && stored.dpi != target.dpi {
        let ratio = target.dpi as f64 / stored.dpi as f64;
        r.w = ((r.w as f64) * ratio) as i32;
        r.h = ((r.h as f64) * ratio) as i32;
    }

    clamp_on_screen(r, current)
}

fn pick_target<'a>(stored: &StoredPlacement, current: &'a [Display]) -> Option<&'a Display> {
    if let Some(key) = stored.display_key.as_deref() {
        if let Some(d) = current.iter().find(|d| d.key == key) {
            return Some(d);
        }
    }
    current
        .iter()
        .find(|d| d.is_primary)
        .or_else(|| current.first())
}

/// Nudges a rectangle until enough of it is reachable.
///
/// This is what prevents the "restored 3000px to the left, cannot be clicked" failure
/// when the layout shrank between capture and restore.
pub fn clamp_on_screen(mut r: Rect, displays: &[Display]) -> Rect {
    if displays.is_empty() {
        return r;
    }

    // A window larger than every work area is shrunk to fit the largest.
    if let Some(biggest) = displays.iter().max_by_key(|d| d.work.2 as i64 * d.work.3 as i64) {
        let (_, _, bw, bh) = biggest.work;
        if r.w > bw {
            r.w = bw;
        }
        if r.h > bh {
            r.h = bh;
        }
    }

    if visible_area(r, displays) >= MIN_VISIBLE_W * MIN_VISIBLE_H {
        return r;
    }

    // Not reachable where it is: move it onto the nearest display's work area.
    let target = nearest_display(r, displays);
    let (tx, ty, tw, th) = target.work;
    r.x = r.x.clamp(tx, (tx + tw - MIN_VISIBLE_W).max(tx));
    r.y = r.y.clamp(ty, (ty + th - MIN_VISIBLE_H).max(ty));

    if visible_area(r, displays) < MIN_VISIBLE_W * MIN_VISIBLE_H {
        r.x = tx + ((tw - r.w).max(0)) / 2;
        r.y = ty + ((th - r.h).max(0)) / 2;
    }
    r
}

fn visible_area(r: Rect, displays: &[Display]) -> i32 {
    let mut best = 0;
    for d in displays {
        let (dx, dy, dw, dh) = d.bounds;
        let ox = (r.x + r.w).min(dx + dw) - r.x.max(dx);
        let oy = (r.y + r.h).min(dy + dh) - r.y.max(dy);
        if ox > 0 && oy > 0 {
            best = best.max(ox.min(r.w) * oy.min(r.h));
        }
    }
    best
}

fn nearest_display<'a>(r: Rect, displays: &'a [Display]) -> &'a Display {
    let cx = r.x + r.w / 2;
    let cy = r.y + r.h / 2;
    displays
        .iter()
        .min_by_key(|d| {
            let (dx, dy, dw, dh) = d.bounds;
            let ddx = (dx + dw / 2 - cx) as i64;
            let ddy = (dy + dh / 2 - cy) as i64;
            ddx * ddx + ddy * ddy
        })
        .unwrap_or(&displays[0])
}

/// Applies a placement to a live window.
#[cfg(windows)]
pub fn apply(hwnd: isize, rect: Rect, show_cmd: &str) -> anyhow::Result<()> {
    use windows::Win32::Foundation::{HWND, RECT};
    use windows::Win32::UI::WindowsAndMessaging::{
        SetWindowPlacement, SW_SHOWMAXIMIZED, SW_SHOWMINIMIZED, SW_SHOWNORMAL, WINDOWPLACEMENT,
    };

    let show = match show_cmd {
        "maximized" => SW_SHOWMAXIMIZED,
        "minimized" => SW_SHOWMINIMIZED,
        _ => SW_SHOWNORMAL,
    };

    // SetWindowPlacement with the same structure shape that was captured, so a
    // maximized window is restored as maximized *and* keeps the size it returns to.
    // SetWindowPos would force a literal rect and lose the show state.
    let wp = WINDOWPLACEMENT {
        length: std::mem::size_of::<WINDOWPLACEMENT>() as u32,
        flags: Default::default(),
        showCmd: show.0 as u32,
        ptMinPosition: Default::default(),
        ptMaxPosition: Default::default(),
        rcNormalPosition: RECT {
            left: rect.x,
            top: rect.y,
            right: rect.x + rect.w,
            bottom: rect.y + rect.h,
        },
    };

    unsafe {
        SetWindowPlacement(HWND(hwnd as *mut core::ffi::c_void), &wp)?;
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn apply(_hwnd: isize, _rect: Rect, _show_cmd: &str) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disp(key: &str, primary: bool, b: (i32, i32, i32, i32), dpi: u32) -> Display {
        Display {
            key: key.into(),
            friendly_name: None,
            is_primary: primary,
            bounds: b,
            work: b,
            dpi,
        }
    }

    fn stored(rect: Rect, key: &str, dpi: u32) -> StoredPlacement {
        StoredPlacement {
            rect,
            display_key: Some(key.into()),
            dpi,
        }
    }

    #[test]
    fn an_unchanged_layout_places_the_window_exactly() {
        let ds = vec![disp("m1", true, (0, 0, 1920, 1080), 96)];
        let s = stored(Rect { x: 100, y: 100, w: 800, h: 600 }, "m1", 96);
        assert_eq!(map_to_current(&s, &ds), Rect { x: 100, y: 100, w: 800, h: 600 });
    }

    #[test]
    fn a_window_on_a_second_monitor_stays_there() {
        let ds = vec![
            disp("m1", true, (0, 0, 1920, 1080), 96),
            disp("m2", false, (1920, 0, 1920, 1080), 96),
        ];
        let s = stored(Rect { x: 2000, y: 100, w: 800, h: 600 }, "m2", 96);
        assert_eq!(map_to_current(&s, &ds).x, 2000);
    }

    #[test]
    fn an_unplugged_monitor_sends_the_window_to_primary() {
        // The laptop-undocked case. Left where it was, the window would be invisible.
        let ds = vec![disp("m1", true, (0, 0, 1920, 1080), 96)];
        let s = stored(Rect { x: 2400, y: 200, w: 800, h: 600 }, "gone", 96);
        let r = map_to_current(&s, &ds);
        assert!(r.x >= 0 && r.x < 1920, "landed off-screen at x={}", r.x);
        assert!(visible_area(r, &ds) >= MIN_VISIBLE_W * MIN_VISIBLE_H);
    }

    #[test]
    fn a_window_from_a_bigger_screen_is_scaled_down_to_fit() {
        let ds = vec![disp("small", true, (0, 0, 1366, 768), 96)];
        let s = stored(Rect { x: 0, y: 0, w: 3000, h: 1800 }, "gone-4k", 96);
        let r = map_to_current(&s, &ds);
        assert!(r.w <= 1366 && r.h <= 768, "did not fit: {r:?}");
    }

    #[test]
    fn dpi_change_rescales_the_window() {
        // 96 -> 144 dpi is 100% -> 150%: the same window should come back larger.
        let ds = vec![disp("m1", true, (0, 0, 3840, 2160), 144)];
        let s = stored(Rect { x: 0, y: 0, w: 800, h: 600 }, "m1", 96);
        let r = map_to_current(&s, &ds);
        assert_eq!(r.w, 1200);
        assert_eq!(r.h, 900);
    }

    #[test]
    fn identical_dpi_does_not_resize() {
        let ds = vec![disp("m1", true, (0, 0, 1920, 1080), 120)];
        let s = stored(Rect { x: 10, y: 10, w: 800, h: 600 }, "m1", 120);
        let r = map_to_current(&s, &ds);
        assert_eq!((r.w, r.h), (800, 600));
    }

    #[test]
    fn a_window_far_off_screen_is_pulled_back() {
        let ds = vec![disp("m1", true, (0, 0, 1920, 1080), 96)];
        let r = clamp_on_screen(Rect { x: -3000, y: 400, w: 800, h: 600 }, &ds);
        assert!(visible_area(r, &ds) >= MIN_VISIBLE_W * MIN_VISIBLE_H, "{r:?}");
    }

    #[test]
    fn a_window_just_off_the_bottom_is_pulled_back() {
        let ds = vec![disp("m1", true, (0, 0, 1920, 1080), 96)];
        let r = clamp_on_screen(Rect { x: 200, y: 1075, w: 800, h: 600 }, &ds);
        assert!(visible_area(r, &ds) >= MIN_VISIBLE_W * MIN_VISIBLE_H, "{r:?}");
    }

    #[test]
    fn a_mostly_offscreen_window_that_is_still_grabbable_is_left_alone() {
        // Deliberately conservative: users do park windows half off the edge, and
        // moving one that is still usable would be the tool fighting the user.
        let ds = vec![disp("m1", true, (0, 0, 1920, 1080), 96)];
        let original = Rect { x: 1700, y: 100, w: 800, h: 600 };
        assert_eq!(clamp_on_screen(original, &ds), original);
    }

    #[test]
    fn a_window_bigger_than_every_screen_is_shrunk() {
        let ds = vec![disp("m1", true, (0, 0, 1366, 768), 96)];
        let r = clamp_on_screen(Rect { x: 0, y: 0, w: 5000, h: 4000 }, &ds);
        assert!(r.w <= 1366 && r.h <= 768);
    }

    #[test]
    fn no_displays_leaves_the_rect_untouched() {
        let s = stored(Rect { x: 5, y: 6, w: 7, h: 8 }, "m1", 96);
        assert_eq!(map_to_current(&s, &[]), Rect { x: 5, y: 6, w: 7, h: 8 });
    }

    #[test]
    fn a_window_with_no_recorded_display_still_lands_somewhere_usable() {
        let ds = vec![disp("m1", true, (0, 0, 1920, 1080), 96)];
        let s = StoredPlacement {
            rect: Rect { x: 100, y: 100, w: 800, h: 600 },
            display_key: None,
            dpi: 96,
        };
        let r = map_to_current(&s, &ds);
        assert!(visible_area(r, &ds) >= MIN_VISIBLE_W * MIN_VISIBLE_H);
    }
}
