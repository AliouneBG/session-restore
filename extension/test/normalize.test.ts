import { describe, it, expect } from "vitest";
import {
  normalizeUrl,
  isRestorable,
  tabCompareKey,
  extractPlaceholderTarget,
} from "../src/shared/normalize.js";

describe("normalizeUrl", () => {
  it("treats a bare host and its root path as the same page", () => {
    expect(normalizeUrl("https://example.com")).toBe(normalizeUrl("https://example.com/"));
  });

  it("ignores the fragment", () => {
    expect(normalizeUrl("https://example.com/a#section-2")).toBe(
      normalizeUrl("https://example.com/a"),
    );
  });

  it("lowercases scheme and host but not the path", () => {
    expect(normalizeUrl("HTTPS://Example.COM/CaseSensitive")).toBe(
      "https://example.com/CaseSensitive",
    );
  });

  it("drops default ports", () => {
    expect(normalizeUrl("https://example.com:443/a")).toBe(normalizeUrl("https://example.com/a"));
    expect(normalizeUrl("http://example.com:80/a")).toBe(normalizeUrl("http://example.com/a"));
  });

  it("keeps a non-default port", () => {
    expect(normalizeUrl("https://example.com:8443/a")).not.toBe(
      normalizeUrl("https://example.com/a"),
    );
  });

  it("strips tracking parameters", () => {
    expect(normalizeUrl("https://example.com/a?utm_source=x&utm_campaign=y")).toBe(
      "https://example.com/a",
    );
    expect(normalizeUrl("https://example.com/a?fbclid=abc")).toBe("https://example.com/a");
  });

  it("keeps meaningful query parameters", () => {
    // The regression this guards: stripping the query string wholesale would make
    // every article on a ?id= site look like the same tab.
    expect(normalizeUrl("https://example.com/view?id=1234")).not.toBe(
      normalizeUrl("https://example.com/view?id=5678"),
    );
  });

  it("keeps a meaningful param while dropping a tracking one beside it", () => {
    expect(normalizeUrl("https://example.com/view?id=9&utm_source=nl")).toBe(
      "https://example.com/view?id=9",
    );
  });

  it("does not strip ambiguous params that some sites route on", () => {
    expect(normalizeUrl("https://example.com/a?ref=hn")).toBe("https://example.com/a?ref=hn");
  });

  it("is order-insensitive for query parameters", () => {
    expect(normalizeUrl("https://example.com/a?b=2&a=1")).toBe(
      normalizeUrl("https://example.com/a?a=1&b=2"),
    );
  });

  it("leaves no dangling ? when every param was stripped", () => {
    expect(normalizeUrl("https://example.com/a?utm_source=x")).toBe("https://example.com/a");
  });

  it("distinguishes deep paths that differ only by trailing slash", () => {
    // Conservative on purpose: some servers really do serve different content.
    expect(normalizeUrl("https://example.com/docs")).not.toBe(
      normalizeUrl("https://example.com/docs/"),
    );
  });

  it("returns unparseable input unchanged instead of throwing", () => {
    expect(normalizeUrl("not a url")).toBe("not a url");
    expect(normalizeUrl("")).toBe("");
  });

  it("resolves a placeholder tab to the page it stands for", () => {
    const real = "https://example.com/article?id=7";
    const placeholder =
      "chrome-extension://abcdefghijklmnop/pages/restore.html?u=" +
      encodeURIComponent(real) +
      "&t=Article";
    expect(normalizeUrl(placeholder)).toBe(normalizeUrl(real));
  });
});

describe("extractPlaceholderTarget", () => {
  it("returns the wrapped URL for our own placeholder", () => {
    const u = new URL(
      "chrome-extension://abc/pages/restore.html?u=" + encodeURIComponent("https://x.test/a"),
    );
    expect(extractPlaceholderTarget(u)).toBe("https://x.test/a");
  });

  it("returns null for an ordinary page", () => {
    expect(extractPlaceholderTarget(new URL("https://example.com/"))).toBeNull();
  });

  it("returns null for another extension's page", () => {
    expect(extractPlaceholderTarget(new URL("chrome-extension://abc/options.html"))).toBeNull();
  });
});

describe("isRestorable", () => {
  it("accepts ordinary web pages", () => {
    expect(isRestorable("https://example.com/")).toBe(true);
    expect(isRestorable("http://example.com/")).toBe(true);
  });

  it("rejects schemes an extension cannot navigate to", () => {
    for (const u of [
      "chrome://settings",
      "edge://flags",
      "about:config",
      "devtools://devtools/bundled/x.html",
      "view-source:https://example.com",
      "javascript:alert(1)",
      "data:text/html,hi",
    ]) {
      expect(isRestorable(u), u).toBe(false);
    }
  });

  it("rejects an arbitrary extension page but accepts our placeholder", () => {
    expect(isRestorable("chrome-extension://abc/options.html")).toBe(false);
    expect(
      isRestorable("chrome-extension://abc/pages/restore.html?u=" + encodeURIComponent("https://a.test/")),
    ).toBe(true);
  });

  it("rejects empty and malformed input", () => {
    expect(isRestorable("")).toBe(false);
    expect(isRestorable("   ")).toBe(false);
    expect(isRestorable("nonsense")).toBe(false);
  });
});

describe("tabCompareKey", () => {
  it("treats the same URL in different windows as different tabs", () => {
    expect(tabCompareKey("w1", "https://example.com/")).not.toBe(
      tabCompareKey("w2", "https://example.com/"),
    );
  });

  it("treats equivalent URLs in the same window as the same tab", () => {
    expect(tabCompareKey("w1", "https://example.com?utm_source=a#x")).toBe(
      tabCompareKey("w1", "https://example.com/"),
    );
  });
});
