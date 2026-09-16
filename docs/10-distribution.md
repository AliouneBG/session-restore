# 10 - Distribution

How Session Restore is packaged, signed and published, and which of those steps a build
can do on its own.

## Building a release

```powershell
.\scripts\package.ps1
```

That builds the agent in release, builds the extension, and writes:

| Output | What it is |
|---|---|
| `dist\SessionRestore-<version>\` | The install payload, ready to run |
| `dist\SessionRestore-<version>.zip` | The same, for download |
| `dist\extension-chrome-<version>.zip` | Upload to Chrome Web Store and Edge Add-ons |
| `dist\extension-firefox-<version>.zip` | Upload to addons.mozilla.org |

Add `-Sign` to sign the executables, and `-SkipExtension` to skip the Node build when
only the agent changed.

## The installer

`sr-setup.exe`. It is a plain executable, not an MSI, and it **never asks for
administrator**.

That is the product, not a shortcut. Everything Session Restore touches is per-user by
design: the data directory under `%LOCALAPPDATA%`, the native messaging registry keys
under HKCU, the logon task, the Start Menu shortcut. All of it is per-user because the
agent is per-user ([ADR-0001](adr/0001-user-agent-not-windows-service.md)). An MSI would
add a build dependency and an elevation prompt in exchange for a privilege the product
never uses.

What it does:

1. Copies the payload to `%LOCALAPPDATA%\Programs\SessionRestore`.
2. Runs `sr-agent --install`, which owns registration. The installer deliberately does
   not know how to write a native messaging manifest. There is one implementation of
   that, in the agent, with tests.
3. Writes an Add/Remove Programs entry under HKCU, so Apps and Features lists it.
4. Starts the agent, which shows the welcome flow on first run.

| Command | Effect |
|---|---|
| `sr-setup.exe` | Install, with a dialog at the end |
| `sr-setup.exe --silent` | Install with no dialogs, for unattended deployment |
| `sr-setup.exe --uninstall` | Remove, keeping captured sessions |
| `sr-setup.exe --uninstall --purge` | Remove, deleting captured sessions too |

### Things that were not obvious

**Upgrading has to stop the relays, not just the agent.** Windows will not overwrite a
running executable. The relays are the easy ones to forget, because they are children
of the *browsers*, not of the agent, so they hold `sr-relay.exe` open long after the
agent is gone. The browsers respawn them on the next connect.

**A locked file is moved aside rather than failing.** Windows refuses to overwrite a
running executable but is happy to rename one, and a renamed file keeps running until
its last handle closes. The old copy becomes `.old` and is deleted on the next install.

**The agent is spawned detached.** A plain spawn hands the agent the installer's
stdout, and the agent runs until logout, so anything piping the installer's output
would wait forever for a handle that never closes. An unattended install would hang
rather than finish.

**Re-registering never revokes an extension.** `sr-agent --install` merges with the
allowlist already in the manifest, because the installer re-runs it with no ids at all:
an unpacked extension's id is derived from its path and is only known after a browser
has loaded it once. Overwriting the allowlist with an empty one silently disconnected a
working extension, and nothing said so except tabs quietly not being captured.

## Code signing

Unsigned builds work. They just make SmartScreen warn on any machine that is not the
one that built them, and the warning is the kind that stops people installing.

```powershell
$env:SR_SIGN_THUMBPRINT = "<certificate thumbprint>"
.\scripts\package.ps1 -Sign
```

This needs `signtool.exe` from the Windows SDK and a certificate in the current user's
store. A certificate is a purchase, not a build step:

| Kind | Effect on SmartScreen | Rough cost |
|---|---|---|
| Self-signed | None. Useful only for testing the signing path | Free |
| OV (organisation validated) | Warns until the signature builds reputation | ~$200-400 a year |
| EV (extended validation) | Trusted immediately, on a hardware token | ~$300-600 a year |

Sign with SHA-256 and always timestamp, which `package.ps1` does. Without a timestamp
the signature stops being valid the day the certificate expires, rather than the day it
was issued being what matters.

## Publishing the extension

**An installer cannot install the extension.** Chrome and Edge removed silent external
extension installs on Windows deliberately, to stop malware doing exactly that. The
only supported path for a consumer application is a store listing the user adds with
one click. Enterprise policy (`ExtensionInstallForcelist`) can force-install, but it
needs admin and a managed machine, which is the wrong shape for this.

This is why the welcome flow is built the way it is. It cannot be a progress bar, so it
is a thing that opens the right page and then notices when the extension connects.

| Store | Account | Notes |
|---|---|---|
| Chrome Web Store | one-off $5 developer fee | Review is slower for anything requesting incognito access |
| Edge Add-ons | free | Accepts the same package as Chrome |
| addons.mozilla.org | free | Signs the add-on, which is what makes it survive a Firefox restart |

Until the Firefox add-on is signed it can only be loaded as a temporary add-on, which
is removed when Firefox restarts.

### What the review will ask about

The extension requests access to tabs and, optionally, to private windows. Expect to
justify both. The honest answers are in
[06-privacy-security.md](06-privacy-security.md):

- Nothing is uploaded. There is no server and no account.
- Private windows are off by default and need two separate opt-ins, one in the
  application and one in the browser.
- Private tabs are encrypted with a DPAPI-wrapped key, stored with no plaintext
  columns at all, and hard deleted on a timer.
- There is a test that ingests a known private URL and then scans every byte the agent
  wrote, to keep the central claim falsifiable rather than promised.

### After the listings exist

A published extension has a fixed id, which is what the native messaging manifest
allowlist needs. Once the ids are stable, put them in `setup::Ids::default()` so a
fresh install allows the store build without the user copying an id from
`chrome://extensions`.

## Version numbering

One version, in `agent/Cargo.toml`, read by `package.ps1` and reported by
`sr-agent --status`. The extension carries its own version in its manifest; keep them
in step.
