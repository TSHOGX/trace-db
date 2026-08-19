"""Thin Python bindings for the TraceDB Rust archive facade."""

from ._native import PyTraceDb

TraceDb = PyTraceDb

__all__ = ["TraceDb"]
