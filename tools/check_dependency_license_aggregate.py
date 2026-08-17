#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Validate aggregate Rust, npm, and Python dependency-license evidence."""

from __future__ import annotations

import argparse
import hashlib
import json
import sys
import tomllib
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[1]
AGGREGATE_PATH = Path(
    "docs/gates/evidence/relicensing-1.2.0-dependency-license-aggregate.json"
)
RUST_RECEIPT_PATH = Path(
    "docs/gates/evidence/relicensing-1.2.0-dependencies-fcf2f918.json"
)
NPM_LOCK_PATHS = (
    Path("conformance/mcp/hosts/package-lock.json"),
    Path("integrations/host-smoke/package-lock.json"),
    Path("integrations/javascript/package-lock.json"),
    Path("sdks/typescript/package-lock.json"),
)
PYTHON_INVENTORY_PATH = Path("sdks/python/build-dependencies.json")
PYTHON_MANIFEST_PATH = Path("sdks/python/pyproject.toml")
ALLOWED_NPM_LICENSES = frozenset(
    {
        "0BSD",
        "Apache-2.0",
        "BlueOak-1.0.0",
        "BSD-2-Clause",
        "BSD-3-Clause",
        "CC-BY-4.0",
        "CC0-1.0",
        "ISC",
        "MIT",
        "MPL-2.0",
        "Python-2.0",
    }
)
PROPRIETARY_TOOLING_EXCEPTION = (
    ("node_modules/@anthropic-ai/claude-code", "2.1.233", "SEE LICENSE IN README.md"),
    ("node_modules/@anthropic-ai/claude-code-darwin-arm64", "2.1.233", "SEE LICENSE IN LICENSE.md"),
    ("node_modules/@anthropic-ai/claude-code-darwin-x64", "2.1.233", "SEE LICENSE IN LICENSE.md"),
    ("node_modules/@anthropic-ai/claude-code-linux-arm64", "2.1.233", "SEE LICENSE IN LICENSE.md"),
    ("node_modules/@anthropic-ai/claude-code-linux-arm64-musl", "2.1.233", "SEE LICENSE IN LICENSE.md"),
    ("node_modules/@anthropic-ai/claude-code-linux-x64", "2.1.233", "SEE LICENSE IN LICENSE.md"),
    ("node_modules/@anthropic-ai/claude-code-linux-x64-musl", "2.1.233", "SEE LICENSE IN LICENSE.md"),
    ("node_modules/@anthropic-ai/claude-code-win32-arm64", "2.1.233", "SEE LICENSE IN LICENSE.md"),
    ("node_modules/@anthropic-ai/claude-code-win32-x64", "2.1.233", "SEE LICENSE IN LICENSE.md"),
)
LGPL_OBLIGATIONS = (
    "Preserve the applicable LGPL-3.0-or-later notices and license text with any redistributed covered binary.",
    "Provide the complete corresponding source for the LGPL-covered library, or a valid written or network source offer permitted by LGPL-3.0-or-later.",
    "Preserve recipients' practical right to replace or relink the LGPL-covered library, including required installation information.",
    "Do not include these development-only packages in Hyphae runtime, SDK, crate, Python, npm, or native release archive payloads.",
)


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def canonical_sha256(value: Any) -> str:
    return hashlib.sha256(
        json.dumps(value, ensure_ascii=True, separators=(",", ":"), sort_keys=True).encode(
            "utf-8"
        )
    ).hexdigest()


def _load_json(path: Path) -> dict[str, Any]:
    value = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(value, dict):
        raise ValueError(f"{path}: must contain a JSON object")
    return value


def _dependency_inventory_paths(root: Path, name: str) -> set[Path]:
    return {
        path.relative_to(root)
        for path in root.rglob(name)
        if path.is_file()
        and not {"build", "dist", "node_modules", "target"}.intersection(
            path.relative_to(root).parts
        )
        and path.relative_to(root).parts[0] != "website"
    }


