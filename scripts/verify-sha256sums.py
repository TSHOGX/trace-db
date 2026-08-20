#!/usr/bin/env python3
"""Verify a complete SHA256SUMS manifest against a release directory."""

from __future__ import annotations

import argparse
import hashlib
import re
from pathlib import Path


HEX_DIGEST = re.compile(r"^[0-9a-fA-F]{64}$")


def fail(message: str) -> None:
    raise SystemExit(f"checksum verification failed: {message}")


def verify(directory: Path, manifest: Path) -> None:
    if not manifest.is_file():
        fail(f"manifest does not exist: {manifest}")
    entries: dict[str, str] = {}
    for line_number, line in enumerate(manifest.read_text(encoding="utf-8").splitlines(), 1):
        if not line.strip():
            continue
        parts = line.split(maxsplit=1)
        if len(parts) != 2:
            fail(f"malformed line {line_number}")
        digest, name = parts
        name = name.removeprefix("*")
        if not HEX_DIGEST.fullmatch(digest):
            fail(f"invalid SHA-256 digest on line {line_number}")
        if not name or Path(name).name != name or name in entries:
            fail(f"invalid or duplicate asset name on line {line_number}: {name!r}")
        entries[name] = digest.lower()

    assets = {
        path.name
        for path in directory.iterdir()
        if path.is_file() and path.name != manifest.name
    }
    if assets != set(entries):
        missing = sorted(assets - set(entries))
        extra = sorted(set(entries) - assets)
        fail(f"manifest coverage mismatch (missing={missing}, extra={extra})")
    for name, expected in sorted(entries.items()):
        actual = hashlib.sha256((directory / name).read_bytes()).hexdigest()
        if actual != expected:
            fail(f"digest mismatch for {name}")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("directory", type=Path)
    parser.add_argument("--manifest", type=Path, default=None)
    args = parser.parse_args()
    directory = args.directory.resolve()
    manifest = (args.manifest or directory / "SHA256SUMS").resolve()
    verify(directory, manifest)
    print(f"verified SHA256SUMS: {manifest}")


if __name__ == "__main__":
    main()
