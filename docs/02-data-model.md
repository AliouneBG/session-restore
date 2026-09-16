# 02 - Data model

## Storage layout

```
%LOCALAPPDATA%\SessionRestore\
  sessions.db          SQLite, WAL mode
  sessions.db-wal
  sessions.db-shm
  keys.bin             DPAPI-wrapped data keys (see 06)
  logs\agent-YYYYMMDD.log
  icons\<sha256>.png   cached app/favicon images, content-addressed
```

SQLite is opened with `journal_mode=WAL`, `synchronous=NORMAL`, and
`busy_timeout=5000`. WAL matters here: it is what makes a hard power cut leave a
consistent database rather than a corrupt one.

> `synchronous=NORMAL` under WAL can lose the last few committed transactions on power
> loss but cannot corrupt the file. That is the right trade for us - we already accept
> up to 60s of loss by design, and `FULL` would mean an fsync every two seconds for the
> life of the machine.

## The snapshot-id trick

Rather than maintaining separate "live" and "archived" table sets, **every entity table
carries a `snapshot_id`**, and snapshot `0` is reserved to mean *live current state*.

- Capture writes continuously into `snapshot_id = 0`.
- Taking a snapshot is `INSERT INTO ... SELECT ... WHERE snapshot_id = 0` with a new id.
- Restore reads any `snapshot_id` through exactly the same queries.

One code path, one schema, and "restore what was open" and "restore last Tuesday" are
the same operation.

## Schema

### Metadata and configuration

```sql
CREATE TABLE meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
-- schema_version, machine_id (random UUID, NOT a hardware id), install_id, created_at

CREATE TABLE settings (
  key        TEXT PRIMARY KEY,
  value      TEXT NOT NULL,
  updated_at INTEGER NOT NULL
);
-- capture_private_windows      : bool, DEFAULT false   (see ADR-0004)
-- private_ttl_hours            : int,  DEFAULT 24
-- reconcile_interval_seconds   : int,  DEFAULT 60
-- snapshot_retention_count     : int,  DEFAULT 20
-- restore_mode                 : 'ask' | 'auto' | 'off', DEFAULT 'ask'
-- cloud_backup_enabled         : bool, DEFAULT false

CREATE TABLE app_rules (
  id         INTEGER PRIMARY KEY,
  match_kind TEXT NOT NULL CHECK (match_kind IN ('exe_path','aumid','exe_name','title_regex')),
  pattern    TEXT NOT NULL,
  action     TEXT NOT NULL CHECK (action IN ('capture','ignore','never_restore')),
  reason     TEXT,
  created_at INTEGER NOT NULL
);
```

`app_rules` is the per-app allowlist. It ships with a seeded ignore list (installers,
UAC prompts, our own binaries, `explorer.exe` shell windows, screensavers) and the user
adds to it from the tray UI.

### Snapshots

```sql
CREATE TABLE snapshots (
  id            INTEGER PRIMARY KEY,     -- 0 = LIVE, reserved
  captured_at   INTEGER NOT NULL,
  kind          TEXT NOT NULL CHECK (kind IN ('live','shutdown','periodic','manual','pre_restore')),
  label         TEXT,
  machine_id    TEXT NOT NULL,
  topology_hash TEXT,                    -- hash of the display layout at capture time
  pinned        INTEGER NOT NULL DEFAULT 0,
  app_count     INTEGER NOT NULL DEFAULT 0,
  tab_count     INTEGER NOT NULL DEFAULT 0
);
```

`kind = 'pre_restore'` is important: **the agent snapshots the current session before
performing any restore**, so a restore is always undoable.

### Displays

```sql
CREATE TABLE displays (
  snapshot_id  INTEGER NOT NULL,
  display_key  TEXT NOT NULL,   -- stable: hash of EDID/device-interface path, not index
  friendly_name TEXT,
  is_primary   INTEGER NOT NULL,
  bounds_x     INTEGER NOT NULL,
  bounds_y     INTEGER NOT NULL,
  bounds_w     INTEGER NOT NULL,
  bounds_h     INTEGER NOT NULL,
  work_x INTEGER, work_y INTEGER, work_w INTEGER, work_h INTEGER,
  dpi          INTEGER NOT NULL,
  PRIMARY KEY (snapshot_id, display_key)
);
```

