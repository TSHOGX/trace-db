#!/usr/bin/env python3
"""Generate a deterministic CycloneDX JSON SBOM from Cargo metadata."""

from __future__ import annotations

import argparse
import json
import subprocess
from pathlib import Path


ROOT = Path(__file__).resolve().parent.parent


def cargo_metadata(manifest: Path) -> dict:
    result = subprocess.run(
        [
            "cargo",
            "metadata",
            "--locked",
            "--format-version",
            "1",
            "--manifest-path",
            str(manifest),
        ],
        cwd=ROOT,
        check=True,
        capture_output=True,
        text=True,
    )
    return json.loads(result.stdout)


def package_url(name: str, version: str) -> str:
    return f"pkg:cargo/{name}@{version}"


def build_bom(metadata: dict) -> dict:
    components = []
    seen = set()
    for package in sorted(
        metadata["packages"],
        key=lambda item: (item["name"], item["version"], item["id"]),
    ):
        purl = package_url(package["name"], package["version"])
        if purl in seen:
            continue
        seen.add(purl)
        component = {
            "type": "library",
            "bom-ref": purl,
            "name": package["name"],
            "version": package["version"],
            "purl": purl,
        }
        if package.get("license"):
            component["licenses"] = [{"expression": package["license"]}]
        components.append(component)
    root_id = metadata.get("resolve", {}).get("root")
    root_package = next(
        (package for package in metadata["packages"] if package["id"] == root_id),
        next(package for package in metadata["packages"] if package["name"] == "trace-db"),
    )
    return {
        "bomFormat": "CycloneDX",
        "specVersion": "1.5",
        "version": 1,
        "metadata": {
            "component": {
                "type": "application",
                "name": "trace-db",
                "version": root_package["version"],
            }
        },
        "components": components,
    }


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--manifest-path", type=Path, default=ROOT / "Cargo.toml")
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    bom = build_bom(cargo_metadata(args.manifest_path.resolve()))
    rendered = json.dumps(bom, indent=2, sort_keys=True) + "\n"
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(rendered, encoding="utf-8")
    else:
        print(rendered, end="")


if __name__ == "__main__":
    main()