def _npm_inventory(root: Path, relative: Path) -> tuple[list[dict[str, str]], list[str]]:
    failures: list[str] = []
    document = _load_json(root / relative)
    packages = document.get("packages")
    if not isinstance(packages, dict):
        return [], [f"{relative}: packages must be an object"]
    inventory: list[dict[str, str]] = []
    proprietary: set[tuple[str, str, str]] = set()
    for package_path, package in sorted(packages.items()):
        if package_path == "" or not isinstance(package, dict):
            continue
        if package.get("link") is True:
            resolved = package.get("resolved")
            target = packages.get(resolved) if isinstance(resolved, str) else None
            if not isinstance(target, dict) or target.get("license") != "Apache-2.0":
                failures.append(f"{relative}:{package_path}: linked package is unresolved")
            continue
        version = package.get("version")
        license_value = package.get("license")
        if not isinstance(version, str) or not version:
            failures.append(f"{relative}:{package_path}: version is missing")
            continue
        if not isinstance(license_value, str) or not license_value:
            failures.append(f"{relative}:{package_path}: license is missing")
            continue
        identity = (package_path, version, license_value)
        if license_value.startswith("SEE LICENSE"):
            if relative != NPM_LOCK_PATHS[0] or identity not in PROPRIETARY_TOOLING_EXCEPTION:
                failures.append(
                    f"{relative}:{package_path}: proprietary or nonstandard license is not an exact tooling exception"
                )
            else:
                proprietary.add(identity)
        elif "LGPL-" in license_value:
            valid_lgpl = (
                (package_path.startswith("node_modules/@img/sharp-libvips-") and version == "1.3.2" and license_value == "LGPL-3.0-or-later")
                or (package_path == "node_modules/@img/sharp-wasm32" and version == "0.35.3" and license_value == "Apache-2.0 AND LGPL-3.0-or-later AND MIT")
                or (package_path in {
                    "node_modules/@img/sharp-win32-arm64",
                    "node_modules/@img/sharp-win32-ia32",
                    "node_modules/@img/sharp-win32-x64",
                } and version == "0.35.3" and license_value == "Apache-2.0 AND LGPL-3.0-or-later")
            )
            if relative not in {
                Path("integrations/host-smoke/package-lock.json"),
                Path("integrations/javascript/package-lock.json"),
            } or not valid_lgpl:
                failures.append(f"{relative}:{package_path}: LGPL package is outside the exact reviewed boundary")
        elif license_value not in ALLOWED_NPM_LICENSES:
            failures.append(
                f"{relative}:{package_path}: unreviewed or incompatible license {license_value!r}"
            )
        inventory.append(
            {"path": package_path, "version": version, "license": license_value}
        )
    if relative == NPM_LOCK_PATHS[0] and proprietary != set(PROPRIETARY_TOOLING_EXCEPTION):
        failures.append(f"{relative}: exact proprietary tooling inventory differs")
    return inventory, failures


