# TraceDB architecture

TraceDB has four layers:

1. **Discovery and parsers** enumerate native stores and emit a
   `ParsedSession` containing a cross-agent session plus ordered events.
2. **The archive store** transactionally upserts sessions, events, provenance,
   and optional full native objects into SQLite.
3. **Retrieval** uses SQLite FTS5 for bounded event candidates, aggregates them
   at session level, and collapses parent/fork/subagent lineage. Lineage is
   loaded with a recursive query rooted at matched sessions, so unrelated
   archive history does not participate in every search.
4. **Interfaces** expose the Rust crate, the CLI, the long-running watch loop,
   and the line-oriented JSON protocol.

## Rust module map

```text
src/
  config.rs       typed TOML/environment/CLI resolution and exclusion globs
  lib.rs          crate exports and database path resolution
  facade.rs       typed TraceDb lifecycle and request/result API
                  watch loop with notification and periodic fallback
  benchmark.rs    deterministic native fixtures, lifecycle runner, and metrics
  bin/
    trace-db-bench.rs  standalone text/JSON benchmark interface
    trace-db-relevance.rs  standalone labeled search evaluator
  service.rs      tracedb.v1 gRPC adapter and local transports
  main.rs         clap CLI and JSON protocol server
  model.rs        agents, events, spans, sessions, provenance
  store.rs        SQLite schema, upsert, FTS, search, reconstruction
  parsers/
    mod.rs        parser trait and registry
    claude.rs
    codex.rs
    opencode.rs
    gemini.rs
    pi.rs
native/fts5-jieba/  optional Rust FTS5 tokenizer extension
proto/tracedb/v1/   stable cross-language Protobuf contract
```

## Rebuildability and full capture

Normalized tables are deterministic projections and can be rebuilt from native
stores. Every ingest also stores a compressed object for each source, addressed
by SHA-256; objects are deduplicated across sessions and restored only through
validated relative paths. The archive never treats the normalized projection as
the source of truth.

Capture is unconditional, so there is no ingest-mode column and no lossy
record to reason about. Because the normalized tables are a rebuildable
projection, TraceDB carries no per-column schema upgrades either: an archive
whose stored `schema_version` differs from the running build is refused with
guidance to delete it and re-ingest. `trace-db import` requires the same exact
version match instead of probing the source for individual columns.

`TraceDb::backup` uses SQLite's consistent `VACUUM INTO` snapshot mechanism in
a sibling staging directory, refuses existing destinations, atomically publishes
the completed file, and verifies the published archive before returning.

`TraceDb::gc(true)` reports unreferenced content-addressed object payloads. The
current lifecycle contract is deliberately dry-run-only: object deletion is not
performed until recovery, retention, and crash-safety semantics are specified.

No privacy transformation is applied at ingest or retrieval. Any redaction must
be an explicit caller-owned presentation/export operation and must not be
persisted back into the archive.

Reconstruction can additionally emit a versioned restore manifest containing
the output paths, source locators, object hashes, sizes, and preserved metadata
for every atomically written file.

OpenCode full capture stores the original database file (and durable WAL
sidecar when present), plus both a native SQLite session bundle
(`opencode-native-session-v1`) and a portable JSON fallback. The native bundle
clones the source database schema and migration journal, then copies only the
selected project/session/message/part rows plus compatibility metadata. This
preserves the exact schema of the source OpenCode installation instead of
assuming a fixed four-table shape. `scripts/verify-opencode-compat.py` validates
the restored bundle with an installed OpenCode CLI; future OpenCode releases
must be rechecked because their migration schema can change.

## Runtime configuration

`TraceDbConfig` is the canonical resolved runtime configuration shared by the
CLI and `TraceDb::open_default`. It applies CLI/embedding overrides,
environment variables, a strict TOML file, and platform defaults in descending
precedence. Python and Node default opens remain thin facade calls and inherit
the same behavior. Explicit-path facade opens retain their narrow legacy
semantics for embedders that manage storage and tokenizer loading themselves.

Configuration-file paths are anchored to the file directory before later
layers are applied. Native-source exclusions compile once per ingest request
and run against normalized candidate locators and paths before parsing.

