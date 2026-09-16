/**
 * Status and onboarding panel.
 *
 * The extension cannot request private-window access programmatically - the user must
 * toggle it in browser settings. So this explains the step with the right wording for
 * the browser in question, and states plainly what turning it on means. It does not
 * nag: if the user declines, everything else works and the option stays available.
 */

import { detectCaps } from "../shared/caps.js";

const caps = detectCaps();

const INCOGNITO_STEPS: Record<string, string[]> = {
  chrome: [
    "Open chrome://extensions",
    "Find Session Restore and click Details",
    'Turn on "Allow in Incognito"',
  ],
  edge: [
    "Open edge://extensions",
    "Find Session Restore and click Details",
    'Turn on "Allow in InPrivate"',
  ],
  firefox: [
    "Open about:addons",
    "Select Session Restore",
    'Set "Run in Private Windows" to Allow',
  ],
};

function set(id: string, text: string, cls?: string): void {
  const el = document.getElementById(id);
  if (!el) return;
  el.textContent = text;
  if (cls) el.className = `val ${cls}`;
}

async function render(): Promise<void> {
  const steps = document.getElementById("steps");
  if (steps) {
    steps.innerHTML = "";
    for (const s of INCOGNITO_STEPS[caps.browser] ?? INCOGNITO_STEPS["chrome"]!) {
      const li = document.createElement("li");
      li.textContent = s;
      steps.appendChild(li);
    }
  }

  let allowed = false;
  try {
    allowed = await chrome.extension.isAllowedIncognitoAccess();
  } catch {
    allowed = false;
  }
  set("private", allowed ? "allowed" : "not allowed", allowed ? "ok" : "");

  const details = document.getElementById("incognito-help") as HTMLDetailsElement | null;
  if (details && allowed) details.open = false;

  try {
    const tabs = await chrome.tabs.query({});
    set("tabs", String(tabs.length));
  } catch {
    set("tabs", "–");
  }

  // The badge is the source of truth for connectivity; the background sets it.
  try {
    const text = await chrome.action.getBadgeText({});
    const connected = text !== "!";
    set("agent", connected ? "connected" : "not running", connected ? "ok" : "bad");
  } catch {
    set("agent", "unknown");
  }
}

void render();
// Re-check when the user comes back from browser settings.
window.addEventListener("focus", () => void render());
