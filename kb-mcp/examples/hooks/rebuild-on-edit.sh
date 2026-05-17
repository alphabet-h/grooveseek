#!/usr/bin/env bash
# rebuild-on-edit.sh — Claude Code PostToolUse hook sample for kb-mcp.
#
# Invoked by Claude Code after `Write` / `Edit` / `MultiEdit` / `Skill`.
# Reads the tool-use JSON payload from stdin, filters for files under
# `$KB_PATH`, and re-indexes only when one of the edited files is a
# Markdown document inside the knowledge base.
#
# Usage: wire it up via `.claude/settings.json`:
#
#   {
#     "hooks": {
#       "PostToolUse": [
#         {
#           "matcher": "Write|Edit|MultiEdit|Skill",
#           "hooks": [
#             { "type": "command", "command": "/abs/path/rebuild-on-edit.sh" }
#           ]
#         }
#       ]
#     }
#   }
#
# Set KB_PATH (absolute) before running, or hard-code it below. The script
# exits 0 silently when the edited file is not under $KB_PATH, which keeps
# unrelated edits from triggering a rebuild.

set -euo pipefail

# --- configure ---------------------------------------------------------------
KB_PATH="${KB_PATH:-}"               # e.g. /repo/knowledge-base
KB_MCP_BIN="${KB_MCP_BIN:-kb-mcp}"   # override if not on PATH
# -----------------------------------------------------------------------------

if [[ -z "$KB_PATH" ]]; then
  echo "rebuild-on-edit.sh: KB_PATH is not set; skipping" >&2
  exit 0
fi

payload="$(cat)"

# Extract tool_input.file_path (Write/Edit) or tool_input.file_paths (MultiEdit).
# Fall back to always-rebuild if jq is not available.
if command -v jq >/dev/null 2>&1; then
  files="$(printf '%s' "$payload" | jq -r '
    ((.tool_input.file_path // empty) | select(length > 0)),
    ((.tool_input.file_paths // [])[]?)
  ' 2>/dev/null || true)"
else
  files=""
fi

should_rebuild=false
if [[ -z "$files" ]]; then
  # jq unavailable or payload doesn't carry file paths (e.g. Skill) → rebuild
  # unconditionally. Incremental hashing in `kb-mcp index` makes this cheap
  # when nothing actually changed.
  should_rebuild=true
else
  while IFS= read -r f; do
    [[ -z "$f" ]] && continue
    # Normalise to absolute for the prefix check
    case "$f" in
      /*) abs="$f" ;;
      *)  abs="$PWD/$f" ;;
    esac
    if [[ "$abs" == "$KB_PATH"* && "$abs" == *.md ]]; then
      should_rebuild=true
      break
    fi
  done <<< "$files"
fi

if [[ "$should_rebuild" != "true" ]]; then
  exit 0
fi

"$KB_MCP_BIN" index --kb-path "$KB_PATH" >&2
