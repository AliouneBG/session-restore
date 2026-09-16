# Session Restore

Put your Windows 11 session back the way you left it: the applications you had open,
where their windows were, and the tabs in every browser window, including private and
incognito windows if you ask for them.

It is built for the case where you did not get to save anything. A reboot for updates,
a battery that ran out, a machine that locked up. The session on disk is never more
than about sixty seconds behind what is on screen, so a hard power cut costs you a
minute of context rather than an afternoon of it.

**Status:** working end to end on Chrome, Edge and Firefox. No installer yet, so you
build it and run `--install` yourself. See [Status](#status).

---

## Contents

- [What it actually does](#what-it-actually-does)
- [The review window](#the-review-window)
- [How it works](#how-it-works)
- [Install](#install)
- [Daily use](#daily-use)
- [What it recovers, and what it does not](#what-it-recovers-and-what-it-does-not)
- [Privacy](#privacy)
- [Command reference](#command-reference)
- [Project layout](#project-layout)
- [Documentation](#documentation)
- [Status](#status)
- [Development](#development)

---

## What it actually does

| It restores | Detail |
|---|---|
| Applications | Relaunched by fidelity tier, best method first |
| Window layout | Exact position, size and maximized state, remapped if your monitors changed |
| Multi-monitor setups | Windows land on the right display even at a different DPI or resolution |
| Browser tabs | Per window, in order, pinned state and active tab preserved |
| Documents | Files an application had open are reopened by the shell |
| Private windows | Only with an explicit opt-in, encrypted at rest, auto-expiring |

It also refuses to do a few things on purpose. It will not restore an elevated
application, it will not guess a file path it cannot resolve, and it will not open a
second copy of something you already have running.

### Restore fidelity tiers

Every application is classified at capture time, and the review window tells you which
tier it landed in so nothing is a surprise.

| Tier | Shown as | Method |
|---|---|---|
| A | `exact` | Relaunched with its original command line |
| B | `launch only` | Started fresh, no arguments to replay |
| C | `reopens the document` | The files it had open are opened by the shell |
| D | `cannot restore` | Elevated, or unsafe to launch. Listed, never started |

---

## The review window

Nothing is restored without you seeing it first. On the next sign-in you get one
window, it follows your system light or dark theme, and everything in it is a checkbox.

```
 Session Restore                                              [_][□][X]
 ---------------------------------------------------------------------
  Restore your session?                                   Select none
  Captured just now. 5 apps, 6 tabs.
 ---------------------------------------------------------------------
  APPLICATIONS
  [x] Code            1 window                                   exact
  [x] Discord         1 window       launch only - no saved arguments
  [x] WindowsTerminal 3 windows                            launch only
  [x] Notepad         notes.txt - 2 unsaved      reopens the document

  BROWSER WINDOWS
  [x] Edge            4 tabs
      [x] Example Domain         example.com
      [x] Example Domains        www.iana.org/help/example-domains
      [x] Rust Documentation     doc.rust-lang.org/book/
      [ ] Some tab you are done with

  [ ] 2 private windows (2 tabs)       expires in 23h        [ Show ]
      Stored encrypted on this computer, never uploaded, and deleted
      automatically. Not shown until you ask.
 ---------------------------------------------------------------------
  [ Restore selected (10) ]  [ Not now ]           Never ask again
 ---------------------------------------------------------------------
```

Things worth knowing about it:

- **Tabs are named, not counted.** You can bring back yesterday's research without the
  twenty tabs you were finished with.
- **Unticking every tab in a window unticks the window**, because restoring a window
  with no tabs would just open an empty one.
- **Private windows start unticked and unrevealed.** The URLs are not even in the page
  until you press Show. If you are screen sharing after a reboot, they are not on your
  screen.
- **Unsaved documents are counted, never named.** In Windows 11 Notepad the title of an
  unsaved note is its first line of content, so naming it would put the content of a
  private note into a list. It says `2 unsaved` instead.
- **Dismissing the window decides nothing.** It is not consent, and it is not a refusal
  you have to undo later.
- **Escape is Not now, Enter is Restore.**

---

## How it works

Three pieces, one local database.

```
   Chrome / Edge / Firefox
            |
      [ extension ]                    watches tabs and windows
            |  native messaging (stdio)
       [ relay.exe ]                   one thin child process per browser
            |  named pipe, locked to your user SID
       [ sr-agent ]                    watches apps and windows, owns restore
            |
      sessions.db (SQLite, WAL)        the only place anything is stored
```

The browser half and the desktop half are separate because only the extension can see
tabs and only a native process can see windows. The relay exists because browsers will
only speak to a child process they spawn, and that child should not be the thing
holding your database open.

### Why it survives a power cut

Chrome's MV3 service worker gets no shutdown event, so there is no moment at which the
session can be "saved". Instead it is journaled continuously, in three tiers:

| Tier | Trigger | Staleness | Role |
|---|---|---|---|
| T0 | Tab and window events, debounced 2s | about 2s | Normal operation |
| T1 | Timer every 60s, both halves | 60s worst case | Heals anything T0 missed |
| T2 | `WM_QUERYENDSESSION`, 2s budget | best effort | A bonus, never relied on |

**Worst case data loss is one T1 interval even on a hard power cut.** T2 is an
optimization. If it never ran, the design still holds.

---

## Install

Windows 11.

### From a release build

```powershell
.\scripts\package.ps1          # builds everything into dist.\dist\SessionRestore-0.1.0\sr-setup.exe
```

The installer is per-user and **never asks for administrator**, because nothing in
Session Restore uses one. It copies itself to
`%LOCALAPPDATA%\Programs\SessionRestore`, registers the browsers and the logon task,
adds a Start Menu entry and an Apps and Features entry, then starts the agent, which
walks you through the rest.

To remove it, use Apps and Features, or `sr-setup.exe --uninstall`. Captured sessions
are kept unless you add `--purge`.

`--silent` suppresses the dialogs, for unattended deployment.

### From source

### 1. Prerequisites

```powershell
winget install --id Rustlang.Rustup -e
winget install --id BrechtSanders.WinLibs.POSIX.MSVCRT -e   # MinGW for the GNU toolchain
winget install --id OpenJS.NodeJS.LTS -e                    # Node 22+ for the extension
```

Open a new terminal afterwards so the updated `PATH` takes effect. Full notes,
including how to use MSVC instead, are in [CONTRIBUTING.md](CONTRIBUTING.md).

### 2. Build

```powershell
cd agent
cargo build --release

cd ..\extension
npm install
npm run build
```

### 3. Load the extension

**Chrome and Edge:** open `chrome://extensions` or `edge://extensions`, turn on
Developer mode, choose Load unpacked, and select `extension\dist\chrome`. Copy the
extension ID it shows you.

**Firefox:** open `about:debugging#/runtime/this-firefox`, choose Load Temporary
Add-on, and select `extension\dist\firefox\manifest.json`.

> Firefox temporary add-ons are removed when Firefox restarts. Until the add-on is
> signed, reload it each session.

The extension has to stay enabled in each browser you want covered. It is the only
thing that can see tabs, so a browser without it still has its applications and window
geometry restored, but no tabs. There is nothing to turn on day to day: once it is
loaded and enabled, it connects on its own every time the browser starts.

### 4. Register the agent

```powershell
.\agent\target\release\sr-agent.exe --install --chrome-id=<ID from step 3>
```

That writes the native messaging manifests for all three browsers and creates a logon
scheduled task. Confirm it:

```powershell
sr-agent --status
```

```
Registered:      yes
Starts at logon: yes
Tabs tracked:    7 in 6 windows
Private capture: off
Restore mode:    ask
```

### 5. Private windows, only if you want them

Off by default, and turning it on takes two deliberate steps because one is not enough
to be meaningful.

1. Enable `capture_private_windows` in the agent.
2. Grant the browser permission: in Chrome or Edge, Extensions, Session Restore,
   Details, Allow in Incognito or Allow in InPrivate. In Firefox, `about:addons`,
   Session Restore, Run in Private Windows, Allow.

Without step 2 the extension reports `incognito_access=false` and captures nothing,
whatever step 1 says.

---

## Daily use

**It is a background process, and you do not open it.** There is no main window.

Here is the whole loop:

1. **You sign in.** The scheduled task starts `sr-agent`. A tray icon appears and that
   is the only sign it is running.
2. **It watches, quietly.** Applications, window positions, monitors, and every browser
   tab, written to SQLite as you work. No prompts, no interruptions.
3. **Your machine goes down.** Shutdown, reboot, crash, dead battery. It does not
   matter which, because nothing needed to happen at shutdown for the session to be on
   disk already.
4. **You sign back in.** The agent snapshots the session it had recorded *before* any
   browser reconnects and starts wiping it, then shows you the review window.
5. **You choose.** Tick what you want, press Restore. Applications launch, windows go
   back where they were, browsers open and fill with their tabs. If a browser was not
   running, the restore starts it for you, in the profile it was captured in.
6. **You changed your mind.** `sr-agent --undo` puts the windows back the way they were
   immediately before the restore.

The tray menu covers the rest:

| Menu item | What it does |
|---|---|
| Restore last session | Opens the review window again, any time |
| Capture now | Forces a capture instead of waiting for the next 60s pass |
| Pause capture | Stops recording until you turn it back on |
| Open data folder | Opens `%LOCALAPPDATA%\SessionRestore` |
| Quit Session Restore | Stops the agent for this session |

### If you quit it, how do you start it again

Search Start for **Session Restore** and press Enter. `--install` puts a shortcut in
your Start Menu for exactly this, because the app has no main window of its own and
would otherwise be findable only by hunting down the executable.

It also comes back on its own at your next sign-in, since the logon task starts it
whether or not you quit.

If you prefer the command line:

```powershell
schtasks /Run /TN "SessionRestore\Agent"
```

### Restore modes

| Mode | Behaviour |
|---|---|
| `ask` (default) | Shows the review window and restores only what you tick |
| `auto` | Restores everything at sign-in without asking |
| `off` | Captures nothing and restores nothing |

---

## What it recovers, and what it does not

This is the part most worth being clear about, because the obvious guess is wrong.

### If you accidentally close a tab or a browser window

**Session Restore will not bring it back, and that is by design.** It records what is
open *now*. Close a tab and it is removed from the store within a couple of seconds,
because a tool that reopened tabs you deliberately closed would be worse than useless.

Use your browser instead. It is better at this:

| Situation | What to press |
|---|---|
| Closed a tab | `Ctrl` + `Shift` + `T`, repeatedly, to walk back through recent closes |
| Closed a window | `Ctrl` + `Shift` + `T` reopens the whole window with its tabs |
| Closed a browser | Reopen it, then History, Recently closed |

Session Restore covers the case your browser cannot: the machine went away, and the
applications and window layout went with it. The two are complements, not substitutes.

### If you accidentally close an application

Same answer, with one exception. The application is gone from live state within about
a minute. But if you have already restored once this session, `sr-agent --undo` returns
to the snapshot taken immediately before that restore, and if the application was open
then, it comes back.

### Known limits

- **A browser that is not running cannot be filled with tabs** until it starts. The
  restore starts it for you, but its extension needs a second or two to connect.
- **An application already open is placed, not restarted.** If it had three windows and
  one is open, the other two do not come back. A window has no command line, the
  process does, so nothing recorded says how to recreate window two.
- **Documents are found through window titles**, so a document sitting in a background
  tab of a tabbed application is invisible to capture.
- **Window z-order and focus are not restored.**
- **Virtual desktop membership is not captured.** The public API does not expose enough
  to do it honestly.

---

## Privacy

The short version: it is a local tool, nothing is uploaded, and the private-window
feature is built so that the strong claim about it is checkable rather than promised.

- **Everything lives in one file**, `%LOCALAPPDATA%\SessionRestore\sessions.db`. There
  is no server and no account.
- **Private and incognito windows are off by default** and need two separate opt-ins.
- **When on, private tabs are encrypted** with AES-256-GCM under a key wrapped by
  Windows DPAPI, stored in their own table with no plaintext columns at all. Not the
  URL, not the title.
- **They expire.** Default 24 hours, then hard deleted, with `secure_delete` and
  `VACUUM` so the pages are not left in the freelist.
- **They are structurally barred from any future cloud sync.** The sync layer accepts a
  `NormalTab`, and a private tab has no way to become one. That is a compile error, not
  a runtime check somebody forgets to write in two years.
- **Browser window titles are never stored.** A browser window's title is the current
  page title, which would route private page titles straight around the encrypted path.
- **Unsaved document titles are never stored**, for the reason given further up.
- **Command lines are redacted** for tokens, passwords and signed URLs before storage.

There is one test that exists to make the central claim falsifiable:
`agent/crates/sr-agent/tests/privacy.rs` ingests a known private URL, then scans every
byte of every file the agent wrote for it. It is a release blocker, never a flake. The
same scan has been run by hand against real private windows in all three browsers, with
a control to prove the scan works: it finds the plaintext URLs of normal tabs, and
finds zero traces of private ones.

Read [06-privacy-security.md](docs/06-privacy-security.md) for the threat model and
[ADR-0004](docs/adr/0004-incognito-posture.md) for why the posture is opt-in and
expiring rather than simply refusing to capture private windows at all.

---

## Command reference

```
sr-agent                      Run the agent. This is what the logon task starts
sr-agent --install [IDS]      Register native messaging hosts, the logon task,
                              and a Start Menu shortcut
    --chrome-id=<ID>            Chrome or Edge extension ID, repeatable
    --unpacked=<DIR>            Work out the ID for an unpacked extension directory
    --firefox-id=<ID>           Firefox add-on ID
sr-agent --uninstall          Remove registrations, the task, and captured sessions
    --keep-data                 Leave the captured sessions in place
sr-agent --status             Registration, counts, settings, recent snapshots
sr-agent --capture            Run one capture pass and print what it saw
sr-agent --restore-apps       Restore applications from the newest snapshot
    --dry-run                   Decide everything, start nothing
    --snapshot=<ID>             Restore a specific snapshot
sr-agent --undo               Return to the session open before the last restore
sr-agent --documents          Show how window titles resolve to document paths

SR_LOG=debug                  Environment variable for verbose logging
```

---

## Project layout

```
agent/
  crates/
    sr-agent/     The agent: capture, storage, restore, tray, review window
    sr-relay/     The thin stdio to named-pipe relay the browser spawns
    sr-proto/     Wire types and framing, shared by agent and relay
    sr-ipc/       Named pipe transport with the SID-scoped ACL
extension/
  src/
    background/   Service worker: tab events, reconcile, outbox, port
    shared/       Protocol types, generated from the schema
    pages/        Options, and the restore progress page
schema/
  protocol.schema.json    Single source of truth for the wire protocol
docs/             Architecture, data model, capture, restore, privacy, ADRs
```

The protocol schema generates the TypeScript types, so the two halves cannot drift.
Never edit the generated file.

---

## Documentation

| Doc | What it covers |
|---|---|
| [01-architecture.md](docs/01-architecture.md) | Components, process model, why the pieces are split this way |
| [02-data-model.md](docs/02-data-model.md) | SQLite schema, journal against snapshot, retention |
| [03-capture.md](docs/03-capture.md) | What gets captured, when, and how staleness is bounded |
| [04-restore.md](docs/04-restore.md) | Restore algorithm, fidelity tiers, ordering, failure handling |
| [05-ipc-protocol.md](docs/05-ipc-protocol.md) | The extension to agent wire protocol |
| [06-privacy-security.md](docs/06-privacy-security.md) | Threat model, encryption, incognito posture |
| [07-extension.md](docs/07-extension.md) | Extension spec, Chrome, Edge and Firefox differences |
| [08-agent.md](docs/08-agent.md) | Windows agent internals |
| [09-roadmap.md](docs/09-roadmap.md) | Milestones, what is verified by running it, known gaps |
| [10-distribution.md](docs/10-distribution.md) | Packaging, the installer, signing, store submission |
| [adr/](docs/adr/) | Decision records, the reasoning behind the contested choices |

Six decisions are written up as ADRs because they were the contested ones: a logon
agent rather than a Windows Service, native messaging rather than a localhost port, no
Playwright or CDP, the incognito posture, Rust for the agent, and the GNU toolchain.

---

## Status

Working and verified by running it, not only by tests:

- A full reboot cycle on Chrome, Edge and Firefox. Applications relaunch into their old
  positions, browsers get their missing tabs back, nothing is duplicated.
- Window placement round-trips exactly, including across a changed monitor layout.
- Private windows captured for real in all three browsers, with the privacy scan run
  against that data with a control.
- The review window driven end to end, with per-tab selections honoured across the wire.
- `--undo` re-places the previous windows and launches nothing that is already open.

Not done:

- **No code signing.** The installer and binaries are unsigned, so SmartScreen will
  warn on any machine that did not build them. A certificate is a purchase, not a build
  step; `package.ps1 -Sign` is wired up and waiting for one. See
  [10-distribution.md](docs/10-distribution.md).
- Firefox add-on is not signed, so it is a temporary add-on for now.
- **The extension is not on any store yet**, so it has to be loaded unpacked. An
  installer cannot install it for you: Chrome and Edge removed silent external
  extension installs on Windows, and the only supported path for a consumer app is a
  store listing the user adds with one click. Onboarding can open those pages and
  detect when each browser connects, but the click is always the user's.
- Cloud backup, cross-device restore, scroll position and macOS support are all
  deliberately out of scope for v1. The reasoning is in
  [09-roadmap.md](docs/09-roadmap.md).

---

## Development

```powershell
cd agent
cargo build
cargo test
cargo clippy --all-targets -- -D warnings

cd ..\extension
npm install
npm run check
```

`npm run gen:proto` regenerates the protocol types from `schema/protocol.schema.json`.
It runs as part of `build` and `typecheck`, so you should not need it directly.

See [CONTRIBUTING.md](CONTRIBUTING.md) for the full setup, including the MSVC path and
which tests are release blockers.
