#!/usr/bin/env python3
"""Verify a reconstructed OpenCode bundle with an installed OpenCode CLI."""

from __future__ import annotations

import argparse
import json
import os
import shutil
import sqlite3
import subprocess
import tempfile
from pathlib import Path


def run(command: list[str], *, env: dict[str, str] | None = None) -> subprocess.CompletedProcess[str]:
    return subprocess.run(command, text=True, capture_output=True, env=env, check=False)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("database", type=Path, help="an existing OpenCode opencode.db")
    parser.add_argument("--trace-db", type=Path, default=Path("target/debug/trace-db"))
    parser.add_argument("--opencode", default="opencode")
    args = parser.parse_args()
    database = args.database.resolve()
    trace_db = args.trace_db.resolve()
    if not database.is_file():
        parser.error(f"OpenCode database does not exist: {database}")
    if not trace_db.is_file():
        parser.error(f"TraceDB binary does not exist: {trace_db}")
    if shutil.which(args.opencode) is None:
        parser.error(f"OpenCode executable is not on PATH: {args.opencode}")

    with tempfile.TemporaryDirectory(prefix="tracedb-opencode-compat-") as directory:
        root = Path(directory)
        archive = root / "trace.db"
        ingest = run(
            [
                str(trace_db), "--db", str(archive), "ingest", "--agent", "opencode",
                "--mode", "full", "--root", str(database), "--format", "json",
            ]
        )
        if ingest.returncode != 0:
            raise SystemExit(f"TraceDB ingest failed:\n{ingest.stderr}")
        report = json.loads(ingest.stdout)
        ingested = sum(agent["ingested"] for agent in report["agents"])
        if ingested < 1:
            raise SystemExit("TraceDB did not ingest an OpenCode session")

        with sqlite3.connect(database) as connection:
            session_id = connection.execute(
                "SELECT id FROM session ORDER BY time_updated DESC, id LIMIT 1"
            ).fetchone()[0]
        trace_id = f"opencode:{session_id}"
        output = root / "restore"
        reconstruct = run(
            [str(trace_db), "--db", str(archive), "reconstruct", trace_id,
             "--out", str(output), "--format", "json"]
        )
        if reconstruct.returncode != 0:
            raise SystemExit(f"TraceDB reconstruction failed:\n{reconstruct.stderr}")
        native = output / f"{session_id}.db"
        if not native.is_file():
            raise SystemExit(f"reconstruction did not produce {native}")

        environment = {**os.environ, "OPENCODE_DB": str(native)}
        query = run(
            [args.opencode, "db", "SELECT count(*) AS sessions FROM session", "--format", "json"],
            env=environment,
        )
        if query.returncode != 0:
            raise SystemExit(f"OpenCode could not open reconstructed database:\n{query.stderr}")
        if json.loads(query.stdout)[0]["sessions"] != 1:
            raise SystemExit("reconstructed OpenCode database has an unexpected session count")

        exported = run([args.opencode, "export", session_id], env=environment)
        if exported.returncode != 0:
            raise SystemExit(f"OpenCode export failed:\n{exported.stderr}")
        json.loads(exported.stdout)
        version = run([args.opencode, "--version"])
        version_text = version.stdout.strip() or version.stderr.strip()
        print(f"OpenCode compatibility verified ({version_text}) for session {session_id}")


if __name__ == "__main__":
    main()
