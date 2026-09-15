# ADR-0004 — Private windows: encrypted, opt-in, auto-expiring

**Status:** Accepted
**Date:** 2026-09-13

## Context

Restoring incognito tabs is a headline requirement. It is also in direct tension with
what incognito mode is for. Incognito's guarantee is "nothing about this session is
written to disk." Restoring it after a reboot requires writing it to disk. There is no
clever design that avoids this trade — only designs that are honest about it.

Three options were considered:

1. Treat private tabs exactly like normal tabs
2. Record only counts, restore empty private windows
3. Encrypted, opt-in, auto-expiring

## Decision

**Option 3.** Private tab data is:

- **Off by default** (`capture_private_windows = false`)
- Gated behind **two independent opt-ins** — the browser's own "Allow in Incognito"
  toggle, which we cannot request programmatically, plus our own setting
- Stored in a **separate table** with **no plaintext columns at all** — URL and title
  live inside an AES-256-GCM ciphertext, keyed by a DPAPI-wrapped DEK
- **Hard-deleted after a TTL** (default 24h), on restore, when the last private window
  closes, and immediately if the setting is turned off
- **Structurally excluded from cloud sync** — enforced by the type system, not a runtime
  check
- **Collapsed and unlabeled** in the restore UI until explicitly revealed, and never part
  of an automatic restore

## Why not option 1 (treat them like normal tabs)

It is simpler, and it silently destroys the property the user was relying on when they
opened a private window. Someone who opens incognito for something genuinely sensitive
and later finds it in a plaintext local database — or worse, synced to a cloud backup —
has been actively harmed by a tool they trusted. The convenience is not worth it, and
"the user installed a session restorer" is not informed consent for that.

## Why not option 2 (counts only)

It is the most private and it does not do the job the user asked for. Reopening three
empty incognito windows is not restoring a session. Choosing it would mean shipping a
product that does not have its headline feature, in the name of a principle the user has
already considered and decided against for their own machine.

## Why option 3

It makes the trade explicit and bounded instead of implicit and permanent:

| Property | How option 3 handles it |
|---|---|
| User knows it is happening | Two deliberate opt-ins, plain-language warning, off by default |
| Exposure is time-bounded | 24h TTL, plus deletion on restore and on last-window-close |
| Exposure survives disk theft? | No — DPAPI user-scoped key is not in the file |
| Exposure leaves the machine? | Never — excluded from sync by type, not by an `if` |
| Exposure on a screen share | No — collapsed, unlabeled, requires an explicit click |

## Residual disclosure, stated honestly

Even with this design, someone with the database file learns **that N private tabs
existed across M windows at time T**, because `tabs_private` has plaintext
`snapshot_id`, `tab_index`, and row counts. Encrypting those too would mean encrypting
the whole table as one blob, which breaks incremental updates and TTL sweeping.

This is an accepted residual. It should be in the user-facing privacy note rather than
quietly omitted: *"We record that private windows existed and when, but not what was in
them."*

## Enforcement

The property "a private URL never reaches `tabs`, `journal`, a log, or the sync queue"
is enforced three ways, deliberately redundantly:

1. **At the source** — when `capture_private = false`, the extension drops private events
   before sending them. The cheapest enforcement point is the earliest one.
2. **At the chokepoint** — one function in the agent (`ingest_tab`) makes the routing
   decision. Auditing the property means reading one function.
3. **In the type system** — the cloud sync layer accepts `&NormalTab`, a type that a
   `tabs_private` row cannot be converted into. A future contributor cannot accidentally
   sync private data; the code will not compile.

Plus a CI test that ingests a known private URL and greps the entire database file, every
log file, and the sync queue for it.

## Consequences

- Extra complexity: key management, rotation, a TTL sweeper, `secure_delete`, `VACUUM`.
- The feature is off by default, so most users never see it. Correct default.
- Store review will scrutinize the incognito permission. The justification is written for
  reviewers in [06](../06-privacy-security.md) and this ADR is the supporting document.
- The TTL will occasionally frustrate: reboot after 25 hours and the private tabs are
  gone. This is the design working. Make the TTL configurable, show the expiry countdown
  in the review UI, and do not offer "never expire" as an option.
