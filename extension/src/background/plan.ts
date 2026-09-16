/**
 * Restore planning: decide what to create, given what is already open.
 *
 * This exists because of the failure mode most likely to make the product feel broken
 * (docs/04-restore.md). Chrome, Edge and Firefox all have their own "continue where
 * you left off". If it is on, the browser reopens the user's tabs by itself, and then
 * we inject the same tabs again and they get everything twice.
 *
 * Two rules, and the second is not negotiable:
 *
 * - **Reconcile, never blindly inject.** Diff the desired session against what is
 *   actually open and create only the difference.
 * - **Never close anything.** A restore tool that closes tabs is a tool that destroys
 *   work. We only ever add.
 *
 * The hard part is that stored windows carry IDs from a session that no longer exists,
 * so they must be matched to current windows by content rather than identity.
 */

import { normalizeUrl } from "../shared/normalize.js";
import type { RestoreTab, RestoreWindow } from "../shared/protocol.generated.js";

export interface OpenTab {
  windowRef: string;
  url: string;
}

export interface PlannedTab {
  tab: RestoreTab;
  /** Existing window to add it to, or null to open a new window for it. */
  targetWindowRef: string | null;
}

export interface RestorePlan {
  /** Windows to create, each with the tabs that go in it. */
  newWindows: { source: RestoreWindow; tabs: RestoreTab[] }[];
  /** Tabs to add to windows that already exist. */
  addToExisting: PlannedTab[];
  /** Tabs already open; counted so the summary can be honest about what it skipped. */
  alreadyOpen: number;
}

/**
 * Fraction of a stored window's tabs that must already be open in a current window
 * before we treat them as the same window.
 *
 * Tuned low deliberately. The browser's own restore is not always complete - it may
 * drop pinned tabs or fail on a few - and the cost of guessing wrong in each direction
 * is asymmetric: too high a threshold opens a duplicate window (annoying, visible,
 * user fixes it in one click), while too low merges two genuinely different windows
 * (confusing, and the user cannot easily undo it). Neither is destructive, which is
 * what makes a heuristic acceptable here at all.
 */
export const WINDOW_MATCH_THRESHOLD = 0.3;

export function planRestore(desired: RestoreWindow[], open: OpenTab[]): RestorePlan {
  // Index what is currently open, by window, as *counts* rather than a set.
  //
  // Having the same URL open in two tabs is a legitimate thing a user does, and it is
  // state worth restoring. Set semantics would quietly collapse those two tabs into
  // one, so occurrences are tracked and consumed one at a time.
  const openByWindow = new Map<string, Map<string, number>>();
  for (const t of open) {
    const key = normalizeUrl(t.url);
    if (key === "") continue;
    let counts = openByWindow.get(t.windowRef);
    if (!counts) {
      counts = new Map();
      openByWindow.set(t.windowRef, counts);
    }
    counts.set(key, (counts.get(key) ?? 0) + 1);
  }

  const plan: RestorePlan = { newWindows: [], addToExisting: [], alreadyOpen: 0 };
  const claimed = new Set<string>();

  for (const want of desired) {
    const wanted = want.tabs
      .map((t) => ({ tab: t, key: normalizeUrl(t.url) }))
      .filter((x) => x.key !== "");
    if (wanted.length === 0) continue;

    const match = bestMatch(
      wanted.map((x) => x.key),
      openByWindow,
      claimed,
    );

    if (match === null) {
      // Nothing resembling this window is open. Create it whole.
      plan.newWindows.push({ source: want, tabs: wanted.map((x) => x.tab) });
      continue;
    }

    // The browser already restored this window. Top up only what is missing.
    claimed.add(match);
    const present = openByWindow.get(match)!;
    for (const { tab, key } of wanted) {
      const remaining = present.get(key) ?? 0;
      if (remaining > 0) {
        // Consume one occurrence, so a URL the user had open twice and the browser
        // restored once still gets its second tab back.
        present.set(key, remaining - 1);
        plan.alreadyOpen++;
      } else {
        plan.addToExisting.push({ tab, targetWindowRef: match });
      }
    }
  }

  return plan;
}

/**
 * Finds the open window that best corresponds to a stored one.
 *
 * Scored as (shared tabs / stored tabs) rather than Jaccard: a current window that has
 * everything we want plus twenty unrelated tabs the user opened since is still the
 * right window to add to, and Jaccard would penalize it for those extras.
 */
function bestMatch(
  wantedKeys: string[],
  openByWindow: Map<string, Map<string, number>>,
  claimed: Set<string>,
): string | null {
  let best: string | null = null;
  let bestScore = 0;

  for (const [ref, present] of openByWindow) {
    if (claimed.has(ref)) continue;
    let shared = 0;
    for (const k of wantedKeys) if ((present.get(k) ?? 0) > 0) shared++;
    const score = shared / wantedKeys.length;
    if (score > bestScore) {
      bestScore = score;
      best = ref;
    }
  }

  return bestScore >= WINDOW_MATCH_THRESHOLD ? best : null;
}

/**
 * Orders tabs for creation within a window.
 *
 * Pinned first: the browser packs pinned tabs at the left, so creating an unpinned tab
 * before a pinned one shifts every index that follows.
 */
export function orderForCreation(tabs: RestoreTab[]): RestoreTab[] {
  return [...tabs].sort((a, b) => {
    const ap = a.pinned ? 0 : 1;
    const bp = b.pinned ? 0 : 1;
    if (ap !== bp) return ap - bp;
    return a.index - b.index;
  });
}
