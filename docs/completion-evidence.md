# Completion Evidence

This matrix maps the project completion criteria to current direct evidence.
It is intentionally conservative: a configured CI step is not considered an
executed result until a runner has actually completed it.

| Criterion | Current evidence | Status |
| --- | --- | --- |
| Five-agent discovery, parsing, incremental ingest, errors | Real installed-version dry-runs for Claude Code, Codex, OpenCode, Gemini CLI, and Pi; parser robustness and structured-failure tests | Proven for recorded versions |
| No implicit redaction or data loss | Credential-preservation tests, exact native-source fixture reconstruction, and real selected-file SHA-256 matches | Proven for tested inputs |
| Capture/source/reconstruction boundaries | Full-only canonical mode, legacy partial verification, restore manifests, source-trace preservation, real reconstruction probes | Proven |
| Verify/doctor/backup/restore/lifecycle | Corruption, permission, telemetry, idempotent import, verified backup, reconstruction preflight, and dry-run-only GC tests | Proven |
| CLI/API/gRPC output and errors | JSON/JSONL/Markdown contracts, structured API errors, gRPC status-code/range/kind/security tests, protocol compatibility rules | Proven for v1 contracts |
| Platform install, upgrade, checksums, packages | Five-target release matrix; Unix offline install/upgrade test; Windows zip, wheel, Node package, checksum, SBOM, and attestation workflow checks | Proven except published attestation execution |
| Long-running watch and daemon recovery | Watch notification/fallback tests; real macOS launchd and Linux systemd startup ingest, crash restart, state transition, and cleanup smokes | Proven on macOS/Linux |
| 100k search/resource baseline | Versioned 100,000-session/601,000-event report with 60-sample search p95, RSS, DB size, write amplification, unchanged ingest | Proven on recorded host |
| Relevance/lineage/context | Labeled deterministic evaluation with ranking metrics, lineage-collapse accuracy, context answerability, and tagged slices | Proven for evaluation suite |
| gRPC reads not globally serialized | Independent read pool tests and a read-not-blocked-by-writer concurrency test | Proven |
| OpenCode native reconstruction | OpenCode 1.18.18 opens, queries, and exports a schema-cloned reconstructed session | Proven for 1.18.18 |

## Remaining direct evidence

The Windows Task Scheduler implementation and reusable
`scripts/smoke-windows-task.ps1` are covered by Windows-target compilation,
XML contract tests, and an actionlint-valid Windows CI matrix step. This local
checkout has no git remote, and `TSHOGX/trace-db` is not visible through the
authenticated GitHub API, so the workflow cannot be triggered or inspected
from the current environment. A successful execution on a real Windows runner
remains the only unproven daemon platform result.
