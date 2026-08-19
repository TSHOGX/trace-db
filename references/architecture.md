# Architecture

TraceDB has four boundaries:

1. Parsers discover native sessions and map agent-specific records to the
   unified session/event types.
2. Ingest compares fingerprints and transactionally replaces changed sessions.
3. SQLite owns the normalized store and external-content FTS index.
4. The CLI and TypeScript exports provide retrieval and operational access.

Native traces remain authoritative. `raw_sources` records where they are and
enough size/mtime metadata to detect drift. TraceDB does not duplicate native
bytes, except for normalized event content required for retrieval.

## Module Map

```text
src/
  parsers/       one adapter per coding agent
  types.ts       unified session, source, event, and parser contracts
  ingest.ts      incremental discovery and upsert loop
  db.ts          trace.db schema, transactions, and FTS rebuild
  tokenizer.ts   bundled fts5-jieba loading and runtime resolution
  cli.ts         CLI commands and presentation
  index.ts       public TypeScript API
native/
  fts5-jieba/    Rust FTS5 tokenizer source
references/
  search-algorithm.md
```

## Deliberate Exclusions

No table stores generated summaries, embeddings, labels, memory candidates, or
workflow state. No code invokes a model. This keeps `trace.db` mechanically
rebuildable and makes downstream semantic processing an independent concern.
