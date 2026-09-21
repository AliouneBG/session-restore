//! Tray icon and the review window.
//!
//! Windows requires a message pump on the thread that owns windows, and tao's event
//! loop never returns, so **the UI owns the main thread** and the pipe server, sweeper
//! and capture pass run behind it. That is the opposite of how the agent started out
//! and it is the right way round: the server has no thread affinity and the UI does.
//!
//! One loop hosts both the tray and the review window, because two event loops on one
//! thread is not a thing Windows will do.

pub mod onboarding;
pub mod review;
pub mod settings;
pub mod tray;
pub mod undo_window;

use crate::store::db::Db;
use crate::store::keys::KeyManager;
use anyhow::Result;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub struct UiContext {
    pub db: Arc<Mutex<Db>>,
    pub keys: Arc<KeyManager>,
    /// Shared with the pipe server, so a decision made here governs what browsers are
    /// offered when they connect.
    pub shared: Arc<crate::server::Shared>,
    /// The snapshot offered for restore, if there is one.
    pub pending_snapshot: Option<i64>,
    /// Whether to open the review window as soon as the agent starts.
    pub review_at_start: bool,
    /// Whether this is a first run and the welcome flow should be shown.
    pub onboard_at_start: bool,
    pub data_dir: std::path::PathBuf,
}

#[derive(Debug)]
enum UserEvent {
    /// Another launch of the agent asked for the interface to be shown.
    ShowUi,
    /// A message from the review window.
    Ipc(String),
    /// A message from the settings window.
    SettingsIpc(String),
    /// A message from the first-run flow.
    OnboardingIpc(String),
    /// A message from the undo window.
    UndoIpc(String),
    /// A launch asked for the undo window specifically.
    ShowUndo,
    Menu(tray_icon::menu::MenuId),
}

