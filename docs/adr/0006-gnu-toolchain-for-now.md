# ADR-0006 — Build on the GNU toolchain for now, keep MSVC as the ship target

**Status:** Accepted (provisional — revisit before first release)
**Date:** 2026-09-15
**Amends:** [ADR-0005](0005-rust-for-the-agent.md)

## Context

ADR-0005 chose Rust and assumed the standard Windows target,
`x86_64-pc-windows-msvc`. That target needs `link.exe`, which comes from Visual Studio
Build Tools.

The dev machine has no Visual Studio of any kind. Two attempts to install
`Microsoft.VisualStudio.2022.BuildTools` via winget failed with installer exit code
**1602 — user cancelled**: the elevation prompt does not reach the user from a
background process, and Build Tools cannot install without admin.

Rust itself installed fine (rustup is per-user), so the blocker is specifically the
MSVC linker, not Rust.

## Decision

Default to **`x86_64-pc-windows-gnu`**, pinned in `rust-toolchain.toml`. Keep
`x86_64-pc-windows-msvc` as the intended release target and add it to CI as soon as
there is CI.

## Why this is acceptable

The GNU target ships its own linker via rustup's `rust-mingw` component, so it needs no
admin rights and no multi-gigabyte install. The open question was whether `windows-rs`
— particularly COM — actually works there. It was verified empirically rather than
assumed, with a probe exercising every Win32 area this project depends on:

| Probe | Result |
|---|---|
| `EnumWindows` + `IsWindowVisible` + `GetWindowTextW` | Pass |
| `DwmGetWindowAttribute(DWMWA_CLOAKED)` | Pass — correctly filtered cloaked windows |
| `GetWindowPlacement` | Pass |
| `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)` + `QueryFullProcessImageNameW` | Pass |
| `CryptProtectData` (DPAPI) | Pass |
| `CoCreateInstance(ApplicationActivationManager)` -> `IApplicationActivationManager` | Pass |

COM instantiation working is the one that mattered; it is the piece most likely to be
weak off-MSVC, and it is not optional for launching UWP apps.

## Why MSVC remains the ship target

- It is what Windows users' debuggers, crash dumps, and symbol servers expect. PDB
  generation on GNU is not equivalent.
- Authenticode signing and the usual Windows toolchain assume MSVC output.
- A few crates have MSVC-only code paths or better-tested MSVC builds.
- ETW consumption (needed in M2) is more commonly exercised on MSVC; if it misbehaves on
  GNU, that is the likely first place to find out.

## Consequences

- `rust-toolchain.toml` pins the GNU toolchain so builds are reproducible for now.
- Nothing in the codebase may depend on GNU-specific behavior. Both targets must stay
  buildable; CI should build both from the first day it exists.
- Before release, install Build Tools **interactively** (so the UAC prompt is actually
  answerable) and switch the default:
  ```
  winget install --id Microsoft.VisualStudio.2022.BuildTools -e --override ^
    "--quiet --wait --norestart --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
  rustup default stable-x86_64-pc-windows-msvc
  ```
- Binary size and startup are slightly worse on GNU. Irrelevant during development,
  and re-measured against the [08-agent.md](../08-agent.md) budget once on MSVC.

## Note

This is a development-environment decision, not an architectural one. It changes no
interface and no on-disk format, and reverting it is a one-line change to
`rust-toolchain.toml`.
