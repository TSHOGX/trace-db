# Benchmark Baseline and Optimization Checkpoints

The reproducible command below measures the standard 100k-session workload and
writes the versioned JSON report to the selected output file:

```bash
cargo run --release --bin trace-db-bench -- \
  --sessions 100k --out target/bench-100k --json > target/bench-100k.json
```

The historical pre-optimization baseline was captured on 2026-08-21 with TraceDB benchmark schema
`tracedb-benchmark-v3`, macOS 26.2 on an Apple M1 Pro (arm64):

| Measure | Baseline |
| --- | ---: |
| Sessions / events | 100,000 / 601,000 |
| Native source bytes | 141,010,000 |
| Search samples | 60 (three queries × 20 repetitions) |
| Search p95 | 1.67 s |
| Search operation wall time | 58.37 s |
| Peak RSS | 658 MiB |
| First full-ingest wall time | 188.55 s |
| First full-ingest write amplification | 222.30x |
| Unchanged-ingest parsed / unchanged | 0 / 100,000 |
| 1% changed-ingest wall time | 30.34 s |
| 1% changed-ingest write amplification | 231.26x |

These values are an environment-specific baseline, not performance limits. The
JSON report is authoritative for reruns; compare like-for-like host, build mode,
database storage, and workload before drawing regressions.

## Post-optimization checkpoints

The following checkpoints were captured on 2026-08-25 on the same arm64 macOS
class of host. They are not a replacement for the 100k baseline because they
use smaller workloads, but they make the architectural improvements visible:

| Checkpoint | First ingest | Unchanged ingest | Search operation | Peak RSS | Write amplification |
| --- | ---: | ---: | ---: | ---: | ---: |
| 1k, current `a66db36` | 267 ms | 5.8 ms | 507 ms | 17.9 MiB | 2.62x |
| 10k, post-batch/search `91e8189` checkpoint | 3.80 s | 104 ms | 4.34 s | 97.9 MiB | 4.75x |

### Materialized session aggregates

Measured on the 10,000-session suite immediately before and after materializing
the session aggregates, same host and build mode:

| Operation | Before | After | Change |
| --- | ---: | ---: | ---: |
| `list` | 2.6 ms | 0.2 ms | −93% |
| `stats` | 9.4 ms | 1.7 ms | −82% |
| `unchanged_ingest` | 70.7 ms | 60.2 ms | −15% |
| `first_ingest` | 5.06 s | 5.10 s | +0.8% |
| `reindex` | 62.6 ms | 196.8 ms | +215% |
| `verify` | 374 ms | 411 ms | +10% |

`list` no longer scans the session table: `EXPLAIN QUERY PLAN` reports a single
`SCAN s USING INDEX sessions_sort_idx`, replacing a full scan plus two
correlated subqueries per row and a temp B-tree sort. The old plan cost grew
with archive size even for a fixed page — 0.21 ms of query CPU at 1,000 sessions
against 0.73 ms at 10,000 — while the materialized projection stays at roughly
0.09 ms regardless.

The `reindex` and `verify` regressions are the intended cost of the new
contract: `reindex` now repairs every materialized aggregate in addition to
rebuilding the FTS index, and `verify` gained a drift check that recomputes the
aggregates it validates.

## Windowed show

The standard suite's sessions hold roughly six events each, which cannot show
the cost of a windowed read. A 20,000-event single session, measured over 20
iterations after warmup on the same arm64 macOS class of host:

| `show` call | Before | After |
| --- | ---: | ---: |
| Whole session (20,000 events) | 33.7 ms | 33.5 ms |
| 10-event window (`--from 1000 --to 1009`) | 33.2 ms | 0.35 ms |

Before the change a 10-event window cost the same as the whole session, because
every row was fetched and fully deserialized — including `data_json` and
`usage_json` — and then discarded in memory. Pushing the bounds into SQL makes
the window cost proportional to the window. `events_session_idx` serves it as a
two-bounded `SEARCH`, so nothing scans:

```text
SEARCH events USING INDEX events_session_idx (session_id=? AND idx>? AND idx<?)
```

The whole-session path is deliberately unchanged; it does the same work it
always did.

The current 1k run also wrote only 16 KiB during the unchanged pass. The
streaming parser primarily reduces memory pressure for large native JSONL files;
the synthetic benchmark's small files therefore show similar wall time to the
previous 1k batch-ingest checkpoint. Re-run the full suite after any storage or
parser architecture change:

```bash
cargo run --release --bin trace-db-bench -- \
  --sessions 1k,10k,100k --json > target/bench-current.json
```
