#!/usr/bin/env python3
"""Install a built TraceDB wheel into a clean virtual environment and smoke it."""

from __future__ import annotations

import argparse
import os
import subprocess
import tempfile
import venv
from pathlib import Path


ROOT = Path(__file__).resolve().parent.parent


def virtualenv_python(environment: Path) -> Path:
    if os.name == "nt":
        return environment / "Scripts" / "python.exe"
    return environment / "bin" / "python"


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("wheel", type=Path)
    args = parser.parse_args()
    wheel = args.wheel.resolve()
    if not wheel.is_file() or wheel.suffix != ".whl":
        parser.error(f"wheel does not exist: {wheel}")

    with tempfile.TemporaryDirectory(prefix="tracedb-wheel-smoke-") as directory:
        environment = Path(directory) / "venv"
        venv.EnvBuilder(with_pip=True, clear=True).create(environment)
        python = virtualenv_python(environment)
        subprocess.run(
            [
                str(python),
                "-m",
                "pip",
                "install",
                "--disable-pip-version-check",
                "--no-deps",
                str(wheel),
            ],
            check=True,
        )
        subprocess.run(
            [str(python), str(ROOT / "bindings/python/smoke.py")],
            check=True,
        )
        print("python wheel install smoke: ok")


if __name__ == "__main__":
    main()
