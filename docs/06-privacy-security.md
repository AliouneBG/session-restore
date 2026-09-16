# 06 - Privacy and security

## The uncomfortable premise, stated up front

This product writes a list of every application you run and every page you have open to
disk, continuously, and offers to include the pages you opened in a private window. That
is inherently a concentration of sensitive data. The design below is about bounding it
honestly - not about claiming it away.

The product UI must say this in plain language at install time. A privacy design that
only exists in a spec document is not a privacy design.

## Threat model

| Adversary | Defended? | How / why not |
|---|---|---|
| Another (non-admin) user on this PC | **Yes** | Per-user `%LOCALAPPDATA%` ACLs; DPAPI keys are user-scoped and will not unwrap for them; named pipe ACL'd to one SID |
| Someone who steals the disk or a backup copy of the `.db` | **Yes, for private tabs** | DPAPI user key is derived from the account credential and is not in the file; private URLs stay ciphertext. Normal tabs are plaintext by design. |
| A local process running as *you* | **No** | It can call DPAPI as you, read the pipe, and read the DB. Nothing at your own integrity level can defend against your own integrity level. |
| Local admin / SYSTEM | **No** | Can impersonate you and unwrap DPAPI. Out of scope. |
| Cloud provider (if backup enabled) | **Partly** | Private tabs are structurally excluded from sync. Normal tabs would be E2E-encrypted before upload - see [09](09-roadmap.md); v1 ships no cloud path at all. |
| A hostile web page | **Yes** | No content scripts, no host permissions, no page-reachable surface in v1 |
| A hostile extension in the same browser | **Yes** | Extensions cannot read each other's native messaging ports; `allowed_origins` pins our ID |
| Someone looking at your screen after reboot | **Yes, deliberately** | Private tabs collapsed and unlabeled in the review UI until explicitly revealed ([04](04-restore.md)) |

The two "No" rows are real and should be in the user-facing FAQ, not buried. A tool that
overstates its protection is worse than one that is clear about its limits.

## Data classification

| Class | Examples | At rest | Cloud-eligible |
|---|---|---|---|
| **C0 Config** | settings, app rules | Plaintext | Yes |
| **C1 App metadata** | exe paths, window geometry, monitor layout | Plaintext | Yes |
| **C2 Browsing** | normal tab URLs and titles | Plaintext | Yes (E2E, v2) |
| **C3 Sensitive args** | command lines | Plaintext **after redaction** | Yes |
| **C4 Private browsing** | incognito URLs and titles | **AES-256-GCM** | **Never** |

**A window title can contain document *content*, not just a name.** Windows 11 Notepad
puts the first line of an unsaved note in its title. Titles are therefore stored only
when they name a file; anything else is dropped rather than kept as "metadata". See
[03-capture.md](03-capture.md).

**Window titles are classified by owning process, not by content.** A browser window's
title is the page title, so it is C2 - or C4 if the window is private. Since the agent
cannot reliably tell which browser window is private (and must not try to, by
title-matching), it stores **no title at all** for any browser process. See
[03-capture.md](03-capture.md). Titles for non-browser applications stay C1.

## Encryption design (C4)

```
random 32-byte DEK  --DPAPI(CRYPTPROTECT_UI_FORBIDDEN, user scope)-->  wrapped_key
                                                                         |
                                                                    crypto_keys table

per row:  AES-256-GCM(key = DEK,
                      nonce = 12 random bytes, unique per row,
                      plaintext = JSON {url, title, pinned, active, muted},
                      aad = snapshot_id || tab_key || key_id)
```

Details that matter:

- **`CRYPTPROTECT_UI_FORBIDDEN`** - the agent is a background process; a DPAPI prompt
  there would be an invisible hang, and a background process should never be able to put
  a credential prompt on screen anyway.
- **AAD binds the ciphertext to its row.** Without it, an attacker with write access to
  the `.db` could move a ciphertext blob to a different `tab_key` or `snapshot_id` and
  the agent would decrypt it happily in the wrong context. Cheap to add, and it turns a
  silent integrity failure into a decryption error.
- **Nonce is random per row and never reused.** With a single DEK and random 96-bit
  nonces, stay well under 2^32 encryptions before rotating the DEK. Rotate on a schedule
  anyway (below), which makes this a non-issue in practice.
- **Key rotation:** generate a new DEK monthly; mark the old one `retired_at`. Rows
  reference `key_id`, so old rows stay readable until their TTL kills them. Once no row
  references a retired key, delete it.
- Add an optional **entropy parameter** to the DPAPI call derived from a value stored in
  `keys.bin` rather than the DB, so possessing `sessions.db` alone is insufficient even
  on the same account.

### What encryption here does and does not buy

It protects against offline access to the database file - a stolen laptop with the
account locked, a backup copy, a synced OneDrive folder, another user on the machine. It
does **not** protect against malware running as you, because DPAPI will unwrap for that
process exactly as it unwraps for us. Say so in the UI.

