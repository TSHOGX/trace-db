"""Installed-wheel smoke test for the public Python binding."""

from __future__ import annotations

import json
import tempfile
from pathlib import Path

from tracedb import TraceDb


def main() -> None:
    with tempfile.TemporaryDirectory(prefix="tracedb-python-") as directory:
        root = Path(directory)
        (root / "session-python.json").write_text(
            json.dumps(
                {
                    "sessionId": "python",
                    "startTime": "2026-08-19T00:00:00Z",
                    "lastUpdated": "2026-08-19T00:00:01Z",
                    "messages": [
                        {"id": "u", "type": "user", "content": "deploy python"}
                    ],
                }
            ),
            encoding="utf-8",
        )
        database = TraceDb.open(root / "trace.db")
        report = database.ingest(["gemini"], root=root)
        assert report["agents"][0]["ingested"] == 1
        assert database.search("deploy")[0]["id"] == "gemini:python"
        assert database.show("gemini:python")["events"][0]["text"] == "deploy python"
        assert database.reconstruct("gemini:python", root / "restored") == []
        assert json.loads(database.stats_json())["totalSessions"] == 1
        database.reindex()


if __name__ == "__main__":
    main()
