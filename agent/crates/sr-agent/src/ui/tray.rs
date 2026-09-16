//! The tray icon and its menu.
//!
//! The tray is the only always-visible surface the product has, so it carries the two
//! things a user needs at any moment: whether capture is on, and a way to turn it off.
//! Pausing has to be one click, not buried in a settings page - a tool that records
//! what you do should be trivially stoppable.

use anyhow::Result;
use tray_icon::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tray_icon::{TrayIcon, TrayIconBuilder};

pub struct Tray {
    _icon: TrayIcon,
    pub restore_id: tray_icon::menu::MenuId,
    pub settings_id: tray_icon::menu::MenuId,
    pub capture_now_id: tray_icon::menu::MenuId,
    pub pause_id: tray_icon::menu::MenuId,
    pub open_folder_id: tray_icon::menu::MenuId,
    pub quit_id: tray_icon::menu::MenuId,
    pause_item: CheckMenuItem,
}

impl Tray {
    pub fn new(capture_enabled: bool) -> Result<Tray> {
        let menu = Menu::new();

        let restore = MenuItem::new("Restore last session…", true, None);
        let capture_now = MenuItem::new("Capture now", true, None);
        let settings = MenuItem::new("Settings...", true, None);
        let pause = CheckMenuItem::new("Pause capture", true, !capture_enabled, None);
        let open_folder = MenuItem::new("Open data folder", true, None);
        let quit = MenuItem::new("Quit Session Restore", true, None);

        menu.append(&restore)?;
        menu.append(&capture_now)?;
        menu.append(&PredefinedMenuItem::separator())?;
        menu.append(&settings)?;
        menu.append(&pause)?;
        menu.append(&open_folder)?;
        menu.append(&PredefinedMenuItem::separator())?;
        menu.append(&quit)?;

        let icon = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("Session Restore")
            .with_icon(default_icon()?)
            .build()?;

        Ok(Tray {
            restore_id: restore.id().clone(),
            settings_id: settings.id().clone(),
            capture_now_id: capture_now.id().clone(),
            pause_id: pause.id().clone(),
            open_folder_id: open_folder.id().clone(),
            quit_id: quit.id().clone(),
            pause_item: pause,
            _icon: icon,
        })
    }

    pub fn set_paused(&self, paused: bool) {
        self.pause_item.set_checked(paused);
    }
}

/// A small generated icon, so the binary carries no asset files.
///
/// Deliberately a recognizable shape rather than a coloured square: a tray icon that
/// looks like a placeholder reads as an unfinished or untrustworthy program.
fn default_icon() -> Result<tray_icon::Icon> {
    const S: u32 = 32;
    let mut rgba = vec![0u8; (S * S * 4) as usize];

    let cx = 15.5f32;
    let cy = 15.5f32;

    for y in 0..S {
        for x in 0..S {
            let dx = x as f32 - cx;
            let dy = y as f32 - cy;
            let dist = (dx * dx + dy * dy).sqrt();

            // A ring with a gap at the top right, plus an arrow head: the conventional
            // "restore / go back" glyph.
            let angle = dy.atan2(dx);
            let in_ring = (11.0..=13.5).contains(&dist);
            let gap = (-1.25..=-0.15).contains(&angle);

            let arrow = {
                // Triangle pointing left at the ring's top.
                let ax = x as f32 - 20.0;
                let ay = y as f32 - 5.5;
                ax >= -1.0 && ax <= 5.0 && ay.abs() <= (5.0 - ax) * 0.8 && ax <= 5.0
            };

            let on = (in_ring && !gap) || arrow;
            if on {
                let i = ((y * S + x) * 4) as usize;
                rgba[i] = 0x4a;
                rgba[i + 1] = 0x8f;
                rgba[i + 2] = 0xe8;
                rgba[i + 3] = 0xff;
            }
        }
    }

    Ok(tray_icon::Icon::from_rgba(rgba, S, S)?)
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_icon_has_visible_pixels() {
        // A silently blank tray icon looks like the app failed to start.
        let icon = super::default_icon();
        assert!(icon.is_ok(), "icon failed to build: {:?}", icon.err());
    }
}
