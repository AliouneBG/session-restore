import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import {
  Outbox,
  splitBatches,
  emptyState,
  DEBOUNCE_MS,
  MAX_PENDING_BEFORE_FLUSH,
  MAX_TABS_PER_MESSAGE,
  type PendingState,
} from "../src/background/outbox.js";
import type { StateBody, TabDelta } from "../src/shared/protocol.generated.js";

function tab(key: string, over: Partial<TabDelta> = {}): TabDelta {
  return {
    op: "upsert",
    tab_key: key,
    window_id: "w1",
    index: 0,
    url: `https://example.test/${key}`,
    ...over,
  } as TabDelta;
}

function harness(sendImpl?: (b: StateBody) => Promise<void>) {
  const sent: StateBody[] = [];
  const persisted: PendingState[] = [];
  const ob = new Outbox({
    send: async (b) => {
      if (sendImpl) return sendImpl(b);
      sent.push(b);
    },
    persist: async (s) => {
      // Deep copy: the outbox mutates its own state, and a live reference would make
      // every recorded snapshot look identical.
      persisted.push(JSON.parse(JSON.stringify(s)));
    },
    setTimer: (fn, ms) => setTimeout(fn, ms),
    clearTimer: (h) => clearTimeout(h as ReturnType<typeof setTimeout>),
  });
  return { ob, sent, persisted };
}

describe("Outbox", () => {
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => vi.useRealTimers());

  it("does not send before the debounce window elapses", async () => {
    const { ob, sent } = harness();
    ob.tab(tab("w1:t1"));
    await vi.advanceTimersByTimeAsync(DEBOUNCE_MS - 100);
    expect(sent).toHaveLength(0);
  });

  it("sends once the window is quiet", async () => {
    const { ob, sent } = harness();
    ob.tab(tab("w1:t1"));
    await vi.advanceTimersByTimeAsync(DEBOUNCE_MS + 10);
    expect(sent).toHaveLength(1);
    expect(sent[0]!.tabs).toHaveLength(1);
  });

  it("coalesces repeated changes to one tab into a single delta", async () => {
    // The onUpdated storm: loading -> title -> favicon -> complete.
    const { ob, sent } = harness();
    ob.tab(tab("w1:t1", { title: "loading" }));
    ob.tab(tab("w1:t1", { title: "half" }));
    ob.tab(tab("w1:t1", { title: "Final Title" }));
    await vi.advanceTimersByTimeAsync(DEBOUNCE_MS + 10);

    expect(sent).toHaveLength(1);
    expect(sent[0]!.tabs).toHaveLength(1);
    expect(sent[0]!.tabs[0]!.title).toBe("Final Title");
  });

  it("keeps the debounce trailing - activity resets the timer", async () => {
    const { ob, sent } = harness();
    ob.tab(tab("w1:t1"));
    await vi.advanceTimersByTimeAsync(DEBOUNCE_MS - 200);
    ob.tab(tab("w1:t2"));
    await vi.advanceTimersByTimeAsync(DEBOUNCE_MS - 200);
    expect(sent).toHaveLength(0);
    await vi.advanceTimersByTimeAsync(300);
    expect(sent).toHaveLength(1);
    expect(sent[0]!.tabs).toHaveLength(2);
  });

  it("flushes immediately once the pending cap is reached", async () => {
    const { ob, sent } = harness();
    for (let i = 0; i < MAX_PENDING_BEFORE_FLUSH; i++) ob.tab(tab(`w1:t${i}`));
    await vi.advanceTimersByTimeAsync(0);
    expect(sent).toHaveLength(1);
    expect(sent[0]!.tabs).toHaveLength(MAX_PENDING_BEFORE_FLUSH);
  });

  it("a remove supersedes an earlier upsert for the same tab", async () => {
    const { ob, sent } = harness();
    ob.tab(tab("w1:t1"));
    ob.tab({ op: "remove", tab_key: "w1:t1", window_id: "w1" } as TabDelta);
    await vi.advanceTimersByTimeAsync(DEBOUNCE_MS + 10);
    expect(sent[0]!.tabs).toHaveLength(1);
    expect(sent[0]!.tabs[0]!.op).toBe("remove");
  });

  it("persists on every mutation, not only on flush", async () => {
    // The eviction guarantee: a flush that never happens must not lose deltas.
    const { ob, persisted } = harness();
    ob.tab(tab("w1:t1"));
    ob.tab(tab("w1:t2"));
    await vi.advanceTimersByTimeAsync(0);
    expect(persisted.length).toBeGreaterThanOrEqual(2);
    expect(Object.keys(persisted[1]!.tabs)).toContain("w1:t2");
  });

  it("recovers pending state from a previous worker generation", async () => {
    const { ob, sent } = harness();
    ob.hydrate({
      tabs: { "w1:t9": tab("w1:t9") },
      windows: {},
      groups: {},
    });
    await vi.advanceTimersByTimeAsync(DEBOUNCE_MS + 10);
    expect(sent).toHaveLength(1);
    expect(sent[0]!.tabs[0]!.tab_key).toBe("w1:t9");
  });

  it("lets newer in-memory deltas win over hydrated ones", async () => {
    const { ob, sent } = harness();
    ob.tab(tab("w1:t1", { title: "newer" }));
    ob.hydrate({ tabs: { "w1:t1": tab("w1:t1", { title: "stale" }) }, windows: {}, groups: {} });
    await vi.advanceTimersByTimeAsync(DEBOUNCE_MS + 10);
    expect(sent[0]!.tabs[0]!.title).toBe("newer");
  });

  it("retains deltas when the send fails", async () => {
    // A transient disconnect must not silently drop state - the stored session would
    // be wrong and reconcile would take up to a minute to notice.
    let failNext = true;
    const seen: StateBody[] = [];
    const { ob } = harness(async (b) => {
      if (failNext) throw new Error("pipe closed");
      seen.push(b);
    });

    ob.tab(tab("w1:t1"));
    await vi.advanceTimersByTimeAsync(DEBOUNCE_MS + 10);
    expect(seen).toHaveLength(0);
    expect(ob.pendingCount()).toBe(1);

    failNext = false;
    await vi.advanceTimersByTimeAsync(DEBOUNCE_MS + 10);
    expect(seen).toHaveLength(1);
    expect(ob.pendingCount()).toBe(0);
  });

  it("clears pending after a successful flush", async () => {
    const { ob } = harness();
    ob.tab(tab("w1:t1"));
    await vi.advanceTimersByTimeAsync(DEBOUNCE_MS + 10);
    expect(ob.pendingCount()).toBe(0);
  });

  it("does nothing when flushed with nothing pending", async () => {
    const { ob, sent } = harness();
    await ob.flush();
    expect(sent).toHaveLength(0);
  });
});

