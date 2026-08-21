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

The other four agents also passed a real native reconstruction probe on the
same host. Each probe used a fresh temporary archive, ingested the containing
native directory, reconstructed one selected session, and ran `verify`:

| Agent | Full-ingest candidates | Restored files | Verify |
| --- | ---: | ---: | --- |
| Claude Code | 3 | 1 | `passed=true` |
| Codex | 2 | 1 | `passed=true` |
| Gemini CLI | 3 | 1 | `passed=true` |
| Pi | 33 | 1 | `passed=true` |

This is evidence of real-source reconstruction and archive verification, not a
byte-for-byte claim for every historical session. The canonical byte-fidelity
contract remains enforced by the fixture and archive lifecycle tests; repeat
the probe after native format upgrades.

A separate isolated real-file probe copied one current native session per
file-backed agent, performed full ingest and reconstruction, and compared the
source/restored SHA-256 digests. All selected files matched byte-for-byte:

| Agent | Native bytes | SHA-256 result |
| --- | ---: | --- |
| Claude Code | 5,112,052 | Match |
| Codex | 6,830,542 | Match |
| Gemini CLI | 24,309 | Match |
| Pi | 1,020,818 | Match |

The byte counts identify the exact probe scope without publishing source paths,
content, or digests. OpenCode uses a multi-artifact SQLite reconstruction model
and is covered by the real CLI open/query/export result above instead.

To reproduce the probes without writing an archive, run one command per row:

```bash
trace-db --db /tmp/claude.db ingest --dry-run --json \
  --agent claude --root "$HOME/.claude/projects"
```

Use a fresh temporary `--db` path for each agent. The result is a host snapshot,
not a guarantee that future native format versions remain compatible; rerun it
after upgrading any agent CLI.
