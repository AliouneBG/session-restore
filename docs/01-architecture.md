# 01 - Architecture

## Components

```
+---------------------------------------------------------------------+
|  User session (Session 1, medium integrity, interactive)            |
|                                                                     |
|  +---------------+   +---------------+   +---------------+          |
|  | Chrome        |   | Edge          |   | Firefox       |          |
|  |  +---------+  |   |  +---------+  |   |  +---------+  |          |
|  |  |Extension|  |   |  |Extension|  |   |  |Extension|  |          |
|  |  |  (MV3)  |  |   |  |  (MV3)  |  |   |  |  (MV3)  |  |          |
|  |  +----+----+  |   |  +----+----+  |   |  +----+----+  |          |
|  +-------|-------+   +-------|-------+   +-------|-------+          |
|          | stdio             | stdio             | stdio            |
|          | (native messaging - browser spawns the child)            |
|     +----v-----+        +----v-----+        +----v-----+            |
|     |  relay   |        |  relay   |        |  relay   |  short-    |
|     |  (thin)  |        |  (thin)  |        |  (thin)  |  lived     |
|     +----+-----+        +----+-----+        +----+-----+            |
|          |                   |                   |                  |
|          +-------------------+-------------------+                  |
|                    named pipe (ACL: this user's SID only)           |
|                              |                                      |
|                      +-------v------------------------+             |
|                      |      Session Agent (Rust)      |  long-lived |
|                      |  +--------------------------+  |  started at |
|                      |  | Window/Process Watcher   |  |  logon      |
|                      |  | Tab Ingest               |  |             |
|                      |  | Journal Writer           |  |             |
|                      |  | Restore Orchestrator     |  |             |
|                      |  | Tray UI + Review Window  |  |             |
|                      |  | Crypto (DPAPI-wrapped)   |  |             |
|                      |  +------------+-------------+  |             |
|                      +---------------+----------------+             |
|                                      |                              |
|                         +------------v-------------+                |
|                         |  SQLite (WAL)            |                |
|                         |  %LOCALAPPDATA%\         |                |
|                         |    SessionRestore\       |                |
|                         +------------+-------------+                |
+--------------------------------------+------------------------------+
                                       | optional, off by default
                              +--------v---------+
                              |  Cloud backup    |  <- never receives
                              |  (deferred, v2)  |     private-window data
                              +------------------+
```

## The four processes

### 1. Browser extension (one build for Chrome/Edge, one for Firefox)

Observes tabs, windows, and tab groups. Emits **deltas**, not snapshots. Holds no
durable state of its own beyond a small `storage.session` outbox - the agent's SQLite
store is the single source of truth.

Runs in `incognito: "spanning"` mode so one background context sees both normal and
private windows, with an `incognito` flag on each event. (`"split"` would spawn a
second, separate service worker per private profile and a second native-messaging
port - more moving parts, no benefit here.)

### 2. Native messaging relay (thin, short-lived)

A ~200-line Rust binary. The browser spawns it as a child process when the extension
calls `connectNative`; it dies when the browser does. It does exactly two things:
frame-translate between native messaging's stdio framing and the agent's named-pipe
framing, and tag every message with which browser and profile it came from.

It deliberately contains **no logic and no disk access**. It is the only component the
browser can start, so it is the component given the least authority.

### 3. Session agent (long-lived, per user)

The core. Enumerates windows and processes, owns the SQLite store, holds the
encryption keys, runs restores, and draws the tray icon and review window.

**It is not a Windows Service.** It is a per-user process launched at logon by a
Scheduled Task. This is load-bearing - see [ADR-0001](adr/0001-user-agent-not-windows-service.md).

### 4. Local store

SQLite in WAL mode under `%LOCALAPPDATA%\SessionRestore\`. Schema in
[02-data-model.md](02-data-model.md).

## The central design principle: journal, never snapshot-on-exit

The obvious design is "when the machine shuts down, write out the session." It does
not work, for three independent reasons:

1. **Chrome MV3 has no shutdown event.** `chrome.runtime.onSuspend` is not implemented
   for service workers and the Chromium team has indicated it is unlikely to be -
   precisely because a crash would bypass it and give false confidence.
2. **The MV3 service worker is evicted after ~30s idle.** There is no persistent
   background page to hold state in memory until exit.
3. **Power loss and bugchecks run no code at all.** Any exit-time design has a failure
   mode where it captures nothing.

So the system **continuously journals**, in three tiers:

| Tier | Trigger | Latency | Purpose |
|---|---|---|---|
| **T0 - delta** | Extension / Win32 events, debounced 2s | ~2s | Normal operation |
| **T1 - reconcile** | Timer, every 60s (`chrome.alarms` + agent timer) | <=60s | Heals events missed during a service-worker eviction |
| **T2 - shutdown** | `WM_QUERYENDSESSION` + `ShutdownBlockReasonCreate` | best-effort | Final flush; a bonus, never relied on |

**Worst-case data loss is one T1 interval (60s), even on a hard power cut.** T2 is an
optimization, not a dependency. This is the single most important property of the
design, and every later decision should preserve it.

T1 also fixes a correctness problem that is easy to miss: the MV3 service worker can be
evicted mid-session and miss `tabs.onRemoved` entirely. Without periodic full
reconciliation, closed tabs would linger in the store forever. T1 is a full
`tabs.query({})` diffed against stored state, so it both adds what was missed and reaps
what is gone.

## Why this split, and not something simpler

**Why not just the extension?** It cannot see applications, window geometry, or
monitors. It also dies with the browser, so it cannot run the post-logon restore.

**Why not just the agent?** A native process cannot read browser tabs without either
parsing the browser's session files (undocumented, version-dependent, and locked while
the browser runs) or attaching a debugger (see
[ADR-0003](adr/0003-no-playwright-or-cdp.md)). Tabs are the extension's job.

**Why a relay instead of the extension talking to the agent directly?** Native
messaging spawns a *child of the browser* - it cannot connect to an already-running
process. The alternatives are a localhost socket (rejected:
[ADR-0002](adr/0002-native-messaging-over-localhost.md)) or this thin relay. The relay
also gives us a natural place to attribute each message to a specific browser and
profile, which the extension cannot reliably self-report.

## Trust boundaries

| Boundary | Control |
|---|---|
| Web page -> extension | No content scripts in v1. Tab metadata only, never page content. |
| Extension -> relay | Native messaging manifest pins `allowed_origins` to our extension IDs. |
| Relay -> agent | Named pipe ACL'd to the interactive user's SID; relay runs unprivileged. |
| Agent -> disk | Private-window rows encrypted under a separate data key. |
| Agent -> cloud | Opt-in; private-window rows structurally excluded (see [06](06-privacy-security.md)). |

## What this architecture explicitly does not defend against

Malware already running as you, at your integrity level, can read everything this system
can read - including the DPAPI-wrapped key, because DPAPI unwraps for *you* and that
code is running as you. This is stated plainly in
[06-privacy-security.md](06-privacy-security.md) and should be stated just as plainly in
the product UI. The privacy design bounds *exposure over time* and *blast radius*; it
does not survive a compromised account.
