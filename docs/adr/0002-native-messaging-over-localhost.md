# ADR-0002 - Native messaging + named pipe, not a localhost server

**Status:** Accepted
**Date:** 2026-09-13

## Context

The extension must talk to the agent. Two realistic options: a localhost HTTP/WebSocket
server in the agent, or Chrome/Firefox native messaging.

## Decision

**Native messaging**, with a thin relay bridging stdio to a named pipe.

## Why not localhost

A localhost listener is easier to build and worse in every way that matters here:

- **Any local process can connect.** There is no OS-level caller authentication on a TCP
  socket. The agent would be handing complete browsing history to whatever connects
  first. Bolting on a shared-secret token means storing that token somewhere both the
  extension and the agent can read - which is somewhere any local process can read too.
- **Any web page can probe it.** Pages can issue cross-origin requests to
  `http://127.0.0.1:<port>`. Even without reading responses, this is a fingerprinting
  and CSRF surface, and it has been the root of several high-profile local-service
  vulnerabilities.
- **Browsers are actively closing this path.** Private Network Access restrictions on
  requests from public sites to localhost keep tightening. Building on it means building
  on something the platform is deprecating around you.
- **Port allocation is a real problem.** Fixed port means collisions; dynamic port means
  a discovery file, which is another thing on disk that any process can read.
- **It shows up in firewall prompts**, which is an alarming first-run experience for a
  tool whose pitch is "your data stays local."

## Why native messaging

- The browser will only start the host named in a manifest whose `allowed_origins` /
  `allowed_extensions` pins our extension ID. Another extension cannot connect.
- No listening socket exists, so there is no network surface at all and no firewall
  prompt.
- The channel is a pipe between parent and child - not addressable by anything else.
- It is the mechanism both Chrome and Firefox document for exactly this purpose, which
  matters for store review.

## Why the relay is necessary

Native messaging spawns a **child of the browser**. It cannot connect to an
already-running process, and each browser profile spawns its own copy. That is
incompatible with a single long-lived agent that owns the database.

So the host is a ~200-line relay that translates stdio framing to named-pipe framing and
exits when the browser does. It holds no state and touches no disk.

This also improves the security posture rather than just working around a limitation:
the only component the browser can start is the one with the least authority. A bug in
message parsing gets an attacker a process that can talk to a pipe, not one that can
read the database.

## Named pipe hardening

- Name includes a hash of the user's SID: `\\.\pipe\SessionRestore.<sid-hash>`
- Security descriptor grants access to the interactive user's SID only
- Created with `FILE_FLAG_FIRST_PIPE_INSTANCE` so a process that starts before the agent
  cannot squat the name and intercept relay connections - a standard named-pipe hijack
  that is trivial to prevent and easy to forget

## Consequences

- Installation must write registry keys for each browser, and verify them at every agent
  startup since browser updates have been observed to clear them.
- Debugging is harder than curling an HTTP endpoint. Mitigate with a `--relay-stdio`
  debug mode on the agent.
- Message size is capped at 1 MB, which forces batching limits ([05](../05-ipc-protocol.md)).
  This is a mild constraint, not a problem.
