# ADR-0001 — A per-user logon agent, not a Windows Service

**Status:** Accepted
**Date:** 2026-09-13

## Context

The original sketch called for a "System Service" alongside the browser extension. On
Windows that word has a specific meaning — a process registered with the Service Control
Manager, typically running as `LocalSystem` in **Session 0**.

## Decision

Ship a **per-user process started by a Scheduled Task at logon**, running in the
interactive session at medium integrity. Not a Windows Service.

## Why

Since Windows Vista, services run in Session 0, which has no interactive desktop. Every
core function of this product breaks across that boundary:

| Need | In Session 0 |
|---|---|
| `EnumWindows` over the user's windows | Returns Session 0's window station, which is empty. The user's windows are invisible. |
| `SetWindowPlacement` on a user window | No handle to it; wrong desktop. |
| Launching an app into the user's desktop | Requires `CreateProcessAsUser` + `WTSQueryUserToken` + duplicating the token + setting the right window station and desktop. Possible, fragile, and a well-known privilege-escalation footgun. |
| DPAPI with the user's key | `LocalSystem`'s DPAPI scope is not the user's. Private-tab encryption would have to be redesigned around a machine key, which is strictly weaker. |
| Showing the restore review window | Services cannot draw UI. `WTSSendMessage` gives a message box, nothing more. |
| Multiple logged-in users | One service instance must multiplex sessions and keep their data separate — a whole isolation problem we would be inventing for ourselves. |

A per-user agent gets all of this for free: it is already in the right session, already
has the user's token, and already has the user's DPAPI scope. Per-user data isolation is
the OS's problem, not ours.

## Consequences

- No capture happens when no user is logged on. Correct — there is no session to capture.
- The agent cannot survive a user logoff. Correct, and desirable.
- Nothing runs as `LocalSystem`, so a bug in our window-launching code is not a route to
  SYSTEM. This is the single biggest security win of the decision.
- Installation is a Scheduled Task, which is per-user and does not require admin to
  create. A per-user install with no UAC prompt is also a meaningfully better first-run
  experience.
- Auto-start reliability is slightly lower than a service. Mitigated with task restart
  settings and `RegisterApplicationRestart`.

## Note on the word "service"

The high-level design can keep calling this "the service" informally — it is a
background component. Just do not implement it as an SCM service. If a future
requirement genuinely needs pre-logon work (there is none today), add a *small* service
for that narrow job and keep the user agent as the place where all window and browser
work happens.
