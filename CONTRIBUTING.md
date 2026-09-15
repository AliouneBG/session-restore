# Building session-restore

Windows 11. Two halves, built independently.

## Prerequisites

```powershell
# Rust (per-user, no admin)
winget install --id Rustlang.Rustup -e

# MinGW-w64 - needed by the GNU toolchain for rusqlite's bundled SQLite (gcc)
# and windows-sys (dlltool). Use MSVCRT, not UCRT: Rust's windows-gnu target
# links MSVCRT and mixing C runtimes causes subtle breakage. See docs/adr/0006.
winget install --id BrechtSanders.WinLibs.POSIX.MSVCRT -e

# Node 22+ for the extension
winget install --id OpenJS.NodeJS.LTS -e
```

Both installers add themselves to the user `PATH`. **Open a new terminal afterwards** —
an already-running shell keeps its stale environment.

Verify:

```powershell
cargo --version ; gcc --version ; node --version
```

### Why not MSVC?

MSVC is the intended *release* target and is preferred if you have it. It needs Visual
Studio Build Tools, which requires admin. If you have it (or install it), switch with:

```powershell
winget install --id Microsoft.VisualStudio.2022.BuildTools -e --override `
  "--quiet --wait --norestart --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
rustup default stable-x86_64-pc-windows-msvc
```

Then delete `rust-toolchain.toml`, or change its channel. Both targets must stay
buildable — see [ADR-0006](docs/adr/0006-gnu-toolchain-for-now.md).

## Agent

```powershell
cd agent
cargo build
cargo test
cargo clippy --all-targets -- -D warnings
```

## Extension

```powershell
cd extension
npm install
npm run check      # typecheck + tests
npm test
```

`npm run gen:proto` regenerates `src/shared/protocol.generated.ts` from
`schema/protocol.schema.json`. It runs automatically as part of `build` and
`typecheck`. **Never edit the generated file** — edit the schema, which is the source
of truth for both halves.

## Tests that are release blockers

`agent/crates/sr-agent/tests/privacy.rs` asserts the product's central privacy claim:
a known private URL is ingested, then every byte of every file the agent wrote is
scanned for it. A failure there means the claim is false. It is never flaky — treat a
failure as a blocker, not a retry.

## Layout

```
schema/           protocol.schema.json - source of truth for both halves
agent/crates/
  sr-proto/       wire types + framing (shared)
  sr-agent/       the long-lived per-user agent
  sr-relay/       thin stdio<->named-pipe bridge the browser spawns
extension/        MV3 extension, one source tree, two manifests
docs/             specs; docs/adr/ for decisions
```
