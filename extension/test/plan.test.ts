import { describe, it, expect } from "vitest";
import { planRestore, orderForCreation, type OpenTab } from "../src/background/plan.js";
import type { RestoreTab, RestoreWindow } from "../src/shared/protocol.generated.js";

function t(url: string, index = 0, over: Partial<RestoreTab> = {}): RestoreTab {
  return { url, index, ...over } as RestoreTab;
}

function w(window_id: string, tabs: RestoreTab[], over: Partial<RestoreWindow> = {}): RestoreWindow {
  return { window_id, tabs, ...over } as RestoreWindow;
}

function open(windowRef: string, ...urls: string[]): OpenTab[] {
  return urls.map((url) => ({ windowRef, url }));
}

describe("planRestore", () => {
  it("creates everything when nothing is open", () => {
    const plan = planRestore(
      [w("w1", [t("https://a.test/", 0), t("https://b.test/", 1)])],
      [],
    );
    expect(plan.newWindows).toHaveLength(1);
    expect(plan.newWindows[0]!.tabs).toHaveLength(2);
    expect(plan.addToExisting).toHaveLength(0);
    expect(plan.alreadyOpen).toBe(0);
  });

  it("creates nothing when the browser already restored the session", () => {
    // The headline regression: browser's own "continue where you left off" is on.
    const desired = [w("w1", [t("https://a.test/", 0), t("https://b.test/", 1)])];
    const current = open("cur1", "https://a.test/", "https://b.test/");
    const plan = planRestore(desired, current);

    expect(plan.newWindows).toHaveLength(0);
    expect(plan.addToExisting).toHaveLength(0);
    expect(plan.alreadyOpen).toBe(2);
  });

  it("matches despite tracking parameters and fragments", () => {
    const desired = [w("w1", [t("https://a.test/page", 0), t("https://b.test/", 1)])];
    const current = open("cur1", "https://a.test/page?utm_source=x#section", "https://b.test/");
    expect(planRestore(desired, current).alreadyOpen).toBe(2);
  });

  it("tops up only the tabs the browser missed", () => {
    const desired = [
      w("w1", [t("https://a.test/", 0), t("https://b.test/", 1), t("https://c.test/", 2)]),
    ];
    const current = open("cur1", "https://a.test/", "https://b.test/");
    const plan = planRestore(desired, current);

    expect(plan.newWindows).toHaveLength(0);
    expect(plan.addToExisting).toHaveLength(1);
    expect(plan.addToExisting[0]!.tab.url).toBe("https://c.test/");
    expect(plan.addToExisting[0]!.targetWindowRef).toBe("cur1");
    expect(plan.alreadyOpen).toBe(2);
  });

  it("never plans to close a tab the user has open but did not save", () => {
    // There is no "close" in a RestorePlan at all - this asserts the shape, which is
    // the real guarantee.
    const desired = [w("w1", [t("https://a.test/", 0)])];
    const current = open("cur1", "https://a.test/", "https://unrelated.test/");
    const plan = planRestore(desired, current);

    expect(Object.keys(plan)).toEqual(["newWindows", "addToExisting", "alreadyOpen"]);
    expect(plan.newWindows).toHaveLength(0);
    expect(plan.addToExisting).toHaveLength(0);
  });

  it("still matches a window the user has since added many tabs to", () => {
    // Scoring is shared/stored, not Jaccard, so unrelated extras do not break the match.
    const desired = [w("w1", [t("https://a.test/", 0), t("https://b.test/", 1)])];
    const current = open(
      "cur1",
      "https://a.test/",
      "https://b.test/",
      ...Array.from({ length: 20 }, (_, i) => `https://later-${i}.test/`),
    );
    const plan = planRestore(desired, current);
    expect(plan.alreadyOpen).toBe(2);
    expect(plan.newWindows).toHaveLength(0);
  });

  it("opens a new window when an open one is unrelated", () => {
    const desired = [w("w1", [t("https://a.test/", 0), t("https://b.test/", 1)])];
    const current = open("cur1", "https://completely.test/", "https://different.test/");
    const plan = planRestore(desired, current);
    expect(plan.newWindows).toHaveLength(1);
    expect(plan.addToExisting).toHaveLength(0);
  });

  it("does not map two stored windows onto the same open window", () => {
    const desired = [
      w("w1", [t("https://a.test/", 0), t("https://b.test/", 1)]),
      w("w2", [t("https://a.test/", 0), t("https://b.test/", 1)]),
    ];
    const current = open("cur1", "https://a.test/", "https://b.test/");
    const plan = planRestore(desired, current);

    // First claims cur1; the second must get a window of its own.
    expect(plan.alreadyOpen).toBe(2);
    expect(plan.newWindows).toHaveLength(1);
  });

  it("routes each stored window to its own matching open window", () => {
    const desired = [
      w("w1", [t("https://a.test/", 0), t("https://b.test/", 1)]),
      w("w2", [t("https://x.test/", 0), t("https://y.test/", 1)]),
    ];
    const current = [
      ...open("curA", "https://a.test/", "https://b.test/"),
      ...open("curB", "https://x.test/", "https://y.test/"),
    ];
    const plan = planRestore(desired, current);
    expect(plan.newWindows).toHaveLength(0);
    expect(plan.alreadyOpen).toBe(4);
  });

  it("restores a URL the user had open twice, twice", () => {
    // Duplicate tabs are legitimate user state. Set semantics would collapse them,
    // silently giving back one tab where there were two.
    const desired = [
      w("w1", [t("https://a.test/", 0), t("https://dup.test/", 1), t("https://dup.test/", 2)]),
    ];
    const plan = planRestore(desired, []);
    expect(plan.newWindows[0]!.tabs).toHaveLength(3);
  });

  it("tops up the second copy when the browser restored only one", () => {
    const desired = [
      w("w1", [t("https://a.test/", 0), t("https://dup.test/", 1), t("https://dup.test/", 2)]),
    ];
    const current = open("cur1", "https://a.test/", "https://dup.test/");
    const plan = planRestore(desired, current);

    expect(plan.alreadyOpen).toBe(2);
    expect(plan.addToExisting).toHaveLength(1);
    expect(plan.addToExisting[0]!.tab.url).toBe("https://dup.test/");
  });

  it("counts both copies as already open when both are present", () => {
    const desired = [w("w1", [t("https://dup.test/", 0), t("https://dup.test/", 1)])];
    const current = open("cur1", "https://dup.test/", "https://dup.test/");
    const plan = planRestore(desired, current);

    expect(plan.alreadyOpen).toBe(2);
    expect(plan.addToExisting).toHaveLength(0);
  });

  it("ignores tabs with unusable URLs", () => {
    const plan = planRestore([w("w1", [t("", 0), t("https://a.test/", 1)])], []);
    expect(plan.newWindows[0]!.tabs).toHaveLength(1);
  });

  it("skips a stored window that has no usable tabs at all", () => {
    const plan = planRestore([w("w1", [t("", 0)])], []);
    expect(plan.newWindows).toHaveLength(0);
  });
});

describe("orderForCreation", () => {
  it("puts pinned tabs first so indices do not shift underneath them", () => {
    const ordered = orderForCreation([
      t("https://c.test/", 2),
      t("https://pinned.test/", 5, { pinned: true }),
      t("https://a.test/", 0),
    ]);
    expect(ordered[0]!.url).toBe("https://pinned.test/");
    expect(ordered[1]!.url).toBe("https://a.test/");
    expect(ordered[2]!.url).toBe("https://c.test/");
  });

  it("preserves relative order within each group", () => {
    const ordered = orderForCreation([
      t("https://p2.test/", 1, { pinned: true }),
      t("https://p1.test/", 0, { pinned: true }),
    ]);
    expect(ordered.map((x) => x.url)).toEqual(["https://p1.test/", "https://p2.test/"]);
  });

  it("does not mutate its input", () => {
    const input = [t("https://b.test/", 1), t("https://a.test/", 0, { pinned: true })];
    const copy = [...input];
    orderForCreation(input);
    expect(input).toEqual(copy);
  });
});
