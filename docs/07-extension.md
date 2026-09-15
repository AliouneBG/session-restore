# 07 — Browser extension

## Build targets

One TypeScript source tree, two manifests, two bundles.

```
extension/
  src/
    background/
      index.ts          entry; wires listeners, owns nothing
      collector.ts      event handlers -> pending map
      outbox.ts         debounce, storage.session mirror, batching
      port.ts           native messaging connect / reconnect / backoff
      reconcile.ts      T1 full_state via chrome.alarms
      restore.ts        applies restore_session, diff-against-current
    pages/
      restore.html/ts   lazy placeholder page (Chrome/Edge)
      options.html/ts   status, permissions, links to the agent UI
    shared/
      protocol.ts       message types, generated from one schema
      normalize.ts      URL normalization for diffing
      caps.ts           runtime capability detection
  manifest.chrome.json
  manifest.firefox.json
```

`shared/protocol.ts` and the agent's Rust types are both generated from a single JSON
Schema in `/schema`, so the wire format cannot drift between the two halves. This is
worth the small build complexity — protocol drift between independently-updated
components is otherwise a recurring, hard-to-diagnose bug class.

## Manifest — Chrome / Edge

```json
{
  "manifest_version": 3,
  "name": "Session Restore",
  "version": "1.0.0",
  "minimum_chrome_version": "116",
  "background": { "service_worker": "background.js", "type": "module" },
  "permissions": ["tabs", "tabGroups", "storage", "alarms", "nativeMessaging"],
  "host_permissions": [],
  "incognito": "spanning",
  "action": { "default_popup": "options.html" },
  "icons": { "16": "...", "48": "...", "128": "..." }
}
```

## Manifest — Firefox

```json
{
  "manifest_version": 3,
  "name": "Session Restore",
  "version": "1.0.0",
  "background": { "scripts": ["background.js"], "type": "module" },
  "permissions": ["tabs", "storage", "alarms", "nativeMessaging"],
  "incognito": "spanning",
  "browser_specific_settings": {
    "gecko": { "id": "session-restore@yourdomain.example", "strict_min_version": "128.0" }
  }
}
```

Two deliberate differences: Firefox MV3 uses an **event page** (`background.scripts`),
not a service worker — it is less aggressively evicted and it *does* implement
`runtime.onSuspend`. And `tabGroups` is omitted because Firefox has no such API.

## Browser capability matrix

| Capability | Chrome | Edge | Firefox | Handling |
|---|---|---|---|---|
| Background shutdown event | No | No | **Yes** (`onSuspend`) | Opportunistic final flush on FF; never depended on |
| `tabs.create({discarded})` | No | No | **Yes** | FF uses it; Chrome/Edge use the placeholder page ([04](04-restore.md)) |
| Tab groups | Yes | Yes | No | Groups captured and restored on Chromium; degrade to plain tabs on FF |
| `incognito: "split"` | Yes | Yes | Not supported | We use `"spanning"` everywhere regardless |
| Private access opt-in | "Allow in Incognito" | "Allow in InPrivate" | "Run in Private Windows" | Same API check, different UI strings in onboarding |
| Native messaging registry root | `Software\Google\Chrome` | `Software\Microsoft\Edge` | `Software\Mozilla` | Installer writes all three |
| Manifest allowlist key | `allowed_origins` | `allowed_origins` | `allowed_extensions` | Two manifest files |
| Background eviction | ~30s idle | ~30s idle | Less aggressive | T1 reconcile covers both |

Use the `browser`/`chrome` polyfill (`webextension-polyfill`) so the source is
promise-based everywhere, and gate the differences above behind `caps.ts` rather than
sniffing user agent.

## Surviving service-worker eviction

The single biggest source of MV3 bugs. Rules for this codebase:

1. **No module-level mutable state that matters.** Anything that must survive is in
   `chrome.storage.session`. Treat every top-level variable as if it is wiped between
   any two events, because it is.
2. **The outbox is written to `storage.session` on every mutation**, not on the flush.
   A flush that never happens because the worker died must not lose the deltas.
3. **`chrome.alarms`, never `setTimeout`, for the T1 reconcile.** `setTimeout` does not
   survive eviction; alarms wake the worker. Minimum alarm period is 1 minute, which is
   exactly our 60s interval — convenient, but it means the interval cannot be tuned
   below 60s on Chromium. Document that as a hard floor.
4. **Debounce with an alarm too**, or accept that a 2s debounce may be cut short by
   eviction. Cut short is fine: it flushes early, which is harmless. Lost is not fine,
   which is why rule 2 exists.
5. **Do not try to keep the worker alive.** No `setInterval` heartbeats, no persistent
   port tricks. They are fragile, they burn battery, and they are a known cause of store
   review friction.

On `runtime.onStartup` and `runtime.onInstalled`: replay the `storage.session` outbox if
anything survived, then immediately run a full reconcile to establish ground truth.

## Restore path in the extension

Receives `restore_session` ([05](05-ipc-protocol.md)), then:

```
wait 3s   # let the browser's own session restore finish first
current = await tabs.query({})
plan    = diff(desired, current)        # normalized-URL keyed
create only plan.missing
never close anything
report restore_result
```

The "wait then diff" is what prevents double tabs when the browser has its own
"continue where you left off" enabled. See [04](04-restore.md) for the full reasoning —
it is the most important behavior in this file.

## The placeholder page (Chrome/Edge lazy loading)

`restore.html?u=<encoded>&t=<title>`:

- Sets `document.title` to the saved title so the tab strip looks right
- Sets a favicon from the saved `favicon_hash` via the agent's icon cache
- Renders the URL as readable text with a "Open now" button
- Navigates to the real URL on `document.visibilitychange` -> visible

It must **not** navigate on load — that would defeat lazy loading entirely. And it must
degrade well: if the extension is uninstalled while placeholder tabs are open, the user
sees a dead page. Mitigate by writing the real URL into the page body as selectable
text, and by replacing all placeholder tabs with their real URLs on
`runtime.onSuspend` where available (Firefox) or on any restore-complete signal.

## Onboarding

The extension cannot request incognito access programmatically; the user must toggle it.
The options page therefore needs a short, honest walkthrough:

1. Detect `isAllowedIncognitoAccess()` -> false
2. Show browser-specific instructions with the correct string
   ("Allow in Incognito" / "Allow in InPrivate" / "Run in Private Windows")
3. State plainly what turning it on means: private URLs will be written to your disk,
   encrypted, and deleted within 24 hours
4. Re-check on focus; confirm when it flips

Do not nag. If the user declines, the extension works fully for normal tabs and the
option stays available in settings.
