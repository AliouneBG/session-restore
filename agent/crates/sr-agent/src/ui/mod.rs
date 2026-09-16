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
    /// Shared with the pipe server, so a decision made here governs what browsers are
    /// offered when they connect.
    pub shared: Arc<crate::server::Shared>,
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

    // Window event hooks need a message pump, which is this thread. The capture they
    // trigger does not, so it runs on a worker - a capture pass walks every process on
    // the machine and must never block the tray.
    match crate::watcher::events::install() {
        Ok(rx) => {
            let db = Arc::clone(&ctx.db);
            std::thread::spawn(move || {
                while rx.recv().is_ok() {
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
    let mut open_at_start = ctx.review_at_start;

    let db = ctx.db;
    let keys = ctx.keys;
    let pending = ctx.pending_snapshot;
    let shared = ctx.shared;

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
                            apply_choice(&db, &keys, &shared, snapshot_id, &choice);
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
                //
                // It does have to release browsers waiting on an answer, though. They
                // were deliberately not offered anything while the window was open, and
                // without this their offer would never arrive at all.
                {
                    let mut pending = shared.pending_restore.lock().unwrap();
                    pending.review_dismissed();
                }
                crate::server::offer_to_connected(&shared);

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

    let selected: std::collections::HashSet<&str> =
        choice.apps.iter().map(|s| s.as_str()).collect();

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
            .filter(|a| selected.contains(a.app_key.as_str()))
            .collect();
        if picked.is_empty() {
            tracing::info!("no applications selected");
            return;
        }

        // The undo point, and it has to be taken here rather than left to the browser
        // half. This path used to launch applications without opening a run at all, so
        // a restore answered in the review window was the one restore `--undo` could
        // not reverse: with no run of its own it reached back to whatever ran last,
        // and "undo" meant restoring some older session over the top of this one.
        //
        // Capture first: the undo point is what is on screen *now*, and the periodic
        // sweep may be up to a minute stale, which at startup is exactly the minute
        // in which the review window is answered.
        if let Err(e) = crate::watcher::capture_into_live(&db) {
            tracing::warn!(error = %e, "capture before restore failed; undo point may be stale");
        }
        // "ask" and not a mode of its own: `restore_runs.mode` records how the
        // restore was decided, and the review window is what asking looks like.
        // It also keeps one episode's rows consistent - the browser half passes
        // the same setting value when it connects a moment later.
        let run_id = match crate::restore::begin_run(&db, snapshot_id, "ask") {
            Ok(id) => id,
            Err(e) => {
                // Without a run there is no undo, and silently restoring anyway would
                // hand the user an irreversible change they had no way to know about.
                tracing::error!(error = %e, "could not open a restore run; not restoring");
                return;
            }
        };

        // A browser that is not running will never connect, and the offer recorded
        // above waits for a connection. After a reboot that is every browser.
        let wanted: std::collections::HashSet<String> =
            choice.browser_windows.iter().cloned().collect();
        let browsers = crate::restore::browsers_to_launch(&db, snapshot_id, &wanted)
            .unwrap_or_else(|e| {
                tracing::warn!(error = %e, "could not work out which browsers to start");
                Vec::new()
            });

        let displays = crate::watcher::displays::enumerate().unwrap_or_default();
        (picked, displays, run_id, browsers)
    };

    // Launching blocks on staggered sleeps, so it runs off the UI thread; holding the
    // message pump would freeze the tray for the duration.
    let db = Arc::clone(db);
    std::thread::spawn(move || {
        for b in &browsers {
            match crate::restore::launch::launch_via_shell(&b.exe_path) {
                Ok(_) => tracing::info!(browser = %b.browser, "started for restore"),
                Err(e) => tracing::warn!(browser = %b.browser, error = %e, "could not start"),
            }
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
