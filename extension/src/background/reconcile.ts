/**
 * T1: the periodic full reconcile.
 *
 * This is the floor on correctness, and the piece to build first (docs/09-roadmap.md).
 * Chrome MV3 has no shutdown event and evicts the service worker after ~30s idle, so
 * `tabs.onRemoved` can be missed entirely. Without a periodic authoritative sweep,
 * closed tabs would linger in the store forever and a power cut could lose an
 * arbitrary amount of session.
 *
 * With it, worst-case staleness is one interval - 60s - no matter what else fails.
 *
 * Driven by `chrome.alarms`, never `setTimeout`: timers do not survive eviction, while
 * alarms wake the worker. Chromium's minimum alarm period is 1 minute, which is
 * exactly our interval and therefore a hard floor we cannot tune below.
 */

import { enqueueTab, groupToDelta, windowToDelta, windowKey } from "./collector.js";
import type { StateBody, TabDelta, TabGroupDelta, BrowserWindowDelta } from "../shared/protocol.generated.js";

export const ALARM_NAME = "session-restore-reconcile";
export const MIN_INTERVAL_MINUTES = 1;

export interface ReconcileDeps {
  capturePrivate: () => boolean;
  /** Sends the authoritative snapshot. */
  send: (body: StateBody) => Promise<void>;
}

export function scheduleReconcile(intervalSeconds: number): void {
  const minutes = Math.max(MIN_INTERVAL_MINUTES, intervalSeconds / 60);
  chrome.alarms.create(ALARM_NAME, {
    periodInMinutes: minutes,
    // Fire one interval from now; startup already runs a reconcile directly.
    delayInMinutes: minutes,
  });
}

/**
 * Builds and sends the authoritative current state.
 *
 * Everything the browser reports is included; anything the agent holds for this
 * browser that is absent here gets reaped on the other side.
 */
export async function runReconcile(deps: ReconcileDeps): Promise<void> {
  const capturePrivate = deps.capturePrivate();

  const windows: BrowserWindowDelta[] = [];
  const tabs: TabDelta[] = [];
  const groups: TabGroupDelta[] = [];

  // Collect via a throwaway outbox-shaped sink so the same mapping and the same
  // private-drop rule apply here as on the event path. Two code paths producing
  // deltas by different rules is exactly how a privacy leak gets introduced.
  const sink = {
    tab: (d: TabDelta) => tabs.push(d),
    window: (d: BrowserWindowDelta) => windows.push(d),
    group: (d: TabGroupDelta) => groups.push(d),
  };

  const allWindows = await chrome.windows.getAll({ populate: true });
  for (const w of allWindows) {
    if (w.incognito && !capturePrivate) continue;

    const wd = windowToDelta(w);
    if (wd) sink.window(wd);

    for (const t of w.tabs ?? []) {
      enqueueTab(t, {
        outbox: sink as never,
        capturePrivate: deps.capturePrivate,
      });
    }
  }

  if (__HAS_TAB_GROUPS__) {
    try {
      for (const g of await chrome.tabGroups!.query({})) {
        const gd = groupToDelta(g);
        if (gd) sink.group(gd);
      }
    } catch {
      // Group query can fail transiently during window teardown. Tabs still
      // reconcile correctly; groups reattach on the next pass.
    }
  }

  await deps.send({ windows, tabs, groups });
}

/**
 * True if a window id belongs to a window we are still tracking.
 *
 * Used by the agent-side reap via `full_state`; kept here so the rule that private
 * windows are excluded when capture is off lives next to the code that applies it.
 */
export function trackedWindowIds(windows: BrowserWindowDelta[]): Set<string> {
  return new Set(windows.map((w) => w.window_id));
}

export { windowKey };
