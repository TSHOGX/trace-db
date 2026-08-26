# TraceDB service protocol

TraceDB's durable cross-language contract is the Protobuf package
`tracedb.v1`, defined in `proto/tracedb/v1/tracedb.proto`. The Rust CLI and
gRPC server both call the same `TraceDb` facade, so transport clients and
in-process Rust callers share storage and retrieval semantics.

## Compatibility rules

- Removed fields are retired with `reserved` so their numbers and names are
  never reused. The ingest-mode fields were removed this way once capture
  became unconditional.
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
| `List` | Returns stable cursor-paginated session summaries with agent, cwd, time, model, provider, optional terminal status, fingerprint, direct lineage metadata (`parentSessionId`, `parentRelation`, `subagentCount`), and materialized counters (`turns`, `toolCalls`, `errors`, and nullable `inputTokens`/`outputTokens`/`totalTokens`). `cwdExact` avoids substring-prefix collisions; optional lineage collapse hides a child only when its parent is in the same filtered scope. |
| `Coverage` | Returns one session's fingerprint, archive commit time, event/source counts, `sourceBytes`, and latest source mtime without loading its trace. Every value is a materialized column, so the call is a single indexed row read. |
| `Show` | Returns session metadata, sources, normalized events, and first-class turn-internal spans. Events may include producer-supplied `createdAtMs` and `endedAtMs`; absent end times remain null rather than being inferred. `parentKind` discriminates overloaded native parent links. |
| `Stats` | Returns archive-wide and per-agent counts. |
| `Reindex` | Rebuilds the gated FTS index and repairs every derived projection: session aggregates and turn-internal spans. It is the one command that recomputes derived state. |
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

## The line protocol

`trace-db api` reads one JSON request per line and writes one JSON response per
line. There is exactly one protocol version, and it is the only one: the
earlier unversioned spelling was removed rather than frozen, because it was
internally inconsistent — `search` returned `endedAtMs` while `show` returned
`ended_at_ms` for the same concept — and preserving it would have meant
preserving that defect. `"version":2` may be sent explicitly and is validated;
omitting it selects the same protocol.

Every key the archive owns is `camelCase`, in requests and responses alike:
`sinceMs`, `cwdExact`, `collapseLineage`, `fromIdx`, `toIdx`, `kinds`,
`outDir`. The rule is enforced at the source — the Rust model and facade types
serialize `camelCase` directly — so no serialization step rewrites keys, and no
response can disagree with another about how a field is spelled.

Requests are typed and reject unknown fields with `invalid_argument`. This is
deliberate: a misspelled `since_ms` is an error rather than a silently dropped
constraint that would turn a filtered query into an unfiltered archive scan.
Dispatch is an internally tagged enum, so adding an operation is a compile
error until it is handled.

Vendor-opaque payloads are the one exception to the casing rule. The `dataJson`
and `meta` *keys* follow it, but their values are producer JSON and pass
through byte-for-byte — a `vendor_key` written by an agent is still
`vendor_key` on the way out. Consumers can therefore always recover exactly
what the producer wrote.

Errors use a stable envelope. `invalid_json` means the line was not JSON;
`invalid_argument` means the request was JSON but violated the schema;
`unsupported_operation` names the operations that exist; `operation_failed`
carries an archive or filesystem failure. A malformed line never terminates the
stream.

## Concurrency semantics

`Search`, `Show`, and `Stats` use a bounded pool of read-only WAL connections
and may execute concurrently with each other and with a write. `Ingest`,
`Reindex`, and `Reconstruct` use one serialized writer, so mutating calls never
execute concurrently on the canonical facade connection.
All SQLite work is dispatched to blocking workers rather than running on tonic
runtime threads. SQLite busy handling remains bounded by the archive's
five-second busy timeout.