## Private-window lifecycle

```
capture --> encrypt --> tabs_private (expires_at = now + TTL, default 24h)
                            |
                            +-- restored by user  --> DELETE immediately + VACUUM
                            +-- TTL expires       --> DELETE (5-min sweeper) + VACUUM
                            +-- setting turned off --> DELETE ALL immediately + VACUUM
                            +-- browser private window closed --> DELETE those rows
```

That last transition is worth calling out: when the user closes their last private
window, the session is over in every sense the browser means it, and our copy should go
too. Keeping it until the TTL would preserve data past the point the user believes it
was destroyed. Restore-after-reboot is the use case; restore-after-you-deliberately-
closed-it is not.

**`PRAGMA secure_delete=ON`** and a `VACUUM` after bulk deletion. SQLite's default
deletion leaves page content in the freelist where a raw file scan finds it, which would
make the TTL cosmetic.

## The single chokepoint

Privacy routing lives in exactly one function in the agent:

```rust
/// The ONLY place private-flagged tab data may enter storage.
/// Every path into tabs/tabs_private goes through here. Do not add a second one.
fn ingest_tab(tab: TabDelta, ctx: &Ctx) -> Result<()> {
    if tab.private {
        if !ctx.settings.capture_private_windows { return Ok(()); }   // drop silently
        return store_private_encrypted(tab, ctx);                     // tabs_private only
    }
    store_normal(tab, ctx)                                            // tabs only
}
```

The value of a chokepoint is that the security property becomes checkable: verifying
"private URLs never reach `tabs`, `journal`, or the sync queue" is reading one function,
not auditing the whole codebase. Enforce it mechanically:

- `store_private_encrypted` is the only function that may write `tabs_private`, and it is
  the only one that can obtain a `Dek` handle.
- The cloud sync layer takes `&NormalTab`, a type `tabs_private` rows cannot be converted
  into. **Cloud exclusion is a type error, not a runtime check.** A runtime `if` can be
  forgotten in a new code path two years from now; a type that cannot be constructed
  cannot.
- A CI test asserts that `SELECT url FROM tabs` never matches a URL that was ingested
  with `private: true`.

## Redaction of command lines

Applied at capture, before anything is written (see [03](03-capture.md)). Patterns:

```
--password=*  --token=*  --api-key=*  --secret=*  --client-secret=*
password=*  token=*  api_key=*  access_token=*  (in query strings)
Bearer <base64ish>          eyJ[A-Za-z0-9_-]{10,}\.      (JWT-shaped)
AKIA[0-9A-Z]{16}           ghp_[A-Za-z0-9]{36}           (known key formats)
any https?:// URL containing a query string  --> scheme://host/path?<redacted>
```

Redacted spans are replaced with `<redacted:N>` where N is the original length, so the
restore UI can say "this app had arguments we could not safely save" and drop it to
tier B rather than silently launching it wrong.

Better still: **prefer not to store the command line at all** for apps where it adds
nothing. Most apps restore fine from the exe path alone. Maintain a small allowlist of
apps whose arguments genuinely matter (browsers with `--profile-directory`, editors with
a folder path, terminals) and store `NULL` for everything else. Collecting less is
stronger than redacting more.

## Extension permission budget

```json
{
  "permissions": ["tabs", "tabGroups", "storage", "alarms", "nativeMessaging"],
  "host_permissions": [],
  "optional_permissions": []
}
```

No `host_permissions`, no `<all_urls>`, no content scripts, no `webRequest`. `tabs` alone
grants URL and title access, which is everything we need.

This is not only a privacy stance, it is a shipping strategy: an extension requesting
`<all_urls>` plus native messaging plus incognito access draws heavy scrutiny in both
Chrome Web Store and AMO review, and reviewers reject vague justifications. With this
permission set the justification is one sentence per permission and each maps to a
visible feature.

**The incognito justification must be written for a reviewer, not for a user.** Both
stores treat private-browsing access as a high-risk signal. State explicitly: data is
encrypted at rest under a user-scoped OS key, never transmitted off-device, expires
within 24 hours, and requires two independent opt-ins. Expect to be asked; have the
answer ready and link to this document.

## Logging

Agent logs must never contain URLs, window titles, or command lines at default level.
Log `tab_key` hashes and counts. A verbose mode may log normal URLs; **no log level ever
writes a private URL**, including crash dumps - install a panic hook that scrubs, and
disable Windows Error Reporting dumps for the agent process, since a dump would contain
decrypted URLs and the DEK in memory.

## Uninstall

Uninstall must offer, and default to, **deleting all captured data**: the database, the
key file, cached icons, logs, the native messaging registry keys, and the scheduled
task. A privacy tool that leaves a complete browsing history on disk after removal has
failed at the last step.
