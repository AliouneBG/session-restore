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
import { applyRestore, closeRestoredTabs } from "./restore.js";
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

      case "close_tabs":
        void closeRestoredTabs(msg.body).then(({ closed, notFound }) => {
          try {
            port.send("close_tabs_result", {
              run_id: msg.body.run_id,
              closed,
              not_found: notFound,
            } as never);
          } catch {
            // The agent records the request; losing the echo is not worth failing over.
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

    // Re-introduce ourselves on every reconnect, not only on worker start.
    //
    // hello is the only message carrying incognito_access and the extension version,
    // and the agent shows both in its settings window. Sending it once per service
    // worker meant a browser left open for a day never told the agent anything again,
    // so the agent's view of it aged out while the connection stayed healthy.
    if (connected) {
      void sayHello().catch(() => {
        // The port schedules its own retry; the reconcile alarm keeps us correct.
      });
    }
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

const PROFILE_ID_KEY = "sr_profile_id";

/**
 * A stable id for the profile this extension instance is running in.
 *
 * Chromium runs ONE browser process for every profile, so the agent cannot tell two
 * profiles apart from the process that spawned the relay: both report the same command
 * line, and their tabs collapse into a single bucket. Work tabs get restored into
 * Personal, and a session restored into the wrong profile is worse than none.
 *
 * `storage.local` is per-profile by definition, so an id kept there identifies the
 * profile without the extension ever being able to see the profile path. The value is
 * random and says nothing about the user.
 */
async function profileId(): Promise<string | undefined> {
  try {
    const stored = await chrome.storage.local.get(PROFILE_ID_KEY);
    const existing = stored?.[PROFILE_ID_KEY];
    if (typeof existing === "string" && existing.length > 0) return existing;

    const fresh = crypto.randomUUID();
    await chrome.storage.local.set({ [PROFILE_ID_KEY]: fresh });
    return fresh;
  } catch {
    // storage.local can be unavailable in odd states. The agent falls back to the
    // relay's view, which is what it did before this existed.
    return undefined;
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
    profile_id: await profileId(),
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