/// Runs the tray and, when asked, the review window. Never returns.
#[cfg(windows)]
pub fn run_app(ctx: UiContext) -> Result<()> {
    use tao::event::{Event, StartCause};
    use tao::event_loop::{ControlFlow, EventLoopBuilder};

    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    // Tray menu clicks arrive on their own channel; forward them into the loop so
    // there is a single place where all input is handled.
    {
        let proxy = proxy.clone();
        tray_icon::menu::MenuEvent::set_event_handler(Some(move |e: tray_icon::menu::MenuEvent| {
            let _ = proxy.send_event(UserEvent::Menu(e.id));
        }));
    }

    // A second launch is how someone opens an app with no main window.
    {
        let proxy = proxy.clone();
        if let Err(e) = crate::single_instance::listen_for_show_ui(move || {
            let _ = proxy.send_event(UserEvent::ShowUi);
        }) {
            tracing::warn!(error = %e, "not listening for further launches");
        }
    }
    {
        let proxy = proxy.clone();
        if let Err(e) = crate::single_instance::listen_for_show_undo(move || {
            let _ = proxy.send_event(UserEvent::ShowUndo);
        }) {
            tracing::warn!(error = %e, "not listening for undo requests");
        }
    }

    let capture_enabled = {
        let db = ctx.db.lock().unwrap();
        db.setting_bool("capture_enabled", true)
    };
    let tray = tray::Tray::new(capture_enabled)?;

    // Window event hooks need a message pump, which is this thread. The capture they
    // trigger does not, so it runs on a worker - a capture pass walks every process on
    // the machine and must never block the tray.
    match crate::watcher::events::install() {
        Ok(rx) => {
            let db = Arc::clone(&ctx.db);
            std::thread::spawn(move || {
                // Coalesced, not one capture per event.
                //
                // A capture walks every process on the machine and takes a few hundred
                // milliseconds while holding the database lock. Window events arrive in
                // bursts: opening one application can produce a dozen, and a restore
                // that launches five produces a storm. One capture per event meant the
                // restore competed with a queue of captures for the same lock, and
                // anything else that wanted the database waited behind all of them.
                // That is what "Session Restore is not responding" was.
                //
                // Instead: wait for an event, let the burst finish, then capture once.
                // This is the same debounce the browser side already applies to tab
                // events (T0 in docs/01-architecture.md).
                const SETTLE: Duration = Duration::from_millis(400);
                const MIN_INTERVAL: Duration = Duration::from_secs(2);
                let mut last = std::time::Instant::now() - MIN_INTERVAL;

                while rx.recv().is_ok() {
                    // Let the rest of the burst land, then swallow it.
                    std::thread::sleep(SETTLE);
                    while rx.try_recv().is_ok() {}

                    // A floor on how often this can run at all, so a pathological
                    // source of events cannot turn into a busy loop.
                    let since = last.elapsed();
                    if since < MIN_INTERVAL {
                        std::thread::sleep(MIN_INTERVAL - since);
                        while rx.try_recv().is_ok() {}
                    }
                    last = std::time::Instant::now();

                    let db = db.lock().unwrap();
                    if !db.setting_bool("capture_enabled", true) {
                        continue;
                    }
                    match crate::watcher::capture_into_live(&db) {
                        Ok(s) => tracing::debug!(
                            apps = s.apps,
                            windows = s.windows,
                            "captured after window events"
                        ),
                        Err(e) => tracing::warn!(error = %e, "event-driven capture failed"),
                    }
                }
            });
            tracing::info!("window event hooks installed");
        }
        // Not fatal: the 60s reconcile is the floor on correctness and still runs.
        Err(e) => tracing::warn!(error = %e, "could not install window event hooks"),
    }

    // Held across iterations. Dropping the pair closes the window.
    let mut review_window: Option<(tao::window::Window, wry::WebView)> = None;
    let mut review_snapshot: Option<i64> = None;
    let mut settings_window: Option<(tao::window::Window, wry::WebView)> = None;
    let mut onboarding_window: Option<(tao::window::Window, wry::WebView)> = None;
    let mut undo_window: Option<(tao::window::Window, wry::WebView)> = None;
    let mut open_at_start = ctx.review_at_start;
    let mut onboard_at_start = ctx.onboard_at_start;

    let db = ctx.db;
    let keys = ctx.keys;
    let pending = ctx.pending_snapshot;
    let shared = ctx.shared;
    let data_dir = ctx.data_dir;

    event_loop.run(move |event, target, control_flow| {
        *control_flow = ControlFlow::Wait;

        match event {
            Event::NewEvents(StartCause::Init) => {
                if onboard_at_start {
                    onboard_at_start = false;
                    // Ahead of the review window on purpose. A first run usually has
                    // nothing worth restoring, and two windows at once is not a welcome.
                    open_at_start = false;
                    match open_window(
                        target,
                        &proxy,
                        "Welcome to Session Restore",
                        onboarding::ONBOARDING_HTML,
                        (620.0, 560.0),
                        UserEvent::OnboardingIpc,
                    ) {
                        Ok(w) => onboarding_window = Some(w),
                        Err(e) => tracing::error!(error = %e, "could not open the welcome window"),
                    }
                }
                if open_at_start {
                    open_at_start = false;
                    if let Some(id) = pending {
                        match open_review(target, &proxy, &db, id) {
                            Ok(w) => {
                                review_window = Some(w);
                                review_snapshot = Some(id);
                            }
                            Err(e) => tracing::error!(error = %e, "could not open the review window"),
                        }
                    }
                }
            }

            Event::UserEvent(UserEvent::Menu(id)) => {
                if id == tray.restore_id {
                    if review_window.is_some() {
                        return;
                    }
                    let snapshot = pending.or_else(|| {
                        let db = db.lock().unwrap();
                        crate::store::snapshot::newest_restorable(&db).ok().flatten()
                    });
                    match snapshot {
                        Some(id) => match open_review(target, &proxy, &db, id) {
                            Ok(w) => {
                                review_window = Some(w);
                                review_snapshot = Some(id);
                            }
                            Err(e) => tracing::error!(error = %e, "could not open the review window"),
                        },
                        None => tracing::info!("nothing to restore"),
                    }
                } else if id == tray.undo_id {
                    // A window rather than an immediate action. Undo has two halves
                    // with different risks: moving windows back changes nothing you
                    // cannot see, while closing tabs destroys something. The second
                    // one has to be a choice, per tab.
                    if let Some((window, _)) = undo_window.as_ref() {
                        window.set_focus();
                        return;
                    }
                    match open_window(
                        target,
                        &proxy,
                        "Undo the last restore",
                        undo_window::UNDO_HTML,
                        (560.0, 560.0),
                        UserEvent::UndoIpc,
                    ) {
                        Ok(w) => undo_window = Some(w),
                        Err(e) => tracing::error!(error = %e, "could not open the undo window"),
                    }
                } else if id == tray.settings_id {
                    if settings_window.is_some() {
                        return;
                    }
                    match open_window(
                        target,
                        &proxy,
                        "Session Restore settings",
                        settings::SETTINGS_HTML,
                        (560.0, 640.0),
                        UserEvent::SettingsIpc,
                    ) {
                        Ok(w) => settings_window = Some(w),
                        Err(e) => tracing::error!(error = %e, "could not open settings"),
                    }
                } else if id == tray.capture_now_id {
                    // Off the message pump. A capture walks every process on the
                    // machine, and doing that here froze the tray for the duration.
                    let db = Arc::clone(&db);
                    std::thread::spawn(move || {
                        let db = db.lock().unwrap();
                        match crate::watcher::capture_into_live(&db) {
                            Ok(s) => tracing::info!(apps = s.apps, windows = s.windows, "captured"),
                            Err(e) => tracing::warn!(error = %e, "capture failed"),
                        }
                    });
                } else if id == tray.pause_id {
                    let db = db.lock().unwrap();
                    let now_enabled = db.setting_bool("capture_enabled", true);
                    let _ = db.set_setting("capture_enabled", if now_enabled { "false" } else { "true" });
                    tray.set_paused(now_enabled);
                    tracing::info!(paused = now_enabled, "capture toggled");
                } else if id == tray.open_folder_id {
                    if let Ok(dir) = crate::data_dir() {
                        let _ = std::process::Command::new("explorer.exe").arg(dir).spawn();
                    }
                } else if id == tray.quit_id {
                    *control_flow = ControlFlow::Exit;
                }
            }

            Event::UserEvent(UserEvent::ShowUi) => {
                // Settings is the window that answers "I clicked the app": it says what
                // is being watched and what the settings are. Focus an open one rather
                // than opening a second.
                if let Some((window, _)) = settings_window.as_ref() {
                    window.set_focus();
                    return;
                }
                match open_window(
                    target,
                    &proxy,
                    "Session Restore settings",
                    settings::SETTINGS_HTML,
                    (560.0, 640.0),
                    UserEvent::SettingsIpc,
                ) {
                    Ok(w) => settings_window = Some(w),
                    Err(e) => tracing::error!(error = %e, "could not open settings"),
                }
            }

            Event::UserEvent(UserEvent::ShowUndo) => {
                if let Some((window, _)) = undo_window.as_ref() {
                    window.set_focus();
                    return;
                }
                match open_window(
                    target,
                    &proxy,
                    "Undo the last restore",
                    undo_window::UNDO_HTML,
                    (560.0, 560.0),
                    UserEvent::UndoIpc,
                ) {
                    Ok(w) => undo_window = Some(w),
                    Err(e) => tracing::error!(error = %e, "could not open the undo window"),
                }
            }

            Event::UserEvent(UserEvent::UndoIpc(body)) => {
                let Some((_, webview)) = undo_window.as_ref() else {
                    return;
                };
                let Ok(action) = serde_json::from_str::<undo_window::UndoAction>(&body) else {
                    return;
                };

                match action {
                    undo_window::UndoAction::Close => {
                        undo_window = None;
                        return;
                    }
                    undo_window::UndoAction::UndoWindows => {
                        // Off the message pump: this re-places windows and can relaunch
                        // applications, which is the same work a restore does.
                        let db = Arc::clone(&db);
                        let proxy = proxy.clone();
                        std::thread::spawn(move || {
                            let outcome = {
                                let db = db.lock().unwrap();
                                crate::restore::undo::undo_last(&db)
                            };
                            let message = match outcome {
                                Ok(o) => o.message(),
                                Err(e) => format!("Could not undo: {e:#}"),
                            };
                            let _ = proxy.send_event(UserEvent::UndoIpc(
                                serde_json::json!({ "action": "ready", "note": message })
                                    .to_string(),
                            ));
                        });
                    }
                    undo_window::UndoAction::CloseTabs { urls } => {
                        let (run_id, browser) = {
                            let db = db.lock().unwrap();
                            let payload = undo_window::collect(&db).ok();
                            (
                                payload.as_ref().and_then(|p| p.run_id).unwrap_or(0),
                                payload
                                    .as_ref()
                                    .and_then(|p| undo_window::browser_for(&p.tabs))
                                    .unwrap_or_else(|| "chrome".into()),
                            )
                        };
                        let sent = crate::server::close_tabs(&shared, &browser, run_id, &urls);
                        if sent == 0 {
                            let _ = webview.evaluate_script(
                                "window.srUndoDone('That browser is not connected right now.')",
                            );
                        } else {
                            let note = format!(
                                "Asked {browser} to close {} tab(s).",
                                urls.len()
                            );
                            let literal = serde_json::to_string(&note)
                                .unwrap_or_else(|_| "\"\"".to_string());
                            let _ = webview.evaluate_script(&format!(
                                "window.srUndoDone({literal})"
                            ));
                        }
                    }
                    undo_window::UndoAction::Ready => {}
                }

                // Always re-render from fresh state, so the list reflects what is
                // actually left rather than what was there when the window opened.
                let payload = {
                    let db = db.lock().unwrap();
                    undo_window::collect(&db)
                };
                if let Ok(p) = payload {
                    if let Ok(json) = serde_json::to_string(&p) {
                        let _ = webview.evaluate_script(&format!("window.srUndo({json})"));
                    }
                }
                if let Some(note) = serde_json::from_str::<serde_json::Value>(&body)
                    .ok()
                    .and_then(|v| v.get("note").and_then(|n| n.as_str()).map(str::to_string))
                {
                    // JSON-encoded rather than hand-escaped: this is
                    // interpolated into JavaScript, and a quote or a backslash
                    // in the message would otherwise end the string early.
                    let literal = serde_json::to_string(&note)
                        .unwrap_or_else(|_| "\"\"".to_string());
                    let _ = webview
                        .evaluate_script(&format!("window.srUndoDone({literal})"));
                }
            }

            Event::UserEvent(UserEvent::SettingsIpc(body)) => {
                let Some((_, webview)) = settings_window.as_ref() else {
                    return;
                };
                let Ok(action) = serde_json::from_str::<settings::SettingsAction>(&body) else {
                    return;
                };

                if matches!(action, settings::SettingsAction::Close) {
                    settings_window = None;
                    return;
                }

                let refresh = {
                    let db = db.lock().unwrap();
                    match settings::apply(&db, &action) {
                        Ok(r) => r,
                        Err(e) => {
                            tracing::warn!(error = %e, "settings change refused");
                            true
                        }
                    }
                };

                // The extensions are told immediately. `capture_private` is what makes
                // them drop private events at the source, so a browser that has not
                // heard about the change is still enforcing the old answer.
                if !matches!(action, settings::SettingsAction::Ready) {
                    crate::server::push_settings(&shared);
                }

                // Re-render on anything that changes what the other controls should
                // say, and on the initial `ready`. A stale toggle is worse than a slow
                // one, because it claims something that is not true.
                if refresh || matches!(action, settings::SettingsAction::Ready) {
                    let connected = crate::server::connected_browsers(&shared);
                    let payload = {
                        let db = db.lock().unwrap();
                        settings::collect(&db, &connected, &data_dir)
                    };
                    if let Ok(p) = payload {
                        if let Ok(json) = serde_json::to_string(&p) {
                            let _ = webview.evaluate_script(&format!("window.srSettings({json})"));
                        }
                    }
                }
            }

            Event::UserEvent(UserEvent::OnboardingIpc(body)) => {
                let Some((_, webview)) = onboarding_window.as_ref() else {
                    return;
                };
                let Ok(action) = serde_json::from_str::<onboarding::OnboardingAction>(&body) else {
                    return;
                };

                {
                    let db = db.lock().unwrap();
                    if let Err(e) = onboarding::apply(&db, &action) {
                        tracing::warn!(error = %e, "onboarding action failed");
                    }
                }
                if matches!(action, onboarding::OnboardingAction::SetCapturePrivate { .. }) {
                    crate::server::push_settings(&shared);
                }

                if matches!(action, onboarding::OnboardingAction::Finish) {
                    onboarding_window = None;
                    tracing::info!("first run completed");
                    return;
                }

                // Every message re-sends the state, which is what makes the browser
                // step tick itself off: the page polls, and a browser that connected a
                // second ago shows up here.
                let connected = crate::server::connected_browsers(&shared);
                let payload = {
                    let db = db.lock().unwrap();
                    onboarding::collect(&db, &connected)
                };
                if let Ok(p) = payload {
                    if let Ok(json) = serde_json::to_string(&p) {
                        let _ = webview.evaluate_script(&format!("window.srOnboarding({json})"));
                    }
                }
            }

            Event::UserEvent(UserEvent::Ipc(body)) => {
                let Some((_, webview)) = review_window.as_ref() else {
                    return;
                };
                let Some(snapshot_id) = review_snapshot else {
                    return;
                };
                let Ok(msg) = serde_json::from_str::<serde_json::Value>(&body) else {
                    return;
                };

                match msg.get("action").and_then(|v| v.as_str()) {
                    Some("ready") => {
                        let payload = {
                            let db = db.lock().unwrap();
                            review::collect(&db, snapshot_id)
                        };
                        if let Ok(data) = payload {
                            if let Ok(json) = serde_json::to_string(&data) {
                                let _ = webview.evaluate_script(&format!("window.srData({json})"));
                            }
                        }
                    }

                    // The only path that decrypts anything, and it exists solely
                    // because the user pressed Show (ADR-0004).
                    Some("reveal_private") => {
                        let revealed = {
                            let db = db.lock().unwrap();
                            review::reveal_private(&db, &keys, snapshot_id)
                        };
                        let json = revealed
                            .ok()
                            .and_then(|w| serde_json::to_string(&w).ok())
                            .unwrap_or_else(|| "[]".into());
                        let _ =
                            webview.evaluate_script(&format!("window.srPrivateRevealed({json})"));
                    }

                    Some("choose") => {
                        if let Ok(choice) = serde_json::from_value::<review::ReviewChoice>(msg) {
                            // Close before doing the work: leaving a dead window on
                            // screen while applications launch behind it looks broken.
                            review_window = None;
                            review_snapshot = None;
                            apply_choice(&db, &keys, &shared, snapshot_id, &choice);
                        }
                    }

                    _ => {}
                }
            }

            Event::WindowEvent {
                event: tao::event::WindowEvent::CloseRequested,
                window_id,
                ..
            } => {
                // Which window. Before there was only one and this arm could assume it;
                // with three, assuming would mean closing settings silently declined a
                // restore the user had not been asked about yet.
                let is = |w: &Option<(tao::window::Window, wry::WebView)>| {
                    w.as_ref().map(|(win, _)| win.id() == window_id).unwrap_or(false)
                };

                if is(&undo_window) {
                    undo_window = None;
                } else if is(&settings_window) {
                    settings_window = None;
                } else if is(&onboarding_window) {
                    // Closing the welcome flow counts as finishing it. Showing it again
                    // at the next sign-in would be the most irritating thing this
                    // application could do.
                    {
                        let db = db.lock().unwrap();
                        let _ = onboarding::mark_done(&db);
                    }
                    onboarding_window = None;
                } else if is(&review_window) {
                    // Dismissal decides nothing. Treating a closed window as consent
                    // would restore a session the user never agreed to.
                    //
                    // It does have to release browsers waiting on an answer, though.
                    // They were deliberately not offered anything while the window was
                    // open, and without this their offer would never arrive at all.
                    {
                        let mut pending = shared.pending_restore.lock().unwrap();
                        pending.review_dismissed();
                    }
                    crate::server::offer_to_connected(&shared);

                    review_window = None;
                    review_snapshot = None;
                }
            }

            _ => {}
        }
    });
}

