#!/usr/bin/env python3
"""Verify that every published TraceDB package uses the release version."""

from __future__ import annotations

import argparse
import json
import tomllib
from pathlib import Path


ROOT = Path(__file__).resolve().parent.parent


def toml_version(path: str) -> str:
    with (ROOT / path).open("rb") as file:
        document = tomllib.load(file)
    return document.get("project", document.get("package", {}))["version"]


def json_version(path: str) -> str:
    with (ROOT / path).open(encoding="utf-8") as file:
        return json.load(file)["version"]


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "version",
        nargs="?",
        help="release version, optionally prefixed with v; defaults to the Rust crate",
    )
    arguments = parser.parse_args()
    versions = {
        "Rust crate": toml_version("Cargo.toml"),
        "tokenizer crate": toml_version("native/fts5-jieba/Cargo.toml"),
        "Python crate": toml_version("bindings/python/Cargo.toml"),
        "Python package": toml_version("bindings/python/pyproject.toml"),
        "Node crate": toml_version("bindings/node/Cargo.toml"),
        "Node package": json_version("bindings/node/package.json"),
    }
    release_version = (
        arguments.version.removeprefix("v")
        if arguments.version
        else versions["Rust crate"]
    )
    mismatches = {
        name: version
        for name, version in versions.items()
        if version != release_version
    }
    if mismatches:
        details = ", ".join(
            f"{name}={version}" for name, version in mismatches.items()
        )
        raise SystemExit(
            f"release version {release_version} does not match: {details}"
        )
    print(f"all published packages use version {release_version}")


if __name__ == "__main__":
    main()
