#!/usr/bin/env bash
# mempal Codex Stop hook — ask Codex to persist decisions without looping.
#
# Codex Stop hooks differ from Claude Code:
# - Codex expects a continuation reason on stderr with exit code 2
# - Codex includes `stop_hook_active` in the JSON payload so hooks can avoid
#   re-triggering themselves on the continuation stop event

set -euo pipefail

INTERVAL="${MEMPAL_SAVE_INTERVAL:-10}"
COUNTER_FILE="${HOME}/.mempal/.codex_stop_counter"
mkdir -p "$(dirname "$COUNTER_FILE")"

payload="$(cat)"

# If this stop event is already the continuation created by a prior Stop hook,
# do nothing so we don't create an infinite continue loop.
if printf '%s' "$payload" | grep -Eq '"stop_hook_active"[[:space:]]*:[[:space:]]*true'; then
  exit 0
fi

count=$(cat "$COUNTER_FILE" 2>/dev/null || echo 0)
count=$((count + 1))
echo "$count" > "$COUNTER_FILE"

if (( count % INTERVAL != 0 )); then
  exit 0
fi

cat >&2 <<'EOF'
Before stopping, save the key decisions from this conversation to mempal.

For each decision:
1. Identify the decision + its rationale (not just "we used X", but "we used X because Y")
2. Call mempal_ingest with:
   - content: full decision + rationale
   - wing: current project name
   - room: (optional, let mempal auto-route)

If no new decisions were reached this round, respond "nothing to save" and stop.
EOF

exit 2
