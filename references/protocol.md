# TraceDB service protocol

TraceDB's durable cross-language contract is the Protobuf package
`tracedb.v1`, defined in `proto/tracedb/v1/tracedb.proto`. The Rust CLI and
gRPC server both call the same `TraceDb` facade, so transport clients and
in-process Rust callers share storage and retrieval semantics.

## Compatibility rules

- Existing field numbers are never reused or changed.
- Existing method request and response types remain wire-compatible.
- New fields are additive and clients must tolerate unknown fields.
- New coding agents and event kinds are represented as lowercase strings so an
  older generated client can carry values introduced by a newer server.
- A breaking contract requires a new Protobuf package such as `tracedb.v2`.
- Request validation failures use `INVALID_ARGUMENT`; disabled reconstruction
  uses `PERMISSION_DENIED`; archive and worker failures use `INTERNAL`.
  Clients should branch on the gRPC status code and treat the message as
  diagnostic text rather than a stable identifier.

The generated Rust modules are exported as `tracedb::proto`. Python, Node.js,
Go, and other clients should generate their native bindings directly from the
checked-in `.proto` file.

## Running the service

Loopback TCP is the cross-platform default:

```bash
trace-db serve
trace-db serve --listen 127.0.0.1:50052
```

Unix platforms can use a local domain socket:

```bash
trace-db serve --socket /tmp/tracedb.sock
```

The service has no authentication or TLS. TraceDB refuses a non-loopback TCP
listener unless `--allow-remote` is explicit. Remote deployments should place
the service behind a secured transport rather than expose it directly.

Reconstruction is disabled unless the server starts with
`--reconstruct-root PATH`. Clients then provide only a safe relative
`out_dir`; absolute paths and parent traversal are rejected.

The service performs no implicit privacy redaction. CLI, gRPC, and language
bindings receive the stored values unchanged. Callers that need redaction must
apply it to their presentation/export copy and must not write that copy back.

The Rust facade exposes `RestoreManifest` with schema version
`tracedb-restore-manifest-v1`; the CLI can write this artifact with
`reconstruct --manifest PATH`. Existing reconstruction APIs continue returning
the written paths for compatibility.

OpenCode full captures include the original SQLite database bytes (and durable
WAL sidecar when present), plus a native SQLite bundle tagged
`opencode-native-session-v1` and a portable JSON fallback. The native bundle
copies the source schema and migration journal for the selected OpenCode
database, so it is version-matched to that source. Verify it against the
OpenCode release you intend to use with
`scripts/verify-opencode-compat.py`; compatibility with future migrations is
not implied.

## Methods

The table includes the stable gRPC RPCs and the related Rust facade/CLI
operations. `Backup` and `Gc` are intentionally facade/CLI-only operations;
they are not part of the `tracedb.v1` wire service.

| Method | Behavior |
|---|---|
| `Ingest` | Discovers native stores and transactionally ingests sessions, returning structured per-locator warnings and failures plus a durable monotonic `ack` sequence. Consumers should persist the ack instead of deriving a watermark from `endedAtMs`. |
| `Search` | Returns lineage-collapsed session hits. |
| `List` | Returns stable cursor-paginated session summaries with agent, cwd, time, mode, model, provider, optional terminal status, fingerprint, and direct lineage metadata (`parentSessionId`, `parentRelation`, `subagentCount`). `cwdExact` avoids substring-prefix collisions; optional lineage collapse hides a child only when its parent is in the same filtered scope. |
| `Coverage` | Returns one session's fingerprint, archive commit time, capture mode, event/source counts, and latest source mtime without loading its trace. |
| `Show` | Returns session metadata, sources, normalized events, and first-class turn-internal spans. Events may include producer-supplied `createdAtMs` and `endedAtMs`; absent end times remain null rather than being inferred. `parentKind` discriminates overloaded native parent links. |
| `Stats` | Returns archive-wide and per-agent counts. |
| `Reindex` | Rebuilds the gated FTS index. |
| `Backup` | Exposed by the CLI and Rust facade; creates a verified archive snapshot. |
| `Gc` | Exposed by the CLI and Rust facade as a non-destructive orphan-object dry run. |
| `Reconstruct` | Writes full-capture native sources below a server-local output directory. |

Messages are capped at 64 MiB by the bundled server. Generated clients may
need their receive limit raised to the same value when reading large sessions.

Session status is optional and uses `active`, `completed`, `failed`,
`interrupted`, or `abandoned`. Null means the producer did not provide enough
evidence; TraceDB does not equate a last observed timestamp with clean
completion.

Event `parentId` is intentionally accompanied by optional `parentKind`:
`previous_event` is a transcript predecessor chain, `message_parent` is a
native message relationship, and `native_mixed` warns that the producer uses
multiple meanings. Consumers must not treat an untyped or unsupported parent
link as a structural span.

Spans are session-local trajectories with stable IDs, optional parent IDs,
tool/delegation kind, event bounds, time bounds, outcome, and structured native
metadata. They do not require a corresponding child session. Events that
participate in a trajectory expose `spanId`; multiplexed delegates can exist as
child spans even when the source emitted them inside one host event.

The line-oriented `trace-db api` rejects unknown request fields with an
`invalid_argument` error. This is deliberate: for example, `list` accepts the
human-friendly `since` string, while an accidental `since_ms` field is rejected
instead of silently turning into an unfiltered archive scan.

Line API version 2 is selected with `"version":2`. Its operation requests are
deserialized into strict typed structures with unknown-field rejection and
consistent camelCase names (`sinceMs`, `cwdExact`, `fromIdx`, `toIdx`, `kinds`,
`collapseLineage`, `outDir`). Version 1 remains available for compatibility. V2 camel-cases only
the normalized envelope/model; vendor keys inside `dataJson` and `meta` remain
byte-for-byte semantic JSON keys.

## Concurrency semantics

`Search`, `Show`, and `Stats` use a bounded pool of read-only WAL connections
and may execute concurrently with each other and with a write. `Ingest`,
`Reindex`, and `Reconstruct` use one serialized writer, so mutating calls never
execute concurrently on the canonical facade connection.
All SQLite work is dispatched to blocking workers rather than running on tonic
runtime threads. SQLite busy handling remains bounded by the archive's
five-second busy timeout.
