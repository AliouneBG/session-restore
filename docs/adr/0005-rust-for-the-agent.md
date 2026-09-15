# ADR-0005 — Rust for the agent and relay

**Status:** Accepted
**Date:** 2026-09-13

## Context

The agent is a background process that runs on the user's machine from logon to logoff,
every day, holding encryption keys and the user's browsing history. The relay parses
untrusted-ish input from the browser. Candidates considered: Rust, Go, C#/.NET, Node.

## Decision

**Rust** for both the agent and the relay. TypeScript for the extension (no choice
there).

## Why

The decisive factors, in order:

1. **It is always running.** A background agent's resource footprint determines whether
   it stays installed. Rust gives a ~6 MB binary with no runtime, no GC pauses, and
   idle memory in the low tens of MB. A .NET or Node agent starts at 60-100 MB RSS and
   shows up in Task Manager's startup-impact list, which is how a tool like this gets
   uninstalled.

2. **No runtime to install.** Single self-contained `.exe`. A self-contained .NET
   publish is ~60 MB; framework-dependent means checking for and possibly installing a
   runtime during setup. Node means bundling Node. For a per-user, no-admin install,
   one small binary is a materially better story.

3. **windows-rs is first-class.** Microsoft generates it from the Windows metadata, so
   every API this project needs — `EnumWindows`, `SetWinEventHook`,
   `DwmGetWindowAttribute`, `GetWindowPlacement`, DPAPI, and the COM interfaces
   `IApplicationActivationManager` and `IVirtualDesktopManager` — is available with
   correct signatures and real COM support. The usual "Rust on Windows means hand-writing
   FFI" objection has not been true for years.

4. **It handles keys and parses IPC.** Memory-safety matters most in exactly the two
   places this project has: crypto key handling and parsing messages that crossed a trust
   boundary. `aes-gcm` from RustCrypto is well-audited, and `zeroize` gives reliable key
   scrubbing that is genuinely awkward in a GC'd language where you cannot control when
   a key buffer is collected or whether it was copied during compaction.

## Why not the others

**C#/.NET** — the strongest alternative, and the best Windows API ergonomics of the
four. Rejected on footprint for an always-on process and on the self-contained publish
size. If the team were already a .NET shop this would be a defensible flip; the
architecture does not depend on the choice.

**Go** — good middle ground, fast to write, cross-compiles cleanly. Weaker on the COM
interop this project genuinely needs (`IApplicationActivationManager` for UWP launching
is not optional), and the Win32 story is thinner than windows-rs. Larger binaries, GC
pauses that do not matter here.

**Node/TypeScript** — the appeal is one language across extension and agent. Rejected:
Win32 window enumeration via `koffi`/FFI or by shelling out to PowerShell is awkward and
slow, the runtime footprint is the worst of the four, and holding encryption keys in a
GC'd heap with no zeroize guarantee is the weakest option for the most sensitive part of
the system. Type sharing is preserved anyway by generating both sides from one JSON
Schema ([07](../07-extension.md)).

## Consequences

- Steeper if you have not written Rust. The window-watching and COM code is the hard
  part; the SQLite and protocol layers are ordinary.
- Mitigation: the milestone order in [09](../09-roadmap.md) front-loads the easy layers
  (M0/M1 are pipes, SQLite, and JSON) and defers the Win32-heavy work to M2, so there is
  a working system before the difficult code starts.
- `windows-rs` has a large API surface; enable only the needed features to keep compile
  times reasonable.
- Unsafe blocks are unavoidable for Win32. Confine them to `watcher/` and `restore/`,
  wrap each in a safe abstraction, and add `#![warn(unsafe_op_in_unsafe_fn)]`. The PEB
  command-line read ([03](../03-capture.md)) is the most dangerous single piece of code
  in the project — isolate it, make it fail soft, and never let a failure there take down
  the agent.

## Not a one-way door

The component boundaries are process boundaries with a documented wire protocol
([05](../05-ipc-protocol.md)). The agent could be rewritten in another language without
touching the extension. This decision is reversible at the cost of one component, which
is the main reason it is safe to make now rather than after a prototype.
