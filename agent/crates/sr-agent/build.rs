//! Places `WebView2Loader.dll` next to the built binary.
//!
//! The MSVC target links the WebView2 loader statically and needs none of this. The
//! GNU target imports it as a DLL, so without this the agent fails to start at all
//! with `0xC0000135` (DLL not found) - and because the failure happens before `main`,
//! there is no log line and no error message, just a process that vanishes.
//!
//! `webview2-com-sys` ships the DLL for each architecture; this finds the copy its
//! build script unpacked and puts it where the loader will look.

use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    let target = std::env::var("TARGET").unwrap_or_default();
    if !target.contains("windows-gnu") {
        // MSVC links it statically.
        return;
    }

    let Ok(out_dir) = std::env::var("OUT_DIR") else {
        return;
    };
    let out_dir = PathBuf::from(out_dir);

    // OUT_DIR is <target>/<profile>/build/<crate>-<hash>/out. The binary lands in
    // <target>/<profile>, four levels up.
    let Some(profile_dir) = out_dir.ancestors().nth(3) else {
        return;
    };

    let arch = if target.starts_with("aarch64") {
        "arm64"
    } else if target.starts_with("i686") {
        "x86"
    } else {
        "x64"
    };

    let build_dir = profile_dir.join("build");
    let Some(src) = find_loader(&build_dir, arch) else {
        println!(
            "cargo:warning=WebView2Loader.dll not found under {}; the agent will not start \
             until it is placed next to the binary",
            build_dir.display()
        );
        return;
    };

    let dest = profile_dir.join("WebView2Loader.dll");
    if let Err(e) = std::fs::copy(&src, &dest) {
        // Already-running agents hold the file open; that is not a build failure.
        println!("cargo:warning=could not copy WebView2Loader.dll: {e}");
    }
}

fn find_loader(build_dir: &Path, arch: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir(build_dir).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("webview2-com-sys-") {
            continue;
        }
        let candidate = entry.path().join("out").join(arch).join("WebView2Loader.dll");
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}
