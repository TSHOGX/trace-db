#!/usr/bin/env python3
"""Validate the contents and version metadata of TraceDB release artifacts.

The release workflow publishes three artifact families: native CLI archives,
Python wheels, and Node.js packages. This checker deliberately validates the
published shape rather than trusting the build directory, so a missing binary,
binding, manifest, or protocol file fails before an artifact is uploaded.
"""

from __future__ import annotations

import argparse
import json
import re
import tarfile
import zipfile
from pathlib import PurePosixPath
from typing import Iterable, NoReturn


NATIVE_BINARIES = ("trace-db", "trace-db-bench", "trace-db-relevance")


def fail(message: str) -> NoReturn:
    raise SystemExit(f"package verification failed: {message}")


def safe_member(name: str) -> str:
    if not name or name.startswith("/"):
        fail(f"unsafe absolute member path: {name!r}")
    path = PurePosixPath(name)
    if any(part in ("", ".", "..") for part in path.parts):
        fail(f"unsafe member path: {name!r}")
    return "/".join(path.parts)


def unique_names(names: Iterable[str]) -> list[str]:
    normalized = [safe_member(name) for name in names]
    if len(normalized) != len(set(normalized)):
        fail("artifact contains duplicate member names")
    return normalized


def infer_kind(path: str) -> str:
    if path.endswith((".tar.gz", ".tgz")):
        return "archive" if path.endswith(".tar.gz") else "npm"
    if path.endswith((".whl", ".zip")):
        return "wheel" if path.endswith(".whl") else "archive"
    fail(f"cannot infer artifact kind from {path!r}; pass --kind")


def expected_extension(target: str | None, names: list[str]) -> str:
    if target and "windows" in target:
        return "fts5jieba.dll"
    if target and "apple-darwin" in target:
        return "libfts5jieba.dylib"
    if target and "linux" in target:
        return "libfts5jieba.so"
    candidates = [
        name
        for name in names
        if name.endswith(("libfts5jieba.so", "libfts5jieba.dylib", "fts5jieba.dll"))
    ]
    if len(candidates) != 1:
        fail("native archive must contain exactly one fts5jieba extension")
    return candidates[0].rsplit("/", 1)[-1]


def verify_native(
    names: list[str],
    version: str | None,
    target: str | None,
    modes: dict[str, int] | None = None,
) -> None:
    roots = {name.split("/", 1)[0] for name in names}
    if len(roots) != 1:
        fail("native archive must have exactly one top-level package directory")
    root = next(iter(roots))
    if version and not root.startswith(f"trace-db-{version}-"):
        fail(f"native archive root {root!r} does not contain version {version}")
    if target and not root.endswith(f"-{target}"):
        fail(f"native archive root {root!r} does not contain target {target}")

    binary_suffix = ".exe" if target and "windows" in target else ""
    required = {
        *(f"{root}/{binary}{binary_suffix}" for binary in NATIVE_BINARIES),
        f"{root}/README.md",
        f"{root}/LICENSE",
        f"{root}/proto/tracedb/v1/tracedb.proto",
        f"{root}/{expected_extension(target, names)}",
    }
    missing = sorted(required - set(names))
    if missing:
        fail("native archive is missing: " + ", ".join(missing))
    if not (target and "windows" in target) and modes is not None:
        for binary in NATIVE_BINARIES:
            member = f"{root}/{binary}"
            if not modes.get(member, 0) & 0o111:
                fail(f"native executable is not marked executable: {member}")


