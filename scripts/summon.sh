#!/usr/bin/env bash
# scripts/summon.sh — Generate the Nephara soul-summoning prompt.
# Copy the output and paste it into Claude Opus 4.6 (or similar).
# The LLM will respond with a soul seed — review it, then save to souls/<name>.seed.md
TODAY=$(date +%Y-%m-%d)
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
sed "s/\[today's date\]/$TODAY/g" "$SCRIPT_DIR/../rituals/summoning.md"
