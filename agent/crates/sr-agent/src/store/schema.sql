-- Session Restore schema, v1. See docs/02-data-model.md.
--
-- Every entity table carries `snapshot_id`, and 0 is reserved to mean LIVE current
-- state. Taking a snapshot is INSERT..SELECT with a new id, so "restore what was open"
-- and "restore last Tuesday" are the same query against the same tables.

PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS settings (
  key        TEXT PRIMARY KEY,
  value      TEXT NOT NULL,
  updated_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS app_rules (
  id         INTEGER PRIMARY KEY,
  match_kind TEXT NOT NULL CHECK (match_kind IN ('exe_path','aumid','exe_name','title_regex')),
  pattern    TEXT NOT NULL,
  action     TEXT NOT NULL CHECK (action IN ('capture','ignore','never_restore')),
  reason     TEXT,
  created_at INTEGER NOT NULL,
  UNIQUE (match_kind, pattern)
);

CREATE TABLE IF NOT EXISTS snapshots (
  id            INTEGER PRIMARY KEY,
  captured_at   INTEGER NOT NULL,
  kind          TEXT NOT NULL CHECK (kind IN ('live','shutdown','periodic','manual','pre_restore')),
  label         TEXT,
  machine_id    TEXT NOT NULL,
  topology_hash TEXT,
  pinned        INTEGER NOT NULL DEFAULT 0,
  app_count     INTEGER NOT NULL DEFAULT 0,
  tab_count     INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS displays (
  snapshot_id   INTEGER NOT NULL,
  display_key   TEXT NOT NULL,
  friendly_name TEXT,
  is_primary    INTEGER NOT NULL DEFAULT 0,
  bounds_x INTEGER NOT NULL, bounds_y INTEGER NOT NULL,
  bounds_w INTEGER NOT NULL, bounds_h INTEGER NOT NULL,
  work_x INTEGER, work_y INTEGER, work_w INTEGER, work_h INTEGER,
  dpi           INTEGER NOT NULL DEFAULT 96,
  PRIMARY KEY (snapshot_id, display_key)
);

CREATE TABLE IF NOT EXISTS apps (
  snapshot_id  INTEGER NOT NULL,
  app_key      TEXT NOT NULL,
  kind         TEXT NOT NULL CHECK (kind IN ('win32','uwp','unknown')),
  exe_path     TEXT,
  aumid        TEXT,
  display_name TEXT,
  icon_hash    TEXT,
  command_line TEXT,
  working_dir  TEXT,
  -- JSON array of env-folded paths the app had open. Sourced from the command line,
  -- which is the only place a *full* path appears reliably; window titles usually
  -- show a bare filename and guessing its directory would reopen the wrong file.
  documents    TEXT,
  is_browser   INTEGER NOT NULL DEFAULT 0,
  restore_tier TEXT NOT NULL CHECK (restore_tier IN ('A','B','C','D')),
  PRIMARY KEY (snapshot_id, app_key)
);

CREATE TABLE IF NOT EXISTS windows (
  snapshot_id  INTEGER NOT NULL,
  window_key   TEXT NOT NULL,
  app_key      TEXT NOT NULL,
  -- ALWAYS NULL when is_browser = 1. A browser window's title is the current page
  -- title, which for a private window is C4 data. Enforced in code and by a test;
  -- see docs/03-capture.md.
  title        TEXT,
  display_key  TEXT,
  norm_x INTEGER, norm_y INTEGER, norm_w INTEGER, norm_h INTEGER,
  show_cmd     TEXT NOT NULL DEFAULT 'normal'
                 CHECK (show_cmd IN ('normal','maximized','minimized')),
  z_order      INTEGER,
  virtual_desktop_id TEXT,
  is_browser   INTEGER NOT NULL DEFAULT 0,
  browser_window_id  TEXT,
  PRIMARY KEY (snapshot_id, window_key)
);

CREATE TABLE IF NOT EXISTS browser_windows (
  snapshot_id       INTEGER NOT NULL,
  browser_window_id TEXT NOT NULL,
  browser           TEXT NOT NULL CHECK (browser IN ('chrome','edge','firefox')),
  profile_key       TEXT NOT NULL,
  is_private        INTEGER NOT NULL DEFAULT 0,
  window_state      TEXT CHECK (window_state IN ('normal','maximized','minimized','fullscreen')),
  x INTEGER, y INTEGER, w INTEGER, h INTEGER,
  focused           INTEGER NOT NULL DEFAULT 0,
  updated_at        INTEGER NOT NULL,
  PRIMARY KEY (snapshot_id, browser_window_id)
);

CREATE TABLE IF NOT EXISTS tab_groups (
  snapshot_id       INTEGER NOT NULL,
  group_key         TEXT NOT NULL,
  browser_window_id TEXT NOT NULL,
  title             TEXT,
  color             TEXT,
  collapsed         INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (snapshot_id, group_key)
);

-- Normal (non-private) tabs. Plaintext, cloud-syncable.
CREATE TABLE IF NOT EXISTS tabs (
  snapshot_id       INTEGER NOT NULL,
  tab_key           TEXT NOT NULL,
  browser_window_id TEXT NOT NULL,
  group_key         TEXT,
  tab_index         INTEGER NOT NULL DEFAULT 0,
  url               TEXT NOT NULL,
  title             TEXT,
  favicon_hash      TEXT,
  pinned            INTEGER NOT NULL DEFAULT 0,
  active            INTEGER NOT NULL DEFAULT 0,
  muted             INTEGER NOT NULL DEFAULT 0,
  restorable        INTEGER NOT NULL DEFAULT 1,
  last_accessed     INTEGER,
  updated_at        INTEGER NOT NULL,
  PRIMARY KEY (snapshot_id, tab_key)
);

CREATE INDEX IF NOT EXISTS idx_tabs_window ON tabs(snapshot_id, browser_window_id);

-- Private/incognito tabs. Encrypted, TTL'd, NEVER cloud-synced.
--
-- Note what is absent: no url, no title, no favicon_hash. Those live inside the
-- ciphertext and nowhere else. Someone with the raw .db learns only that N private
-- tabs existed in M windows at time T - a residual disclosure documented in ADR-0004
-- rather than quietly omitted.
CREATE TABLE IF NOT EXISTS tabs_private (
  snapshot_id       INTEGER NOT NULL,
  tab_key           TEXT NOT NULL,
  browser_window_id TEXT NOT NULL,
  tab_index         INTEGER NOT NULL DEFAULT 0,
  nonce             BLOB NOT NULL,
  ciphertext        BLOB NOT NULL,
  key_id            INTEGER NOT NULL,
  expires_at        INTEGER NOT NULL,
  updated_at        INTEGER NOT NULL,
  PRIMARY KEY (snapshot_id, tab_key),
  FOREIGN KEY (key_id) REFERENCES crypto_keys(id)
);

CREATE INDEX IF NOT EXISTS idx_tabs_private_expiry ON tabs_private(expires_at);

CREATE TABLE IF NOT EXISTS crypto_keys (
  id          INTEGER PRIMARY KEY,
  purpose     TEXT NOT NULL,
  wrapped_key BLOB NOT NULL,
  wrap_method TEXT NOT NULL,
  created_at  INTEGER NOT NULL,
  retired_at  INTEGER
);

CREATE TABLE IF NOT EXISTS restore_runs (
  id               INTEGER PRIMARY KEY,
  snapshot_id      INTEGER NOT NULL,
  started_at       INTEGER NOT NULL,
  finished_at      INTEGER,
  mode             TEXT NOT NULL CHECK (mode IN ('ask','auto','manual')),
  undo_snapshot_id INTEGER
);

CREATE TABLE IF NOT EXISTS restore_items (
  run_id    INTEGER NOT NULL,
  item_kind TEXT NOT NULL CHECK (item_kind IN ('app','browser_window','tab')),
  item_key  TEXT NOT NULL,
  status    TEXT NOT NULL CHECK (status IN ('pending','launched','placed','skipped','failed')),
  detail    TEXT,
  PRIMARY KEY (run_id, item_kind, item_key),
  FOREIGN KEY (run_id) REFERENCES restore_runs(id) ON DELETE CASCADE
);

-- What each browser told us about itself, the last time it connected.
--
-- Deliberately not snapshot-scoped: this describes the *installation*, not a session,
-- so it is never copied into a snapshot. It exists so the settings and onboarding
-- windows can answer "is the extension actually working in Edge?" without guessing.
-- incognito_access in particular cannot be read from anywhere else: only the extension
-- can see whether the browser granted it, and it says so in every hello.
CREATE TABLE IF NOT EXISTS browser_status (
  browser          TEXT NOT NULL,
  profile_key      TEXT NOT NULL,
  ext_version      TEXT,
  incognito_access INTEGER NOT NULL DEFAULT 0,
  -- The Chromium profile *directory* (`Default`, `Profile 1`), which is the only
  -- thing that can reopen a specific profile. profile_key cannot: it is a hash, on
  -- purpose, because a profile path can contain the user's name. Only known-shaped
  -- directory names are ever written here; see watcher::profiles.
  profile_dir      TEXT,
  last_seen        INTEGER NOT NULL,
  PRIMARY KEY (browser, profile_key)
);

-- Debugging and recovery aid, not the primary store. Private events are never
-- journaled in plaintext; they appear as {"private":true} with no URL.
CREATE TABLE IF NOT EXISTS journal (
  id      INTEGER PRIMARY KEY,
  at      INTEGER NOT NULL,
  source  TEXT NOT NULL,
  event   TEXT NOT NULL,
  payload TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_journal_at ON journal(at);