`display_key` must be derived from something stable across reboots and port changes -
EDID manufacturer/serial, or the device interface path. **Monitor index is not stable**
and using it is the classic cause of "all my windows piled onto one screen."

### Applications and windows

```sql
CREATE TABLE apps (
  snapshot_id  INTEGER NOT NULL,
  app_key      TEXT NOT NULL,   -- stable identity, see below
  kind         TEXT NOT NULL CHECK (kind IN ('win32','uwp','unknown')),
  exe_path     TEXT,            -- env-folded: %ProgramFiles%\... 
  aumid        TEXT,            -- Store/UWP apps
  display_name TEXT,
  icon_hash    TEXT,
  command_line TEXT,            -- NULL if unreadable; see 03 for the caveats
  working_dir  TEXT,
  restore_tier TEXT NOT NULL CHECK (restore_tier IN ('A','B','C','D')),
  PRIMARY KEY (snapshot_id, app_key)
);

CREATE TABLE windows (
  snapshot_id  INTEGER NOT NULL,
  window_key   TEXT NOT NULL,   -- stable-ish within a snapshot
  app_key      TEXT NOT NULL,
  -- ALWAYS NULL when is_browser = 1: a browser window's title is the page title,
  -- which for a private window is C4 data. See 03-capture.md and 06-privacy-security.md.
  title        TEXT,
  display_key  TEXT,
  -- GetWindowPlacement: restored (normal) rect, independent of current show state
  norm_x INTEGER, norm_y INTEGER, norm_w INTEGER, norm_h INTEGER,
  show_cmd     TEXT NOT NULL CHECK (show_cmd IN ('normal','maximized','minimized')),
  z_order      INTEGER,
  virtual_desktop_id TEXT,      -- best effort, see 08
  is_browser   INTEGER NOT NULL DEFAULT 0,
  browser_window_id TEXT,       -- joins to browser_windows when is_browser = 1
  PRIMARY KEY (snapshot_id, window_key),
  FOREIGN KEY (snapshot_id, app_key) REFERENCES apps(snapshot_id, app_key)
);
```

**`app_key` derivation** - this is the value everything else hangs off, so it must be
deterministic and stable across reboots:

| App kind | `app_key` |
|---|---|
| Win32 | `win32:` + sha256(lowercased, env-folded exe path) |
| UWP / Store | `uwp:` + AUMID |
| Unknown | `unk:` + sha256(lowercased exe name) |

Env-folding (`C:\Program Files\...` -> `%ProgramFiles%\...`) is what lets a profile
survive a drive-letter change or a future cross-device sync.

### Browser windows, tabs, groups

```sql
CREATE TABLE browser_windows (
  snapshot_id       INTEGER NOT NULL,
  browser_window_id TEXT NOT NULL,
  browser           TEXT NOT NULL CHECK (browser IN ('chrome','edge','firefox')),
  profile_key       TEXT NOT NULL,  -- hash of profile dir; distinguishes Work vs Personal
  is_private        INTEGER NOT NULL DEFAULT 0,
  window_state      TEXT CHECK (window_state IN ('normal','maximized','minimized','fullscreen')),
  x INTEGER, y INTEGER, w INTEGER, h INTEGER,
  focused           INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (snapshot_id, browser_window_id)
);

CREATE TABLE tab_groups (
  snapshot_id       INTEGER NOT NULL,
  group_key         TEXT NOT NULL,
  browser_window_id TEXT NOT NULL,
  title             TEXT,
  color             TEXT,
  collapsed         INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (snapshot_id, group_key)
);

-- Normal (non-private) tabs. Plaintext. Cloud-syncable.
CREATE TABLE tabs (
  snapshot_id       INTEGER NOT NULL,
  tab_key           TEXT NOT NULL,
  browser_window_id TEXT NOT NULL,
  group_key         TEXT,
  tab_index         INTEGER NOT NULL,
  url               TEXT NOT NULL,
  title             TEXT,
  favicon_hash      TEXT,
  pinned            INTEGER NOT NULL DEFAULT 0,
  active            INTEGER NOT NULL DEFAULT 0,
  muted             INTEGER NOT NULL DEFAULT 0,
  last_accessed     INTEGER,
  PRIMARY KEY (snapshot_id, tab_key)
);

-- Private/incognito tabs. Encrypted, TTL'd, NEVER cloud-synced.
CREATE TABLE tabs_private (
  snapshot_id       INTEGER NOT NULL,
  tab_key           TEXT NOT NULL,
  browser_window_id TEXT NOT NULL,
  tab_index         INTEGER NOT NULL,
  -- AES-256-GCM over the JSON {url,title,pinned,active,muted}. No plaintext columns.
  nonce             BLOB NOT NULL,
  ciphertext        BLOB NOT NULL,
  key_id            INTEGER NOT NULL,
  expires_at        INTEGER NOT NULL,   -- captured_at + private_ttl_hours
  PRIMARY KEY (snapshot_id, tab_key),
  FOREIGN KEY (key_id) REFERENCES crypto_keys(id)
);

CREATE INDEX idx_tabs_private_expiry ON tabs_private(expires_at);
```