#[cfg(windows)]
fn open_review(
    target: &tao::event_loop::EventLoopWindowTarget<UserEvent>,
    proxy: &tao::event_loop::EventLoopProxy<UserEvent>,
    _db: &Arc<Mutex<Db>>,
    _snapshot_id: i64,
) -> Result<(tao::window::Window, wry::WebView)> {
    use tao::window::WindowBuilder;
    use wry::WebViewBuilder;

    let window = WindowBuilder::new()
        .with_title("Session Restore")
        .with_inner_size(tao::dpi::LogicalSize::new(640.0, 580.0))
        .with_min_inner_size(tao::dpi::LogicalSize::new(440.0, 340.0))
        .with_always_on_top(true)
        .build(target)?;

    // The page follows the system theme through `prefers-color-scheme`, but the title
    // bar is drawn by Windows and does not. Without this a dark review window wears a
    // white caption bar, which looks like a rendering bug rather than a theme.
    apply_titlebar_theme(&window);

    let proxy = proxy.clone();
    let webview = WebViewBuilder::new()
        .with_html(review::REVIEW_HTML)
        .with_ipc_handler(move |req| {
            let _ = proxy.send_event(UserEvent::Ipc(req.body().to_string()));
        })
        .build(&window)?;

    Ok((window, webview))
}

