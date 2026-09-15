/**
 * Runtime capability detection.
 *
 * The browsers differ in ways that matter (docs/07-extension.md), and we gate on
 * *feature presence* rather than sniffing a version string, so a future Chrome that
 * ships `tabs.create({discarded})` starts using it without a code change.
 *
 * The browser name reported here is advisory. The relay stamps the authoritative
 * `src.browser` on every message, because an extension cannot be trusted to report
 * its own host correctly (docs/05-ipc-protocol.md).
 */

import type { BrowserKind } from "./protocol.generated.js";

export type Capability = "tab_groups" | "discarded_create" | "on_suspend";

export interface Caps {
  /** Advisory only; the relay's stamp wins. */
  browser: BrowserKind;
  capabilities: Capability[];
  /** Chrome/Edge evict the service worker aggressively; Firefox's event page less so. */
  evictsBackground: boolean;
}

function hasTabGroups(): boolean {
  return typeof chrome !== "undefined" && typeof (chrome as any).tabGroups !== "undefined";
}

/**
 * Firefox supports `discarded: true` on tabs.create, which is the clean way to restore
 * a tab unloaded. Chrome does not, and `tabs.discard()` after creation is not a
 * substitute - the page has already begun loading by then (docs/04-restore.md).
 *
 * Detected via the Firefox-only `runtime.getBrowserInfo`, since there is no direct
 * feature test for an options-bag property.
 */
function isGecko(): boolean {
  return (
    typeof chrome !== "undefined" &&
    typeof chrome.runtime !== "undefined" &&
    typeof (chrome.runtime as any).getBrowserInfo === "function"
  );
}

function hasOnSuspend(): boolean {
  return (
    typeof chrome !== "undefined" &&
    typeof chrome.runtime !== "undefined" &&
    typeof (chrome.runtime as any).onSuspend !== "undefined"
  );
}

function detectBrowser(): BrowserKind {
  if (isGecko()) return "firefox";
  const ua = typeof navigator !== "undefined" ? navigator.userAgent : "";
  // Edge identifies as "Edg/" (no 'e') in Chromium builds.
  if (/\bEdg\//.test(ua)) return "edge";
  return "chrome";
}

let cached: Caps | null = null;

export function detectCaps(): Caps {
  if (cached) return cached;

  const gecko = isGecko();
  const capabilities: Capability[] = [];
  if (hasTabGroups()) capabilities.push("tab_groups");
  if (gecko) capabilities.push("discarded_create");
  if (hasOnSuspend()) capabilities.push("on_suspend");

  cached = {
    browser: detectBrowser(),
    capabilities,
    evictsBackground: !gecko,
  };
  return cached;
}

export function hasCap(c: Capability): boolean {
  return detectCaps().capabilities.includes(c);
}

/** Test seam. Not called in production code. */
export function __resetCapsForTest(): void {
  cached = null;
}
