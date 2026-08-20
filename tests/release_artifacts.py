#!/usr/bin/env python3
"""Exercise release artifact verification and the offline installer path."""

from __future__ import annotations

import hashlib
import io
import os
import platform
import subprocess
import tarfile
import tempfile
import unittest
import zipfile
from pathlib import Path


ROOT = Path(__file__).resolve().parent.parent
VERIFY = ROOT / "scripts/verify-release-package.py"
INSTALL = ROOT / "scripts/install-release.sh"
VERSION = "9.8.7"


def run_verifier(artifact: Path, *extra: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["python3", str(VERIFY), str(artifact), "--version", VERSION, *extra],
        cwd=ROOT,
        text=True,
        capture_output=True,
        check=False,
    )


def add_tar_file(archive: tarfile.TarFile, name: str, data: bytes, mode: int = 0o644) -> None:
    info = tarfile.TarInfo(name)
    info.size = len(data)
    info.mode = mode
    archive.addfile(info, io.BytesIO(data))


class ReleaseArtifactTests(unittest.TestCase):
    def test_native_wheel_and_node_shapes(self) -> None:
        with tempfile.TemporaryDirectory(prefix="tracedb-artifacts-") as directory:
            root = Path(directory)
            native_root = "trace-db-9.8.7-x86_64-unknown-linux-gnu"
            native = root / "native.tar.gz"
            with tarfile.open(native, "w:gz") as archive:
                for name in (
                    f"{native_root}/trace-db",
                    f"{native_root}/trace-db-bench",
                    f"{native_root}/trace-db-relevance",
                ):
                    add_tar_file(archive, name, b"#!/bin/sh\n", 0o755)
                for name in (
                    f"{native_root}/libfts5jieba.so",
                    f"{native_root}/README.md",
                    f"{native_root}/LICENSE",
                    f"{native_root}/proto/tracedb/v1/tracedb.proto",
                ):
                    add_tar_file(archive, name, b"artifact")
            result = run_verifier(
                native,
                "--target",
                "x86_64-unknown-linux-gnu",
            )
            self.assertEqual(result.returncode, 0, result.stderr)

            wheel = root / "tracedb-9.8.7-py3-none-any.whl"
            with zipfile.ZipFile(wheel, "w") as archive:
                archive.writestr("tracedb/__init__.py", "")
                archive.writestr("tracedb/py.typed", "")
                archive.writestr("tracedb/_native.abi3.so", b"native")
                archive.writestr(
                    "tracedb-9.8.7.dist-info/METADATA",
                    "Name: tracedb\nVersion: 9.8.7\n",
                )
            result = run_verifier(wheel)
            self.assertEqual(result.returncode, 0, result.stderr)

            npm = root / "tracedb-core-9.8.7.tgz"
            with tarfile.open(npm, "w:gz") as archive:
                add_tar_file(
                    archive,
                    "package/package.json",
                    b'{"name":"@tracedb/core","version":"9.8.7"}',
                )
                for name in (
                    "package/index.js",
                    "package/index.d.ts",
                    "package/tracedb_node.node",
                ):
                    add_tar_file(archive, name, b"artifact")
            result = run_verifier(npm)
            self.assertEqual(result.returncode, 0, result.stderr)

    def test_verifier_rejects_path_traversal(self) -> None:
        with tempfile.TemporaryDirectory(prefix="tracedb-artifacts-") as directory:
            artifact = Path(directory) / "unsafe.tar.gz"
            with tarfile.open(artifact, "w:gz") as archive:
                add_tar_file(archive, "../escape", b"unsafe")
            result = run_verifier(artifact, "--kind", "npm")
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("unsafe", result.stderr)

    @unittest.skipUnless(
        (platform.system(), platform.machine().lower())
        in {
            ("Darwin", "x86_64"),
            ("Darwin", "amd64"),
            ("Darwin", "arm64"),
            ("Darwin", "aarch64"),
            ("Linux", "x86_64"),
            ("Linux", "amd64"),
            ("Linux", "aarch64"),
            ("Linux", "arm64"),
        },
        "installer test requires a supported Unix host",
    )
    def test_installer_uses_checksum_and_installs_to_prefix(self) -> None:
        system = platform.system()
        machine = platform.machine().lower()
        if system == "Darwin":
            target = "aarch64-apple-darwin" if machine in ("arm64", "aarch64") else "x86_64-apple-darwin"
            extension = "libfts5jieba.dylib"
        else:
            target = "aarch64-unknown-linux-gnu" if machine in ("arm64", "aarch64") else "x86_64-unknown-linux-gnu"
            extension = "libfts5jieba.so"

        with tempfile.TemporaryDirectory(prefix="tracedb-installer-") as directory:
            root = Path(directory)
            package_name = f"trace-db-{VERSION}-{target}"
            package = root / package_name
            package.mkdir(parents=True)
            for name, body in {
                "trace-db": f"#!/bin/sh\nprintf 'trace-db {VERSION}\\n'\n",
                "trace-db-bench": "#!/bin/sh\nexit 0\n",
                "trace-db-relevance": "#!/bin/sh\nexit 0\n",
            }.items():
                file = package / name
                file.write_text(body, encoding="utf-8")
                file.chmod(0o755)
            (package / extension).write_bytes(b"extension")
            (package / "README.md").write_text("README", encoding="utf-8")
            (package / "LICENSE").write_text("LICENSE", encoding="utf-8")
            proto = package / "proto/tracedb/v1"
            proto.mkdir(parents=True)
            (proto / "tracedb.proto").write_text("syntax = 'proto3';", encoding="utf-8")
            archive = root / f"{package_name}.tar.gz"
            with tarfile.open(archive, "w:gz") as tar:
                tar.add(package, arcname=package_name)
            digest = hashlib.sha256(archive.read_bytes()).hexdigest()
            (root / "SHA256SUMS").write_text(
                f"{digest}  {archive.name}\n", encoding="utf-8"
            )
            prefix = root / "prefix"
            environment = {
                **os.environ,
                "TRACEDB_RELEASE_BASE_URL": root.as_uri(),
                "TRACEDB_INSTALL_PREFIX": str(prefix),
            }
            result = subprocess.run(
                ["bash", str(INSTALL), "--version", VERSION],
                cwd=ROOT,
                env=environment,
                text=True,
                capture_output=True,
                check=False,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(
                subprocess.check_output([str(prefix / "bin/trace-db"), "--version"], text=True).strip(),
                f"trace-db {VERSION}",
            )
            self.assertTrue((prefix / "lib" / extension).exists())


if __name__ == "__main__":
    unittest.main()