def verify_wheel(path: str, names: list[str], version: str | None) -> None:
    init_files = [name for name in names if name == "tracedb/__init__.py"]
    native_files = [
        name
        for name in names
        if name.startswith("tracedb/")
        and name.endswith((".so", ".dylib", ".pyd"))
    ]
    dist_infos = [
        name
        for name in names
        if name.endswith(".dist-info/METADATA") and name.count("/") == 1
    ]
    required = {"tracedb/__init__.py", "tracedb/py.typed"}
    missing = sorted(required - set(names))
    if missing:
        fail("Python wheel is missing: " + ", ".join(missing))
    if len(init_files) != 1 or len(native_files) != 1:
        fail("Python wheel must contain one package initializer and one native module")
    if len(dist_infos) != 1:
        fail("Python wheel must contain exactly one dist-info/METADATA file")
    metadata = read_zip_text(path, dist_infos[0])
    metadata_version = metadata_field(metadata, "Version")
    if version and metadata_version != version:
        fail(f"Python wheel metadata version {metadata_version!r} != {version!r}")
    if "Name: tracedb" not in metadata:
        fail("Python wheel metadata has the wrong package name")


def verify_npm(path: str, names: list[str], version: str | None) -> None:
    required = {
        "package/index.js",
        "package/index.d.ts",
        "package/package.json",
        "package/tracedb_node.node",
    }
    missing = sorted(required - set(names))
    if missing:
        fail("Node package is missing: " + ", ".join(missing))
    document = json.loads(read_tar_text(path, "package/package.json"))
    package_version = document.get("version")
    if version and package_version != version:
        fail(f"Node package version {package_version!r} != {version!r}")
    if document.get("name") != "@tracedb/core":
        fail("Node package has the wrong package name")


def read_zip_text(path: str, name: str) -> str:
    with zipfile.ZipFile(path) as archive:
        return archive.read(name).decode("utf-8")


def read_tar_text(path: str, name: str) -> str:
    with tarfile.open(path, "r:gz") as archive:
        member = archive.getmember(name)
        handle = archive.extractfile(member)
        if handle is None:
            fail(f"member is not a regular file: {name}")
        return handle.read().decode("utf-8")


def metadata_field(metadata: str, field: str) -> str:
    match = re.search(rf"^{re.escape(field)}:\s*(\S+)\s*$", metadata, re.MULTILINE)
    if not match:
        fail(f"wheel metadata does not contain {field}")
    return match.group(1)


def inspect(path: str, kind: str, version: str | None, target: str | None) -> None:
    if kind == "archive" and path.endswith(".zip"):
        with zipfile.ZipFile(path) as archive:
            infos = archive.infolist()
            names = unique_names(info.filename for info in infos)
            modes = {
                safe_member(info.filename): (info.external_attr >> 16) & 0o777
                for info in infos
                if not info.is_dir()
            }
            for info in infos:
                if info.is_dir():
                    continue
                mode = (info.external_attr >> 16) & 0o170000
                if mode == 0o120000:
                    fail(f"archive member is a symlink: {info.filename}")
        verify_native(names, version, target, modes)
        return

    if kind in ("archive", "npm"):
        mode = "r:gz"
        with tarfile.open(path, mode) as archive:
            members = archive.getmembers()
            names = unique_names(member.name for member in members)
            modes = {safe_member(member.name): member.mode for member in members}
            for member in members:
                if member.isdir():
                    continue
                if member.issym() or member.islnk() or not member.isfile():
                    fail(f"archive member is not a regular file: {member.name}")
        if kind == "archive":
            verify_native(names, version, target, modes)
        else:
            verify_npm(path, names, version)
        return

    with zipfile.ZipFile(path) as archive:
        infos = archive.infolist()
        names = unique_names(info.filename for info in infos)
        for info in infos:
            if info.is_dir():
                continue
            mode = (info.external_attr >> 16) & 0o170000
            if mode == 0o120000:
                fail(f"wheel member is a symlink: {info.filename}")
    verify_wheel(path, names, version)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("artifact", help="release artifact to inspect")
    parser.add_argument("--version", help="expected package version without a leading v")
    parser.add_argument(
        "--kind", choices=("auto", "archive", "wheel", "npm"), default="auto"
    )
    parser.add_argument("--target", help="expected Rust target for a native archive")
    args = parser.parse_args()
    kind = infer_kind(args.artifact) if args.kind == "auto" else args.kind
    version = args.version.removeprefix("v") if args.version else None
    inspect(args.artifact, kind, version, args.target)
    print(f"verified {kind} package: {args.artifact}")


if __name__ == "__main__":
    main()