Successful ingest calls persist compact last-run telemetry, a cumulative
failure count, and a monotonic acknowledgement sequence in `schema_meta`.
The acknowledgement is committed after all candidate writes and is returned
only after the metadata transaction succeeds, so callers do not need to infer
an ingestion boundary from source `endedAtMs` values. Doctor reads this metadata without migrating
the archive, compares the newest native candidate with the last ingest time,
probes watcher and permission readiness, and derives backup guidance from the
number of archived sessions.

The gRPC adapter keeps one serialized writer for ingest, reindex, and
reconstruction, plus a bounded pool of read-only SQLite connections for search,
show, and stats. Each operation runs on a blocking worker so synchronous
SQLite calls do not occupy asynchronous runtime threads; in-memory test
archives intentionally use the writer connection for reads because SQLite
`:memory:` databases are connection-local.

Ingest has two explicit performance invariants:

- Native parsing is parallel only within a bounded worker count derived from
  available CPU, and results are reassembled in discovery order. This keeps
  throughput scalable without creating one OS thread per candidate or making
  reports/session writes nondeterministic.
- Parsed sessions for one agent are committed in one SQLite transaction. If a
  candidate makes that batch fail, the facade retries the same batch as
  individual transactions to preserve best-effort failure isolation. The fast
  path therefore pays O(agent) commit boundaries while the recovery path keeps
  the historical per-candidate semantics.
- Claude, Codex, and Pi JSONL sources are parsed as a stream. The parser keeps
  only the normalized event projection and bounded metadata, rather than a
  second in-memory copy of every native JSON value; Gemini's legacy embedded
  array format remains buffered because its shape requires materializing the
  document before normalization.

## Materialized session aggregates

Session-level facts that are deterministic functions of the event stream and
retained sources are materialized at ingest rather than recomputed per query:
event/turn/tool-call/error counts, token totals, the first user and last
assistant previews, source count and bytes, the newest source mtime, and the
`sort_time` ordering key. `list` therefore projects only stored columns and
pages directly from `sessions_sort_idx`, and `coverage` is one indexed row read.

`model::SessionAggregates` is the single definition of the event-derived values.
Two aggregates cannot come from the event stream and are derived in SQL instead:
`child_count` is a property of the parent that only the set of other sessions
knows, and the source totals must describe the rows the archive actually
retained, which include snapshots preserved from earlier ingests.

`child_count` is recomputed from the indexed lineage edge inside the writing
transaction rather than incremented. Deriving instead of adjusting is what makes
it correct when a child is ingested before its parent, when the same child is
re-ingested, and when a child is re-parented; a counter would drift in all
three cases.

Because these are projections, `reindex` repairs them and `verify` reports drift
as the `session_aggregates` check. `import` recomputes them after a merge, since
the counts describe the union rather than either input archive.

## Lineage

There are three independent relationship layers:

- Typed event lineage preserves producer-native predecessor/message links
  without treating them as structural spans.
- Session lineage is one typed edge: `parent_session_id` plus a
  `parent_relation` of `subagent` or `fork`. Forks additionally record the
  parent's native branch point, so no field packs a tuple into a string and no
  query predicate parses one.
- First-class spans represent turn-internal tools and delegations. Spans can
  exist without a child session or a dedicated event, so one Workflow call can
  parent multiple `task_id` delegates and promoted tool calls remain native to
  their containing session.

Claude subagents prove their parent through the nested path
`<parent>/subagents/agent-*.jsonl`. Codex stores the edge only in the parent's
`spawn_agent` call/output pair, so the parser performs a cross-rollout pre-pass.
OpenCode exposes `parent_id` directly in its SQLite session table.

Span projection is deterministic and materialized during ingestion. Tool
calls/results with the same `call_id` form one interval; known delegation tools
use `delegation` kind. Structured payloads containing multiple `task_id`
objects create child delegation spans under the host call. Missing end/status
evidence remains null rather than being inferred.

## Compatibility and extension policy

The repository contains only the Rust implementation. New agent support should
implement the `Parser` trait and register the parser without changing storage
or interface contracts. The CLI and JSON protocol call the same `TraceDb`
facade used by in-process Rust integrations. Cross-language clients should use
the protocol rather than couple themselves to SQLite internals.
