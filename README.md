# TraceDB

TraceDB is a local, loss-aware archive for coding-agent trajectories. It is a
Rust workspace with a SQLite storage engine, a native CLI, an embeddable Rust
library, and a line-oriented JSON protocol for other languages.

TraceDB mechanically discovers Claude Code, Codex, OpenCode, Gemini CLI, and Pi
sessions. It normalizes their useful common structure into seven event kinds
while retaining native provenance and session lineage.

## Capture modes

In `partial` mode (the default), TraceDB stores the full normalized event text,
metadata, lineage, and pointers to native sources. This is the fast,
rebuildable index mode.

In `full` mode, TraceDB additionally stores compressed, content-addressed native
snapshots. Full capture is sticky: a later partial ingest never removes an
existing snapshot. `trace-db reconstruct` restores those snapshots into a safe
output directory. Partial databases can be rebuilt from native stores; full
databases are archives and should be backed up.

## Install and run

Requirements: Rust 1.82+ and Cargo.

```bash
cargo install --path .
trace-db ingest
trace-db ingest --mode full --agent codex
trace-db search "deploy netlify" --limit 20 --json
trace-db stats --json
```

The repository wrapper is also available during development:

```bash
./trace-db --help
```

The database path is `$TRACEDB_PATH` when set, otherwise
`$XDG_DATA_HOME/trace-db/trace.db`, falling back to
`~/.local/share/trace-db/trace.db`.

## CLI

```text
trace-db ingest [--agent A[,A...]] [--mode partial|full] [--since DAYS|RFC3339] [--root PATH]
trace-db search QUERY [--agent A] [--cwd SUBSTRING] [--since DAYS|RFC3339] [--limit N] [--json]
trace-db show SESSION_ID [--include-tools] [--json]
trace-db reconstruct SESSION_ID --out DIRECTORY
trace-db reindex
trace-db stats [--json]
trace-db api
```

`trace-db api` reads one JSON request per line from stdin and writes one JSON
response per line. Supported operations are `stats`, `search`, `show`, and
`reconstruct`:

```bash
printf '%s\n' '{"op":"search","query":"deploy","limit":5}' | trace-db api
```

This protocol is intentionally simple and language-neutral for Python, Node.js,
Go, and shell clients without exposing SQLite internals. The Rust crate is the
preferred high-performance integration surface:

```rust,no_run
use tracedb::{SearchRequest, TraceDb};

let db = TraceDb::open_default()?;
let rows = db.search(SearchRequest::new("deploy"))?;
for row in rows {
    println!("{}: {} hits", row.id, row.hits);
}
# Ok::<(), anyhow::Error>(())
```

`TraceDb` also provides typed `ingest`, `ingest_session`, `show`, `stats`,
`reindex`, and `reconstruct` methods. Lower-level `model`, `parsers`, and
`store` modules remain public for custom importers and specialized SQL access.

## Native stores and data model

| Agent | Native store |
|---|---|
| Claude Code | `~/.claude/projects/**/*.jsonl` |
| Codex | `~/.codex/sessions/**/rollout-*.jsonl` |
| OpenCode | `~/.local/share/opencode/opencode.db` |
| Gemini CLI | `~/.gemini/tmp/*/chats/session-*` |
| Pi | `~/.pi/agent/sessions/**/*.jsonl` |

The normalized event kinds are `user`, `assistant`, `thinking`, `tool_call`,
`tool_result`, `system`, and `usage`. Tool results and usage are stored but are
excluded from the default FTS index. Event lineage (`parent_id`) and
cross-session lineage (`parent_session_id`, `forked_from`) are separate trees.

The SQLite schema records its version, archive contract, and selected tokenizer
in `schema_meta`. Full native snapshots live in a content-addressed `objects`
table and are referenced by `raw_sources`.

## Search

Search is bounded and session-oriented. FTS candidates are ranked by strongest
BM25 hit, then aggregated per session; parent/fork/subagent results collapse to
one lineage root and hit counts are merged. Agent, cwd, and time filters are
applied before aggregation. The algorithm is documented in
[`references/search-algorithm.md`](references/search-algorithm.md).

For Chinese segmentation and English stemming, build the optional tokenizer:

```bash
cargo build --release -p fts5-jieba
TRACEDB_JIEBA_EXT=target/release/libfts5jieba.dylib trace-db ingest
```

Without `TRACEDB_JIEBA_EXT`, the portable binary uses SQLite's bundled
`unicode61` tokenizer.

## Development and release checks

```bash
cargo fmt --all -- --check
cargo test
cargo build --release
cargo package --allow-dirty
cargo test -p fts5-jieba
```

The `native/fts5-jieba` crate is an optional loadable SQLite extension and is
licensed under MIT OR Apache-2.0. The TraceDB core is MIT licensed.
