# 03 — Capture

## Capture tiers

Restated from [01](01-architecture.md) because everything below depends on it:

| Tier | Trigger | What it writes |
|---|---|---|
| T0 | Event + 2s debounce | Deltas into `snapshot_id = 0` |
| T1 | Every 60s | Full reconcile of `snapshot_id = 0`; adds missed, reaps stale |
| T2 | Shutdown / logoff | `kind='shutdown'` snapshot; best-effort |

T1 is the floor on correctness. If T0 and T2 both broke entirely, the product would
still work with 60s of staleness. Build T1 first.

## Application and window capture (agent)

### Enumeration

`EnumWindows` -> for each HWND, collect and filter. A window is **capturable** only if
all of these hold:

| Check | API | Why |
|---|---|---|
| Visible | `IsWindowVisible` | Skip hidden helpers |
| Not a tool window | `GetWindowLongPtr(GWL_EXSTYLE)` lacks `WS_EX_TOOLWINDOW` | Skip palettes, tooltips |
| Top-level | `GetWindow(GW_OWNER)` is NULL, or it is a visible owned dialog | Skip child/owned chrome |
| Has a title | `GetWindowTextLength > 0` | Untitled top-levels are almost always internal |
| **Not cloaked** | `DwmGetWindowAttribute(DWMWA_CLOAKED)` returns 0 | **Load-bearing — see below** |
| Not rule-ignored | `app_rules` | User allowlist |

**The cloaking check is the one people miss.** Every suspended UWP/Store app, and every
window sitting on a *different virtual desktop*, remains `IsWindowVisible == TRUE` while
being invisible to the user. Without `DWMWA_CLOAKED` you capture a pile of phantom
windows — Calculator, Mail, Settings — that the user never had open, and then
cheerfully "restore" them all on next boot. Filter on cloaked, but record
`virtual_desktop_id` separately so genuinely-on-another-desktop windows are kept rather
than dropped.

### Identifying the application behind a window

```
HWND -> GetWindowThreadProcessId -> PID
     -> OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)
     -> QueryFullProcessImageNameW  => exe path
     -> GetApplicationUserModelId   => AUMID, if it is a packaged app
```

`PROCESS_QUERY_LIMITED_INFORMATION` is deliberate: it is the least privilege that
returns the image name, and unlike `PROCESS_QUERY_INFORMATION` it succeeds against
protected and higher-integrity processes. Use it everywhere.

### Command line: three options, none of them clean

Restoring "Chrome with `--profile-directory=Work`" or "VS Code with a folder argument"
requires the command line, and Windows has no supported, cheap way to read another
process's command line.

| Method | Cost | Breaks when |
|---|---|---|
| `NtQueryInformationProcess` -> PEB -> `RTL_USER_PROCESS_PARAMETERS` | Fast (~µs) | Undocumented; needs `PROCESS_VM_READ`; 32/64-bit mismatch needs `Wow64` variants; struct offsets shift between Windows builds |
| WMI `Win32_Process.CommandLine` | Slow (~100ms+ per query, WMI init is seconds) | Fine, but far too slow for a 60s reconcile loop over 40 processes |
| ETW `ProcessStart` kernel events | Cheap at steady state | Needs a session; only sees processes started *after* you begin listening |

**Decision: ETW as primary, PEB as fallback, WMI never.**

Subscribe to the `Microsoft-Windows-Kernel-Process` provider at agent startup and cache
`PID -> command line` for every process started from then on. For processes already
running when the agent starts, do a one-time PEB read. This gives correct command lines
at near-zero steady-state cost, and confines the undocumented-struct risk to a single
code path run once per boot that is allowed to fail.

When the command line is unavailable, the app drops to restore tier B (launch with no
arguments) rather than failing. Record `command_line = NULL`, never a guess.

> **Sanitize before storing.** Command lines routinely contain access tokens, database
> passwords, and signed URLs. Run every captured command line through a redaction pass
> (`--password=`, `--token=`, `api_key=`, bearer-shaped strings, anything matching a
> URL with a query string) and store the redacted form. A redacted argument is replaced
> with a sentinel that restore treats as "cannot restore exactly" — degrading to tier B
> — rather than replaying a secret onto a command line where it will show up in
> Task Manager and any local process listing.

### Browser window titles are page data, not app metadata

A browser window's Win32 title is the **current page title**:

```
"AliouneBG/session-restore and 1 more page - Profile 1 - Microsoft Edge"
"<page title> - Mozilla Firefox Private Browsing"
```

This matters more than it looks. If the agent stores `windows.title` verbatim for every
window, then private browsing page titles land in the plaintext `windows` table — routing
around the encrypted `tabs_private` path entirely. The extension half would be airtight
and the agent half would leak beside it.

**Rule: never store a raw window title for a process identified as a browser.** For
browser windows, store `title = NULL` and rely on the extension for tab data, which is
the component that knows what is private. The browser and profile identity is already
captured in `browser_windows`; the title adds nothing the extension does not supply
better.

Detection is by `app_key` against the known-browser list, which is checked *before* the
title is read — not by pattern-matching the title for markers like "Private Browsing".
Those markers are localized, differ per browser, and are trivially spoofed by any page
that sets `document.title`. Identify the process, then decide; never parse the title to
decide whether the title is sensitive.

