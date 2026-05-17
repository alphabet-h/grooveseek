# kb-mcp: Claude Code PostToolUse hook samples

Claude Code's [PostToolUse hook](https://docs.claude.com/en/docs/claude-code/hooks) can
invoke `kb-mcp index` after the agent writes, edits, or runs a skill, so the
search index stays in sync with the knowledge base without the user having to
re-run indexing manually.

> **日本語版**: [README.ja.md](./README.ja.md)

## Files

| File | Purpose |
|---|---|
| `settings.snippet.json` | Minimal `hooks` block to copy into your project's `.claude/settings.json` — it is **not** a complete settings file. Rebuilds the index unconditionally after any `Write` / `Edit` / `MultiEdit` / `Skill`. |
| `rebuild-on-edit.sh` | Richer shell hook that inspects the tool payload and only rebuilds when the edited file is under `$KB_PATH`. Recommended when the Claude Code project touches files outside the knowledge base. Requires a Unix-like shell (bash + jq); Windows users should run it from Git Bash or WSL. |

**Notes on the `Skill` matcher**: Claude Code exposes skills via a `Skill` tool at the time of writing (v1.x). If your installed Claude Code version renames or splits this tool, adjust the matcher accordingly — no other part of kb-mcp depends on the tool name.

## Tier A — unconditional rebuild (simplest)

Place this in `.claude/settings.json` alongside your other settings:

```json
{
  "hooks": {
    "PostToolUse": [
      {
        "matcher": "Write|Edit|MultiEdit|Skill",
        "hooks": [
          { "type": "command", "command": "kb-mcp index" }
        ]
      }
    ]
  }
}
```

`kb-mcp index` uses SHA-256 content-hash diffing, so unchanged files are
skipped. In practice the second and subsequent invocations finish in well
under a second on small KBs. If the binary is not on `PATH`, replace
`kb-mcp` with an absolute path.

The `kb_path` is read from `kb-mcp.toml` (see the README's *Config file
discovery* section for the full lookup order — typically the project
root or alongside the binary). You can also hard-code it with
`kb-mcp index --kb-path /abs/path/to/knowledge-base`.

## Tier B — path-filtered rebuild (script)

Use `rebuild-on-edit.sh` when the project edits files outside the knowledge
base, so the hook stays silent for unrelated edits.

1. Copy `rebuild-on-edit.sh` somewhere on disk (e.g. `~/.local/bin/`) and make
   it executable: `chmod +x rebuild-on-edit.sh`.
2. Set `KB_PATH` to the absolute path of your `knowledge-base/` directory
   (the script aborts early if this is empty).
3. Wire it up via `.claude/settings.json`:

```json
{
  "hooks": {
    "PostToolUse": [
      {
        "matcher": "Write|Edit|MultiEdit|Skill",
        "hooks": [
          {
            "type": "command",
            "command": "KB_PATH=/abs/path/to/knowledge-base /abs/path/to/rebuild-on-edit.sh"
          }
        ]
      }
    ]
  }
}
```

The script reads the hook payload from stdin, extracts the edited file path
(via `jq` if available), and only invokes `kb-mcp index` when the edit
targets a `.md` file under `$KB_PATH`. `Skill` invocations have no file
path in the payload, so they fall through to an unconditional rebuild (cheap
thanks to diffing).

## Notes

- **Concurrency**: SQLite is configured in WAL mode, so a running MCP server
  and a hook-triggered `kb-mcp index` can coexist. The hook blocks the tool
  use until the rebuild finishes; for small KBs this is imperceptible.
- **Quality filter**: rebuild respects `[quality_filter]` in
  `kb-mcp.toml`. Backfill runs at the start of every `kb-mcp index` but is
  idempotent.
- **Skipping rebuilds**: to disable temporarily without removing the hook,
  set `KB_PATH=` (empty) in Tier B, or comment out the entry in Tier A.
