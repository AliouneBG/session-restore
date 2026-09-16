# 04 - Restore

## When restore happens

The agent starts at logon and waits for the session to settle before doing anything:

```
logon
  -> agent starts (Scheduled Task, 30s delay)
  -> wait for shell ready: explorer.exe running AND a foreground window exists
  -> wait for display topology to stabilize (2 consecutive identical enumerations, 3s apart)
  -> choose the candidate snapshot
  -> restore_mode?
       'off'    -> do nothing, just resume capture
       'ask'    -> show review window  (DEFAULT)
       'auto'   -> restore immediately, toast with an Undo button
```

The topology wait is not optional. Displays attach several seconds after logon,
especially over DisplayPort/Thunderbolt docks. Restoring before they settle puts
everything on the built-in panel.

### Choosing the snapshot

Prefer, in order: the newest `kind='shutdown'` snapshot from this boot cycle; otherwise
the live state (`snapshot_id = 0`) as of last write. If the newest shutdown snapshot is
more than 7 days old, do not auto-restore - ask, because the user's situation has
probably moved on.

## The review window

`restore_mode = 'ask'` is the default and should stay the default. The window lists
what will be restored, grouped by application, with per-item checkboxes and the
honest fidelity tier:

```
  Restore your session from Sat 12 Sep, 11:42 PM?

  [x] Visual Studio Code          2 windows      exact
  [x] Chrome (Personal)          18 tabs, 3 groups
  [x] Chrome (Work)               7 tabs
  [x] Slack                       1 window       launch only - no saved arguments
  [x] Spotify                     1 window       launch only
  [ ] Docker Desktop              1 window       needs admin - cannot restore
  [!] 2 private windows (11 tabs)               expires in 4h  [ Show ]  [ Restore ]

       [ Restore selected ]   [ Not now ]   [ Never ask again ]
```

Two things in that mock are deliberate:

- **Tiers are shown, not hidden.** "launch only - no saved arguments" sets the right
  expectation before the user clicks, instead of producing a confusing result after.
- **Private windows are collapsed, unchecked, and require a separate click.** Their
  URLs are not rendered until the user presses *Show*. Someone doing a screen share
  after a reboot should not have their private tabs listed on screen by default. This
  is the single most likely real-world harm this product can cause, and the UI is
  where it gets prevented.

## Restore ordering

Restoring 12 applications at once on a cold boot is the worst possible thing to do to a
machine that is already busy with logon work. Restore runs as a staggered pipeline:

```
Phase 1  Browsers        launch first - they take longest to become scriptable
Phase 2  Tier A apps     exe + command line, 400ms stagger
Phase 3  Tier B apps     AUMID activation / bare launch, 400ms stagger
Phase 4  Tier C apps     document reopen
Phase 5  Placement       position and size every window that appeared
Phase 6  Tab injection   push tabs into the browsers from phase 1
Phase 7  Z-order/focus   restore focus last
```

Placement is a separate phase because applications do not create their windows
synchronously with process start. Each launched app gets a window-appearance watcher
with a **20-second budget**; if no matching window appears, the item is marked
`failed` with `detail = 'no window within timeout'` and the restore moves on. It never
blocks the pipeline.

Matching a new window back to the app that was launched uses PID first (the PID returned
by `CreateProcess`), falling back to `app_key` match on any new window - necessary
because many apps (Chrome, Slack, Teams) relaunch through a stub process and the window
ends up owned by a different PID than the one we started.

## Launching applications

| Tier | Mechanism |
|---|---|
| A | `CreateProcessW` with the stored command line and working directory |
| B (Win32) | `ShellExecuteExW` on the exe path |
| B (UWP) | `IApplicationActivationManager::ActivateApplication(aumid, ...)` |
| C | `ShellExecuteExW` on the *document* path, letting the shell pick the handler |
| D | Never launched; listed as skipped with a reason |

UWP apps genuinely require `IApplicationActivationManager` - packaged apps have no
launchable exe path, and `CreateProcess` against the one in `WindowsApps` fails or
produces a broken instance. Instantiate it with `CLSCTX_LOCAL_SERVER`.

**Never launch elevated.** The agent runs at medium integrity and stays there. An app
that was running elevated is tier D, listed as "needs admin - start it yourself." The
alternative - an auto-elevating restore path - would be a local privilege escalation
primitive sitting on the machine permanently, triggered by data in a database file. Not
worth it for the convenience.

## Window placement and topology changes

```
for each window in snapshot:
    target = display matching window.display_key
    if target missing:                      # monitor unplugged
        target = primary display
        scale the normalized rect into the new work area, preserving aspect
    place with SetWindowPlacement(norm_rect, show_cmd)
    clamp so at least 120x40 px of the title bar is on-screen
```

