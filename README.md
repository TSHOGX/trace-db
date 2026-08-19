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

Ingestion first discovers lightweight candidates from file metadata or native
session rows. It compares their fingerprints with archived source locators and
only parses changed sessions or sessions that need a partial-to-full upgrade.
The CLI and APIs report discovered, parsed, ingested, unchanged, skipped,
failed, warning, and time-filtered counts separately. Best-effort ingest
continues past malformed or unreadable candidates and reports structured
per-locator failures. `--strict` prints the same complete report, then exits
nonzero when any candidate failed.

`trace-db ingest --dry-run` performs the same discovery, fingerprint comparison,
and parsing without creating, migrating, or changing archive records. Its
JSON report has stable `dryRun`, `mode`, and `agents` fields. Each agent reports
`discovered`, `changed`, `unchanged`, `skipped`, `skippedBySince`, `failed`, and
`estimatedFullCaptureBytes`; the last value is the uncompressed native-source
size for successfully parsed sessions that a full ingest would capture.

## Install and run

Requirements: Rust 1.82+ and Cargo.

```bash
cargo install --path .
trace-db ingest
trace-db ingest --dry-run --json
trace-db ingest --mode full --agent codex
trace-db ingest --strict --json
trace-db search "deploy netlify" --limit 20 --json
trace-db stats --json
trace-db verify --json
trace-db doctor --json
```

Tagged releases publish archives for x86-64 and ARM64 Linux, x86-64 and ARM64
macOS, and x86-64 Windows. Each archive contains the CLI, the optional
`fts5-jieba` extension, the `tracedb.v1` Protobuf contract, README, and license.
The same release also attaches ABI3 Python wheels and target-labeled Node.js
package tarballs for all five targets. These are GitHub release artifacts, not
automatic PyPI or npm registry publications. `SHA256SUMS` covers every attached
archive, wheel, and package, and GitHub publishes signed build-provenance
attestations for the release assets.

The repository wrapper is also available during development:

```bash
./trace-db --help
```

The database path is `$TRACEDB_PATH` when set, otherwise
`$XDG_DATA_HOME/trace-db/trace.db`, falling back to
`~/.local/share/trace-db/trace.db`.

## CLI

```text
trace-db ingest [--agent A[,A...]] [--mode partial|full] [--since DAYS|RFC3339] [--root PATH] [--dry-run] [--strict] [--json]
trace-db search QUERY [--agent A] [--cwd SUBSTRING] [--since DAYS|RFC3339] [--limit N] [--json]
trace-db list [--limit N] [--cursor CURSOR] [--agent A] [--cwd SUBSTRING] [--since DAYS|RFC3339] [--mode partial|full] [--model MODEL] [--provider PROVIDER] [--json]
trace-db show SESSION_ID [--from EVENT_INDEX] [--to EVENT_INDEX] [--kind KIND[,KIND...]] [--include-tools] [--json]
trace-db reconstruct SESSION_ID --out DIRECTORY [--overwrite]
trace-db reindex
trace-db stats [--json]
trace-db verify [--json]
trace-db doctor [--json]
trace-db api
trace-db serve [--listen 127.0.0.1:50051 | --socket PATH] [--reconstruct-root PATH]
```

`trace-db api` reads one JSON request per line from stdin and writes one JSON
response per line. Supported operations are `stats`, `search`, `list`, `show`,
and `reconstruct`. The `show` operation accepts optional inclusive `from`/`to`
event indexes and a `kind` string or array:

```bash
printf '%s\n' '{"op":"search","query":"deploy","limit":5}' | trace-db api
```

This protocol is intentionally simple and language-neutral for Python, Node.js,
Go, and shell clients without exposing SQLite internals. Every non-empty input
line produces either `{"ok":true,"result":...}` or a stable
`{"ok":false,"error":{"code":...,"message":...,"details":...}}` envelope;
malformed or invalid requests do not terminate the stream. The Rust crate is
the preferred high-performance integration surface:

```rust,no_run
use tracedb::{SearchRequest, TraceDb};

let db = TraceDb::open_default()?;
let rows = db.search(SearchRequest::new("deploy"))?;
for row in rows {
    println!("{}: {} hits", row.id, row.hits);
}
# Ok::<(), anyhow::Error>(())
```

`TraceDb` also provides typed `ingest`, `ingest_session`, `list`, `show`,
`stats`, `reindex`, and `reconstruct` methods. Lower-level `model`, `parsers`, and
`store` modules remain public for custom importers and specialized SQL access.

For a versioned cross-language boundary, `trace-db serve` exposes the
`tracedb.v1` gRPC service over loopback TCP or a Unix domain socket. The
checked-in Protobuf contract supports ingest, search, show, stats, reindex, and
reconstruction; see [`references/protocol.md`](references/protocol.md) for
compatibility and security details.

The optional Python package in `bindings/python` is a thin PyO3 wrapper around
the same `TraceDb` facade. It uses the stable Python 3.10 ABI and exposes
native Python dictionaries and lists while retaining raw JSON methods for
low-overhead integrations:

```bash
cd bindings/python
python -m pip install maturin
maturin develop --release
```

```python
from tracedb import TraceDb

db = TraceDb.open()
rows = db.search("deploy", limit=10)
trace = db.show(rows[0]["id"])
```

The optional Node.js package in `bindings/node` provides the equivalent thin
napi-rs wrapper with native JavaScript objects and bundled TypeScript
declarations. It targets N-API 6 and builds without a JavaScript dependency
install:

```bash
cd bindings/node
npm run build
npm test
```

```javascript
const { TraceDb } = require("@tracedb/core");
const db = TraceDb.open();
const rows = db.search("deploy", { limit: 10 });
const trace = db.show(rows[0].id);
```

Both bindings cover ingest, search, show, stats, reindex, and full-capture
reconstruction. Methods ending in `Json`/`_json` remain available when callers
prefer to avoid an intermediate object conversion.

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

Search is bounded and session-oriented. It combines phrase and term recall,
then scores normalized BM25 strength, hit and term coverage, event kind,
recency, and title match. Parent, fork, and subagent results collapse to one
lineage; results include an explainable score breakdown, strongest snippet,
first request, and final outcome. Agent, cwd, and time filters are applied in
SQL before aggregation. The algorithm is documented in
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
cargo clippy --all-targets --all-features -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
```

The `native/fts5-jieba` crate is an optional loadable SQLite extension and is
licensed under MIT OR Apache-2.0. The TraceDB core is MIT licensed.

CI enforces Rust 1.82 compatibility and runs the core and tokenizer tests on
Linux, macOS, and Windows. A semantic version tag such as `v0.1.0` must match
`Cargo.toml`; matching tags build and publish the platform archives.
