#!/usr/bin/env python3
"""Verify the crates.io release graph and every generated package's assets."""

from __future__ import annotations

import json
import re
import subprocess
import sys
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
RELEASE_CONFIG = ROOT / "config" / "crates-io-release.json"
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


def validate_release_graph(
    release: dict[str, object],
    packages: dict[str, dict[str, object]],
    publishable_crates: tuple[str, ...],
) -> tuple[tuple[str, ...], list[str]]:
    """Return the ordered release closure and any graph validation failures."""
    expected_version = release["version"]
    layers = release["layers"]
    if not isinstance(expected_version, str) or not isinstance(layers, list):
        return (), ["release config version or layers have invalid types"]
    expected_crates = tuple(crate for layer in layers for crate in layer)
    expected_set = set(expected_crates)
    layer_by_crate = {
        crate: layer_index
        for layer_index, layer in enumerate(layers)
        for crate in layer
    }

    failures: list[str] = []

    if len(expected_crates) != len(expected_set):
        failures.append("release config contains duplicate crate names")
    if set(publishable_crates) != expected_set:
        missing = sorted(expected_set - set(publishable_crates))
        unexpected = sorted(set(publishable_crates) - expected_set)
        failures.append(
            f"publishable crate set differs from release config; "
            f"missing={missing}, unexpected={unexpected}"
        )

    for crate in expected_crates:
        package = packages.get(crate)
        if package is None:
            failures.append(f"{crate}: package is missing from cargo metadata")
            continue
        if package["version"] != expected_version:
            failures.append(
                f"{crate}: version {package['version']} does not match {expected_version}"
            )
        crate_layer = layer_by_crate[crate]
        for dependency in package["dependencies"]:
            dependency_name = dependency["name"]
            if dependency_name not in expected_set:
                continue
            if dependency["kind"] == "dev":
                continue
            if dependency["req"] != f"={expected_version}":
                failures.append(
                    f"{crate}: {dependency_name} requirement {dependency['req']} is not "
                    f"={expected_version}"
                )
            dependency_layer = layer_by_crate[dependency_name]
            if dependency_layer >= crate_layer:
                failures.append(
                    f"{crate}: {dependency_name} must be in an earlier release layer"
                )

    return expected_crates, failures


def main() -> int:
    release = json.loads(RELEASE_CONFIG.read_text(encoding="utf-8"))
    expected_version = release["version"]
    metadata = json.loads(run("cargo", "metadata", "--no-deps", "--format-version", "1"))
    packages = {package["name"]: package for package in metadata["packages"]}
    manifests = {
        name: Path(package["manifest_path"]).resolve()
        for name, package in packages.items()
    }
    publishable_crates = tuple(
        package["name"]
        for package in metadata["packages"]
        if package["publish"] != []
    )
    expected_crates, failures = validate_release_graph(
        release, packages, publishable_crates
    )
    checked_assets = 0

    for mirror in release.get("mirrors", []):
        source = ROOT / mirror["source"]
        packaged = ROOT / mirror["packaged"]
        if not source.is_file():
            failures.append(f"release mirror source is missing: {mirror['source']}")
        elif not packaged.is_file():
            failures.append(f"packaged release mirror is missing: {mirror['packaged']}")
        elif source.read_bytes() != packaged.read_bytes():
            failures.append(
                f"packaged release mirror differs from {mirror['source']}: "
                f"{mirror['packaged']}"
            )

    if failures:
        for failure in failures:
            print(f"error: {failure}", file=sys.stderr)
        return 1

    for crate in expected_crates:
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
        f"crate package audit passed: {len(expected_crates)} packages at "
        f"{expected_version}, {checked_assets} compile-time assets"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