This applies to the window `EVENT_OBJECT_NAMECHANGE` handler too, which would otherwise
stream page titles into the journal on every navigation.

### Window geometry

Use `GetWindowPlacement`, not `GetWindowRect`. `GetWindowPlacement` returns the
**restored** (normal) rectangle plus a show-state flag, so a maximized window records
both "maximized" and the size it would return to when un-maximized. `GetWindowRect` on a
maximized window gives you the monitor bounds and the original size is lost forever.

Store coordinates as **display-relative**, not desktop-absolute: `display_key` plus an
offset within that display's work area. A desktop-absolute rect is meaningless after the
monitor layout changes, which is exactly the moment restore needs to be smart.

### Change detection

Hook `SetWinEventHook` for:

| Event | Meaning |
|---|---|
| `EVENT_OBJECT_CREATE` / `DESTROY` | Window opened/closed |
| `EVENT_OBJECT_LOCATIONCHANGE` | Moved or resized — **heavily debounced (2s trailing)**, this fires continuously during a drag |
| `EVENT_SYSTEM_FOREGROUND` | Focus/z-order changes |
| `EVENT_OBJECT_NAMECHANGE` | Title changed (document switched) |

Use an out-of-context hook (`WINEVENT_OUTOFCONTEXT`) so the agent does not inject a DLL
into every process on the machine. In-context hooks would be faster and would be an
absolutely unacceptable amount of ambient authority for a convenience tool.

### Restore tier assignment

Assigned at capture time and stored on the row, so the restore UI can be honest before
it tries anything:

| Tier | Meaning | Criteria |
|---|---|---|
| **A** | Exact | Win32, readable exe path, command line captured un-redacted, not elevated |
| **B** | Launch only | Packaged/UWP app (AUMID known), or command line unavailable/redacted |
| **C** | Document reopen | Tier A/B app whose window title maps to a known document path |
| **D** | Not restorable | Elevated process, installer, unresolvable exe, or `never_restore` rule |

## Browser capture (extension)

### Events subscribed

```
tabs.onCreated      tabs.onUpdated     tabs.onRemoved
tabs.onMoved        tabs.onAttached    tabs.onDetached
tabs.onActivated    tabs.onReplaced
windows.onCreated   windows.onRemoved  windows.onFocusChanged
tabGroups.onCreated tabGroups.onUpdated tabGroups.onRemoved   (Chrome/Edge only)
```

`tabs.onUpdated` fires several times per navigation (loading -> title -> favicon ->
complete). Only act on `changeInfo.status === 'complete'` or a `url`/`title` change, and
coalesce per-tab within the debounce window. Naively forwarding every `onUpdated` would
produce roughly 10x the necessary IPC traffic.

### Debounce and batching

Events accumulate in an in-memory map keyed by `tabId`, flushed after **2s of quiet** or
**50 pending changes**, whichever comes first. The flush sends one batched message.

Because the service worker can be evicted mid-debounce, the pending map is mirrored into
`chrome.storage.session` on every mutation and replayed on the next service worker
start. `storage.session` is in-memory and cleared when the browser closes, which is
correct: anything lost that way is picked up by the next T1 reconcile.

### What is captured per tab

`url`, `title`, `favIconUrl`, `pinned`, `index`, `windowId`, `groupId`, `active`,
`mutedInfo.muted`, `lastAccessed`.

**Not captured in v1:** page content, scroll position, form state, cookies,
`sessionStorage`. All of these require content scripts with host permissions on every
site, which is a categorically larger privacy ask than tab metadata and would make the
extension unreviewable for store listing. See [09-roadmap.md](09-roadmap.md).

URLs matching `chrome://`, `edge://`, `about:`, `devtools://`, `view-source:`, and
`file://` are captured but flagged `restorable = false` where the browser forbids
programmatic navigation to them. `file://` is restorable only if the user has granted
file access to the extension.

### Private / incognito windows

Gated on `settings.capture_private_windows`, which is **false by default**.

Even when the setting is on, capture additionally requires the browser-level permission —
`chrome.extension.isAllowedIncognitoAccess()` in Chrome/Edge, the equivalent
`extension.isAllowedIncognitoAccess()` in Firefox. The user must have turned on "Allow in
Incognito" / "Allow in InPrivate" / "Run in Private Windows" themselves; there is no API
to request it, and that is a feature, not an obstacle.

The flow is therefore two independent, explicit opt-ins:

```
1. Browser setting:  "Allow in Incognito"        (user, in browser settings)
2. App setting:      capture_private_windows      (user, in our tray UI, with a
                                                   plain-language warning)
```

Private tab payloads are marked `private: true` in the IPC message. The agent routes
them to `tabs_private` with encryption and a TTL and **never** to `tabs` or `journal`.
This routing decision lives in exactly one function in the agent, which is the only
place that needs auditing to verify the property holds.

### Profile attribution

The extension cannot reliably report which browser profile it is running in. The relay
supplies it: the browser passes the profile directory as an argument to the native
messaging host on some platforms, and otherwise the relay derives it from its own
parent process command line (`--profile-directory=`). `profile_key` is a hash of that
path, so "Chrome Work" and "Chrome Personal" stay distinct on restore without storing
the path itself.