def build_aggregate(root: Path = ROOT) -> tuple[dict[str, Any], list[str]]:
    failures: list[str] = []
    observed_npm_locks = _dependency_inventory_paths(root, "package-lock.json")
    if observed_npm_locks != set(NPM_LOCK_PATHS):
        failures.append("npm lock inventory differs from the exact aggregate scope")
    if _dependency_inventory_paths(root, "pyproject.toml") != {PYTHON_MANIFEST_PATH}:
        failures.append("Python project-manifest inventory differs from exact aggregate scope")
    if _dependency_inventory_paths(root, "build-dependencies.json") != {
        PYTHON_INVENTORY_PATH
    }:
        failures.append("Python dependency-inventory path set differs from exact aggregate scope")
    rust = _load_json(root / RUST_RECEIPT_PATH)
    rust_inventory = rust.get("inventory")
    if not isinstance(rust_inventory, dict) or rust.get("result") != "pass":
        failures.append(f"{RUST_RECEIPT_PATH}: Rust receipt is not passing")
        rust_count = None
        rust_digest = None
    else:
        rust_count = rust_inventory.get("package_count")
        rust_digest = rust_inventory.get("canonical_sha256")

    npm_records: list[dict[str, Any]] = []
    lgpl_packages: list[dict[str, str]] = []
    for relative in NPM_LOCK_PATHS:
        inventory, inventory_failures = _npm_inventory(root, relative)
        failures.extend(inventory_failures)
        lgpl_packages.extend(
            {"lock": relative.as_posix(), **package}
            for package in inventory
            if "LGPL-" in package["license"]
        )
        npm_records.append(
            {
                "lock": relative.as_posix(),
                "sha256": sha256(root / relative),
                "package_count": len(inventory),
                "canonical_sha256": canonical_sha256(inventory),
                "result": "pass" if not inventory_failures else "fail",
            }
        )

    python_inventory = _load_json(root / PYTHON_INVENTORY_PATH)
    python_manifest = tomllib.loads(
        (root / PYTHON_MANIFEST_PATH).read_text(encoding="utf-8")
    )
    python_packages = python_inventory.get("packages")
    runtime_dependencies = python_inventory.get("runtime_dependencies")
    expected_python_packages = [
        {
            "name": "setuptools",
            "version": "80.9.0",
            "license": "MIT",
            "scope": "build-only-not-bundled",
            "artifacts": [
                {
                    "filename": "setuptools-80.9.0-py3-none-any.whl",
                    "sha256": "062d34222ad13e0cc312a4c02d73f059e86a4acbfbdea8f8f76b28c99f306922",
                },
                {
                    "filename": "setuptools-80.9.0.tar.gz",
                    "sha256": "f36b47402ecde768dbfafc46e8e4207b4360c654f1f3bb84475f0a28628fb19c",
                },
            ],
        }
    ]
    python_result = "pass"
    if (
        python_inventory.get("schema") != "hyphae-python-build-dependencies-v1"
        or python_packages != expected_python_packages
        or runtime_dependencies != []
        or python_manifest.get("build-system", {}).get("requires") != ["setuptools==80.9.0"]
        or python_manifest.get("project", {}).get("dependencies") != []
    ):
        python_result = "fail"
        failures.append("Python dependency inventory or manifest boundary differs")

    source = rust.get("source") if isinstance(rust.get("source"), dict) else {}
    aggregate = {
        "$comment": "SPDX-License-Identifier: Apache-2.0",
        "schema": "hyphae-relicensing-dependency-license-aggregate-v1",
        "target_release": "1.2.0",
        "source": {
            "mode": "content-bound-integration-tree",
            "rust_receipt_anchor": {
                "commit": source.get("commit"),
                "tree": source.get("tree"),
                "mode": source.get("mode"),
            },
        },
        "inventories": {
            "rust": {
                "receipt": RUST_RECEIPT_PATH.as_posix(),
                "sha256": sha256(root / RUST_RECEIPT_PATH),
                "package_count": rust_count,
                "canonical_sha256": rust_digest,
                "result": rust.get("result"),
            },
            "npm": npm_records,
            "python": {
                "inventory": PYTHON_INVENTORY_PATH.as_posix(),
                "inventory_sha256": sha256(root / PYTHON_INVENTORY_PATH),
                "manifest": PYTHON_MANIFEST_PATH.as_posix(),
                "manifest_sha256": sha256(root / PYTHON_MANIFEST_PATH),
                "package_count": len(python_packages) if isinstance(python_packages, list) else None,
                "canonical_sha256": canonical_sha256(python_packages),
                "runtime_dependency_count": len(runtime_dependencies) if isinstance(runtime_dependencies, list) else None,
                "result": python_result,
            },
        },
        "compatibility_review": {
            "generic_incompatible_or_unreviewed_licenses": [],
            "proprietary_tooling_exception": {
                "lock": NPM_LOCK_PATHS[0].as_posix(),
                "packages": [
                    {"path": path, "version": version, "license": license_value}
                    for path, version, license_value in PROPRIETARY_TOOLING_EXCEPTION
                ],
                "scope": "opt-in-development-and-real-host-conformance-only-not-redistributable-by-hyphae",
            },
            "lgpl_obligations": {
                "packages": sorted(
                    lgpl_packages,
                    key=lambda package: (package["lock"], package["path"]),
                ),
                "obligations": list(LGPL_OBLIGATIONS),
            },
            "result": "pass" if not failures else "fail",
        },
        "result": "pass" if not failures else "fail",
    }
    return aggregate, failures


def validate_aggregate(root: Path = ROOT) -> list[str]:
    try:
        expected, failures = build_aggregate(root)
        actual = _load_json(root / AGGREGATE_PATH)
    except (OSError, UnicodeError, ValueError, json.JSONDecodeError, tomllib.TOMLDecodeError) as error:
        return [str(error)]
    if actual != expected:
        failures.append(f"{AGGREGATE_PATH}: aggregate evidence differs from exact inventories")
    if actual.get("result") != "pass":
        failures.append(f"{AGGREGATE_PATH}: aggregate result must be pass")
    return failures


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--refresh", action="store_true")
    arguments = parser.parse_args()
    if arguments.refresh:
        try:
            aggregate, failures = build_aggregate(ROOT)
            if failures:
                raise ValueError("; ".join(failures))
            (ROOT / AGGREGATE_PATH).write_text(
                json.dumps(aggregate, indent=2, sort_keys=True) + "\n", encoding="utf-8"
            )
        except (OSError, UnicodeError, ValueError, json.JSONDecodeError, tomllib.TOMLDecodeError) as error:
            print(f"error: {error}", file=sys.stderr)
            return 1
        print("dependency-license aggregate refreshed")
        return 0
    failures = validate_aggregate(ROOT)
    if failures:
        for failure in failures:
            print(f"error: {failure}", file=sys.stderr)
        return 1
    print("dependency-license aggregate passed: Rust + all npm + Python inventories")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
