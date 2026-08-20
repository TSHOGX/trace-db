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
trace-db watch --json
trace-db search "deploy netlify" --limit 20 --json
trace-db stats --json
trace-db verify --json
trace-db doctor --json
trace-db-bench --sessions 1k,10k,100k --json
trace-db-relevance --json
```

Tagged releases publish archives for x86-64 and ARM64 Linux, x86-64 and ARM64
macOS, and x86-64 Windows. Each archive contains the CLI, deterministic
benchmark harness, optional `fts5-jieba` extension, the `tracedb.v1` Protobuf
contract, README, and license.
The same release also attaches ABI3 Python wheels and target-labeled Node.js
package tarballs for all five targets. These are GitHub release artifacts, not
automatic PyPI or npm registry publications. `SHA256SUMS` covers every attached
archive, wheel, and package, and GitHub publishes signed build-provenance
attestations for the release assets.

The repository wrapper is also available during development:

```bash
./trace-db --help
```

TraceDB loads configuration from `$TRACEDB_CONFIG` when set, otherwise from
the platform configuration directory at `trace-db/config.toml`. On Linux this
is normally `~/.config/trace-db/config.toml`; `$XDG_CONFIG_HOME` overrides the
base directory. A missing platform-default file is normal and is never created
implicitly. Use `trace-db config --json` to inspect every resolved value and
the selected file.

```toml
database_path = "data/trace.db"
default_agents = ["claude", "codex", "gemini"]
capture_mode = "partial"
exclude = ["**/private/**", "**/scratch-*"]
tokenizer = "unicode61"
output_format = "text"
redact_patterns = ["customer@example\\.com", "(?i)internal-project-[0-9]+"]
watch_interval_seconds = 300
watch_debounce_ms = 1000
```

Relative `database_path` and `tokenizer_extension` values are resolved from
the configuration file's directory. Exclusion globs match both a candidate's
native locator and its normalized path with `/` separators. Excluded
candidates are reported as skipped and are never parsed or archived.

Configuration precedence is CLI > environment > TOML file > built-in default.
The environment variables are `TRACEDB_PATH`, `TRACEDB_AGENTS`,
`TRACEDB_CAPTURE_MODE`, `TRACEDB_EXCLUDE`, `TRACEDB_TOKENIZER`,
`TRACEDB_JIEBA_EXT`, `TRACEDB_OUTPUT_FORMAT`, `TRACEDB_REDACT_PATTERNS`,
`TRACEDB_WATCH_INTERVAL`, and `TRACEDB_WATCH_DEBOUNCE`; agent and exclusion
lists are comma-separated, while redact patterns are semicolon-separated. The
built-in database path is the platform data directory at
`trace-db/trace.db`, agents default to all five supported agents, capture mode
defaults to `partial`, output defaults to `text`, and watch timing defaults to
300 seconds with a 1000 ms debounce.

## CLI

```text
trace-db [--config PATH] [--db PATH] [--format text|json] [--tokenizer unicode61|jieba] [--tokenizer-extension PATH] COMMAND
trace-db ingest [--agent A[,A...]] [--mode partial|full] [--exclude GLOB[,GLOB...]] [--since DAYS|RFC3339] [--root PATH] [--dry-run] [--strict] [--json]
trace-db search QUERY [--agent A] [--cwd SUBSTRING] [--since DAYS|RFC3339] [--limit N] [--json]
trace-db list [--limit N] [--cursor CURSOR] [--agent A] [--cwd SUBSTRING] [--since DAYS|RFC3339] [--mode partial|full] [--model MODEL] [--provider PROVIDER] [--json]
trace-db show SESSION_ID [--from EVENT_INDEX] [--to EVENT_INDEX] [--kind KIND[,KIND...]] [--include-tools] [--json]
trace-db reconstruct SESSION_ID --out DIRECTORY [--overwrite]
trace-db reindex
trace-db backup PATH [--json]
trace-db gc --dry-run [--json]
trace-db stats [--json]
trace-db verify [--json]
trace-db doctor [--json]
trace-db config [--json]
trace-db watch [--agent A[,A...]] [--mode partial|full] [--root PATH] [--interval SECONDS] [--debounce MS] [--once] [--json]
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
`stats`, `reindex`, `backup`, and `reconstruct` methods. Lower-level `model`,
`parsers`, and `store` modules remain public for custom importers and
specialized SQL access.

`trace-db backup PATH` publishes a consistent SQLite snapshot through a staging
directory and verifies the snapshot before returning. The destination must not
already exist; this avoids accidental replacement and includes WAL state,
normalized rows, the FTS index, provenance, and full-capture objects in one
portable archive file.

`trace-db gc --dry-run` reports unreferenced content-addressed full-capture
objects and their stored payload bytes. It never deletes objects; omitting
`--dry-run` is rejected until an explicit deletion policy and recovery workflow
are defined.

