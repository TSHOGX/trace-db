# TraceDB

TraceDB is a local, unified archive for coding-agent traces. It mechanically
discovers native sessions from Claude Code, Codex, OpenCode, Gemini CLI, and Pi,
normalizes them into one event model, stores that model in SQLite, and exposes
search and reconstruction through a Rust CLI and library API. The historical
Bun/TypeScript implementation remains in `src/*.ts` as a compatibility surface
while the Rust binary is the release path.

TraceDB does not call an LLM. It has no summarization pipeline, semantic overlay,
workflow engine, or fan-out bookkeeping. The database is a deterministic,
rebuildable index over traces that remain in their native stores.

## Why TraceDB

Each coding agent writes a different format and preserves different details.
TraceDB aims to miss as little useful trajectory data as possible without
pretending those formats are identical:

- full normalized event text is stored in SQLite;
- pointers to the original files or source database remain attached to every session;
- tool calls, tool results, reasoning, system events, usage, and both event and
  session lineage are preserved;
- raw sources can be inspected or exported when the normalized representation
  is not enough.

`partial` (the default) stores the normalized, high-value cross-agent projection
and pointers to native traces. `full` additionally stores compressed,
content-addressed native snapshots. A full capture is sticky: later partial
ingests never discard it, and `trace-db reconstruct` restores the captured
source. Partial databases are rebuildable; full databases are archives and must
be backed up.

## Install

Requirements: Rust 1.82+ and Cargo. Bun is only needed when using the legacy
TypeScript package/API.

```bash
cargo install --path .
trace-db ingest                 # partial, all agents
trace-db ingest --mode full    # archive native bytes too
```

The optional Rust build produces one local FTS5 extension. It provides jieba
Chinese word segmentation and English Porter stemming while keeping the
database itself in SQLite. Build and enable it with:

```bash
cargo build --release -p fts5-jieba
TRACEDB_JIEBA_EXT=target/release/libfts5jieba.dylib trace-db ingest
```

Without `TRACEDB_JIEBA_EXT`, the portable Rust binary uses SQLite's bundled
`unicode61` tokenizer; the schema records the selected tokenizer at creation.

By default the database is stored at
`$XDG_DATA_HOME/trace-db/trace.db`, falling back to
`~/.local/share/trace-db/trace.db`. Override it for a deployment or isolated
test:

```bash
TRACEDB_PATH=/srv/traces/trace.db trace-db ingest
```

## CLI

```bash
trace-db ingest [--agent claude,codex,opencode,gemini,pi] [--mode partial|full]
trace-db search <query> [--limit N]
trace-db show <id> [--include-tools] [--json]
trace-db reconstruct <id> --out DIR
trace-db api                    # newline-delimited JSON: {"op":"stats"} / {"op":"search",...}
trace-db reindex
trace-db stats
```

Every command accepts `--json`. Session IDs are stable, namespaced IDs such as
`claude:<uuid>` and `opencode:ses_...`.

Search uses a five-stage episodic-recall pipeline: FTS query planning, bounded
event candidates, per-session scoring, lineage collapse, and context assembly.
See [the search design](references/search-algorithm.md) for the scoring model and
tuning variables.

## Rust and TypeScript APIs

The Rust crate exposes `tracedb::model`, `tracedb::parsers`, and
`tracedb::store` for embedding. The CLI is JSON-friendly so Python, Go, Node,
and shell integrations can consume it without SQLite ABI coupling. For a
long-lived process, `trace-db api` reads one JSON request per line and emits
one JSON response per line (`stats` and `search` are currently supported). The
compatibility TypeScript API remains available:

```ts
import { db, ingest, rebuildFts, type Event } from "trace-db";

ingest({ agents: ["codex"], sinceEpochSec: null });

const sessions = db()
  .query("SELECT id, cwd, ended_at FROM sessions ORDER BY ended_at DESC LIMIT 20")
  .all();

rebuildFts();
```

The exported parser contract makes additional agents possible without changing
the storage layer. Implement `Parser`, add it to the registry, and emit the same
seven event kinds.

## Data Model

`sessions` holds cross-agent metadata and session-level lineage.
`raw_sources` holds pointers and drift metadata for native sources. `events`
holds ordered normalized events. `events_fts` is an external-content FTS5 index
over searchable event kinds.

The seven event kinds are `user`, `assistant`, `thinking`, `tool_call`,
`tool_result`, `system`, and `usage`. Tool results and usage remain in `events`
but are excluded from FTS by default because bulk stdout and numeric accounting
otherwise dominate the index.

Two independent trees are retained:

- event lineage: `parent_id -> native_id` within a session;
- session lineage: `parent_session_id` and `forked_from` across sessions.

## Supported Native Stores

| Agent | Native store |
|---|---|
| Claude Code | `~/.claude/projects/**/*.jsonl` |
| Codex | `~/.codex/sessions/**/rollout-*.jsonl` |
| OpenCode | `~/.local/share/opencode/opencode.db` |
| Gemini CLI | `~/.gemini/tmp/*/chats/session-*` |
| Pi | `~/.pi/agent/sessions/**/*.jsonl` |

Ingest is incremental. A parser first emits a cheap fingerprint; only new or
changed sessions are read and replaced transactionally.

## Development

```bash
bun run build:native
bun run typecheck
bun test
bun run build
```

The native tokenizer is kept under `native/fts5-jieba` so the package has no
runtime dependency on a private workspace path. Generated databases, compiled
binaries, and Cargo targets are ignored.

## Scope

TraceDB owns deterministic ingestion, storage, retrieval, reconstruction, and
export. LLM-generated summaries, embeddings, memory extraction, agent workflows,
and fan-out state belong in downstream projects that consume TraceDB.

## License

MIT. The bundled `fts5-jieba` component is available under MIT OR Apache-2.0.
