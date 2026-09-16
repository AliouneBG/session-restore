# 09 — Roadmap

## Status (2026-09-16)

| Milestone | State |
|---|---|
| M0 — Walking skeleton | **Done.** Verified against real Edge end to end. |
| M1 — Capture, tabs, T1 reconcile | **Done** for Chrome/Edge. Live tabs land in SQLite with title, order, active flag, pinned state, and window geometry. |
| M2 — Apps and windows | **Done.** Apps, windows, geometry, displays, tiers and redacted command lines land in SQLite. |
| M3 — Restore | **Done.** Browsers get their missing tabs back; applications are relaunched by tier and their windows placed, including across a changed monitor layout or DPI. The review window gates both halves, per application, per browser window and per tab. |
| M4 — T0 deltas / T2 shutdown | **Done.** Event deltas plus a `WM_QUERYENDSESSION` flush bounded at 2s. |
| M5 — Private windows | **Done and verified with real private windows in Chrome, Edge and Firefox.** |
| M6 — Firefox | **Done.** Runs in Firefox, connects, captures. AMO lint clean: 0 errors, 0 warnings, 0 notices. |
| M7 — Polish | **Tray, review window and logon task done.** No MSI/MSIX installer yet, and nothing is signed. |

M0-M3, M6 and most of M7 are real, plus half of M4/M5. **A session survives a reboot
end to end on Chrome, Edge and Firefox**: applications relaunch into their old
positions, browsers get their missing tabs back, and `--install` registers the logon
task so it happens on its own. There is a tray icon and a review window to approve it
with.

What remains is distribution - an installer and code signing - and the deferred pieces
listed below.

### Verified by running it, not just by tests

- Real Edge -> extension -> relay -> agent -> SQLite, with correct titles and ordering
- Closing a tab removes it from the store within seconds
- Two consecutive browser restarts leave exactly one window, no phantoms
- A known private URL never appears in any byte of any file the agent writes
- A full reboot cycle: capture 3 tabs, shut down, restart, reopen the browser with a
  different single tab -> the 2 missing tabs are restored and the one already open is
  left alone, with no duplicates
- Restored tabs store their real URLs rather than the lazy placeholder, so the session
  survives repeated reboots instead of degrading
- A live desktop capture: VS Code, File Explorer, Chrome, Firefox, Calculator and
  Notepad, each with the right tier, env-folded paths, and Store apps carrying the
  AUMID needed to relaunch them
- Not one browser window stored a page title, with private windows open at the time
- Firefox: extension loads, connects, and reconciles on the alarm
- The private-window gate refusing correctly: with `capture_private_windows` on but the
  browser permission absent, the extension reported `incognito_access=false` and
  captured zero private tabs
- The review window renders the real session with per-app checkboxes and honest tiers
- Private windows captured for real in **Chrome, Edge and Firefox**: in each, the
  extension reported `incognito_access=false`, then `true` once the browser permission
  was granted, and one encrypted row appeared per browser
- The privacy scan run against that real data, with a control: it **finds** the
  plaintext normal-tab URLs in `sessions.db` and finds **zero** private ones
- Document restore: Notepad closed, restored from a snapshot, and the document reopened
  (window titled `sr-tier-c-demo.txt - Notepad`)
- Closing Calculator and Notepad, then restoring: both relaunched through
  IApplicationActivationManager
- Window placement round-trip: a window at 116,129 (689x489) was moved to 40,40
  (400x300), and restore put it back at exactly 116,129 (689x489)
- The review window driven end to end through UI Automation, on a session of five
  applications and three browser windows: three applications and one tab unticked,
  Restore pressed, and exactly the ticked set came back
- Per-tab selection honoured across the wire: an Edge window captured with three tabs
  was offered two after one was unticked, and Edge reopened with those two
- A browser the restore needed was started by it: Edge was closed at capture time, the
  restore launched it, its extension connected, and it was handed the offer
- A browser that connected while the review window was open was made to wait, then
  handed the answer - the log shows no offer until Restore was pressed
- Dismissing the review released a waiting browser rather than leaving it waiting
  forever
- One restore, one undo point: four runs (the applications, then Chrome, Edge and
  Firefox as they connected) all recorded `undo_snapshot_id = 6`, and exactly one
  `pre_restore` snapshot existed
