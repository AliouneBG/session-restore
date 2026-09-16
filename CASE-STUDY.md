# Session Restore: an engineering case study

This document exists because the README describes what the software does, and that
turns out to undersell it. What follows is the part that is invisible from the outside:
the problem that has no supported solution, the platform behaviour that had to be
reverse engineered, the failures that were found only by running the thing on a real
machine, and the reasoning behind the decisions that shaped it.

It is written to be readable by someone who does not write Windows systems code, and
specific enough to be checked by someone who does. Every claim in it points at a file,
an API, a test or a number.

---

## Contents

- [In one paragraph](#in-one-paragraph)
- [The problem that has no supported solution](#the-problem-that-has-no-supported-solution)
- [Why this class of software is unusually hard to get right](#why-this-class-of-software-is-unusually-hard-to-get-right)
- [Architecture: four processes, three trust boundaries](#architecture-four-processes-three-trust-boundaries)
- [Fourteen problems that only appear once you build it](#fourteen-problems-that-only-appear-once-you-build-it)
- [Privacy as a structural property, not a feature](#privacy-as-a-structural-property-not-a-feature)
- [How correctness was actually established](#how-correctness-was-actually-established)
- [A catalogue of real defects, and what each one taught](#a-catalogue-of-real-defects-and-what-each-one-taught)
- [Judgment calls](#judgment-calls)
- [What is deliberately unfinished](#what-is-deliberately-unfinished)
- [By the numbers](#by-the-numbers)
- [Verify the claims yourself in fifteen minutes](#verify-the-claims-yourself-in-fifteen-minutes)
- [Where this sits](#where-this-sits)

---

## In one paragraph

Session Restore puts a Windows 11 desktop back the way it was after the machine goes
away: the applications that were open, where their windows sat across however many
monitors, and every tab in every browser window, including private and incognito
windows if the user asks for them. It is built for the case where nobody got to save
anything, which is the case that actually happens: a forced update reboot, a flat
battery, a hard lock. The session on disk is never more than about sixty seconds behind
the screen, so a power cut costs a minute of context instead of an afternoon of it.

That description sounds like a utility. The reason it is not is that **the platform
provides no supported way to do it**, and most of the engineering is in the gap.

---

## The problem that has no supported solution

The obvious design is: notice the browser is closing, write the tabs to disk, done.

That design is impossible in a modern browser, and the reason is worth stating
precisely because it is the hinge the whole architecture turns on.

Chrome extensions run under Manifest V3, where the background script is a **service
worker**. Service workers are deliberately ephemeral. Chrome evicts them after roughly
thirty seconds of inactivity and restarts them on the next event. The lifecycle event
that used to mean "you are about to be shut down", `chrome.runtime.onSuspend`, is **not
implemented for service workers**. There is no callback. There is no last-gasp hook.
The extension simply stops existing, with no notice, both when Chrome idles it and when
Chrome exits.

The consequence: there is no moment at which a browser extension can save your session.
Not "it is hard", not "it is unreliable". The event does not exist.

Windows has the same shape of problem on the desktop side. `WM_QUERYENDSESSION` exists,
but it arrives with a strict time budget, Windows will kill a process that dawdles, and
a power cut or a hard lock delivers nothing at all.

So the design inverts. Instead of saving at the end, the system **journals
continuously**, in three tiers, each of which exists because the one below it is not
trustworthy:

| Tier | Trigger | Staleness | What it is for |
|---|---|---|---|
| T0 | Tab and window events, debounced 2 seconds | about 2 seconds | Normal operation |
| T1 | A 60 second timer, on both halves | 60 seconds worst case | Heals everything T0 missed while the service worker was dead |
| T2 | `WM_QUERYENDSESSION`, 2 second budget | best effort | A bonus, explicitly never relied upon |

The load-bearing property, and the one every later decision preserves, is this:
**worst-case data loss is one T1 interval, even on a hard power cut.** T2 is an
optimisation. If it never fired, the design still holds.

There is a second-order consequence that shapes the code. Because T1 is authoritative,
it must be able to *reap*: tabs that no longer exist have to be deleted. That reap is
correct and necessary, and it is also the thing that will happily delete the session you
are about to restore, because a browser that has just restarted presents entirely new
window ids. The agent therefore snapshots the previous session **before the pipe server
starts accepting connections**, so no extension can connect and begin reaping until the
session it would destroy is already safe. Ordering is the whole defence, and it is a
single comment in `main.rs` guarding a bug that would otherwise be intermittent,
data-destroying, and nearly impossible to reproduce.

---

## Why this class of software is unusually hard to get right

Most application projects sit on a platform that wants to help. There are frameworks,
documented happy paths, and a community that has hit your problem before. The
difficulty is real but it is mostly composition, product sense and taste.

This project sits on a platform that actively does not want to help, in four specific
ways:

**1. The browser refuses to tell you when it is closing.** Covered above. This is not
an oversight to be worked around; it is a deliberate platform decision, and the
architecture had to be designed around the absence.

**2. The browser refuses to let you install the extension.** Chrome and Edge removed
silent external extension installation on Windows, deliberately, because malware abused
it. There is no privileged path for a legitimate application. This is why the onboarding
flow cannot be a progress bar and is instead a thing that opens the right page and then
*detects* the extension connecting. The constraint shaped the user experience, not the
other way round.

**3. The operating system hands you data that lies.** `EnumWindows` returns windows that
are not real. Store apps are hosted inside `ApplicationFrameHost.exe`, so the process
that owns the window is not the process that is the application. Process command lines
have no supported API and must be read out of another process's memory. Monitor indices
change when you unplug a dock.

**4. Failure is silent and delayed.** This is the deepest one. A session capture that is
quietly thirty percent wrong looks exactly like one that is perfect. You find out on the
single day you actually need it, after the machine has already died, when there is
nothing left to compare against. There is no user who reports the bug, because by
definition the evidence is gone.

That fourth property inverts the normal economics of testing. In a CRUD application you
can ship, watch, and fix. Here the feedback loop is broken by construction, so
correctness has to be established *before* it matters, deliberately and adversarially.
That is why this repository has a verification discipline that would be excessive for
almost any other kind of project, and why the section on it below is longer than the
section on features.

---

## Architecture: four processes, three trust boundaries

```
  Chrome / Edge / Firefox
           |
     [ extension ]              MV3 service worker. Sees tabs. Sees nothing else.
           |                    native messaging, stdio, 4-byte LE length + UTF-8 JSON
     [ sr-relay.exe ]           One per browser. Spawned by the browser, not by us.
           |                    named pipe, ACL scoped to the user's SID
     [ sr-agent.exe ]           Win32. Sees windows, processes, monitors. Owns restore.
           |
   sessions.db (SQLite, WAL)    The only place anything is stored.
```

Every boundary is there for a reason, and each reason is written up as an architecture
decision record.

**The extension exists** because it is the only thing that can see tabs. No external
process can enumerate a browser's tabs without automation interfaces that would be worse
in every respect, which is the subject of [ADR-0003](docs/adr/0003-no-playwright-or-cdp.md):
driving the browser through Playwright or the Chrome DevTools Protocol would require
launching it with a debugging port, which is both a security hole and a thing that does
not survive the user opening the browser normally.

**The relay exists** because browsers only speak native messaging to a process *they*
spawn, and the thing the browser spawns should not be the thing holding your database
open. The relay is deliberately tiny: it links no database, no crypto, and nothing that
touches disk. It is a pipe with legs.

**A named pipe, not a localhost socket** ([ADR-0002](docs/adr/0002-native-messaging-over-localhost.md)):
a TCP port on loopback is reachable by every process on the machine, including every
other browser tab running Javascript. A named pipe takes a security descriptor. This one
is built with `D:P(A;;GA;;;<the user's SID>)` and `FILE_FLAG_FIRST_PIPE_INSTANCE`, so a
second process cannot squat the name, and no other user on the machine can connect.

**A logon agent, not a Windows Service** ([ADR-0001](docs/adr/0001-user-agent-not-windows-service.md)):
a service runs in Session 0, which is isolated from the desktop precisely so that
services cannot see or touch user windows. A service therefore cannot do the one thing
this application exists to do. Everything downstream follows from that single fact: the
data directory is per-user, the registry keys are under HKCU, the scheduled task is
per-user, and the installer never needs administrator.

**One schema, two languages.** `schema/protocol.schema.json` is 566 lines of JSON Schema
and is the single source of truth for the wire protocol. The TypeScript types are
generated from it; the Rust types are hand-written against it and tested against the same
fixtures. The two halves cannot drift without a build failure.

---

## Fourteen problems that only appear once you build it

This is the section that does not show up in a feature list. Each of these cost real
debugging time, and each one is now a comment in the code explaining why the obvious
thing is wrong.

**1. Synchronous named pipe I/O deadlocks a reader and a writer.** Windows serialises
synchronous operations per *file object*, not per handle. `DuplicateHandle` gives you a
second handle to the same file object. So the natural design of cloning the pipe handle
to get a writer, then reading on one and writing on the other, deadlocks: the write waits
for the read to finish. It hung on a four byte frame. Diagnosing it meant writing an
isolated .NET client to prove the deadlock was in the transport and not in the protocol.
The fix is `FILE_FLAG_OVERLAPPED` plus explicit `await_overlapped()`, which is mandatory
here and looks like premature complexity until you know why.

**2. `FlushFileBuffers` on a named pipe is not a flush.** On a pipe it does not return
until the reading process has consumed everything in the buffer. Calling it from the
standard `Write::flush` implementation deadlocked the relay against the agent. The fix
is a `flush()` that deliberately does nothing, with a comment explaining why, because a
future reader will otherwise "fix" it.

**3. `FILE_GENERIC_WRITE` is the wrong constant for a pipe.** It includes
`FILE_CREATE_PIPE_INSTANCE`, which means something entirely different in this context.
The correct constants are the `GENERIC_*` family. The symptom is an access denied that
points nowhere near the cause.

**4. Command lines have no supported API.** Restoring an application exactly requires the
arguments it was started with. Windows does not expose them. The supported answer is
ETW, which is heavyweight. The practical answer is reading the target process's Process
Environment Block through a hand-declared `NtQueryInformationProcess`, an undocumented
call, with careful pointer-size handling.

**5. A quarter of the windows Windows reports do not exist.** Suspended Store apps keep
window handles alive. They are visible to `EnumWindows` and invisible to the user. The
filter is `DWMWA_CLOAKED` through `DwmGetWindowAttribute`. Without it, the capture is
full of phantom entries and a restore opens applications the user closed days ago.

**6. The process that owns a Store app's window is not the Store app.** UWP windows are
hosted by `ApplicationFrameHost.exe`. Identifying the application means walking to the
hosted child window and resolving *its* process. Before that was handled, no Store
application was capturable at all, and the bug presented as "Calculator never gets
restored" rather than as anything about window hosting.

**7. `GetWindowRect` loses the state you care about.** A maximised window's rect is its
maximised rect, so restoring it produces a window that is the size of the screen but not
actually maximised, and which snaps to the wrong size the moment you touch it.
`GetWindowPlacement` returns the restored rect *and* the show command, which is what
round-trips correctly.

**8. Monitor indices are not stable.** Unplug a dock, plug it back in, and index 1 is a
different display. Window positions are stored against a derived display key and remapped
at restore time onto whatever monitors exist now, including across a DPI change. The
verification for this was a measured round trip: a window at 116,129 sized 689x489 was
moved to 40,40 sized 400x300, and restore put it back at exactly 116,129 sized 689x489.

**9. `SetWinEventHook` will load your DLL into every process on the desktop** if you ask
for an in-context hook. Out-of-context is the only acceptable mode, and it changes the
threading model of everything that consumes the events.

**10. A window title can be its own content.** Windows 11 Notepad puts the first line of
an unsaved note into the title bar. Storing window titles therefore stores the text of
private notes. This was found on a real machine, in a real note. Titles are now kept only
when they name a file, which is both the safe case and the useful one.

**11. Browser window titles are page titles.** Which means storing them routes private
browsing page titles straight around the entire encrypted path. Browser windows are
excluded from title storage by *process identity*, never by pattern matching the title
for markers like "Private Browsing", because those are localised and trivially defeated.

**12. Windows will not overwrite a running executable, but it will rename one.** This is
the standard trick for in-place upgrades, and the reason the installer succeeds where the
naive copy fails. The renamed file keeps running until its last handle closes.

**13. The processes holding your files open are not the ones you expect.** Upgrading
failed on `sr-relay.exe` being in use. The relays are children of the *browsers*, not of
the agent, so stopping the agent does nothing about them. They hold the file open
indefinitely and the browsers respawn them on the next connect.

**14. A GUI subsystem process has no console, and a console subsystem process always has
one.** The agent is both a background application and a command line tool. Built for the
console subsystem, launching it from the Start Menu opens a terminal full of log lines.
Built for the windows subsystem, `--status` prints into the void. The resolution is the
windows subsystem plus `AttachConsole(ATTACH_PARENT_PROCESS)` when the arguments indicate
a human typed a command.

---

## Privacy as a structural property, not a feature

The product captures browser tabs, including private windows. That is a serious claim to
make about someone's machine, and "we promise to be careful" is not an engineering
answer. The design tries to make the central privacy claim **checkable** rather than
promised.

### One door, on purpose

Every path by which tab data can enter storage goes through a single function,
`ingest_tab` in `agent/crates/sr-agent/src/ingest.rs`. There is deliberately no second
one. The value of a chokepoint is not that it is tidy. It is that verifying "private URLs
never reach the plaintext tables, the journal, or the sync queue" becomes *reading one
function* rather than auditing a codebase.

### Three redundant layers, on purpose

1. **At the source.** When private capture is off, the extension is told so in the
   handshake and never sends private events at all. The cheapest enforcement point is
   the earliest one.
2. **At the chokepoint.** One routing decision, in one place.
3. **In the type system.** This is the interesting one. The future cloud sync layer
   accepts a `NormalTab`. `NormalTab` has exactly one constructor, `from_delta`, which
   returns `None` for a private tab. There is no other way to build one. **Cloud
   exclusion of private tabs is therefore a compile error, not a runtime check that
   somebody forgets to write in two years.**

### What is actually stored

Private tabs live in their own table with **no plaintext columns at all**. Not the URL,
not the title, not the favicon. The payload is sealed with AES-256-GCM under a data
encryption key that is itself wrapped by Windows DPAPI, so the key is bound to the user
account. Rows carry a TTL, default 24 hours, after which they are hard deleted with
`secure_delete` and `VACUUM`, because a plain `DELETE` leaves the content sitting in
SQLite freelist pages.

Command lines are redacted before storage for tokens, passwords, API keys and signed
URLs, and a redacted argument is replaced with a length-preserving sentinel so that
restore can tell the difference between "no arguments" and "arguments we refused to
keep", and drop to a lower fidelity tier rather than replaying a secret.

### The falsifiable test

`agent/crates/sr-agent/tests/privacy.rs` ingests a known private URL and then scans
**every byte of every file the agent wrote** looking for it. It is a release blocker and
it is never treated as flaky.

The same scan was run by hand against real private windows in all three browsers, and
this is the methodological point that matters: **it was run with a control.** A scan that
finds nothing proves nothing unless you have first shown it can find something. So the
scan also looks for the URLs of ordinary tabs. The result:

```
=== CONTROL (must be found, proves the scan works) ===
  FOUND rust-lang.org: ['sessions.db', 'sessions.db-wal']
  FOUND example.com:   ['sessions.db', 'sessions.db-wal']

=== PRIVATE (must be found nowhere) ===
  clean: zero plaintext private traces
```

### Consent design

Private capture is off by default and requires **two independent opt-ins**: one in the
application, and one in the browser itself. The application switch alone does nothing,
and the settings window says so in plain language rather than presenting a toggle that
silently achieves nothing. In the review window, private tabs are not merely hidden:
their URLs are **not in the page's data at all** until the user presses Show, on the
reasoning that somebody screen sharing after a reboot should not have their private tabs
rendered on screen by default.

---

## How correctness was actually established

There are 333 automated tests. That is the less interesting half.

The more interesting half is that this project treats **"covered by a test"** and
**"verified by running it"** as different claims, and keeps an explicit, honest list of
the second kind in [docs/09-roadmap.md](docs/09-roadmap.md). That list exists because of
the silent-failure property described earlier: for this class of software, a green test
suite is genuinely weak evidence.

Things that were verified by actually doing them, not by asserting them:

- A full reboot cycle on Chrome, Edge and Firefox: applications relaunched into their old
  positions, browsers received exactly their missing tabs, nothing duplicated.
- The specific duplicate-tab case that users will hit: the browser's own "continue where
  you left off" also enabled, so both systems restore at once. Tabs already reopened by
  the browser come back marked `placed` rather than `launched`, which is the observable
  difference between correct and nearly correct.
- Window placement measured to the pixel across a move and a restore.
- The privacy scan with a control, against genuinely private browsing data.
- The review window driven end to end through UI Automation, with three applications and
  one tab deliberately unticked, confirming that exactly the ticked set came back and the
  unticked tab did not.
- A browser that was closed at capture time being launched by the restore, connecting,
  and being handed its offer.
- `--undo` on a machine where everything was still open: 8 windows re-placed, 0
  applications launched, no duplicate process of anything.
- Install, upgrade over a running copy with live relay processes, uninstall, reinstall.

An example of the discipline: at one point the roadmap contained a section titled *"The
one manual verification left"*, stating plainly that private-window capture had never
been exercised with a real private window, and explaining exactly why it could not be
automated (Chromium protects the setting with an HMAC over `Secure Preferences`, and
Firefox's `extensions.allowPrivateBrowsingByDefault` does not apply to temporarily
installed add-ons). The gap was documented as a gap, with reproduction steps, rather than
quietly rounded up to "done". It was later closed by doing it by hand, and the section
was rewritten to say so.

---

## A catalogue of real defects, and what each one taught

These were found by running the software, mostly after the test suite was already green.
They are listed because the list is more informative about the engineering than a feature
tour would be.

**The review window's restore could not be undone.** Restoring from the review window,
which is the default path a user takes, never opened a `restore_run` record. So `--undo`
reached back to whatever ran last and restored an *older* session over the top of the
current one. The one restore path users actually use was the one undo could not reverse.

**Every run took its own undo point, which poisoned it.** One restore is several runs:
the applications first, then each browser as it connects. Each was taking its own
"before" snapshot. `--undo` reads the most recent run, so it read a snapshot captured
*after* the applications had already been launched. Undo returned you to the state the
restore had just created. Fixed by having all runs in one episode share a single undo
point, verified live: four runs, all recording `undo_snapshot_id = 6`, exactly one
snapshot created.

**Nothing ever launched a browser.** A restore offer waits in memory for a browser
extension to connect, and nothing made one connect. After a reboot no browser is running,
which is the only situation the product exists for. So ticking a browser window in the
review window silently meant "restore these tabs the next time you happen to open Edge".
The checkbox said one thing and did another, with no way for the user to notice.

**The browser raced the review window it was waiting on.** A browser that connected while
the review window was still open was handed the *entire* stored session, because the
offer path had a single way to express "the user has not chosen" and it was the same
value as "there is no review". At logon the browser and the review window start together
and the browser usually wins, so in the common case every checkbox in that window was
bypassed seconds before the user could tick one.

**Restoring duplicated what was already open.** `--undo` restores the applications that
were open before a restore, and they usually still are, so undoing produced a second VS
Code and a second Discord instead of putting anything back.

**Snapshots silently dropped a column.** The snapshot copy listed its columns explicitly.
A column added to the `apps` table later was never added to that list, so every snapshot
lost it. The visible symptom was the review window describing a Notepad window as
"unsaved" when it had recorded the filename seconds earlier. The fix was not just adding
the column: it was adding a test that walks `PRAGMA table_info` for each copied table and
fails if a column is missing from the copy list, so the *next* column cannot repeat it.

**Reinstalling revoked the extension.** The installer re-runs registration with no
extension ids, because it has none to give: an unpacked extension's id is derived from
its path and is only known after a browser has loaded it once. Registration overwrote the
allowlist with an empty one, silently disconnecting a working extension. Nothing would
have reported this except tabs quietly no longer being captured, which is the exact
silent-failure mode the whole project is built to avoid.

**The handshake only happened once.** The extension sent `hello` on service worker start
and never again on reconnect. `hello` is the only message carrying whether the browser
has granted private-window access, so the agent's picture of a browser left open for a
day aged out while the connection stayed perfectly healthy.

**The encryption was bound to the wrong thing.** The additional authenticated data for
private tabs included the snapshot id. Taking a snapshot copies rows to a *new* snapshot
id, so every private row became undecryptable the moment it was copied, which is exactly
what the review window reads. The user-visible symptom was a Show button that appeared to
do nothing.

**The installer hung any script that ran it.** It spawned the agent with its own stdout
inherited, and the agent runs until logout, so anything piping the installer's output
waited forever for a handle that would never close.

---

## Judgment calls

Engineering quality shows up as much in the decisions not to do things.

**Dismissing the review window decides nothing.** Closing it is neither consent nor
refusal. Treating a closed window as consent would restore a session the user never
agreed to; treating it as a permanent refusal would be a decision they did not make
either.

**Unsaved documents are counted, never named.** "2 unsaved" rather than the title,
because in Windows 11 Notepad the title of an unsaved note is its content.

**`--undo` never closes what a restore opened.** Closing applications to undo risks
destroying work done in the meantime, which is a far worse outcome than a few extra
windows being open. The command says so explicitly when it runs.

**An application that is already open is placed, not restarted.** The trade-off is real
and was reasoned through in the commit message: an application with three stored windows
and one open does not get the other two back. Launching it again would not have opened
them either, since almost everything here is single-instance and a second launch just
focuses the first. So the choice is between a missing window and a duplicate process, and
the duplicate is the one the user has to clean up.

**Uninstalling keeps your data unless you say `--purge`.** Deleting somebody's month of
captured sessions because they uninstalled an application is not a decision to make on
their behalf.

**The installer never asks for administrator.** Not because elevation was hard, but
because nothing in the product uses it. Asking would teach the user that this program
needs a privilege it does not have.

**Tier D exists.** Applications that need administrator, or cannot be launched safely,
are modelled as a first-class "cannot restore" outcome and shown in the review window as
such. The system says what it will not do rather than failing quietly.

---

## What is deliberately unfinished

A credible engineering document says where the edges are.

**Not done, and it is a purchase rather than a task:** code signing. The build script
supports it and waits on a certificate. Until then SmartScreen warns on any machine that
did not build the binaries.

**Not done, and it needs accounts:** store listings on the Chrome Web Store, Edge Add-ons
and addons.mozilla.org. Publishing is also what makes the Firefox add-on survive a
restart, and what gives the extensions stable ids.

**Not fixable with the data that exists:** an application with several undifferentiated
windows, one of which is open, does not get the others back. A window has no command
line; the *process* does. Two windows of one process share one command line, so nothing
recorded says how to recreate window two. Closing that gap needs per-application
knowledge, for example VS Code's `--folder-uri`, which is a different project. This is
written up as a data limit rather than a to-do, because that is what it is.

**Known and documented:** window z-order and focus are not restored. Virtual desktop
membership is not captured, because the public API does not expose enough to do it
honestly. Documents are resolved through window titles and the Windows Recent index, so a
document in a background tab of a tabbed application is invisible, and a file that never
touches Recent will not be found. No path is ever guessed.

**Deliberately out of scope for v1, with reasoning:** cloud backup (every privacy claim
gets harder, and it needs end-to-end key management and a conflict model), cross-device
restore (application paths and monitor layouts differ per machine), scroll position and
form state (would need content scripts on every site, a categorically larger permission
ask that would change the store review outcome), and macOS or Linux (the entire watcher
and launcher layer is Win32, though the store, protocol and extension are portable).

---

## By the numbers

| | |
|---|---|
| Rust | 14,082 lines across 42 files, four crates |
| TypeScript | 2,358 lines across 17 files, excluding generated code |
| Interface | 1,450 lines of hand-written HTML and CSS, no framework |
| Protocol schema | 566 lines of JSON Schema, generating the TypeScript types |
| Documentation | 2,130 lines across 10 numbered documents and 6 architecture decision records |
| Tests | 333 total: 265 Rust, 68 TypeScript |
| Database | 16 tables, SQLite in WAL mode |
| Win32 surface | 46 distinct APIs, about 50 `unsafe` sites, each with a safety comment |
| Commits | 32, each with a message explaining the reasoning, not the diff |

The documentation is worth a second look at that ratio. It is roughly one line of prose
for every seven lines of code, and it is not generated API documentation. It is ten
numbered design documents and six architecture decision records covering the contested
choices: why a logon agent instead of a Windows Service, why native messaging instead of
a localhost port, why not Playwright or the DevTools Protocol, what posture to take on
incognito, why Rust, and why the GNU toolchain for now.

---

## Verify the claims yourself in fifteen minutes

Nothing above needs to be taken on trust.

**The hard parts are commented where they happen.** Open
`agent/crates/sr-ipc/src/lib.rs` and read the comment on `flush()`, which explains why a
flush that does nothing is the correct implementation for a named pipe. Open
`agent/crates/sr-agent/src/ingest.rs` and read the module header, which states the
privacy invariant and names the three layers that enforce it. Open
`agent/crates/sr-agent/src/store/crypto.rs` and read the comment on `aad()`, which
documents a real bug and why the obvious binding was wrong.

**The privacy claim is executable.**

```powershell
cd agent
cargo test -p sr-agent --test privacy
```

**The reasoning is in the commit log.** `git log` reads as a narrative of what was tried,
what broke, and why the fix is shaped the way it is. Commit messages explain the defect
and the trade-off rather than restating the diff.

**The honest list is in the roadmap.** [docs/09-roadmap.md](docs/09-roadmap.md) separates
what is verified by running it from what is covered by tests, and keeps a known-gaps
section that includes things that are not going to be fixed and why.

---

## Where this sits

It is easy to look at a list of personal projects and flatten them: a full-stack
application, a machine learning application, a desktop utility. All of them are real
work, and the difference is not effort or polish.

The difference is the *kind* of problem.

A full-stack application is a problem of composition and product judgment on a platform
built to support you. The frameworks want you to succeed, the failure modes are visible,
and when something breaks a user tells you.

This is a systems problem on a platform that provides no supported solution, where:

- the central capability is **impossible as specified** and required redesigning the
  problem, not implementing the answer;
- correctness depends on **undocumented and counterintuitive operating system
  behaviour** that is not in any tutorial and surfaces only as a deadlock or an access
  denied pointing at the wrong place;
- the work spans **four processes, two languages, three trust boundaries and a wire
  protocol**, with a browser extension sandbox at one end and raw Win32 at the other;
- the strongest safety property is enforced by the **type system**, so a class of privacy
  bug cannot be written;
- and the failure mode is **silent and delayed**, so correctness has to be established
  adversarially in advance, because the feedback loop that normally catches mistakes does
  not exist here.

The visible artifact is a tray icon and a window with some checkboxes. That is the point.
The measure of this project is how much had to be understood, designed around and proven
for that window to be able to tell the truth.
