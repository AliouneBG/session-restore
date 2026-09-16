//! Tray icon and the review window.
//!
//! Windows requires a message pump on the thread that owns windows, and tao's event
//! loop never returns, so **the UI owns the main thread** and the pipe server, sweeper
//! and capture pass run behind it. That is the opposite of how the agent started out
//! and it is the right way round: the server has no thread affinity and the UI does.
//!
//! One loop hosts both the tray and the review window, because two event loops on one
//! thread is not a thing Windows will do.

pub mod review;
pub mod tray;

use crate::store::db::Db;
use crate::store::keys::KeyManager;
use anyhow::Result;
use std::sync::{Arc, Mutex};

pub struct UiContext {
    pub db: Arc<Mutex<Db>>,
    pub keys: Arc<KeyManager>,
    /// The snapshot offered for restore, if there is one.
    pub pending_snapshot: Option<i64>,
    /// Whether to open the review window as soon as the agent starts.
    pub review_at_start: bool,
}

#[derive(Debug)]
enum UserEvent {
    Ipc(String),
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

    let capture_enabled = {
        let db = ctx.db.lock().unwrap();
        db.setting_bool("capture_enabled", true)
    };
    let tray = tray::Tray::new(capture_enabled)?;

    // Held across iterations. Dropping the pair closes the window.
    let mut review_window: Option<(tao::window::Window, wry::WebView)> = None;
    let mut review_snapshot: Option<i64> = None;
    let mut open_at_start = ctx.review_at_start;

    let db = ctx.db;
    let keys = ctx.keys;
    let pending = ctx.pending_snapshot;

    event_loop.run(move |event, target, control_flow| {
        *control_flow = ControlFlow::Wait;

        match event {
            Event::NewEvents(StartCause::Init) => {
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
                } else if id == tray.capture_now_id {
                    let db = db.lock().unwrap();
                    match crate::watcher::capture_into_live(&db) {
                        Ok(s) => tracing::info!(apps = s.apps, windows = s.windows, "captured"),
                        Err(e) => tracing::warn!(error = %e, "capture failed"),
                    }
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
                            apply_choice(&db, &keys, snapshot_id, &choice);
                        }
                    }

                    _ => {}
                }
            }

            Event::WindowEvent {
                event: tao::event::WindowEvent::CloseRequested,
                ..
            } => {
                // Dismissal decides nothing. Treating a closed window as consent would
                // restore a session the user never agreed to.
                review_window = None;
                review_snapshot = None;
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

    let proxy = proxy.clone();
    let webview = WebViewBuilder::new()
        .with_html(review::REVIEW_HTML)
        .with_ipc_handler(move |req| {
            let _ = proxy.send_event(UserEvent::Ipc(req.body().to_string()));
        })
        .build(&window)?;

    Ok((window, webview))
}

/// Carries out what the user selected.
#[cfg(windows)]
fn apply_choice(
    db: &Arc<Mutex<Db>>,
    _keys: &Arc<KeyManager>,
    snapshot_id: i64,
    choice: &review::ReviewChoice,
) {
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

    let selected: std::collections::HashSet<&str> =
        choice.apps.iter().map(|s| s.as_str()).collect();

    let (apps, displays) = {
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
            .filter(|a| selected.contains(a.app_key.as_str()))
            .collect();
        let displays = crate::watcher::displays::enumerate().unwrap_or_default();
        (picked, displays)
    };

    if apps.is_empty() {
        tracing::info!("no applications selected");
        return;
    }

    // Launching blocks on staggered sleeps, so it runs off the UI thread; holding the
    // message pump would freeze the tray for the duration.
    std::thread::spawn(move || {
        let report = crate::restore::apps::restore_apps(&apps, &displays, false);
        tracing::info!(
            launched = report.launched.len(),
            placed = report.placed,
            failed = report.failed.len(),
            "restore finished"
        );
    });
}

#[cfg(not(windows))]
pub fn run_app(_ctx: UiContext) -> Result<()> {
    Ok(())
}
