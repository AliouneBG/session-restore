/**
 * Translates browser events into protocol deltas and feeds the outbox.
 *
 * The mapping functions are pure and exported so they can be tested without a `chrome`
 * global; only `attach()` touches the browser.
 */

import { extractPlaceholderTarget, isRestorable } from "../shared/normalize.js";
import type {
  BrowserWindowDelta,
  TabDelta,
  TabGroupDelta,
  WindowState,
} from "../shared/protocol.generated.js";
import type { Outbox } from "./outbox.js";

/**
 * Stable key for a tab within a browser session.
 *
 * Window id is included so the key stays meaningful when a tab is dragged between
 * windows, and so the agent can reap per-window during a reconcile.
 */
export function tabKey(windowId: number, tabId: number): string {
  return `w${windowId}:t${tabId}`;
}

export function windowKey(windowId: number): string {
  return `w${windowId}`;
}

export function groupKey(windowId: number, groupId: number): string {
  return `w${windowId}:g${groupId}`;
}

/** Chrome's sentinel for "this tab is not in a group". */
const TAB_GROUP_ID_NONE = -1;

/** Returns the page a lazy placeholder stands for, or null for any other URL. */
function unwrapPlaceholder(raw: string): string | null {
  if (!raw.startsWith("chrome-extension://") && !raw.startsWith("moz-extension://")) {
    return null;
  }
  try {
    return extractPlaceholderTarget(new URL(raw));
  } catch {
    return null;
  }
}

export function tabToDelta(t: chrome.tabs.Tab): TabDelta | null {
  if (t.id === undefined || t.id < 0 || t.windowId === undefined) return null;

  // A tab still showing our lazy placeholder represents the page it stands for, not
  // the placeholder itself. Storing the placeholder would mean that rebooting twice
  // before opening a restored tab saves a placeholder-of-a-placeholder, and the real
  // URL would drift further out of reach with each cycle.
  const raw = t.url ?? t.pendingUrl ?? "";
  const url = unwrapPlaceholder(raw) ?? raw;
  const gid = (t as { groupId?: number }).groupId;

  return {
    op: "upsert",
    tab_key: tabKey(t.windowId, t.id),
    window_id: windowKey(t.windowId),
    group_key:
      gid !== undefined && gid !== TAB_GROUP_ID_NONE ? groupKey(t.windowId, gid) : null,
    index: t.index,
    url,
    title: t.title ?? "",
    favicon_hash: null,
    pinned: t.pinned ?? false,
    active: t.active ?? false,
    muted: t.mutedInfo?.muted ?? false,
    last_accessed: (t as { lastAccessed?: number }).lastAccessed ?? null,
    // Set from the window, since a tab does not report it directly.
    private: t.incognito ?? false,
    restorable: isRestorable(url),
  };
}

export function windowToDelta(w: chrome.windows.Window): BrowserWindowDelta | null {
  if (w.id === undefined || w.id < 0) return null;
  return {
    op: "upsert",
    window_id: windowKey(w.id),
    private: w.incognito ?? false,
    state: mapWindowState(w.state),
    x: w.left ?? 0,
    y: w.top ?? 0,
    w: w.width ?? 0,
    h: w.height ?? 0,
    focused: w.focused ?? false,
  };
}

export function mapWindowState(s: string | undefined): WindowState {
  switch (s) {
    case "maximized":
      return "maximized";
    case "minimized":
      return "minimized";
    case "fullscreen":
      return "fullscreen";
    default:
      // "normal", "locked-fullscreen", or undefined on older builds.
      return "normal";
  }
}

export function groupToDelta(g: chrome.tabGroups.TabGroup): TabGroupDelta | null {
  if (g.id === undefined || g.windowId === undefined) return null;
  return {
    op: "upsert",
    group_key: groupKey(g.windowId, g.id),
    window_id: windowKey(g.windowId),
    title: g.title ?? null,
    color: g.color ?? null,
    collapsed: g.collapsed ?? false,
  };
}

