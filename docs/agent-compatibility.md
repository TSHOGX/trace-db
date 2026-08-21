# Agent Compatibility Matrix

This matrix records a read-only probe of the native stores present on the
development host on 2026-08-21. It uses the installed CLI versions and
`trace-db ingest --dry-run --json`; no archive or source store was modified.

| Agent | Installed CLI | Native root | Discovered | Changed / parsed | Failed | Skipped | Result |
| --- | --- | --- | ---: | ---: | ---: | ---: | --- |
| Claude Code | 2.1.238 | `~/.claude/projects` | 2,960 | 2,960 | 0 | 0 | Pass |
| Codex | 0.149.0 | `~/.codex/sessions` | 1,250 | 1,250 | 0 | 0 | Pass |
| OpenCode | 1.18.18 | `~/.local/share/opencode/opencode.db` | 85 | 85 | 0 | 0 | Pass |
| Gemini CLI | 0.46.0 | `~/.gemini/tmp` | 38 | 38 | 0 | 0 | Pass |
| Pi | 0.84.2 | `~/.pi/agent/sessions` | 508 | 462 | 0 | 46 | Pass |

`Skipped` for Pi is the parser's existing intentional filtering of synthetic
`/tmp` or `faux` sessions. Claude's 16 workflow journals and Pi's `None.jsonl`
placeholder are excluded during discovery because they are not sessions. Gemini
legacy pretty-printed `.json` documents are parsed as complete JSON documents;
JSONL remains line-oriented. Malformed or concurrently incomplete source files
continue to produce structured `corrupt_data` failures rather than being
silently discarded.

OpenCode native reconstruction was additionally verified against the installed
OpenCode `1.18.18` CLI using `scripts/verify-opencode-compat.py`: the restored
schema-cloned database opened successfully, reported one session, and exported
that session successfully. The script limits ingest to the newest native
session using its exact millisecond timestamp, so compatibility checks remain
bounded even when the source database contains many sessions.

To reproduce the probes without writing an archive, run one command per row:

```bash
trace-db --db /tmp/claude.db ingest --dry-run --json \
  --agent claude --root "$HOME/.claude/projects"
```

Use a fresh temporary `--db` path for each agent. The result is a host snapshot,
not a guarantee that future native format versions remain compatible; rerun it
after upgrading any agent CLI.
