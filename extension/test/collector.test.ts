import { describe, it, expect } from "vitest";
import { tabToDelta, mapWindowState, tabKey, groupKey } from "../src/background/collector.js";

function fakeTab(over: Partial<chrome.tabs.Tab> = {}): chrome.tabs.Tab {
  return {
    id: 7,
    windowId: 3,
    index: 1,
    url: "https://example.test/page",
    title: "Example",
    pinned: false,
    active: false,
    incognito: false,
    ...over,
  } as chrome.tabs.Tab;
}

describe("tabToDelta", () => {
  it("maps an ordinary tab", () => {
    const d = tabToDelta(fakeTab())!;
    expect(d.tab_key).toBe("w3:t7");
    expect(d.window_id).toBe("w3");
    expect(d.url).toBe("https://example.test/page");
    expect(d.restorable).toBe(true);
  });

  it("stores the real page behind a lazy placeholder, not the placeholder", () => {
    // Otherwise rebooting twice before opening a restored tab would save a
    // placeholder-of-a-placeholder and the real URL would drift out of reach.
    const real = "https://deep.test/article?id=9";
    const d = tabToDelta(
      fakeTab({
        url:
          "chrome-extension://abcdef/pages/restore.html?u=" +
          encodeURIComponent(real) +
          "&t=Article",
      }),
    )!;
    expect(d.url).toBe(real);
    expect(d.restorable).toBe(true);
  });

  it("leaves other extension pages alone", () => {
    const d = tabToDelta(fakeTab({ url: "chrome-extension://abcdef/options.html" }))!;
    expect(d.url).toBe("chrome-extension://abcdef/options.html");
    expect(d.restorable).toBe(false);
  });

  it("flags browser-internal pages as not restorable", () => {
    expect(tabToDelta(fakeTab({ url: "edge://settings" }))!.restorable).toBe(false);
  });

  it("carries the incognito flag through", () => {
    expect(tabToDelta(fakeTab({ incognito: true }))!.private).toBe(true);
  });

  it("falls back to pendingUrl while a tab is still loading", () => {
    const d = tabToDelta(fakeTab({ url: undefined, pendingUrl: "https://loading.test/" }))!;
    expect(d.url).toBe("https://loading.test/");
  });

  it("rejects tabs without a usable identity", () => {
    expect(tabToDelta(fakeTab({ id: undefined }))).toBeNull();
    expect(tabToDelta(fakeTab({ id: -1 }))).toBeNull();
  });

  it("groups only when the tab is actually in one", () => {
    expect(tabToDelta(fakeTab({ groupId: -1 } as never))!.group_key).toBeNull();
    expect(tabToDelta(fakeTab({ groupId: 5 } as never))!.group_key).toBe(groupKey(3, 5));
  });
});

describe("mapWindowState", () => {
  it("passes through the states the protocol knows", () => {
    expect(mapWindowState("maximized")).toBe("maximized");
    expect(mapWindowState("minimized")).toBe("minimized");
    expect(mapWindowState("fullscreen")).toBe("fullscreen");
  });

  it("treats anything unfamiliar as normal", () => {
    expect(mapWindowState("locked-fullscreen")).toBe("normal");
    expect(mapWindowState(undefined)).toBe("normal");
  });
});

describe("tabKey", () => {
  it("includes the window so a dragged tab changes identity", () => {
    expect(tabKey(1, 7)).not.toBe(tabKey(2, 7));
  });
});
