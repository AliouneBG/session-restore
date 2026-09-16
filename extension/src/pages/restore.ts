/**
 * The lazy-restore placeholder page (Chromium only; Firefox uses `discarded: true`).
 *
 * Critical behavior: it must **not** navigate on load. Navigating on load would make
 * every restored tab fetch its page immediately, which is exactly the thundering herd
 * lazy restore exists to avoid - and on a machine still finishing logon, forty
 * simultaneous page loads is the worst thing we could do.
 *
 * It navigates only when the user actually looks at the tab.
 */

const params = new URLSearchParams(location.search);
const target = params.get("u") ?? "";
const title = params.get("t") ?? target;

// Set the title so the tab strip reads correctly while unloaded.
document.title = title || "Restored tab";

const urlEl = document.getElementById("url");
const titleEl = document.getElementById("title");
const openEl = document.getElementById("open") as HTMLAnchorElement | null;

if (titleEl) titleEl.textContent = title;
// Rendered as selectable text, not just an href: if the extension is ever removed
// while placeholder tabs are open, the user can still see and copy where it went.
if (urlEl) urlEl.textContent = target;
if (openEl && target) openEl.href = target;

let navigated = false;

function go(): void {
  if (navigated || !target) return;
  navigated = true;
  location.replace(target);
}

document.addEventListener("visibilitychange", () => {
  if (document.visibilityState === "visible") go();
});

// Covers the case where the tab is already foreground when the script runs - which
// happens if the user clicks it during the restore itself.
if (document.visibilityState === "visible") {
  // A tick of delay so a restore that creates and immediately backgrounds the tab
  // does not trip this.
  setTimeout(() => {
    if (document.visibilityState === "visible") go();
  }, 150);
}

openEl?.addEventListener("click", (e) => {
  e.preventDefault();
  go();
});
