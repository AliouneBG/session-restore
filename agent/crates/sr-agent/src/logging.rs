//! Where the agent's log goes, and who is allowed to see it.
//!
//! The agent is two programs in one binary. Run with no arguments it is a background
//! app with a tray icon and no window, and a wall of `INFO reconcile tabs=4` is not
//! something a user launching it from the Start Menu should be reading. Run with a
//! flag it is a command line tool, and then output is the entire point.
//!
//! So the log goes to a file always, and to the terminal only when a human typed a
//! command into one. Nothing is lost either way: the file is the same log, and
//! `--status` prints where it is.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Rotate once past this. One previous file is kept, so the log costs at most twice
/// this on disk. Large enough to hold days of a 60s reconcile, small enough to open
/// in an editor.
const MAX_BYTES: u64 = 4 * 1024 * 1024;

pub fn log_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("logs")
}

pub fn log_path(data_dir: &Path) -> PathBuf {
    log_dir(data_dir).join("agent.log")
}

/// Starts logging to `agent.log`, and to stderr as well when `to_terminal` is set.
///
/// Never fails the caller. A background app that refused to start because it could not
/// open a log file would be trading a working session restore for a diagnostic.
pub fn init(data_dir: &Path, to_terminal: bool) {
    use tracing_subscriber::{fmt, prelude::*, EnvFilter};

    let filter = || EnvFilter::try_from_env("SR_LOG").unwrap_or_else(|_| EnvFilter::new("info"));

    let file = open_log(data_dir);
    let registry = tracing_subscriber::registry();

    // Logs must never contain URLs, window titles, or command lines at default level
    // (docs/06-privacy-security.md). The ingest path logs counts and key hashes only.
    match (file, to_terminal) {
        (Some(f), true) => registry
            .with(fmt::layer().with_ansi(false).with_target(false).with_writer(f).with_filter(filter()))
            .with(fmt::layer().with_target(false).with_writer(std::io::stderr).with_filter(filter()))
            .init(),
        (Some(f), false) => registry
            .with(fmt::layer().with_ansi(false).with_target(false).with_writer(f).with_filter(filter()))
            .init(),
        (None, true) => registry
            .with(fmt::layer().with_target(false).with_writer(std::io::stderr).with_filter(filter()))
            .init(),
        // No file and no terminal: a background run on a machine where the data
        // directory is unwritable. Drop the log rather than refuse to run.
        (None, false) => {}
    }
}

fn open_log(data_dir: &Path) -> Option<std::fs::File> {
    let dir = log_dir(data_dir);
    std::fs::create_dir_all(&dir).ok()?;
    let path = log_path(data_dir);

    // Rotate before opening, so a long-lived agent never holds a handle to the file
    // being renamed out from under it.
    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.len() > MAX_BYTES {
            let _ = std::fs::rename(&path, dir.join("agent.log.1"));
        }
    }

    let mut file = OpenOptions::new().create(true).append(true).open(&path).ok()?;
    // A blank line between runs, so "what happened at this boot" is findable by eye.
    let _ = writeln!(file);
    Some(file)
}

/// True when this invocation is a command the user typed, rather than the background app.
///
/// Anything with a recognised flag wants a terminal. A bare run, or the `--logon` the
/// scheduled task passes, does not.
pub fn wants_terminal(args: &[String]) -> bool {
    const CLI_FLAGS: &[&str] = &[
        "--help",
        "-h",
        "--install",
        "--uninstall",
        "--status",
        "--capture",
        "--documents",
        "--restore-apps",
        "--undo",
    ];
    args.iter().any(|a| CLI_FLAGS.contains(&a.as_str()))
}

/// Borrows the parent process's console, if it has one.
///
/// The binary is built for the `windows` subsystem so that launching it from the Start
/// Menu does not open a console full of log lines. That would also mean `--status`
/// printed into the void, because a windows-subsystem process gets no console at all.
/// Attaching to the parent's gives back exactly the CLI behaviour, and only when a
/// terminal is actually there to attach to.
#[cfg(windows)]
pub fn attach_parent_console() {
    use windows::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
    unsafe {
        // Fails when there is no parent console, for example a double click. That is
        // the normal case for the background app, and there is nothing to do about it.
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

#[cfg(not(windows))]
pub fn attach_parent_console() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_want_a_terminal_and_the_background_app_does_not() {
        assert!(wants_terminal(&["--status".to_string()]));
        assert!(wants_terminal(&["--install".to_string(), "--chrome-id=x".to_string()]));
        assert!(!wants_terminal(&[]), "a bare run is the background app");
        assert!(!wants_terminal(&["--logon".to_string()]), "the logon task is not a terminal");
    }

    #[test]
    fn the_log_lives_under_the_data_directory() {
        let p = log_path(Path::new(r"C:\data"));
        assert!(p.ends_with("agent.log"));
        assert!(p.to_string_lossy().contains("logs"));
    }
}