Privacy boundaries are explicit. Built-in credential patterns and configured
`redact_patterns` apply when normalized sessions/events are stored; the raw
full-capture object remains byte-identical for exact reconstruction. Search
snippets also apply presentation redaction. Set `TRACEDB_REDACT_PATTERNS` to a
semicolon-separated list, or pass repeatable global `--redact-pattern` flags;
CLI values override environment and config-file values. Invalid expressions
are rejected before an ingest starts.

For a versioned cross-language boundary, `trace-db serve` exposes the
`tracedb.v1` gRPC service over loopback TCP or a Unix domain socket. The
checked-in Protobuf contract supports ingest, search, show, stats, reindex, and
reconstruction; see [`references/protocol.md`](references/protocol.md) for
compatibility and security details.

The gRPC adapter uses a bounded pool of read-only SQLite connections for
search, show, and stats calls. Mutating calls share one writer and all SQLite
work runs on blocking workers, so concurrent reads do not queue behind ingest
or behind one another while write ordering remains explicit.

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

## Long-running watch

`trace-db watch` performs an immediate startup ingest, then coalesces native
filesystem notifications using the configured debounce and runs a periodic
fallback scan at the configured interval. It waits briefly for changed files
to become stable before parsing and continues with best-effort per-candidate
errors. If filesystem notifications are unavailable or the watcher channel
closes, periodic scans continue. Press Ctrl-C for a clean shutdown.

Human output is written as concise run summaries, with watcher and ingest
issues on stderr. `--json` (or `output_format = "json"`) writes one JSON event
per line followed by a final summary object, with no progress text mixed into
stdout. `--once` runs only the startup ingest and exits, which is useful for
launch probes and automation tests.

For service-manager examples covering launchd, systemd user services, and
Windows Task Scheduler, see [`references/watch.md`](references/watch.md).

## Health diagnostics

Every mutating ingest stores compact operational telemetry in archive metadata:
completion time, discovered/ingested/skipped/failed counts, and the cumulative
failure count. `trace-db doctor --json` reports that last status together with
archive lag relative to the newest discovered native candidate.

Doctor also probes archive and native-root permissions, verifies configured
watch timings and filesystem-notification readiness, and reports when periodic
fallback will be used. Backup guidance distinguishes empty archives,
rebuildable partial archives, and archives containing full native snapshots;
full captures receive the strongest recommendation because the archive may be
the only exact reconstruction source. A failed most-recent ingest makes doctor
unhealthy while preserving all detailed counts in its JSON report.

## Deterministic benchmarks

`trace-db-bench` generates isolated, deterministic Codex JSONL datasets and
runs the canonical Rust facade through first partial ingest, unchanged ingest,
a deterministic 1% changed ingest, full-capture upgrade, search, list, show,
stats, reindex, verify, and reconstruction. The standard suite covers 1,000,
10,000, and 100,000 sessions; smaller custom counts are useful while developing:

```bash
cargo run --release --bin trace-db-bench -- --sessions 1k --out target/bench-1k
cargo run --release --bin trace-db-bench -- --sessions 1k,10k,100k --json > report.json
```

Every operation reports wall and CPU time, process peak RSS, database size
including SQLite WAL/SHM sidecars, physical process write bytes where the host
exposes them, logical source bytes, and write amplification. Write amplification
is physical process write bytes divided by source bytes parsed or captured in
that ingest phase. It is `null` for unchanged/read-only operations and hosts
without a physical-write counter. The JSON contract is versioned as
`tracedb-benchmark-v1`; CPU/write values are process deltas while peak RSS is a
process-lifetime high-water mark.

## Relevance evaluation

`trace-db-relevance` runs a deterministic labeled-query suite against normalized
fixtures through the same `TraceDb::search` facade used by the CLI and bindings.
The suite includes multilingual and multi-term queries, title ranking, cwd
filtering, tool/error event signals, model/provider metadata preservation,
old-but-important history, parent/subagent and fork lineage, and distant-context
answerability. It reports Recall@5, Recall@10, MRR,
nDCG@10, lineage-collapse accuracy, context answerability, and per-tag slices.

```bash
cargo run --release --bin trace-db-relevance -- --json > relevance.json
```

The machine-readable report is versioned as `tracedb-relevance-v1`. Labels use
graded relevance from 0 (not relevant) through 3 (highly relevant); nDCG uses
the graded gain `2^grade - 1`, while Recall counts relevant lineage results
once after collapse.

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

Without tokenizer configuration, the portable binary uses SQLite's bundled
`unicode61` tokenizer. Setting `tokenizer_extension` or
`TRACEDB_JIEBA_EXT` without an explicit tokenizer selects `jieba`; selecting
`jieba` requires an extension path, and a configured extension load failure is
reported instead of silently falling back. A higher-precedence explicit
`unicode61` selection clears a lower-precedence extension.

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
