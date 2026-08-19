"""Native-style Python bindings for the TraceDB Rust archive facade."""

from __future__ import annotations

import json
import os
from typing import Any, Iterable

from ._native import PyTraceDb


class TraceDb:
    """A small Python facade over the native Rust TraceDB implementation."""

    __slots__ = ("_native",)

    def __init__(self, native: PyTraceDb) -> None:
        self._native = native

    @classmethod
    def open(cls, path: os.PathLike[str] | str | None = None) -> TraceDb:
        """Open an archive path, or the platform default when omitted."""
        native_path = None if path is None else os.fspath(path)
        return cls(PyTraceDb.open(native_path))

    def stats_json(self) -> str:
        """Return archive statistics as raw JSON."""
        return self._native.stats_json()

    def stats(self) -> dict[str, Any]:
        """Return archive statistics as a Python dictionary."""
        return json.loads(self.stats_json())

    def search_json(
        self,
        query: str,
        limit: int = 20,
        agent: str | None = None,
        cwd: str | None = None,
        since_ms: int | None = None,
    ) -> str:
        """Search normalized events and return raw JSON."""
        return self._native.search_json(query, limit, agent, cwd, since_ms)

    def search(
        self,
        query: str,
        limit: int = 20,
        agent: str | None = None,
        cwd: str | None = None,
        since_ms: int | None = None,
    ) -> list[dict[str, Any]]:
        """Search normalized events and return Python dictionaries."""
        return json.loads(self.search_json(query, limit, agent, cwd, since_ms))

    def ingest_json(
        self,
        agents: Iterable[str] | None = None,
        mode: str = "partial",
        root: os.PathLike[str] | str | None = None,
        since_ms: int | None = None,
    ) -> str:
        """Ingest native sessions and return the report as raw JSON."""
        native_root = None if root is None else os.fspath(root)
        native_agents = None if agents is None else list(agents)
        return self._native.ingest_json(native_agents, mode, native_root, since_ms)

    def ingest(
        self,
        agents: Iterable[str] | None = None,
        mode: str = "partial",
        root: os.PathLike[str] | str | None = None,
        since_ms: int | None = None,
    ) -> dict[str, Any]:
        """Ingest native sessions and return the report as a dictionary."""
        return json.loads(self.ingest_json(agents, mode, root, since_ms))

    def show_json(self, session_id: str) -> str:
        """Return one normalized session trace as raw JSON."""
        return self._native.show_json(session_id)

    def show(self, session_id: str) -> dict[str, Any] | None:
        """Return one normalized session trace, or ``None`` when absent."""
        return json.loads(self.show_json(session_id))

    def reconstruct_json(
        self,
        session_id: str,
        out_dir: os.PathLike[str] | str,
        overwrite: bool = False,
    ) -> str:
        """Reconstruct full native sources and return their paths as raw JSON."""
        return self._native.reconstruct_json(session_id, os.fspath(out_dir), overwrite)

    def reconstruct(
        self,
        session_id: str,
        out_dir: os.PathLike[str] | str,
        overwrite: bool = False,
    ) -> list[str]:
        """Reconstruct full native sources and return their written paths."""
        return json.loads(self.reconstruct_json(session_id, out_dir, overwrite))

    def reindex(self) -> None:
        """Rebuild the full-text index."""
        self._native.reindex()


__all__ = ["TraceDb"]
