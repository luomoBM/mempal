#!/usr/bin/env bash
# mempal CLI smoke — drives the real binary through its three agent surfaces:
#   1. read-only doctor against the live DB (must report db_compatible=true)
#   2. isolated-HOME ingest of a one-file fixture (write path, live DB untouched)
#   3. isolated-HOME search that must find the fixture back (retry up to 3x —
#      a search can transiently return [] right after a cold ingest)
#
# Usage: .claude/skills/run-mempal/smoke.sh [path-to-mempal-binary]
#   defaults to ./target/debug/mempal; builds it if missing.
set -euo pipefail

cd "$(dirname "$0")/../../.."   # repo root — skill lives at .claude/skills/run-mempal/
BIN=${1:-./target/debug/mempal}
[ -x "$BIN" ] || cargo build

REAL_HOME=$HOME
SMOKE_HOME=$(mktemp -d /tmp/mempal-smoke.XXXX)
trap 'rm -rf "$SMOKE_HOME"' EXIT

echo "== 1/3 doctor (read-only, live DB) =="
# Write to a file, then grep: piping into `grep -q` closes the pipe on first
# match and mempal panics with Broken pipe (Rust ignores SIGPIPE).
"$BIN" doctor > "$SMOKE_HOME/doctor.txt"
grep -q 'db_compatible=true' "$SMOKE_HOME/doctor.txt"

echo "== 2/3 ingest fixture (isolated HOME) =="
mkdir -p "$SMOKE_HOME/fixture" "$SMOKE_HOME/.cache/huggingface"
printf '# Fixture\n\nDecision: the smoke harness uses an isolated HOME so it never touches the live palace database.\n' \
  > "$SMOKE_HOME/fixture/decision.md"
# Symlink the HF model cache so the embedder does not re-download
# potion-multilingual-128M into the throwaway HOME (~80s -> ~9s).
ln -s "$REAL_HOME/.cache/huggingface/hub" "$SMOKE_HOME/.cache/huggingface/hub"
HOME="$SMOKE_HOME" "$BIN" ingest "$SMOKE_HOME/fixture" --wing smoke-run

echo "== 3/3 search round-trip (isolated HOME) =="
N=0
for i in 1 2 3; do
  N=$(HOME="$SMOKE_HOME" "$BIN" search "isolated HOME palace database" \
        --wing smoke-run --top-k 1 --json 2>/dev/null \
        | python3 -c 'import json,sys; print(len(json.load(sys.stdin)))')
  [ "$N" -ge 1 ] && break
  echo "  search empty on try $i, retrying..."
  sleep 2
done
[ "$N" -ge 1 ] || { echo "FAIL: fixture never found"; exit 1; }

echo "smoke OK"