/// Opens a WebView2 window on a page, routing its messages through `wrap`.
///
/// The review window predates this and keeps its own opener because it is the one
/// window that is always on top: it appears unprompted at sign-in and has to be seen.
/// Settings and the welcome flow are opened by the user, so stealing the foreground
/// from whatever they were doing would be rude.
#[cfg(windows)]
fn open_window(
    target: &tao::event_loop::EventLoopWindowTarget<UserEvent>,
    proxy: &tao::event_loop::EventLoopProxy<UserEvent>,
    title: &str,
    html: &str,
    size: (f64, f64),
    wrap: fn(String) -> UserEvent,
) -> Result<(tao::window::Window, wry::WebView)> {
    use tao::window::WindowBuilder;
    use wry::WebViewBuilder;

    let window = WindowBuilder::new()
        .with_title(title)
        .with_inner_size(tao::dpi::LogicalSize::new(size.0, size.1))
        .with_min_inner_size(tao::dpi::LogicalSize::new(460.0, 400.0))
        .build(target)?;

    apply_titlebar_theme(&window);

    let proxy = proxy.clone();
    let webview = WebViewBuilder::new()
        .with_html(html)
        .with_ipc_handler(move |req| {
            let _ = proxy.send_event(wrap(req.body().to_string()));
        })
        .build(&window)?;

    Ok((window, webview))
}

