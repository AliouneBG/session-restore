//! Resolving which document a window has open.
//!
//! The command line is the obvious source and turns out to be a poor one. Most
//! applications never show a document there:
//!
//! - **Packaged apps** (Windows 11 Notepad, Paint) are launched by activation, so the
//!   path is delivered out of band.
//! - **Single-instance apps** (VS Code, most editors) hand the path to the already
//!   running process and exit, so no surviving process has it on its command line.
//!
//! What both cases *do* leave is a window title containing the file name, and an entry
//! in the user's Recent items containing the full path. Matching one against the other
//! resolves a real path without guessing a directory - and guessing is the thing to
//! avoid, because reopening the wrong file is worse than reopening nothing.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Splits a window title on the separators applications actually use.
///
/// Each is a dash or bar with whitespace on both sides. The whole title is also kept
/// as a candidate, so a window titled with just a filename still resolves.
fn split_on_separators(title: &str) -> Vec<&str> {
    const SEPARATORS: &[&str] = &[" - ", " \u{2013} ", " \u{2014} ", " | ", " \u{2022} "];

    let mut parts = vec![title];
    for sep in SEPARATORS {
        let mut next = Vec::new();
        for part in parts {
            next.extend(part.split(sep));
        }
        parts = next;
    }
    parts
}

/// Filenames pulled out of a window title, most specific first.
///
/// Titles are overwhelmingly `<document> - <app>` or `<document> - <app>`, sometimes
/// with a modified marker. Only segments that look like a filename with an extension
/// are considered, so "Settings - Discord" yields nothing rather than a bogus lookup.
pub fn candidates_from_title(title: &str) -> Vec<String> {
    let mut out = Vec::new();

    // Split on the *separator* form - a dash with spaces around it - not on a bare
    // dash. Hyphens are extremely common inside filenames, and splitting on them
    // turned "sr-tier-c-demo.txt - Notepad" into "demo.txt", which then matched
    // nothing. The separator in a window title always has spaces around it.
    for raw in split_on_separators(title) {
        let seg = raw
            .trim()
            // Editors mark unsaved changes; the file name underneath is unchanged.
            .trim_start_matches('*')
            .trim_start_matches('●')
            .trim();
        if seg.is_empty() || seg.len() > 200 {
            continue;
        }

        let path = Path::new(seg);
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            continue;
        };
        // An "extension" of more than a few characters is usually a sentence fragment
        // that happened to contain a dot.
        if ext.is_empty() || ext.len() > 8 || !ext.chars().all(|c| c.is_ascii_alphanumeric()) {
            continue;
        }
        if !out.contains(&seg.to_string()) {
            out.push(seg.to_string());
        }
    }

    out
}

/// Maps file name -> full path from the user's Recent items.
///
/// Recent is where Windows records what was actually opened, by any means - command
/// line, File > Open, drag and drop, or a jump list. That makes it the one source that
/// covers the cases a command line misses.
#[cfg(windows)]
pub fn recent_index() -> HashMap<String, PathBuf> {
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};

    let mut index = HashMap::new();

    // Shortcut resolution goes through COM, so the calling thread must be initialized.
    // Without this every CoCreateInstance fails with CO_E_NOTINITIALIZED and the whole
    // index comes back empty - silently, since a missing document is not an error.
    // RPC_E_CHANGED_MODE just means the thread is already initialized differently,
    // which is fine.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }

    let Some(appdata) = std::env::var_os("APPDATA") else {
        return index;
    };
    let recent = PathBuf::from(appdata)
        .join("Microsoft")
        .join("Windows")
        .join("Recent");

    let Ok(entries) = std::fs::read_dir(&recent) else {
        return index;
    };

    // Newest first, so a name that appears twice resolves to the most recent target.
    let mut links: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter(|e| {
            e.path()
                .extension()
                .map(|x| x.eq_ignore_ascii_case("lnk"))
                .unwrap_or(false)
        })
        .filter_map(|e| {
            let m = e.metadata().ok()?;
            Some((m.modified().ok()?, e.path()))
        })
        .collect();
    links.sort_by(|a, b| b.0.cmp(&a.0));

    for (_, link) in links.into_iter().take(400) {
        if let Some(target) = resolve_shortcut(&link) {
            if let Some(name) = target.file_name().and_then(|n| n.to_str()) {
                index.entry(name.to_lowercase()).or_insert(target.clone());
            }
        }
    }

    index
}

