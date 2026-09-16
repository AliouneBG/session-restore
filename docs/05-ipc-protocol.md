# 05 - IPC protocol

## Transport chain

```
Extension  --(native messaging: stdio, 4-byte LE length + UTF-8 JSON)-->  Relay
Relay      --(named pipe: 4-byte LE length + UTF-8 JSON)-->               Agent
```

Both hops use the same framing so the relay is a near-memcpy. Native messaging caps a
single message at **1 MB**; the protocol below keeps messages well under that by
batching with an explicit cap (see `tab_delta`).

### Native messaging host registration

Chrome / Edge:

```
HKCU\Software\Google\Chrome\NativeMessagingHosts\com.sessionrestore.relay
HKCU\Software\Microsoft\Edge\NativeMessagingHosts\com.sessionrestore.relay
  (Default) = C:\Users\<u>\AppData\Local\SessionRestore\relay-manifest.json
```

```json
{
  "name": "com.sessionrestore.relay",
  "description": "Session Restore relay",
  "path": "C:\\Users\\<u>\\AppData\\Local\\SessionRestore\\relay.exe",
  "type": "stdio",
  "allowed_origins": ["chrome-extension://<CHROME_EXT_ID>/"]
}
```

Firefox:

```
HKCU\Software\Mozilla\NativeMessagingHosts\com.sessionrestore.relay
```

```json
{
  "name": "com.sessionrestore.relay",
  "description": "Session Restore relay",
  "path": "C:\\Users\\<u>\\AppData\\Local\\SessionRestore\\relay.exe",
  "type": "stdio",
  "allowed_extensions": ["session-restore@yourdomain.example"]
}
```

Note the key difference: Chrome/Edge use `allowed_origins` with an extension ID;
Firefox uses `allowed_extensions` with the addon ID from
`browser_specific_settings.gecko.id`. Same file shape otherwise.

Registration is per-user (`HKCU`), written by the installer and re-verified by the agent
at every startup - browser updates and profile resets have been known to clear them.

### Named pipe

```
\\.\pipe\SessionRestore.<sid-hash>
```

Created by the agent with a security descriptor granting read/write to the interactive
user's SID **only** - not `Everyone`, not `Authenticated Users`. Include the SID hash in
the name so two users on the same machine cannot collide or probe each other's pipe.

The agent must create the pipe with `FILE_FLAG_FIRST_PIPE_INSTANCE` on the first
instance. Without it, a hostile local process that starts first can squat the pipe name
and receive the relay's connections instead - a classic named-pipe hijack.

### Three pipe details that are not optional

All three were found by running the real thing, and each produces a silent hang rather
than an error, so none would show up in a naive implementation until it is deployed.

**1. The pipe must be opened with `FILE_FLAG_OVERLAPPED`, on both ends.**

A handle opened for synchronous I/O serializes every operation on the underlying *file
object*, and `DuplicateHandle` does not create a new file object - it adds a reference
to the same one. The relay reads from the agent on one thread while writing to it on
another, so a parked `ReadFile` blocks the `WriteFile` behind it and the two processes
wait on each other forever. Observed as both sides stuck transferring **four bytes**.

Each operation therefore carries its own `OVERLAPPED` and event and waits on
`GetOverlappedResult`. That keeps the blocking `Read`/`Write` interface the framing
code expects while letting the two directions proceed independently.

Consequence: `ConnectNamedPipe` also needs an `OVERLAPPED`, and returns
`ERROR_IO_PENDING` rather than blocking.

**2. The client must request `GENERIC_READ | GENERIC_WRITE`, not `FILE_GENERIC_*`.**

`FILE_GENERIC_WRITE` contains `FILE_APPEND_DATA` (`0x0004`), and on a named pipe that
bit means `FILE_CREATE_PIPE_INSTANCE` - a different request. The handle opens and looks
connected either way.

**3. Never call `FlushFileBuffers` on the write end.**

It is documented not to return "until the reading process has read all the data from
the pipe", which turns every frame write into a rendezvous with the peer's read
cadence. There is no userspace buffer to flush - `WriteFile` already hands the bytes to
the kernel - so the `flush()` implementation is deliberately a no-op.

A useful bisection tool if this area ever misbehaves again: connect a .NET
`NamedPipeClientStream` to the agent from PowerShell. It is a known-good client, so if
it works, the fault is on our client side.

## Message envelope

```json
{
  "v": 1,
  "id": "01J8X...",
  "type": "tab_delta",
  "ts": 1757800000000,
  "src": { "browser": "chrome", "profile_key": "9f2c...", "ext_version": "1.0.0" },
  "body": { }
}
```

`src` is stamped by the **relay**, not the extension - the extension cannot be trusted
to report its own browser and profile correctly, and the relay knows both from its own
parent process.

## Messages: extension -> agent

### `hello`

Sent once per connection.

```json
{ "type": "hello", "body": {
    "ext_version": "1.0.0",
    "browser_version": "141.0.7390.54",
    "incognito_access": true,
    "capabilities": ["tab_groups", "discarded_create"]
}}
```

