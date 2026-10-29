#!/usr/bin/env bash
# rebuild-on-edit.sh — Claude Code PostToolUse hook sample for groove.
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
# unrelated edits from triggering a rebuild. KB_EXTENSIONS controls which
# file types count as knowledge-base content — set it to match
# `[parsers].enabled` in your groove.toml.
#
# **Do not pair this hook with a `groove serve` whose watcher is on.** That
# server holds the index's writer lease while its watcher runs and already
# picks up every edit, so each `groove index` here would spend the whole
# GROOVE_INDEX_WAIT and then exit 3. Use the hook only beside a server started
# with `--no-watch` (or with no server at all). There, exit 3 means another
# writer -- MCP `rebuild_index`, or an overlapping run of this hook -- kept the
# index for longer than GROOVE_INDEX_WAIT, and this edit may not be indexed
# yet. The script returns 3 as it is, after one line saying so.
#
# **Set GROOVE_CONFIG if your groove.toml lives beside the project rather than
# beside the binary.** groove honours a config it merely discovered only in
# part, and `[parsers]` is one of the keys it resets to the default — Markdown
# alone. That matters more here than anywhere else: `groove index` deletes the
# documents it did not visit, so a rebuild that only collects `.md` removes
# every `.txt`, PDF, Office document and source file already in the index. One
# hook firing is enough. Naming the config with `--config` makes it trusted and
# keeps the parser set the one you configured.

set -euo pipefail

# --- configure ---------------------------------------------------------------
KB_PATH="${KB_PATH:-}"               # e.g. /repo/knowledge-base
GROOVE_BIN="${GROOVE_BIN:-groove}"   # override if not on PATH

# Absolute path to the groove.toml this rebuild should use, or empty to let
# groove discover one. Leave it empty ONLY when the config lives beside the
# binary or was placed by `groove service install` -- those locations are
# trusted, so nothing is reset. A groove.toml sitting in the project is not:
# see the note in the header about what that costs on a rebuild.
GROOVE_CONFIG="${GROOVE_CONFIG:-}"

# Extensions that count as knowledge-base content, space separated and without
# the dot. Keep this in sync with `[parsers].enabled` in your groove.toml:
# listing more than you index only costs a no-op rebuild, listing fewer means
# edits to those files are silently not re-indexed. Defaults to every format a
# default build can parse. "py" is not among them because it needs a grammar
# plugin placed first; add it yourself once you have.
KB_EXTENSIONS="${KB_EXTENSIONS:-md txt pdf docx xlsx pptx rs}"

# Seconds `groove index` waits for another writer (MCP `rebuild_index`, or an
# overlapping run of this hook) before it gives up with exit 3. Claude Code's
# command hooks time out after 600 s by default; if you set `timeout` on this
# hook, keep this at half of it or less, so the index run after the wait and
# the guidance line both fit inside it.
GROOVE_INDEX_WAIT="${GROOVE_INDEX_WAIT:-30}"
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
  # unconditionally. Incremental hashing in `groove index` makes this cheap
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
    [[ "$abs" == "$KB_PATH"* ]] || continue
    # Extension match, case-insensitively (scanners and mail clients happily
    # produce `Report.PDF`, and groove indexes those too).
    ext="${abs##*.}"
    ext="$(printf '%s' "$ext" | tr '[:upper:]' '[:lower:]')"
    for known in $KB_EXTENSIONS; do
      if [[ "$ext" == "$known" ]]; then
        should_rebuild=true
        break 2
      fi
    done
  done <<< "$files"
fi

if [[ "$should_rebuild" != "true" ]]; then
  exit 0
fi

if [[ -n "$GROOVE_CONFIG" && ! -f "$GROOVE_CONFIG" ]]; then
  # groove would stop with "--config path not found", and a hook that fails
  # is noise on every edit. Say what is wrong once and leave the index alone.
  echo "rebuild-on-edit.sh: GROOVE_CONFIG=$GROOVE_CONFIG does not exist; skipping" >&2
  exit 0
fi

# `--config` goes before the subcommand: it is a global flag. Two spelled-out
# calls rather than an argument array: an empty "${arr[@]}" is an unbound
# variable under `set -u` in the bash 3.2 that macOS ships.
run_index() {
  if [[ -n "$GROOVE_CONFIG" ]]; then
    "$GROOVE_BIN" --config "$GROOVE_CONFIG" index --wait "$GROOVE_INDEX_WAIT" --kb-path "$KB_PATH"
  else
    "$GROOVE_BIN" index --wait "$GROOVE_INDEX_WAIT" --kb-path "$KB_PATH"
  fi
}

guidance="rebuild-on-edit.sh: another groove process kept the index for ${GROOVE_INDEX_WAIT}s; this edit may not be indexed yet. Run groove index again, or call rebuild_index."

# groove's stderr goes to a file first, so that on exit 3 the guidance is the
# first stderr line (Claude Code's hook notice shows only that one) and
# groove's own lines follow it. groove's stdout goes to stderr as before.
# `|| rc=$?` keeps `set -e` from ending the script before the guidance.
err="$(mktemp)" || err=""
rc=0
if [[ -z "$err" ]]; then
  # No temp file: the old order, with the guidance after groove's stderr.
  run_index >&2 || rc=$?
  if [[ "$rc" -eq 3 ]]; then
    echo "$guidance" >&2
  fi
  exit "$rc"
fi
trap 'rm -f "$err"' EXIT
run_index >&2 2>"$err" || rc=$?
if [[ "$rc" -eq 3 ]]; then
  echo "$guidance" >&2
fi
cat "$err" >&2 || true
exit "$rc"