Note what is *absent* from `tabs_private`: no `url`, no `title`, no `favicon_hash`. The
URL is inside the ciphertext and nothing else. A person with the raw `.db` file learns
only that N private tabs existed in M windows at time T. See
[06-privacy-security.md](06-privacy-security.md) for why even that is a disclosure worth
naming.

### Keys

```sql
CREATE TABLE crypto_keys (
  id          INTEGER PRIMARY KEY,
  purpose     TEXT NOT NULL,          -- 'private_tabs'
  wrapped_key BLOB NOT NULL,          -- DPAPI(CRYPTPROTECT_UI_FORBIDDEN) over a random 32-byte DEK
  wrap_method TEXT NOT NULL,          -- 'dpapi-user-v1'
  created_at  INTEGER NOT NULL,
  retired_at  INTEGER
);
```

### Restore audit

```sql
CREATE TABLE restore_runs (
  id             INTEGER PRIMARY KEY,
  snapshot_id    INTEGER NOT NULL,
  started_at     INTEGER NOT NULL,
  finished_at    INTEGER,
  mode           TEXT NOT NULL CHECK (mode IN ('ask','auto','manual')),
  undo_snapshot_id INTEGER            -- the 'pre_restore' snapshot
);

CREATE TABLE restore_items (
  run_id     INTEGER NOT NULL,
  item_kind  TEXT NOT NULL CHECK (item_kind IN ('app','browser_window','tab')),
  item_key   TEXT NOT NULL,
  status     TEXT NOT NULL CHECK (status IN ('pending','launched','placed','skipped','failed')),
  detail     TEXT,
  PRIMARY KEY (run_id, item_kind, item_key)
);
```

`restore_items` is what drives the progress UI and, more importantly, what makes
failures legible instead of silent. "7 of 9 apps restored, Photoshop needed admin and
Slack is no longer installed" is a good outcome; "some stuff came back" is not.

### Journal

```sql
CREATE TABLE journal (
  id         INTEGER PRIMARY KEY,
  at         INTEGER NOT NULL,
  source     TEXT NOT NULL,   -- 'agent' | 'chrome' | 'edge' | 'firefox'
  event      TEXT NOT NULL,   -- 'tab_created', 'window_moved', ...
  payload    TEXT NOT NULL    -- JSON
);
```

The journal is a **debugging and recovery aid, not the primary store** - live state is
materialized into `snapshot_id = 0` as events arrive. The journal is capped at 50k rows
and trimmed on a timer. Private-window events are never journaled in plaintext; they are
either omitted or recorded as `{"private": true}` with no URL.

## Retention

| Data | Rule |
|---|---|
| Live state (`snapshot_id = 0`) | Always present, continuously overwritten |
| Snapshots | Keep newest `snapshot_retention_count` (default 20) + all `pinned = 1` |
| `tabs_private` | Hard-`DELETE` where `expires_at < now`, on a 5-minute timer *and* at startup |
| Journal | 50k rows or 7 days, whichever is smaller |
| Icons | Content-addressed; GC'd when unreferenced |

A `VACUUM` runs after any bulk private-row deletion. Without it, plaintext-adjacent
material can linger in freelist pages that a raw file scan would find - deletion in
SQLite does not zero pages by default. Also set `PRAGMA secure_delete=ON` for the
connection that touches `tabs_private`.
