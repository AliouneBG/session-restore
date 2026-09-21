/**
 * Executes a restore plan against the browser.
 *
 * Ordering and the lazy-loading strategy come from docs/04-restore.md. The two rules
 * that matter most are enforced in `plan.ts`, not here: diff before creating, and
 * never close anything.
 *
 * `closeRestoredTabs` at the bottom is the one exception, and it is not a hole in that
 * rule. A restore never closes; *undoing* one closes only what that same restore
 * created, and only while the tab still shows the URL it was created with.
 */

import { hasCap } from "../shared/caps.js";
import { isRestorable } from "../shared/normalize.js";
import { orderForCreation, planRestore, type OpenTab } from "./plan.js";
import type {
  CloseTabsBody,
  RestoreResultItem,
  RestoreSessionBody,
  RestoreTab,
  RestoreWindow,
} from "../shared/protocol.generated.js";

/**
 * How long to wait for the browser's own session restore before looking at what is
 * open. Too short and we diff against an empty window and duplicate everything.
 */
export const SETTLE_MS = 3000;

const PLACEHOLDER_PAGE = "pages/restore.html";

/**
 * URL for a lazily-restored tab.
 *
 * On Chromium there is no `discarded` option on `tabs.create`, and calling
 * `tabs.discard()` after creation is not a substitute: the page has already started
 * loading and hit the network by then, so a restored session would silently fetch
 * every site. The placeholder page never touches the network until the user activates
 * the tab, and it keeps the title readable in the tab strip.
 */
export function placeholderUrl(tab: RestoreTab): string {
  const u = new URL(chrome.runtime.getURL(PLACEHOLDER_PAGE));
  u.searchParams.set("u", tab.url);
  if (tab.title) u.searchParams.set("t", tab.title);
  return u.toString();
}

async function currentlyOpen(): Promise<OpenTab[]> {
  const out: OpenTab[] = [];
  for (const w of await chrome.windows.getAll({ populate: true })) {
    if (w.id === undefined) continue;
    for (const t of w.tabs ?? []) {
      const url = t.url ?? t.pendingUrl ?? "";
      if (url) out.push({ windowRef: String(w.id), url });
    }
  }
  return out;
}

export async function applyRestore(body: RestoreSessionBody): Promise<RestoreResultItem[]> {
  const results: RestoreResultItem[] = [];

  // Let the browser finish its own restore before deciding what is missing.
  await new Promise((r) => setTimeout(r, SETTLE_MS));

  const open = await currentlyOpen();
  const plan = planRestore(body.windows, open);
  const lazy = body.lazy !== false;

  for (const { source, tabs } of plan.newWindows) {
    try {
      await createWindow(source, tabs, lazy, results);
    } catch (e) {
      for (const t of tabs) {
        results.push({ tab_key: t.url, status: "failed", detail: describe(e) });
      }
    }
  }

  // Report tabs that were already open, so the audit trail accounts for every tab in
  // the offer rather than leaving them stuck at "pending" forever.
  for (const w of body.windows) {
    for (const t of w.tabs) {
      const planned =
        plan.addToExisting.some((p) => p.tab === t) ||
        plan.newWindows.some((n) => n.tabs.includes(t));
      if (!planned) {
        results.push({ tab_key: t.url, status: "already_open" });
      }
    }
  }

  for (const { tab, targetWindowRef } of plan.addToExisting) {
    if (targetWindowRef === null) continue;
    try {
      await createTab(Number(targetWindowRef), tab, lazy);
      results.push({ tab_key: tab.url, status: "created" });
    } catch (e) {
      results.push({ tab_key: tab.url, status: "failed", detail: describe(e) });
    }
  }

  return results;
}

async function createWindow(
  source: RestoreWindow,
  tabs: RestoreTab[],
  lazy: boolean,
  results: RestoreResultItem[],
): Promise<void> {
  const ordered = orderForCreation(tabs).filter((t) => {
    if (isRestorable(t.url)) return true;
    results.push({ tab_key: t.url, status: "skipped", detail: "scheme cannot be restored" });
    return false;
  });
  if (ordered.length === 0) return;

  // The active tab loads eagerly so the window is immediately useful; everything
  // else can stay unloaded until the user reaches for it.
  const first = ordered.find((t) => t.active) ?? ordered[0]!;
  const rest = ordered.filter((t) => t !== first);

  const created = await chrome.windows.create({
    url: first.url,
    incognito: source.private ?? false,
    ...(source.x !== undefined ? { left: source.x } : {}),
    ...(source.y !== undefined ? { top: source.y } : {}),
    ...(source.w !== undefined ? { width: source.w } : {}),
    ...(source.h !== undefined ? { height: source.h } : {}),
  });
  results.push({ tab_key: first.url, status: "created" });

  const windowId = created?.id;
  if (windowId === undefined) throw new Error("window creation returned no id");

  for (const t of rest) {
    try {
      await createTab(windowId, t, lazy);
      results.push({ tab_key: t.url, status: "created" });
    } catch (e) {
      results.push({ tab_key: t.url, status: "failed", detail: describe(e) });
    }
  }

  // `windows.create` frequently ignores `state`, so re-apply it afterwards.
  if (source.state && source.state !== "normal") {
    try {
      await chrome.windows.update(windowId, { state: source.state });
    } catch {
      // Cosmetic; a window in the wrong state is still a restored window.
    }
  }
}

async function createTab(windowId: number, tab: RestoreTab, lazy: boolean): Promise<void> {
  if (!isRestorable(tab.url)) throw new Error("scheme cannot be restored");

  // Firefox supports creating a tab already discarded, which is strictly better than
  // the placeholder: real title, real favicon, no navigation at all.
  if (lazy && hasCap("discarded_create")) {
    await chrome.tabs.create({
      windowId,
      url: tab.url,
      index: tab.index,
      pinned: tab.pinned ?? false,
      active: false,
      ...({ discarded: true, title: tab.title } as object),
    });
    return;
  }

  await chrome.tabs.create({
    windowId,
    url: lazy ? placeholderUrl(tab) : tab.url,
    index: tab.index,
    pinned: tab.pinned ?? false,
    active: false,
  });
}

function describe(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

/**
 * Closes tabs a restore opened.
 *
 * Putting windows back is only half an undo: it never removes what the restore added.
 * This is the other half, and it is deliberately narrow in two ways.
 *
 * The agent sends only URLs it recorded itself as having *created*, never ones that
 * were already open when the restore ran. And a tab only closes if it is still showing
 * that exact URL, so one the user has since navigated away from is left alone. Both
 * rules point the same way: closing the wrong tab destroys work, and leaving one open
 * costs a click.
 */
export async function closeRestoredTabs(
  body: CloseTabsBody,
): Promise<{ closed: number; notFound: number }> {
  const wanted = new Set(body.urls);
  if (wanted.size === 0) return { closed: 0, notFound: 0 };

  let all: chrome.tabs.Tab[] = [];
  try {
    all = await chrome.tabs.query({});
  } catch {
    return { closed: 0, notFound: wanted.size };
  }

  const matched = all.filter((t) => typeof t.url === "string" && wanted.has(t.url));
  const ids = matched
    .map((t) => t.id)
    .filter((id): id is number => typeof id === "number");

  let closed = 0;
  for (const id of ids) {
    try {
      await chrome.tabs.remove(id);
      closed += 1;
    } catch {
      // The tab went away on its own between the query and the close, which is the
      // outcome we wanted anyway.
    }
  }

  return { closed, notFound: Math.max(0, wanted.size - closed) };
}
