# Benchmark Baseline

The reproducible command below measures the standard 100k-session workload and
writes the versioned JSON report to the selected output file:

```bash
cargo run --release --bin trace-db-bench -- \
  --sessions 100k --out target/bench-100k --json > target/bench-100k.json
```

The following baseline was captured on 2026-08-21 with TraceDB benchmark schema
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