- `--undo` on a machine where everything was still open: 8 windows re-placed, 0
  applications launched, and no duplicate process of anything
- No duplicate tabs on re-restore: every tab already open came back marked `placed`
  rather than `launched`
- The private reveal against *copied* rows - the case that was broken: with two private
  windows captured (Chrome and Firefox), Show decrypted both out of a snapshot and
  named them, with the TTL shown
- The privacy scan repeated against that data, control included: it **finds**
  `rust-lang.org` and `example.com` in `sessions.db`, and finds **zero** traces of
  either private tab's URL or title anywhere in the data directory

### Known gaps

- No installer. The agent runs from its build directory, and nothing is code-signed,
  so SmartScreen will warn on another machine.
- On the GNU toolchain the agent needs `WebView2Loader.dll` beside it (the build
  places it there automatically). An MSVC build links it statically and needs no DLL.
- Window z-order and focus are not restored.
- Document resolution needs the file to be in Windows Recent. A document opened in a
  way that never touches Recent will not be found, and no path is ever guessed.
- `--undo` re-places the previous windows but never closes what a restore opened.
  Closing applications to undo risks destroying work done since, which is worse than a
  few extra windows.
- A restore does not put a browser back into the profile it was captured from. It
  starts the browser, which opens whichever profile that browser opens by default.
- An application that is already open is placed but not restarted, so one with three
  stored windows and one open does not get the other two back. Starting it again would
  not have opened them either, and would have left a duplicate process behind.
- Command lines come from a PEB read. ETW (the intended primary source) is not wired
  up, so processes are read one at a time rather than cached as they start.
- Virtual desktop membership is not captured; see [08](08-agent.md) for why the public
  API is not enough.

### Reproducing the private-window verification

This is done - see the verified list above - but it is the one step that cannot be
automated, so here is how to repeat it. The browser-side half of the opt-in resists
scripting by design: Chromium protects the setting with an HMAC over
`Secure Preferences`, and Firefox's `extensions.allowPrivateBrowsingByDefault` does not
apply to temporarily-installed add-ons. That resistance is the feature working - it is
exactly why the permission is meaningful.

1. `sr-agent --status` should say `Private capture: on` (set it in the tray/settings first)
2. Chrome/Edge: extensions page -> Session Restore -> Details -> *Allow in Incognito* /
   *Allow in InPrivate*. Firefox: `about:addons` -> Session Restore ->
   *Run in Private Windows* -> Allow
3. Open a private window, load a page, wait ~60s
4. `sr-agent --status` should show a non-zero `Private tabs` count
5. Confirm nothing leaked: the URL must not appear anywhere in
   `%LOCALAPPDATA%\SessionRestore\sessions.db`

## Build order

The sequence matters. Each milestone is independently useful and de-risks the next.

### M0 — Walking skeleton (1 week)

Agent starts, opens SQLite, writes one hardcoded row, tray icon appears. Relay
registered for Chrome, extension connects, `hello`/`hello_ack` round-trips.

**Done when:** you can see a log line in the agent caused by opening a tab in Chrome.

Nothing here is throwaway, and it proves the riskiest integration (native messaging
registration and the pipe ACL) on day one rather than week six.

### M1 — Capture, tabs only, T1 only (1 week)

No T0 deltas, no T2. Just the 60s `full_state` reconcile, normal tabs only, Chrome only.
Writes to `snapshot_id = 0`.

**Done when:** `SELECT * FROM tabs` matches what is open in Chrome, within 60 seconds,
across a service-worker eviction (force one via `chrome://serviceworker-internals`).

**Build T1 before T0.** T1 alone is a working product; T0 is an optimization on top of
it. The opposite order gives you something that appears to work and silently drifts.

### M2 — Capture, apps and windows (1.5 weeks)

`EnumWindows` + filtering (including the `DWMWA_CLOAKED` check), display enumeration with
stable keys, `GetWindowPlacement`, ETW command lines with PEB fallback, redaction,
restore-tier assignment.

**Done when:** the app/window table matches Alt-Tab, with no phantom UWP entries, across
plug/unplug of an external monitor.

### M3 — Restore (2 weeks)

Review window, the ordered phase pipeline, launching by tier, placement with topology
and DPI mapping, tab injection with the diff-against-current rule, `pre_restore`
snapshot and undo, `restore_items` reporting.

