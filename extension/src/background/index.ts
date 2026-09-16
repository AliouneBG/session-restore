/**
 * Background entry point.
 *
 * MV3 rules this file lives by (docs/07-extension.md):
 *
 * - No module-level mutable state that matters. Treat every top-level variable as if
 *   it is wiped between any two events, because it is.
 * - Anything that must survive lives in `chrome.storage.session`.
 * - `chrome.alarms`, never `setTimeout`, for anything periodic.
 * - No attempt to keep the worker alive.
 */

import { detectCaps } from "../shared/caps.js";
import { attach } from "./collector.js";
import { emptyState, Outbox, type PendingState } from "./outbox.js";
import { AgentPort } from "./port.js";
import { ALARM_NAME, runReconcile, scheduleReconcile } from "./reconcile.js";
import { applyRestore } from "./restore.js";
import type { StateBody } from "../shared/protocol.generated.js";

const OUTBOX_KEY = "sr_outbox";
const SETTINGS_KEY = "sr_settings";

interface CachedSettings {
  captureEnabled: boolean;
  capturePrivate: boolean;
  reconcileIntervalS: number;
}

const DEFAULTS: CachedSettings = {
  captureEnabled: true,
  // Off until the agent says otherwise. Defaulting to true would mean a worker that
  // starts before the first hello_ack captures private tabs the user never enabled.
  capturePrivate: false,
  reconcileIntervalS: 60,
};

// Per-generation cache only. Always reloaded from storage.session on wake.
let settings: CachedSettings = { ...DEFAULTS };

async function loadSettings(): Promise<CachedSettings> {
  try {
    const got = await chrome.storage.session.get(SETTINGS_KEY);
    const s = got[SETTINGS_KEY] as Partial<CachedSettings> | undefined;
    settings = { ...DEFAULTS, ...(s ?? {}) };
  } catch {
    settings = { ...DEFAULTS };
  }
  return settings;
}

async function saveSettings(s: CachedSettings): Promise<void> {
  settings = s;
  try {
    await chrome.storage.session.set({ [SETTINGS_KEY]: s });
  } catch {
    // storage.session can be unavailable in odd states; the next hello_ack restores it.
  }
}

const port = new AgentPort({
  onMessage: (msg) => {
    switch (msg.type) {
      case "hello_ack":
        void saveSettings({
          captureEnabled: msg.body.capture_enabled,
          capturePrivate: msg.body.capture_private,
          reconcileIntervalS: msg.body.reconcile_interval_s,
        });
        scheduleReconcile(msg.body.reconcile_interval_s);
        break;

      case "settings_changed":
        void saveSettings({
          captureEnabled: msg.body.capture_enabled,
          capturePrivate: msg.body.capture_private,
          reconcileIntervalS: msg.body.reconcile_interval_s,
        });
        // If private capture was just turned off, stop holding anything private.
        if (!msg.body.capture_private) void dropPendingPrivate();
        break;

      case "restore_session":
        void applyRestore(msg.body).then((items) => {
          try {
            port.send("restore_result", { run_id: msg.body.run_id, items } as never);
          } catch {
            // The agent records its own per-item outcomes; losing the echo is not
            // worth failing a restore over.
          }
        });
        break;

      case "agent_unavailable":
        break;
    }
  },
  onStatusChange: (connected, reason) => {
    chrome.action?.setBadgeText({ text: connected ? "" : "!" });
    if (!connected && reason) console.warn("[session-restore]", reason);
  },
});

const outbox = new Outbox({
  send: async (batch: StateBody) => {
    // Throwing makes the outbox retain the batch rather than drop it.
    port.send("tab_delta", batch);
  },
  persist: async (state) => {
    try {
      await chrome.storage.session.set({ [OUTBOX_KEY]: state });
    } catch {
      // Losing the mirror costs us at most one reconcile interval.
    }
  },
  setTimer: (fn, ms) => setTimeout(fn, ms),
  clearTimer: (h) => clearTimeout(h as ReturnType<typeof setTimeout>),
});

/** Discards any pending private deltas after capture is turned off mid-session. */
async function dropPendingPrivate(): Promise<void> {
  const pending = outbox.snapshot();
  for (const [key, tab] of Object.entries(pending.tabs)) {
    if (tab.private) delete pending.tabs[key];
  }
  for (const [key, w] of Object.entries(pending.windows)) {
    if (w.private) delete pending.windows[key];
  }
  try {
    await chrome.storage.session.set({ [OUTBOX_KEY]: pending });
  } catch {
    /* best effort */
  }
}

async function hydrateOutbox(): Promise<void> {
  try {
    const got = await chrome.storage.session.get(OUTBOX_KEY);
    outbox.hydrate((got[OUTBOX_KEY] as PendingState | undefined) ?? emptyState());
  } catch {
    /* nothing to recover */
  }
}

async function sayHello(): Promise<void> {
  const caps = detectCaps();
  let incognitoAccess = false;
  try {
    incognitoAccess = await chrome.extension.isAllowedIncognitoAccess();
  } catch {
    // Not available in every context; absence means no access.
  }

  port.send("hello", {
    ext_version: chrome.runtime.getManifest().version,
    browser_version: navigator.userAgent,
    incognito_access: incognitoAccess,
    capabilities: caps.capabilities,
  });
}

async function boot(): Promise<void> {
  await loadSettings();
  await hydrateOutbox();

  attach({
    outbox,
    capturePrivate: () => settings.capturePrivate,
  });

  try {
    await sayHello();
  } catch {
    // The port schedules its own retry; the reconcile alarm keeps us correct.
  }

  // Establish ground truth immediately rather than waiting a full interval.
  await reconcileNow();
  scheduleReconcile(settings.reconcileIntervalS);
}

async function reconcileNow(): Promise<void> {
  try {
    await runReconcile({
      capturePrivate: () => settings.capturePrivate,
      send: async (body) => {
        port.send("full_state", body);
      },
    });
  } catch {
    // Agent down. The alarm will try again; nothing is lost because the next
    // successful reconcile is authoritative regardless of what was missed.
  }
}

/// Guards against booting more than once per worker generation.
///
/// `onStartup`, `onInstalled`, and the module-level call can all fire for the same
/// worker, which previously sent three `hello`s and three full reconciles back to back
/// - visible in the agent log as duplicate messages, and wasted work on every wake.
/// A module-level variable is the right scope here: it is reset by eviction, which is
/// exactly when we do want to boot again.
let booting: Promise<void> | null = null;

function bootOnce(): Promise<void> {
  if (!booting) booting = boot();
  return booting;
}

chrome.runtime.onStartup.addListener(() => void bootOnce());
chrome.runtime.onInstalled.addListener(() => void bootOnce());

chrome.alarms.onAlarm.addListener((alarm) => {
  if (alarm.name !== ALARM_NAME) return;
  void (async () => {
    await loadSettings();
    if (!settings.captureEnabled) return;
    await reconcileNow();
  })();
});

// Firefox implements onSuspend for MV3 event pages; Chrome does not implement it for
// service workers at all. Opportunistic only - the design never depends on it.
if (typeof chrome.runtime.onSuspend !== "undefined") {
  chrome.runtime.onSuspend.addListener(() => {
    void outbox.flush();
  });
}

// The worker may be started by an event rather than onStartup/onInstalled, so make
// sure the listeners are attached and state is hydrated in that case too.
void bootOnce();
