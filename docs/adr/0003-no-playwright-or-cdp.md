# ADR-0003 - Not Playwright, and not CDP

**Status:** Accepted
**Date:** 2026-09-13

## Context

Playwright (or Puppeteer, or Selenium) is the obvious tool when someone says "control a
browser from code," and it was raised as a candidate for reading and restoring tabs.

## Decision

Use a browser extension. Do not use Playwright, Puppeteer, Selenium, or a direct Chrome
DevTools Protocol connection.

## Why Playwright is the wrong tool for this specific job

Playwright is a **browser automation** tool. It is excellent at what it does, and what it
does is not this.

1. **It drives its own browser, not yours.** `playwright.chromium.launch()` starts a
   fresh instance with a clean, temporary profile - no logins, no extensions, no
   history, none of your open tabs. The tabs you are trying to save are in an instance
   Playwright has no relationship with.

2. **It cannot attach to a normally-started browser.** `connectOverCDP()` exists, but it
   requires the browser to have been started with `--remote-debugging-port`. Your Chrome,
   launched from the taskbar, was not. You would have to change how Chrome starts on your
   machine forever, which leads directly to the next point.

3. **A browser with a debugging port open is an unlocked browser.** That port has no
   authentication. Any local process - any script, any npm postinstall, any malware -
   can connect to it and read every cookie, every session token, every open page, and
   drive the browser as you. It is a complete bypass of the browser's security model, and
   for a product whose central promise is privacy it would be a catastrophic default.
   Chrome has been progressively hardening against exactly this (including restricting
   which profiles can be used with a debugging port) because attackers use it for session
   token theft in the wild.

4. **Incognito, the actual hard requirement, gets worse rather than better.** A
   CDP-attached browser exposes incognito targets to anything on that port - so the
   private browsing guarantee is broken for every local process, not just for us. The
   extension path requires the user to explicitly grant private-window access to one
   named extension, which is a scoped, revocable, visible permission.

5. **It is enormous for the job.** Playwright ships browser binaries and a Node runtime
   to solve a problem that `chrome.tabs.query()` answers in one line.

6. **Automation is detectable and degrades the browsing experience.** A CDP-attached or
   Playwright-launched browser is flagged by bot detection on many sites, shows the
   "being controlled by automated software" infobar, and behaves subtly differently.

## The general principle

Automation tools are for *driving a browser you own for a task*. Extensions are for
*participating in the browser the user already runs*. This product is the second thing.
The tell is that we need the user's real profile, real logins, and real tabs - the
moment that is true, automation frameworks are the wrong layer.

## What about reading the browser's session files directly?

Chrome's `Current Session` / `Current Tabs` and Firefox's `sessionstore-backups/*.jsonlz4`
contain the data. Rejected because:

- The formats are undocumented and change between browser versions, with no stability
  guarantee.
- Files are locked or mid-write while the browser runs, so reads race with the browser.
- Chrome's session files are written lazily; the on-disk copy lags reality significantly.
- **Incognito is never written to disk at all** - which is the whole point of incognito,
  and it means the headline feature is simply unavailable through this route.
- Parsing another application's private on-disk state is exactly the kind of brittle
  coupling that turns into a support burden on every browser update.

The extension APIs are stable, documented, versioned, permissioned, and see incognito
when - and only when - the user allows it.

## Consequences

- Tab capture requires the user to install an extension. This is real friction and the
  onboarding has to earn it.
- We are bound by extension API limits: no scroll position or form state without content
  scripts and broad host permissions (deferred, see [09](../09-roadmap.md)).
- Three store listings and three review processes.
- In exchange: no security hole, incognito support that is actually legitimate, and no
  breakage on browser updates.

## Where Playwright *is* useful in this project

Integration testing. Driving a real Chrome with the extension loaded
(`--load-extension`) to assert capture and restore behavior in CI is a genuinely good
use of it. Keep it as a dev dependency, out of the shipped product.
