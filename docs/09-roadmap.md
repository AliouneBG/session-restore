# 09 — Roadmap

## Status (2026-09-16)

| Milestone | State |
|---|---|
| M0 — Walking skeleton | **Done.** Verified against real Edge end to end. |
| M1 — Capture, tabs, T1 reconcile | **Done** for Chrome/Edge. Live tabs land in SQLite with title, order, active flag, pinned state, and window geometry. |
| M2 — Apps and windows | Not started. No `EnumWindows` watcher yet. |
| M3 — Restore | **Browser half done.** A reboot cycle restores tabs into a real browser, adding only what is missing. App restore is still M2 work. |
| M4 — T0 deltas / T2 shutdown | T0 event deltas done. T2 shutdown hook not written. |
| M5 — Private windows | Storage, crypto, TTL and the chokepoint are done and tested; the **two-stage opt-in has not been exercised with a real private window**. |
| M6 — Firefox | Builds and manifests exist; **not yet loaded in Firefox**. |
| M7 — Polish | Not started. No installer, no tray, no review UI. |

Roughly M0, M1, the browser half of M3, and half of M4/M5 are real. The honest summary
is that **browser sessions survive a reboot; applications do not exist yet**.

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

### Known gaps

- `profile_key` is hardcoded to `"default"`, so two profiles of the same browser merge.
  Needs the command-line machinery from M2.
- No tray, no review window; `--status` is the only UI.
- A `pre_restore` snapshot is taken on every restore, but nothing consumes it yet -
  there is no Undo action.
- Restore is offered automatically; `restore_mode = ask` is stored and honoured only as
  "off or not", because there is no review UI to ask with.
- Firefox untested.

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