The clamp is what prevents the "window restored 3000px to the left, unreachable"
failure when the layout shrank. Always place via `SetWindowPlacement` with the same
`WINDOWPLACEMENT` structure that was captured, so maximized windows restore as
maximized *on the right monitor* with the correct underlying restored size.

DPI: store the capture-time DPI per display and rescale the rect by
`target_dpi / captured_dpi` when they differ. Without this, windows come back visibly
wrong on any mixed-DPI laptop-plus-external-monitor setup, which is most of them.

## Browser and tab restore

### The duplicate-tab problem

This is the failure mode most likely to make the product feel broken, and it needs
handling explicitly.

Chrome, Edge, and Firefox all have their own "continue where you left off" session
restore. If it is enabled, the browser reopens the user's tabs by itself - and then our
extension connects and injects the same tabs again. The user gets everything twice.

**Resolution: reconcile, never blindly inject.**

```
on extension startup (runtime.onStartup):
    wait 3s for the browser's own restore to settle
    current = tabs.query({})                       # what the browser restored on its own
    desired = snapshot from agent
    plan    = diff(desired, current) keyed by normalized URL + window
    create only the tabs in (desired - current)
    never close tabs in (current - desired)        # user's browser, user's tabs
```

URL normalization for the diff: strip the fragment, strip known tracking parameters,
lowercase the host, drop a trailing slash on an empty path. Do **not** strip the query
string in general - `?id=1234` is a different page.

The "never close" rule matters. A restore tool that closes tabs is a tool that destroys
work. We only ever add.

### Lazy tab loading

Creating 40 tabs that all begin loading simultaneously will spike memory and CPU hard
on a machine still finishing logon. The browsers differ, and the difference is real:

| Browser | Mechanism |
|---|---|
| **Firefox** | `browser.tabs.create({ url, discarded: true, title })` - first-class support; the tab appears with its real title and favicon and loads only when clicked |
| **Chrome / Edge** | No `discarded` option on `tabs.create`. Two workarounds: create the tab, then call `chrome.tabs.discard(tabId)` once it exists; or create a placeholder extension page (`restore.html?u=...&t=...`) that navigates to the real URL on first activation |

For Chrome/Edge, **use the placeholder page.** `tabs.discard()` after creation still
lets the page start loading (and fire its network requests, and run its scripts) before
being discarded, which defeats the purpose and means a restored session silently hits
40 sites. The placeholder page never touches the network until the user activates the
tab, and it preserves the title and favicon in the tab strip. It also degrades
gracefully: if the extension is later removed, the user still has a readable URL.

Restore the active tab of each window eagerly (not lazily) so the window is immediately
useful.

### Restore order within a browser

```
for each browser_window in snapshot:
    windows.create({ url: [active_tab_url], incognito: is_private, state, left, top, width, height })
    for each remaining tab in index order:
        tabs.create({ windowId, index, url: placeholder(url), pinned, active: false })
    for each tab_group:
        tabGroups.group / update  (Chrome/Edge only; Firefox has no tab group API)
    windows.update({ state })   # re-apply maximized/fullscreen, which create() often ignores
```

Pinned tabs must be created before unpinned ones or the indices shift underneath.

### Private window restore

Additional gates beyond the normal flow:

1. `settings.capture_private_windows` is on **and** the rows have not passed `expires_at`
2. The extension still has incognito access (re-check `isAllowedIncognitoAccess()`)
3. The user explicitly clicked *Restore* on the collapsed private group in the review
   window - private windows are **never** part of an `auto` restore, ever

Then: `windows.create({ incognito: true, url: [...] })`. Decryption happens in the agent
immediately before sending, and the plaintext URLs exist only in that one IPC message.

After a successful private restore, the rows are **deleted immediately** rather than
waiting for the TTL. They have served their purpose and there is no reason to keep them.

## Failure handling and undo

Every restore writes a `kind='pre_restore'` snapshot first. The completion toast carries
an **Undo** button for 60 seconds, which closes what the restore opened (tracked
precisely in `restore_items`, so it only touches windows this run created) and does not
touch anything that was already open.

Per-item failures never abort the run. Each lands in `restore_items` with a status and a
reason, and the summary is honest:

```
  Restored 7 of 9 apps, 24 of 25 tabs.
    - Docker Desktop: needs admin
    - Figma: not installed at the saved path
                                              [ Details ]  [ Undo ]
```

## Restore-loop safety

A crash during restore must not produce a boot loop where every restart re-restores and
re-crashes. Guards:

- The agent writes a `restore_in_progress` marker with a timestamp before phase 1 and
  clears it at the end.
- If the marker is found at startup and is less than 10 minutes old, the previous
  restore did not finish cleanly: **do not auto-restore**. Fall back to `ask` mode with
  a warning, regardless of the configured `restore_mode`.
- Three consecutive unclean restores disable auto-restore entirely until the user
  re-enables it.
