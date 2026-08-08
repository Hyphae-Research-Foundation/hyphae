#!/usr/bin/env python3
"""Verify that compile-time assets are present in every published crate."""

from __future__ import annotations

import json
import re
import subprocess
import sys
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
LITERAL_INCLUDE = re.compile(
    r'include_(?:str|bytes)!\(\s*"([^"]+)"\s*\)', re.MULTILINE
)
MANIFEST_INCLUDE = re.compile(
    r'include_(?:str|bytes)!\(\s*concat!\(\s*env!\("CARGO_MANIFEST_DIR"\),'
    r'\s*"([^"]+)"\s*\)\s*\)',
    re.MULTILINE,
)


def run(*args: str) -> str:
    completed = subprocess.run(
        args,
        cwd=ROOT,
        check=True,
        capture_output=True,
        text=True,
    )
    return completed.stdout


def inside(path: Path, root: Path) -> bool:
    try:
        path.relative_to(root)
    except ValueError:
        return False
    return True


def main() -> int:
    metadata = json.loads(run("cargo", "metadata", "--no-deps", "--format-version", "1"))
    manifests = {
        package["name"]: Path(package["manifest_path"]).resolve()
        for package in metadata["packages"]
    }
    publishable_crates = tuple(
        package["name"]
        for package in metadata["packages"]
        if package["publish"] != []
    )
    failures: list[str] = []
    checked_assets = 0

    for crate in publishable_crates:
        manifest = manifests.get(crate)
        if manifest is None:
            failures.append(f"{crate}: package is missing from cargo metadata")
            continue
        crate_root = manifest.parent
        packaged = {
            line.strip().replace("\\", "/").removeprefix("./")
            for line in run(
                "cargo",
                "package",
                "--locked",
                "--allow-dirty",
                "--list",
                "-p",
                crate,
            ).splitlines()
            if line.strip()
        }

        for relative_source in sorted(path for path in packaged if path.endswith(".rs")):
            source = crate_root / relative_source
            if not source.is_file():
                failures.append(f"{crate}: packaged source is missing locally: {relative_source}")
                continue
            encoded = source.read_text(encoding="utf-8")
            includes: list[Path] = []
            includes.extend(
                (source.parent / match.group(1)).resolve()
                for match in LITERAL_INCLUDE.finditer(encoded)
            )
            includes.extend(
                (crate_root / match.group(1).lstrip("/\\")).resolve()
                for match in MANIFEST_INCLUDE.finditer(encoded)
            )

            for asset in includes:
                checked_assets += 1
                if not inside(asset, crate_root):
                    failures.append(
                        f"{crate}: {relative_source} includes an asset outside the crate: {asset}"
                    )
                    continue
                relative_asset = asset.relative_to(crate_root).as_posix()
                if not asset.is_file():
                    failures.append(
                        f"{crate}: {relative_source} includes a missing asset: {relative_asset}"
                    )
                elif relative_asset not in packaged:
                    failures.append(
                        f"{crate}: {relative_source} includes an unpackaged asset: {relative_asset}"
                    )

    if failures:
        for failure in failures:
            print(f"error: {failure}", file=sys.stderr)
        return 1

    print(
        f"crate package audit passed: {len(publishable_crates)} packages, "
        f"{checked_assets} compile-time assets"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