describe("splitBatches", () => {
  it("returns nothing for empty state", () => {
    expect(splitBatches(emptyState())).toHaveLength(0);
  });

  it("keeps a small batch as one message", () => {
    const s = emptyState();
    for (let i = 0; i < 10; i++) s.tabs[`t${i}`] = tab(`t${i}`);
    expect(splitBatches(s)).toHaveLength(1);
  });

  it("splits above the per-message tab cap", () => {
    // Guards the 1 MB native messaging limit.
    const s = emptyState();
    for (let i = 0; i < MAX_TABS_PER_MESSAGE * 2 + 5; i++) s.tabs[`t${i}`] = tab(`t${i}`);
    const batches = splitBatches(s);
    expect(batches).toHaveLength(3);
    expect(batches[0]!.tabs).toHaveLength(MAX_TABS_PER_MESSAGE);
    expect(batches[2]!.tabs).toHaveLength(5);
  });

  it("sends windows and groups once, with the first batch", () => {
    const s = emptyState();
    for (let i = 0; i < MAX_TABS_PER_MESSAGE + 1; i++) s.tabs[`t${i}`] = tab(`t${i}`);
    s.windows["w1"] = { op: "upsert", window_id: "w1" };
    const batches = splitBatches(s);
    expect(batches[0]!.windows).toHaveLength(1);
    expect(batches[1]!.windows).toHaveLength(0);
  });

  it("still emits a message for window-only changes", () => {
    const s = emptyState();
    s.windows["w1"] = { op: "upsert", window_id: "w1", focused: true };
    const batches = splitBatches(s);
    expect(batches).toHaveLength(1);
    expect(batches[0]!.tabs).toHaveLength(0);
  });
});