export interface CollectorOptions {
  outbox: Outbox;
  /** Mirrors the agent's setting; when false, private tabs are never enqueued. */
  capturePrivate: () => boolean;
}

/**
 * Enqueues a tab, dropping private ones at the source when capture is off.
 *
 * This is the first of the three redundant enforcement layers in docs/06: the cheapest
 * place to enforce a privacy rule is the earliest one. The agent drops them again, and
 * the type system makes them unsyncable - but data never sent is data that cannot leak
 * through a bug in either of those.
 */
export function enqueueTab(t: chrome.tabs.Tab, opts: CollectorOptions): void {
  const d = tabToDelta(t);
  if (!d) return;
  if (d.private && !opts.capturePrivate()) return;
  opts.outbox.tab(d);
}

export function attach(opts: CollectorOptions): void {
  const { outbox } = opts;

  chrome.tabs.onCreated.addListener((tab) => enqueueTab(tab, opts));

  chrome.tabs.onUpdated.addListener((_id, changeInfo, tab) => {
    // onUpdated fires several times per navigation. Act only on changes that alter
    // what we store; the rest is noise that would multiply IPC roughly tenfold.
    const relevant =
      changeInfo.status === "complete" ||
      changeInfo.url !== undefined ||
      changeInfo.title !== undefined ||
      changeInfo.pinned !== undefined ||
      changeInfo.mutedInfo !== undefined;
    if (relevant) enqueueTab(tab, opts);
  });

  chrome.tabs.onMoved.addListener((id, info) => {
    chrome.tabs.get(id).then(
      (tab) => enqueueTab(tab, opts),
      () => {
        // Tab vanished between the event and the lookup. Record the removal so we do
        // not keep a row for a tab that no longer exists.
        outbox.tab({
          op: "remove",
          tab_key: tabKey(info.windowId, id),
          window_id: windowKey(info.windowId),
        });
      },
    );
  });

  chrome.tabs.onActivated.addListener((info) => {
    chrome.tabs.get(info.tabId).then(
      (tab) => enqueueTab(tab, opts),
      () => {},
    );
  });

  chrome.tabs.onAttached.addListener((id, info) => {
    chrome.tabs.get(id).then(
      (tab) => enqueueTab(tab, opts),
      () => {},
    );
    // The tab left its old window under its old key; drop that row explicitly.
    outbox.tab({
      op: "remove",
      tab_key: tabKey(info.newWindowId, id),
      window_id: windowKey(info.newWindowId),
    });
  });

  chrome.tabs.onDetached.addListener((id, info) => {
    outbox.tab({
      op: "remove",
      tab_key: tabKey(info.oldWindowId, id),
      window_id: windowKey(info.oldWindowId),
    });
  });

  chrome.tabs.onRemoved.addListener((id, info) => {
    outbox.tab({
      op: "remove",
      tab_key: tabKey(info.windowId, id),
      window_id: windowKey(info.windowId),
    });
  });

  chrome.windows.onCreated.addListener((w) => {
    const d = windowToDelta(w);
    if (!d) return;
    if (d.private && !opts.capturePrivate()) return;
    outbox.window(d);
  });

  chrome.windows.onRemoved.addListener((id) => {
    outbox.window({ op: "remove", window_id: windowKey(id) });
  });

  // Tab groups are Chromium-only; Firefox has no such API.
  if (typeof chrome.tabGroups !== "undefined") {
    const onGroup = (g: chrome.tabGroups.TabGroup) => {
      const d = groupToDelta(g);
      if (d) outbox.group(d);
    };
    chrome.tabGroups.onCreated.addListener(onGroup);
    chrome.tabGroups.onUpdated.addListener(onGroup);
    chrome.tabGroups.onRemoved.addListener((g) => {
      if (g.id !== undefined && g.windowId !== undefined) {
        outbox.group({
          op: "remove",
          group_key: groupKey(g.windowId, g.id),
          window_id: windowKey(g.windowId),
        });
      }
    });
  }
}