`capabilities` lets the agent adapt without version-sniffing: Firefox reports
`discarded_create` and not `tab_groups`; Chrome/Edge the reverse.

### `tab_delta`

The workhorse. Batched, debounced 2s.

```json
{ "type": "tab_delta", "body": {
  "windows": [
    { "op": "upsert", "window_id": "w1", "private": false,
      "state": "maximized", "x": 0, "y": 0, "w": 2560, "h": 1440, "focused": true },
    { "op": "remove", "window_id": "w4" }
  ],
  "tabs": [
    { "op": "upsert", "tab_key": "w1:t7", "window_id": "w1", "index": 3,
      "url": "https://example.com/a", "title": "Example",
      "favicon_hash": "sha256:ab12...", "pinned": false, "active": true,
      "muted": false, "group_key": "w1:g1", "private": false },
    { "op": "upsert", "tab_key": "w9:t2", "window_id": "w9", "index": 0,
      "url": "https://example.org/private-thing", "title": "...",
      "private": true },
    { "op": "remove", "tab_key": "w1:t3" }
  ],
  "groups": [
    { "op": "upsert", "group_key": "w1:g1", "window_id": "w1",
      "title": "Research", "color": "blue", "collapsed": false }
  ]
}}
```

Cap at **200 tab entries per message**; split larger batches across sequential messages
to stay clear of the 1 MB native messaging limit.

`private: true` is the only routing signal the agent needs. Everything with that flag
goes to `tabs_private` - encrypted, TTL'd, never journaled in plaintext, never
cloud-eligible. That routing lives in one function; see
[06-privacy-security.md](06-privacy-security.md).

### `full_state`

The T1 reconcile, every 60s. Same body shape as `tab_delta` but **authoritative**: any
tab or window the agent holds for this `(browser, profile)` that is absent from
`full_state` is deleted.

```json
{ "type": "full_state", "body": { "windows": [...], "tabs": [...], "groups": [...] } }
```

This is what heals a service-worker eviction that swallowed `onRemoved` events.

### `restore_result`

```json
{ "type": "restore_result", "body": {
    "run_id": 42,
    "items": [ { "tab_key": "w1:t7", "status": "created" },
               { "tab_key": "w1:t9", "status": "failed", "detail": "invalid URL scheme" } ]
}}
```

## Messages: agent -> extension

### `hello_ack`

```json
{ "type": "hello_ack", "body": {
    "agent_version": "1.0.0",
    "capture_enabled": true,
    "capture_private": false,
    "reconcile_interval_s": 60
}}
```

`capture_private: false` tells the extension to **drop private events at the source**
rather than send them and rely on the agent to discard. Cheapest place to enforce a
privacy rule is always the earliest one.

### `restore_session`

```json
{ "type": "restore_session", "body": {
  "run_id": 42,
  "lazy": true,
  "windows": [
    { "window_id": "w1", "private": false, "state": "maximized",
      "x": 0, "y": 0, "w": 2560, "h": 1440,
      "tabs": [ { "url": "https://example.com/a", "title": "Example",
                  "index": 0, "pinned": true, "active": false } ],
      "groups": [ { "group_key": "g1", "title": "Research", "color": "blue",
                    "tab_indices": [2,3,4] } ] }
  ]
}}
```

Private windows arrive in their own `restore_session` message, sent only after explicit
user confirmation, and the agent decrypts immediately before sending. The plaintext URLs
exist in exactly one place: this message, in memory, for the duration of the restore.

### `settings_changed`

Pushed when the user changes anything in the tray UI, so the extension does not poll.

## Connection lifecycle and failure

| Situation | Behavior |
|---|---|
| Agent not running | `connectNative` succeeds (relay starts) but the pipe connect fails. Relay replies `{"type":"agent_unavailable"}` and exits. Extension retries with backoff: 5s, 15s, 60s, then every 5 min. |
| Relay missing / not registered | `chrome.runtime.lastError` on `connectNative`. Extension surfaces a badge and a "finish setup" page. |
| Pipe drops mid-session | Relay exits; extension sees `onDisconnect`; reconnect with the same backoff. |
| Extension disconnects | Agent keeps that browser's last-known state but marks it `stale_since`. After 10 min stale, it stops offering those tabs for auto-restore (they are probably long gone). |

Because MV3 evicts the service worker, `connectNative` ports die routinely and that is
**not an error condition**. The extension reconnects lazily on the next event rather
than trying to hold a port open - attempting to keep a port alive purely to stay
resident is both fragile and a documented way to get an extension flagged during store
review.

## Versioning

`v` in the envelope is the protocol version. Agent and extension may be updated
independently by their respective stores, so:

- Agent accepts `v` <= its own; rejects newer with `{"type":"version_too_new"}`.
- Unknown message types are ignored, not fatal.
- Unknown fields inside `body` are ignored, not fatal.

Additive changes do not bump `v`. Only a change in the meaning of an existing field does.
