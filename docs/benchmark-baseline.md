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

The current 1k run also wrote only 16 KiB during the unchanged pass. The
streaming parser primarily reduces memory pressure for large native JSONL files;
the synthetic benchmark's small files therefore show similar wall time to the
previous 1k batch-ingest checkpoint. Re-run the full suite after any storage or
parser architecture change:

```bash
cargo run --release --bin trace-db-bench -- \
  --sessions 1k,10k,100k --json > target/bench-current.json
```