**Done when:** reboot -> click Restore -> apps and normal tabs come back on the right
monitors, with no duplicate tabs when Chrome's own "continue where you left off" is
also enabled. Test that case explicitly; it is the one users will hit.

### M4 — T0 deltas and T2 shutdown (1 week)

Event-driven capture with debounce and the `storage.session` outbox.
`WM_QUERYENDSESSION` with a 2s budget.

**Done when:** a tab opened 3 seconds before a hard power cut (pull the plug on a VM)
is present on restore.

### M5 — Private windows (1.5 weeks)

DPAPI key wrapping, AES-256-GCM with AAD, `tabs_private`, TTL sweeper, `secure_delete`
and `VACUUM`, the two-stage opt-in, collapsed review UI, delete-on-restore and
delete-on-last-private-window-closed.

**Done when:** the CI test asserting no private URL ever appears in `tabs`, `journal`,
or any log file passes — and a hex dump of `sessions.db` after a TTL expiry contains no
trace of the URL.

Ship M5 last among the capture features, not first. It is the highest-risk surface and
it benefits from the rest being stable.

### M6 — Edge and Firefox (1.5 weeks)

Edge is mostly registry keys and a store listing. Firefox is the real work: second
manifest, event page instead of service worker, `discarded: true` instead of the
placeholder page, no tab groups, `allowed_extensions` manifest key.

**Done when:** the capability matrix in [07](07-extension.md) is green end to end on all
three browsers.

### M7 — Polish and ship (2 weeks)

Installer (MSI or MSIX) writing the registry keys and the scheduled task, uninstall
with data deletion, onboarding, store submissions, the performance budget in
[08](08-agent.md) verified.

**Total: roughly 11-12 weeks of focused solo work.** Budget extra for store review,
which is unpredictable and which the incognito permission will slow down.

## Test matrix

Automated where possible, but several of these are inherently manual:

| Scenario | Why it matters |
|---|---|
| Hard power cut (VM, pull virtual plug) | Validates the whole journaling premise |
| Service worker force-evicted mid-debounce | The most common MV3 bug source |
| Browser's own session restore enabled | Duplicate-tab regression |
| External monitor unplugged between capture and restore | Off-screen window regression |
| Mixed DPI (125% laptop + 100% external) | Wrong-size window regression |
| 4 browser profiles open simultaneously | `profile_key` attribution |
| UWP app (Calculator) + cloaked windows | Phantom-window regression |
| Restore while the same apps are already open | Must not duplicate |
| Fast user switching, two accounts | Pipe ACL and DPAPI isolation |
| Agent killed mid-restore | `restore_in_progress` loop guard |
| 200 tabs | Native messaging 1 MB cap, batching |
| Uninstall | No residual data |

## Explicitly deferred

| Feature | Why not v1 | What the schema already allows |
|---|---|---|
| **Cloud backup** | Every privacy claim gets harder; needs E2E key management, an account system, and a sync conflict model | `machine_id`, snapshot immutability, and the `NormalTab` type that makes private exclusion structural |
| **Cross-device restore** | App paths and monitor layouts differ per machine; needs an app-identity resolver | Env-folded paths, `app_key` as a stable hash rather than a raw path |
| **Scroll position / form state** | Requires content scripts on every site — a categorically larger permission ask that would change the store review outcome | Nothing; would need new tables and a new permission |
| **macOS / Linux** | Entire watcher and launcher layer is Win32 | The store, protocol, and extension are all portable; only `sr-agent/watcher` and `restore/` are not |
| **Arc, Brave, Vivaldi, Opera** | Chromium forks work with the Chrome build, but each needs its own registry root and listing; Arc's spaces/splits will not round-trip | Registry roots are a config list, not code |
| **Elevated app restore** | Standing privilege escalation risk ([06](06-privacy-security.md)) | Tier D already models it as a first-class "cannot restore" outcome |

## The one thing to validate before writing much code

Build M0 and M1 first and live with them for a few days. The open question is not
technical feasibility — everything in these specs is verified against the platform
APIs. It is whether a 60-second reconcile against your actual daily browser usage
produces a session that *feels* right when restored. If it does, the rest is
execution. If it does not, the fix is in the capture policy and you want to learn that
in week two rather than week ten.
