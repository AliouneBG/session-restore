# session-restore

Restore your last working session on Windows 11 — applications, window layout, and
browser tabs (including private/incognito windows) — after a shutdown, reboot, crash,
or power loss.

## The one-paragraph version

A per-user background **agent** watches which applications and windows are open. A
**browser extension** watches tabs in Chrome, Edge, and Firefox — including private
windows, when you explicitly allow it. Both write into a single local **SQLite**
store continuously, so the session on disk is never more than ~60 seconds stale even
if the machine loses power. On the next logon the agent offers to put everything
back. Nothing leaves the machine unless you turn on cloud backup, and private-window
URLs are never eligible for it at all.

## Documents

| Doc | What it covers |
|---|---|
| [01-architecture.md](docs/01-architecture.md) | Components, process model, why the pieces are split this way |
| [02-data-model.md](docs/02-data-model.md) | SQLite schema, journal vs. snapshot, retention |
| [03-capture.md](docs/03-capture.md) | What gets captured, when, and how staleness is bounded |
| [04-restore.md](docs/04-restore.md) | Restore algorithm, fidelity tiers, ordering, failure handling |
| [05-ipc-protocol.md](docs/05-ipc-protocol.md) | Extension ↔ agent wire protocol |
| [06-privacy-security.md](docs/06-privacy-security.md) | Threat model, encryption, incognito posture |
| [07-extension.md](docs/07-extension.md) | Extension spec, Chrome/Edge/Firefox differences |
| [08-agent.md](docs/08-agent.md) | Windows agent internals |
| [09-roadmap.md](docs/09-roadmap.md) | Milestones and acceptance criteria |
| [adr/](docs/adr/) | Decision records — the reasoning behind the contested choices |

## Decisions already made

- **Agent language: Rust.** Single self-contained binary, no runtime to install, and
  it is the piece that runs on your machine forever. See [ADR-0005](docs/adr/0005-rust-for-the-agent.md).
- **Browsers: Chrome, Edge, Firefox.** Chrome and Edge share one MV3 build; Firefox
  gets a second build target. See [07-extension.md](docs/07-extension.md).
- **Private windows: encrypted, opt-in, auto-expiring.** Off by default. Stored in a
  separate table under a separate key, hard-deleted after a TTL, permanently barred
  from cloud sync. See [ADR-0004](docs/adr/0004-incognito-posture.md).
- **Not a Windows Service.** It is a per-user logon agent. See [ADR-0001](docs/adr/0001-user-agent-not-windows-service.md).
- **Not Playwright, not CDP.** See [ADR-0003](docs/adr/0003-no-playwright-or-cdp.md).

## Non-goals for v1

- Cross-device restore (schema leaves room; see [09-roadmap.md](docs/09-roadmap.md))
- macOS or Linux
- Restoring *in-app* state — unsaved buffers, scroll position, form contents, playback position
- Restoring elevated (admin) applications
- Replacing your browser's own "continue where you left off"; we coexist with it