/// Matches the window caption to the system's app theme.
///
/// Best effort on purpose: `DWMWA_USE_IMMERSIVE_DARK_MODE` is unsupported before
/// Windows 10 1809 and had a different attribute number before 20H1, so a failure here
/// means an older Windows, not a bug. A light caption is a cosmetic flaw; refusing to
/// open the window over one would not be.
#[cfg(windows)]
fn apply_titlebar_theme(window: &tao::window::Window) {
    use tao::platform::windows::WindowExtWindows;
    use windows::Win32::Foundation::{BOOL, HWND};
    use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_USE_IMMERSIVE_DARK_MODE};

    if !system_prefers_dark() {
        return;
    }
    let hwnd = HWND(window.hwnd() as _);
    let dark = BOOL(1);
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            &dark as *const BOOL as *const std::ffi::c_void,
            std::mem::size_of::<BOOL>() as u32,
        );
    }
}

/// Reads the same setting Windows uses for its own app chrome.
///
/// `AppsUseLightTheme` rather than `SystemUsesLightTheme`: the first is the one that
/// governs application windows, and the two differ on a very common configuration
/// (dark apps, light taskbar).
#[cfg(windows)]
fn system_prefers_dark() -> bool {
    use windows::core::w;
    use windows::Win32::System::Registry::{
        RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD,
    };

    let mut value: u32 = 1;
    let mut size = std::mem::size_of::<u32>() as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize"),
            w!("AppsUseLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut value as *mut u32 as *mut std::ffi::c_void),
            Some(&mut size),
        )
    };
    // Absent means light, which is the Windows default.
    status.is_ok() && value == 0
}

