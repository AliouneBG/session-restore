/**
 * Debounced, coalescing outbox for tab/window deltas.
 *
 * Three constraints shape this (docs/03-capture.md, docs/07-extension.md):
 *
 * 1. `tabs.onUpdated` fires several times per navigation (loading -> title -> favicon
 *    -> complete). Forwarding each would produce roughly 10x the necessary IPC, so
 *    changes coalesce per tab and flush after a quiet period.
 * 2. The MV3 service worker can be evicted mid-debounce. The pending set is therefore
 *    mirrored to `storage.session` on **every mutation**, not on flush - a flush that
 *    never happens must not lose the deltas.
 * 3. Native messaging caps a message at 1 MB, so batches are split at a tab count that
 *    stays well clear of it.
 *
 * Storage and the clock are injected so this is testable as pure logic, with no
 * `chrome` global.
 */

import type { BrowserWindowDelta, StateBody, TabDelta, TabGroupDelta } from "../shared/protocol.generated.js";

export const DEBOUNCE_MS = 2000;
export const MAX_PENDING_BEFORE_FLUSH = 50;
/** Kept well under the 1 MB native messaging limit. */
export const MAX_TABS_PER_MESSAGE = 200;

export interface PendingState {
  tabs: Record<string, TabDelta>;
  windows: Record<string, BrowserWindowDelta>;
  groups: Record<string, TabGroupDelta>;
}

export interface OutboxDeps {
  /** Sends one batch. Rejecting means "could not send"; the batch is retained. */
  send: (batch: StateBody) => Promise<void>;
  /** Mirrors pending state so an eviction does not lose it. */
  persist: (state: PendingState) => Promise<void>;
  setTimer: (fn: () => void, ms: number) => unknown;
  clearTimer: (handle: unknown) => void;
}

export function emptyState(): PendingState {
  return { tabs: {}, windows: {}, groups: {} };
}

export class Outbox {
  private pending: PendingState = emptyState();
  private timer: unknown = null;
  private flushing = false;

  constructor(private deps: OutboxDeps) {}

  /** Restores pending state left behind by a previous service worker generation. */
  hydrate(state: PendingState | null | undefined): void {
    if (!state) return;
    this.pending = {
      tabs: { ...state.tabs, ...this.pending.tabs },
      windows: { ...state.windows, ...this.pending.windows },
      groups: { ...state.groups, ...this.pending.groups },
    };
    if (this.pendingCount() > 0) this.schedule();
  }

  snapshot(): PendingState {
    return this.pending;
  }

  pendingCount(): number {
    return (
      Object.keys(this.pending.tabs).length +
      Object.keys(this.pending.windows).length +
      Object.keys(this.pending.groups).length
    );
  }

  tab(d: TabDelta): void {
    // Last write wins per tab: an upsert followed by a remove is a remove, and three
    // onUpdated events for one navigation collapse to one delta.
    this.pending.tabs[d.tab_key] = d;
    this.touch();
  }

  window(d: BrowserWindowDelta): void {
    this.pending.windows[d.window_id] = d;
    this.touch();
  }

  group(d: TabGroupDelta): void {
    this.pending.groups[d.group_key] = d;
    this.touch();
  }

  private touch(): void {
    // Persist before scheduling: if the worker dies between the two, the data is
    // already safe and the next generation picks it up.
    void this.deps.persist(this.pending);
    if (this.pendingCount() >= MAX_PENDING_BEFORE_FLUSH) {
      void this.flush();
      return;
    }
    this.schedule();
  }

  private schedule(): void {
    if (this.timer !== null) this.deps.clearTimer(this.timer);
    this.timer = this.deps.setTimer(() => {
      this.timer = null;
      void this.flush();
    }, DEBOUNCE_MS);
  }

  /**
   * Sends everything pending, in batches.
   *
   * On failure the un-sent remainder is put back and left for the next trigger. Losing
   * deltas on a transient disconnect would silently corrupt the stored session, and
   * the 60s reconcile would take up to a minute to notice.
   */
  async flush(): Promise<void> {
    if (this.flushing) return;
    if (this.timer !== null) {
      this.deps.clearTimer(this.timer);
      this.timer = null;
    }
    if (this.pendingCount() === 0) return;

    this.flushing = true;
    const outgoing = this.pending;
    this.pending = emptyState();

    try {
      for (const batch of splitBatches(outgoing)) {
        await this.deps.send(batch);
      }
      await this.deps.persist(this.pending);
    } catch {
      // Merge back, letting anything newer that arrived during the send win.
      this.pending = {
        tabs: { ...outgoing.tabs, ...this.pending.tabs },
        windows: { ...outgoing.windows, ...this.pending.windows },
        groups: { ...outgoing.groups, ...this.pending.groups },
      };
      await this.deps.persist(this.pending);
      this.schedule();
    } finally {
      this.flushing = false;
    }
  }
}

/**
 * Splits pending state into messages no larger than `MAX_TABS_PER_MESSAGE` tabs.
 *
 * Windows and groups ride along with the first batch: there are few of them, and a tab
 * referencing a window the agent has not seen yet is harmless (the agent upserts by
 * key), whereas splitting them would need ordering guarantees we do not have.
 */
export function splitBatches(state: PendingState): StateBody[] {
  const tabs = Object.values(state.tabs);
  const windows = Object.values(state.windows);
  const groups = Object.values(state.groups);

  if (tabs.length === 0) {
    if (windows.length === 0 && groups.length === 0) return [];
    return [{ windows, tabs: [], groups }];
  }

  const out: StateBody[] = [];
  for (let i = 0; i < tabs.length; i += MAX_TABS_PER_MESSAGE) {
    const slice = tabs.slice(i, i + MAX_TABS_PER_MESSAGE);
    out.push(
      i === 0
        ? { windows, tabs: slice, groups }
        : { windows: [], tabs: slice, groups: [] },
    );
  }
  return out;
}
