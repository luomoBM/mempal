---
name: run-mempal
description: Build, run, and smoke-test the mempal CLI (Rust cargo workspace). Use when asked to run/start/build/test mempal, verify an ingest-search round-trip, or check the binary works against the live memory DB.
---

# run mempal

mempal is a Rust CLI (cargo workspace, 7 crates + root `mempal` binary) that
stores project memory in SQLite at `~/.mempal/palace.db`. The agent path is
the smoke driver — it exercises the binary's real surfaces without touching
the user's live memory:

```bash
.claude/skills/run-mempal/smoke.sh            # build-if-needed + 3-stage smoke
```

All paths below are relative to the repo root.

## Prerequisites

- Rust toolchain (`cargo`) — the only hard requirement.
- `python3` and `sqlite3` — used by the smoke script's JSON asserts and ad-hoc
  DB inspection.

## Build

```bash
cargo build
```

~22s warm. Binary lands at `./target/debug/mempal`.

## Run (agent path)

The smoke script (read it before modifying): doctor against the live DB →
isolated-HOME ingest of a one-file fixture → isolated-HOME search with retry.
Prints `smoke OK` on success, non-zero on failure.

For direct poking, the three surfaces worth knowing:

```bash
./target/debug/mempal --help
./target/debug/mempal doctor
./target/debug/mempal search "why split into workspace crates" --wing mempal --top-k 2 --json
```

All three are **read-only against the live `~/.mempal/palace.db`** and are safe
to run any time.

## Run (human path)

Same binary interactively; `./target/debug/mempal --help` lists the full
command tree (search/context/brief/phase3/cowork-*/kg/tunnels/...).
`mempal serve --mcp` speaks MCP over stdio for client integration.

## Deploy (local install to ~/.cargo/bin)

Build/run/smoke never touch the installed binary. To deploy the current
checkout as the machine's `mempal` — the one MCP clients spawn:

```bash
cargo install --path . --locked --force
mempal doctor    # expect current_version=0.9.1 and no PATH warning
```

Then restart the MCP client (see Gotchas: long-lived clients keep the old
process). Deploying overwrites whatever binary is installed — if the current
install was built from a different branch with extra features, those are gone
until that branch is rebuilt and reinstalled.

To rehearse the install without touching `~/.cargo/bin`, point `--root` at a
throwaway prefix:

```bash
cargo install --path . --locked --force --root /tmp/verify-install
/tmp/verify-install/bin/mempal doctor
```

## The live-DB rule (read this before any write command)

There is **no `--db` flag and no `MEMPAL_DB` env var** — the DB path is always
resolved as `$HOME/.mempal/palace.db` (`src/main.rs` `home_dir()`). The only
isolation mechanism is overriding `HOME`, which is exactly what the test suite
does.

- Read-only, safe against real HOME: `doctor`, `search`, `status`, `brief`,
  `projects`, `resume`, `cowork-status`, `cowork-runbook`, `fact-check`.
- Writes, only under an isolated HOME: `ingest`, `delete`, `purge`, `reindex`,
  `init`, `kg add`, `knowledge*`, `phase3 adoption record`.
- Isolated HOME also redirects the HF model cache — symlink it in first
  (the smoke script does) or the embedder re-downloads
  `potion-multilingual-128M`, turning a 9s ingest into ~80s.

## Test

```bash
cargo test
```

Full suite. See Gotchas for the MCP-server test binaries.

## Gotchas

- **No `--version` flag** — `mempal --version` exits 2. Get the version from
  `mempal doctor` (`current_version=...`).
- **PATH shadowing**: `which mempal` resolves to the installed
  `~/.cargo/bin/mempal`, which is *not* your dev build. `doctor` warns about
  this (`path_matches_current_exe=false`). Always invoke
  `./target/debug/mempal` explicitly while developing.
- **Search can return `[]` with exit 0** — that is a legitimate "no match",
  and can appear transiently right after a cold ingest (fresh model files /
  cold vector index). Retry with a distinctive query before concluding
  failure; the smoke script retries up to 3x.
- **Long-lived MCP clients cache the binary**: after rebuilding/reinstalling,
  a running `mempal serve --mcp` process keeps the old code. Restart the MCP
  client to pick up the new binary.
- `mempal status` prints the entire embedded MEMORY_PROTOCOL (~10 KB) — pipe
  through `head`/`grep` unless you want all of it.
- **Piping into `grep -q` / `head` crashes mempal**: early pipe close gives
  `failed printing to stdout: Broken pipe` + panic exit 101 (Rust ignores
  SIGPIPE). Capture to a file first, then grep/head the file — the smoke
  script's doctor stage does exactly this.

## Troubleshooting

- `doctor` reports `db_compatible=false` → the live palace.db schema is newer
  than this binary supports; build the latest main and re-check.
- Isolated ingest hangs ~80s → HF cache symlink missing; see the smoke script.
- Search empty right after ingest → see Gotchas; retry, and prefer a
  distinctive multi-word query over code identifiers.