/// Carries out what the user selected.
#[cfg(windows)]
fn apply_choice(
    db: &Arc<Mutex<Db>>,
    _keys: &Arc<KeyManager>,
    shared: &Arc<crate::server::Shared>,
    snapshot_id: i64,
    choice: &review::ReviewChoice,
) {
    // Record the browser half first, so a browser that connects while applications are
    // still launching already knows what the user chose.
    {
        let mut pending = shared.pending_restore.lock().unwrap();
        pending.set_selection(crate::server::BrowserSelection {
            windows: choice.browser_windows.iter().cloned().collect(),
            tabs: choice.tabs.iter().cloned().collect(),
            declined: !choice.confirmed,
            // The one place this is read. Before it was carried here, the checkbox in
            // the review window was collected, stored, and never acted on: private
            // windows were captured, encrypted, revealed on request, and never
            // actually restored.
            restore_private: choice.restore_private,
        });
    }

    if choice.never_ask_again {
        let db = db.lock().unwrap();
        let _ = db.set_setting("restore_mode", "off");
        tracing::info!("restore disabled at the user's request");
        return;
    }
    if !choice.confirmed {
        tracing::info!("restore declined");
        return;
    }

    // Browsers already running connected before the user answered and were deferred.
    // They are waiting on exactly the decision that was just made. Deliberately after
    // the two refusals above, so neither can send an offer on its way out.
    crate::server::offer_to_connected(shared);

    // Everything below runs off the message pump.
    //
    // It captures the desktop, writes a snapshot, plans the restore and launches
    // applications, and the capture alone walks every process on the machine while
    // holding the database lock. Doing that here froze the window at the exact moment
    // the user pressed Restore, which is where "Session Restore is not responding"
    // came from. The selection above is recorded synchronously because a browser can
    // connect at any moment and must not race it; the slow part does not need to be.
    let db = Arc::clone(db);
    let shared = Arc::clone(shared);
    let apps_wanted: std::collections::HashSet<String> = choice.apps.iter().cloned().collect();
    let windows_wanted: std::collections::HashSet<String> =
        choice.browser_windows.iter().cloned().collect();

    std::thread::spawn(move || {
        let _ = &shared;
        let (apps, displays, run_id, browsers) = {
            let db = db.lock().unwrap();

            let all = match crate::restore::apps::plan_from_snapshot(&db, snapshot_id) {
                Ok(a) => a,
                Err(e) => {
                    tracing::error!(error = %e, "could not plan the restore");
                    return;
                }
            };
            let picked: Vec<_> = all
                .into_iter()
                .filter(|a| apps_wanted.contains(a.app_key.as_str()))
                .collect();

            // The undo point is what is on screen *now*, and the periodic sweep can be
            // a minute stale, which at startup is exactly the minute in which the
            // review window is answered.
            if let Err(e) = crate::watcher::capture_into_live(&db) {
                tracing::warn!(error = %e, "capture before restore failed; undo point may be stale");
            }
            let run_id = match crate::restore::begin_run(&db, snapshot_id, "ask") {
                Ok(id) => id,
                Err(e) => {
                    // Without a run there is no undo, and restoring anyway would hand
                    // the user an irreversible change they had no way to know about.
                    tracing::error!(error = %e, "could not open a restore run; not restoring");
                    return;
                }
            };

            // A browser that is not running will never connect, and the offer recorded
            // above waits for a connection. After a reboot that is every browser.
            let browsers = crate::restore::browsers_to_launch(&db, snapshot_id, &windows_wanted)
                .unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "could not work out which browsers to start");
                    Vec::new()
                });

            let displays = crate::watcher::displays::enumerate().unwrap_or_default();
            (picked, displays, run_id, browsers)
        };

        for b in &browsers {
            match crate::restore::launch::launch_with_args(&b.exe_path, &b.args) {
                Ok(_) => tracing::info!(
                    browser = %b.browser,
                    profile = b.profile_dir.as_deref().unwrap_or("unspecified"),
                    args = b.args.len(),
                    "started for restore"
                ),
                Err(e) => tracing::warn!(browser = %b.browser, error = %e, "could not start"),
            }
        }

        if apps.is_empty() {
            tracing::info!("no applications selected");
            return;
        }

        let report = crate::restore::apps::restore_apps(&apps, &displays, false);
        tracing::info!(
            launched = report.launched.len(),
            placed = report.placed,
            failed = report.failed.len(),
            "restore finished"
        );

        let db = db.lock().unwrap();
        if let Err(e) = crate::restore::record_app_outcomes(&db, run_id, &report) {
            tracing::warn!(error = %e, "could not record restore outcomes");
        }
    });
}

#[cfg(not(windows))]
pub fn run_app(_ctx: UiContext) -> Result<()> {
    Ok(())
}
