//! First run.
//!
//! Three steps and it is over. The constraint that shapes this is not design taste, it
//! is that **nothing here can install the extension for the user**. Chrome and Edge
//! removed silent external extension installs on Windows, deliberately, and the only
//! supported path for a consumer application is a page the user clicks Add on.
//!
//! So onboarding cannot be a progress bar. What it can be is a thing that opens the
//! right page, then *notices* when the extension connects and says so, which is what
//! makes it feel like being helped rather than being given homework. The agent already
//! logs `extension connected` at exactly that moment, so the detection costs nothing:
//! the page polls, and the dot turns green on its own.
//!
//! Private windows are the optional third step and stay off unless asked for. Putting
//! them in the flow at all is a deliberate choice: it is the feature people come for,
//! and burying it in settings would mean most people never learn it exists.

use crate::browsers::BrowserInfo;
use crate::store::db::Db;
use anyhow::Result;
use serde::{Deserialize, Serialize};

pub const ONBOARDING_HTML: &str = include_str!("onboarding.html");

/// The setting that records first run is done.
pub const DONE_KEY: &str = "onboarding_completed";

#[derive(Debug, Serialize)]
pub struct OnboardingPayload {
    pub browsers: Vec<BrowserInfo>,
    pub capture_private: bool,
    /// True once any browser has ever connected, so the flow can skip ahead for
    /// someone who set things up before opening this.
    pub any_connected: bool,
    pub agent_version: String,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "action")]
pub enum OnboardingAction {
    #[serde(rename = "ready")]
    Ready,
    /// The page asks again every couple of seconds while the browser step is showing.
    #[serde(rename = "poll")]
    Poll,
    #[serde(rename = "open_extensions_page")]
    OpenExtensionsPage { browser: String },
    #[serde(rename = "set_capture_private")]
    SetCapturePrivate { value: bool },
    #[serde(rename = "finish")]
    Finish,
}

pub fn collect(db: &Db, connected: &[String]) -> Result<OnboardingPayload> {
    let browsers = crate::browsers::detect(db, connected);
    Ok(OnboardingPayload {
        any_connected: browsers.iter().any(|b| b.connected),
        capture_private: db.setting_bool("capture_private_windows", false),
        browsers,
        agent_version: crate::AGENT_VERSION.to_string(),
    })
}

/// Whether the first-run flow should be shown.
///
/// Shown once, and never again once finished, including when the user closes the
/// window without pressing the last button. Reopening it uninvited at every sign-in
/// would be the single most irritating thing this application could do.
pub fn should_show(db: &Db) -> bool {
    !db.setting_bool(DONE_KEY, false)
}

pub fn mark_done(db: &Db) -> Result<()> {
    db.set_setting(DONE_KEY, "true")?;
    Ok(())
}

pub fn apply(db: &Db, action: &OnboardingAction) -> Result<()> {
    match action {
        OnboardingAction::OpenExtensionsPage { browser } => {
            crate::browsers::open_extensions_page(browser)
        }
        OnboardingAction::SetCapturePrivate { value } => {
            db.set_setting(
                "capture_private_windows",
                if *value { "true" } else { "false" },
            )?;
            if !*value {
                let _ = crate::ingest::purge_all_private(db);
            }
            Ok(())
        }
        OnboardingAction::Finish => mark_done(db),
        OnboardingAction::Ready | OnboardingAction::Poll => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::db::Db;

    struct Dir(std::path::PathBuf);
    impl Dir {
        fn new() -> Dir {
            let p = std::env::temp_dir().join(format!("sr-onboard-{}", sr_proto::new_id()));
            std::fs::create_dir_all(&p).unwrap();
            Dir(p)
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn db() -> (Dir, Db) {
        let d = Dir::new();
        let db = Db::open(&d.0.join("sessions.db")).unwrap();
        (d, db)
    }

    #[test]
    fn a_fresh_install_sees_onboarding_once() {
        let (_d, db) = db();
        assert!(should_show(&db), "first run must show the flow");
        mark_done(&db).unwrap();
        assert!(!should_show(&db), "it must not come back");
    }

    /// Closing the window counts as finishing. Reopening the flow uninvited at every
    /// sign-in would be the most irritating thing this application could do.
    #[test]
    fn dismissing_the_flow_still_marks_it_done() {
        let (_d, db) = db();
        apply(&db, &OnboardingAction::Finish).unwrap();
        assert!(!should_show(&db));
    }

    #[test]
    fn private_capture_starts_off_and_is_opt_in() {
        let (_d, db) = db();
        let p = collect(&db, &[]).unwrap();
        assert!(!p.capture_private, "private capture must default to off");

        apply(&db, &OnboardingAction::SetCapturePrivate { value: true }).unwrap();
        assert!(collect(&db, &[]).unwrap().capture_private);
    }

    #[test]
    fn the_payload_notices_a_connected_browser() {
        let (_d, db) = db();
        assert!(!collect(&db, &[]).unwrap().any_connected);
        let p = collect(&db, &["edge".to_string()]).unwrap();
        assert!(p.any_connected);
        assert!(p.browsers.iter().any(|b| b.id == "edge" && b.connected));
    }
}