/// Reads a `.lnk`'s target path.
#[cfg(windows)]
fn resolve_shortcut(link: &Path) -> Option<PathBuf> {
    use windows::core::{Interface, HSTRING, PCWSTR};
    use windows::Win32::System::Com::{
        CoCreateInstance, IPersistFile, CLSCTX_INPROC_SERVER, STGM_READ,
    };
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink, SLGP_RAWPATH};

    unsafe {
        let shell_link: IShellLinkW =
            CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).ok()?;
        let persist: IPersistFile = shell_link.cast().ok()?;

        persist
            .Load(&HSTRING::from(link.as_os_str()), STGM_READ)
            .ok()?;

        let mut buf = [0u16; 1024];
        // SLGP_RAWPATH, not the resolved form: resolution can trigger a network or
        // removable-media probe and block for seconds per shortcut.
        shell_link
            .GetPath(&mut buf, std::ptr::null_mut(), SLGP_RAWPATH.0 as u32)
            .ok()?;

        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        if end == 0 {
            return None;
        }
        let _ = PCWSTR::null();
        let path = PathBuf::from(String::from_utf16_lossy(&buf[..end]));
        // Only real, existing files. A stale Recent entry would produce a restore that
        // opens an error dialog instead of a document.
        if path.is_file() {
            Some(path)
        } else {
            None
        }
    }
}

#[cfg(not(windows))]
pub fn recent_index() -> HashMap<String, PathBuf> {
    HashMap::new()
}

/// Resolves a window title to a document path using the Recent index.
pub fn resolve_from_title(title: &str, index: &HashMap<String, PathBuf>) -> Option<PathBuf> {
    for name in candidates_from_title(title) {
        if let Some(path) = index.get(&name.to_lowercase()) {
            return Some(path.clone());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pulls_a_filename_out_of_a_typical_title() {
        assert_eq!(
            candidates_from_title("report.docx - Word"),
            vec!["report.docx".to_string()]
        );
    }

    #[test]
    fn handles_the_dash_characters_apps_actually_use() {
        assert!(candidates_from_title("notes.md - Obsidian").contains(&"notes.md".to_string()));
        assert!(candidates_from_title("a.txt | Editor").contains(&"a.txt".to_string()));
    }

    #[test]
    fn ignores_an_unsaved_marker() {
        // Editors prefix modified documents; the file underneath is the same one.
        assert!(candidates_from_title("*main.rs - VS Code").contains(&"main.rs".to_string()));
    }

    #[test]
    fn finds_the_document_in_a_multi_part_title() {
        let c = candidates_from_title("main.rs - session-restore - Visual Studio Code");
        assert!(c.contains(&"main.rs".to_string()));
    }

    #[test]
    fn a_hyphenated_filename_survives() {
        // The bug this exists for: splitting on a bare dash turned
        // "sr-tier-c-demo.txt - Notepad" into "demo.txt", which matched nothing.
        // Hyphens are ordinary in filenames; the title separator has spaces around it.
        assert!(candidates_from_title("sr-tier-c-demo.txt - Notepad")
            .contains(&"sr-tier-c-demo.txt".to_string()));
        assert!(candidates_from_title("my-report-final-v2.docx - Word")
            .contains(&"my-report-final-v2.docx".to_string()));
    }

    #[test]
    fn a_title_that_is_only_a_filename_resolves() {
        assert!(candidates_from_title("notes.txt").contains(&"notes.txt".to_string()));
    }

    #[test]
    fn a_title_with_no_document_yields_nothing() {
        // The important negative: no filename means no guess.
        assert!(candidates_from_title("Settings - Discord").is_empty());
        assert!(candidates_from_title("Inbox - Outlook").is_empty());
        assert!(candidates_from_title("Calculator").is_empty());
    }

    #[test]
    fn a_sentence_containing_a_dot_is_not_a_filename() {
        assert!(candidates_from_title("Ready. Waiting for input - App").is_empty());
    }

    #[test]
    fn a_long_pseudo_extension_is_rejected() {
        assert!(candidates_from_title("something.verylongextension - App").is_empty());
    }

    #[test]
    fn resolution_needs_a_matching_recent_entry() {
        let mut index = HashMap::new();
        index.insert("report.docx".to_string(), PathBuf::from(r"C:\Docs\report.docx"));

        assert_eq!(
            resolve_from_title("report.docx - Word", &index),
            Some(PathBuf::from(r"C:\Docs\report.docx"))
        );
        // Never invents a path for something Recent has not seen.
        assert_eq!(resolve_from_title("other.docx - Word", &index), None);
    }

    #[test]
    fn matching_is_case_insensitive_like_the_filesystem() {
        let mut index = HashMap::new();
        index.insert("report.docx".to_string(), PathBuf::from(r"C:\Docs\report.docx"));
        assert!(resolve_from_title("REPORT.DOCX - Word", &index).is_some());
    }
}
