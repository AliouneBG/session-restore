# 08 — Windows agent

## Crate layout

```
agent/
  Cargo.toml                 workspace
  crates/
    sr-agent/                the long-lived binary
      src/
        main.rs              startup, single-instance guard, supervisor
        watcher/
          windows.rs         EnumWindows + SetWinEventHook
          processes.rs       ETW process-start listener, PEB fallback
          displays.rs        monitor enumeration, stable display_key
          desktops.rs        IVirtualDesktopManager (best effort)
        ingest/
          mod.rs             ingest_tab() - the privacy chokepoint (see 06)
          pipe.rs            named pipe server
        store/
          db.rs              SQLite, migrations
          snapshot.rs        snapshot_id = 0 semantics, retention
          crypto.rs          DPAPI wrap/unwrap, AES-256-GCM
        restore/
          plan.rs            build the ordered plan
          launch.rs          CreateProcess / ShellExecute / ActivateApplication
          place.rs           SetWindowPlacement, topology mapping, DPI
          undo.rs            pre_restore snapshot + 60s undo
        ui/
          tray.rs            tray icon + menu
          review.rs          the restore review window
        shutdown.rs          WM_QUERYENDSESSION, ShutdownBlockReasonCreate
    sr-relay/                the ~200-line native messaging relay
    sr-proto/                generated from /schema, shared with the extension
```

## Dependencies

| Crate | Use |
|---|---|
| `windows` (windows-rs) | All Win32/COM: EnumWindows, DWM, DPAPI, IApplicationActivationManager |
| `rusqlite` (bundled) | SQLite; `bundled` so there is no system dependency |
| `aes-gcm` | C4 encryption |
| `tokio` | Async runtime for the pipe server and timers |
| `tray-icon` + `muda` | Tray icon and menu |
| `serde` / `serde_json` | Wire format |
| `tracing` | Structured logging with a scrubbing layer |
| `ferrisetw` *or* hand-rolled | ETW process-start subscription |

Deliberately **not** pulling in a full GUI framework. The tray menu covers most
interaction; the review window is the only real UI and can be a small WebView2 window
loading local HTML — WebView2 ships with Windows 11, so it adds no install burden and
lets the review UI share styling with the extension options page.

## Why not a Windows Service

See [ADR-0001](adr/0001-user-agent-not-windows-service.md). Summary: Session 0
isolation makes a service unable to enumerate the user's windows, unable to launch
apps into the user's desktop, and unable to use the user's DPAPI key. Every one of
those is central to this product.

## Startup

```
1. Single-instance guard (named mutex, per-session)
2. Open/migrate the database
3. Unwrap the DEK via DPAPI (defer until first private write if capture_private is off)
4. Sweep expired tabs_private rows, VACUUM if any were deleted
5. Verify native messaging registry keys for all three browsers; repair if missing
6. Start the named pipe server
7. Start the ETW process listener
8. Start window watchers, displays watcher
9. Register the shutdown message window
10. Check restore_in_progress marker -> decide restore mode (see 04)
11. Wait for shell + topology to settle -> restore flow
12. Enter steady state: T0 events + T1 timer
```

## Installation as a logon task

A Scheduled Task, not a Run key and not a service:

```
Trigger:   At log on of <user>, delay 30 seconds
Action:    %LOCALAPPDATA%\SessionRestore\sr-agent.exe --logon
Settings:  Run with highest privileges = NO        <- deliberate, see 06
           Run only when user is logged on = YES
           Stop if the computer switches to battery = NO
           Restart on failure: 3 times, every 1 minute
           Execution time limit = Disabled (it runs forever)
```

"Run with highest privileges = NO" is a security decision, not an oversight. The agent
never needs admin, and an always-running elevated process that launches other processes
based on the contents of a writable database file would be a standing privilege
escalation primitive.

## Shutdown handling (T2, best-effort)

Create a hidden message-only window and handle:

| Message | Action |
|---|---|
| `WM_QUERYENDSESSION` | Call `ShutdownBlockReasonCreate(hwnd, "Saving your session...")`, start the final flush, return TRUE |
| `WM_ENDSESSION` | Finish the flush, `ShutdownBlockReasonDestroy`, exit cleanly |
| `WM_POWERBROADCAST` / `PBT_APMSUSPEND` | Flush before sleep |
| `WTS_SESSION_LOCK` / `_LOGOFF` | Flush |

Budget the final flush at **2 seconds**, hard. Windows gives an application a limited
window before it force-terminates it, and a shutdown blocker that overstays gets the
user a "this app is preventing shutdown" screen — a guaranteed way to make people
uninstall. Since T1 already bounds loss at 60s, T2 is pure upside and should never be
allowed to become a liability.

Register via `RegisterApplicationRestart` too, so Windows Update restarts bring the
agent back automatically after a forced reboot.

## Virtual desktops

The public `IVirtualDesktopManager` COM interface exposes only `GetWindowDesktopId` and
`MoveWindowToDesktop`. It cannot enumerate desktops, create them, or name them. The
private `IVirtualDesktopManagerInternal` can, but its IID and vtable **change between
Windows builds** and using it means the product breaks on a random Tuesday.

Decision: capture `virtual_desktop_id` via the public API and use it only to
*group* windows in the review UI ("these 4 were on another desktop"). On restore,
place everything on the current desktop and note it in the summary. Revisit only if
Microsoft ships a public enumeration API. A feature that breaks on every feature
update is worse than an absent one.

## Single-writer discipline

The agent is the only process that opens `sessions.db` for writing. The relay has no
disk access. The review UI is in-process. This avoids SQLite lock contention entirely
and means WAL is being used for crash-safety rather than concurrency.

## Watchdog and self-healing

- If the pipe server panics, restart it without killing the process.
- If the ETW session drops (it does, on some Windows Update paths), fall back to PEB
  reads and retry the ETW subscription every 5 minutes.
- If the database is corrupt at startup (`PRAGMA integrity_check` fails), rename it to
  `sessions.db.corrupt-<ts>`, start fresh, and surface a tray notification. Never
  attempt automatic repair of a file whose contents are, by definition, untrustworthy.
- Panic hook scrubs any URL-shaped string before the message reaches a log ([06](06-privacy-security.md)).

## Performance budget

These are targets to test against, not aspirations:

| Metric | Budget |
|---|---|
| Idle CPU | < 0.1% average over 5 minutes |
| Memory (RSS) | < 40 MB steady state |
| T1 reconcile duration | < 150 ms for 60 windows |
| Disk writes | < 1 MB/hour typical |
| Startup to steady state | < 3 s |
| Restore of 10 apps + 40 tabs | < 25 s to usable |

The idle budget is the one that decides whether this stays installed. An always-on
background agent that shows up in Task Manager's startup impact list gets uninstalled
regardless of how well it works. `SetWinEventHook` with out-of-context delivery plus a
60s timer should sit near zero; if it does not, that is a bug, not a tuning exercise.
