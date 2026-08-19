# TraceDB architecture

TraceDB has four layers:

1. **Discovery and parsers** enumerate native stores and emit a
   `ParsedSession` containing a cross-agent session plus ordered events.
2. **The archive store** transactionally upserts sessions, events, provenance,
   and optional full native objects into SQLite.
3. **Retrieval** uses SQLite FTS5 for bounded event candidates, aggregates them
   at session level, and collapses parent/fork/subagent lineage.
4. **Interfaces** expose the Rust crate, the CLI, and the line-oriented JSON
   protocol.

## Rust module map

```text
src/
  lib.rs          crate exports and database path resolution
  facade.rs       typed TraceDb lifecycle and request/result API
  service.rs      tracedb.v1 gRPC adapter and local transports
  main.rs         clap CLI and JSON protocol server
  model.rs        agents, capture modes, events, sessions, provenance
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

Normalized tables are deterministic and can be rebuilt from native stores.
Partial mode stores source pointers only. Full mode stores a compressed object
for each source, addressed by SHA-256. Objects are deduplicated across sessions
and are restored only through validated relative paths.

The `mode` column is monotonic: once a session has been captured in full mode,
subsequent partial ingests retain full mode and recapture the source object.

## Lineage

There are two independent trees:

- Event lineage links native event IDs within a session.
- Session lineage links forks and subagents across sessions.

Claude subagents prove their parent through the nested path
`<parent>/subagents/agent-*.jsonl`. Codex stores the edge only in the parent's
`spawn_agent` call/output pair, so the parser performs a cross-rollout pre-pass.
OpenCode exposes `parent_id` directly in its SQLite session table.

## Compatibility and extension policy

The repository contains only the Rust implementation. New agent support should
implement the `Parser` trait and register the parser without changing storage
or interface contracts. The CLI and JSON protocol call the same `TraceDb`
facade used by in-process Rust integrations. Cross-language clients should use
the protocol rather than couple themselves to SQLite internals.
