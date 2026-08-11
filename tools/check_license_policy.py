#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only

"""Verify Hyphae's software and documentation licensing boundary."""

from __future__ import annotations

import ast
import hashlib
import json
import re
import sys
import tomllib
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
SOFTWARE_IDENTIFIER = "AGPL-3.0-only"
DOCUMENTATION_IDENTIFIER = "CC-BY-SA-4.0"
SOFTWARE_LICENSE_SHA256 = (
    "0d96a4ff68ad6d4b6f1f30f713b18d5184912ba8dd389f86aa7710db079abcb0"
)
DOCUMENTATION_SHA256 = (
    "23ee78c8bae49cf08ea2f0c84945c66b987ebe4520881fb51b3dad4fb43d07c2"
)
SOFTWARE_ROOTS = (
    "crates",
    "conformance",
    "fuzz",
    "integrations",
    "packaging",
    "sdks",
    "tools",
)
SOURCE_SUFFIXES = frozenset({".rs", ".py", ".ts", ".js", ".mjs"})
IGNORED_GENERATED_DIRECTORIES = frozenset(
    {"build", "dist", "node_modules", "target"}
)
ARCHIVE_DOCUMENTS = (
    "LICENSE",
    "LICENSE-DOCUMENTATION",
    "LICENSE-POLICY.md",
    "README.md",
    "THIRD_PARTY_NOTICES.md",
)
PACKAGE_LICENSE_DOCUMENTS = (
    "LICENSE",
    "LICENSE-DOCUMENTATION",
    "LICENSE-POLICY.md",
)


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def source_files(root: Path) -> list[Path]:
    files: list[Path] = []
    for directory in SOFTWARE_ROOTS:
        for path in (root / directory).rglob("*"):
            if (
                path.is_file()
                and path.suffix in SOURCE_SUFFIXES
                and not IGNORED_GENERATED_DIRECTORIES.intersection(path.parts)
            ):
                files.append(path)
    return sorted(files)


def schema_files(root: Path) -> list[Path]:
    return sorted(
        path
        for path in root.rglob("*.schema.json")
        if path.is_file()
        and not IGNORED_GENERATED_DIRECTORIES.intersection(path.parts)
    )


def validate_spdx_file(path: Path) -> str | None:
    lines = path.read_text(encoding="utf-8").splitlines()[:3]
    expected = f"SPDX-License-Identifier: {SOFTWARE_IDENTIFIER}"
    if any(expected in line for line in lines):
        return None
    return f"{path}: executable source lacks {expected} in its first three lines"


def validate_schema_file(path: Path) -> str | None:
    try:
        schema = json.loads(path.read_text(encoding="utf-8"))
    except json.JSONDecodeError:
        return f"{path}: JSON Schema is malformed"
    expected = f"SPDX-License-Identifier: {SOFTWARE_IDENTIFIER}"
    if not isinstance(schema, dict) or schema.get("$comment") != expected:
        return f"{path}: JSON Schema lacks canonical {SOFTWARE_IDENTIFIER} marker"
    return None


def literal_tuple(path: Path, name: str) -> tuple[str, ...] | None:
    tree = ast.parse(path.read_text(encoding="utf-8"), filename=str(path))
    for node in tree.body:
        if not isinstance(node, ast.Assign):
            continue
        if any(isinstance(target, ast.Name) and target.id == name for target in node.targets):
            value = ast.literal_eval(node.value)
            if isinstance(value, tuple) and all(isinstance(item, str) for item in value):
                return value
    return None


def manifest_paths(root: Path, name: str) -> list[Path]:
    return sorted(
        path
        for path in root.rglob(name)
        if path.is_file()
        and not IGNORED_GENERATED_DIRECTORIES.intersection(
            path.relative_to(root).parts
        )
    )


def manifest_name(path: Path, root: Path) -> str:
    return path.relative_to(root).as_posix()


def read_toml_manifest(
    path: Path, root: Path, failures: list[str]
) -> dict[str, object] | None:
    try:
        return tomllib.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, tomllib.TOMLDecodeError) as error:
        failures.append(f"{manifest_name(path, root)}: malformed TOML: {error}")
        return None


def read_json_manifest(
    path: Path, root: Path, failures: list[str]
) -> dict[str, object] | None:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        failures.append(f"{manifest_name(path, root)}: malformed JSON: {error}")
        return None
    if not isinstance(value, dict):
        failures.append(f"{manifest_name(path, root)}: manifest must be an object")
        return None
    return value


def nearest_workspace_license(
    path: Path,
    root: Path,
    cargo_documents: dict[Path, dict[str, object]],
) -> object:
    directory = path.parent
    while True:
        workspace_document = cargo_documents.get(directory / "Cargo.toml")
        workspace = (
            workspace_document.get("workspace")
            if workspace_document is not None
            else None
        )
        if isinstance(workspace, dict):
            package = workspace.get("package")
            return package.get("license") if isinstance(package, dict) else None
        if directory == root:
            return None
        if root not in directory.parents:
            return None
        directory = directory.parent


def validate_cargo_package_manifests(root: Path) -> list[str]:
    failures: list[str] = []
    cargo_documents: dict[Path, dict[str, object]] = {}
    for path in manifest_paths(root, "Cargo.toml"):
        document = read_toml_manifest(path, root, failures)
        if document is not None:
            cargo_documents[path] = document

    for path, document in cargo_documents.items():
        relative = manifest_name(path, root)
        workspace = document.get("workspace")
        if isinstance(workspace, dict) and "package" in workspace:
            package_defaults = workspace.get("package")
            if (
                not isinstance(package_defaults, dict)
                or package_defaults.get("license") != SOFTWARE_IDENTIFIER
            ):
                failures.append(
                    f"{relative}: workspace package license differs from "
                    f"{SOFTWARE_IDENTIFIER}"
                )

        package = document.get("package")
        if package is None:
            continue
        if not isinstance(package, dict):
            failures.append(f"{relative}: package manifest must be a table")
            continue
        license_value = package.get("license")
        if license_value == SOFTWARE_IDENTIFIER:
            continue
        if license_value == {"workspace": True}:
            license_value = nearest_workspace_license(path, root, cargo_documents)
        if license_value != SOFTWARE_IDENTIFIER:
            failures.append(
                f"{relative}: package license does not resolve to "
                f"{SOFTWARE_IDENTIFIER}"
            )
    return failures


def validate_npm_package_manifests(root: Path) -> list[str]:
    failures: list[str] = []
    for path in manifest_paths(root, "package.json"):
        document = read_json_manifest(path, root, failures)
        if document is not None and document.get("license") != SOFTWARE_IDENTIFIER:
            failures.append(
                f"{manifest_name(path, root)}: package license differs from "
                f"{SOFTWARE_IDENTIFIER}"
            )

    for path in manifest_paths(root, "package-lock.json"):
        document = read_json_manifest(path, root, failures)
        if document is None:
            continue
        packages = document.get("packages")
        if not isinstance(packages, dict):
            failures.append(
                f"{manifest_name(path, root)}: packages must be an object"
            )
            continue
        root_package = packages.get("")
        if (
            not isinstance(root_package, dict)
            or root_package.get("license") != SOFTWARE_IDENTIFIER
        ):
            failures.append(
                f"{manifest_name(path, root)}:packages..license differs from "
                f"{SOFTWARE_IDENTIFIER}"
            )
        for package in packages.values():
            if not isinstance(package, dict) or package.get("link") is not True:
                continue
            resolved = package.get("resolved")
            target = packages.get(resolved) if isinstance(resolved, str) else None
            if not isinstance(target, dict) or target.get("license") != SOFTWARE_IDENTIFIER:
                target_name = resolved if isinstance(resolved, str) else "<unresolved>"
                failures.append(
                    f"{manifest_name(path, root)}:packages.{target_name}.license "
                    f"differs from {SOFTWARE_IDENTIFIER}"
                )
    return failures


def validate_python_package_manifests(root: Path) -> list[str]:
    failures: list[str] = []
    for path in manifest_paths(root, "pyproject.toml"):
        document = read_toml_manifest(path, root, failures)
        if document is None or "project" not in document:
            continue
        project = document.get("project")
        relative = manifest_name(path, root)
        if not isinstance(project, dict):
            failures.append(f"{relative}: project manifest must be a table")
            continue
        if project.get("license") != SOFTWARE_IDENTIFIER:
            failures.append(
                f"{relative}: project license differs from {SOFTWARE_IDENTIFIER}"
            )
        license_files = project.get("license-files")
        if (
            not isinstance(license_files, list)
            or not all(isinstance(item, str) for item in license_files)
            or not set(PACKAGE_LICENSE_DOCUMENTS).issubset(license_files)
        ):
            failures.append(f"{relative}: license-files are incomplete")
    return failures


def validate_package_manifests(root: Path) -> list[str]:
    root = root.resolve()
    return [
        *validate_cargo_package_manifests(root),
        *validate_npm_package_manifests(root),
        *validate_python_package_manifests(root),
    ]


def validate_openapi_license(path: Path) -> str | None:
    encoded = path.read_text(encoding="utf-8")
    expected = re.compile(
        rf"(?m)^  license:\n    name: {re.escape(SOFTWARE_IDENTIFIER)}\n"
        rf"    identifier: {re.escape(SOFTWARE_IDENTIFIER)}$"
    )
    if expected.search(encoded) is None:
        return f"{path}: OpenAPI license is not {SOFTWARE_IDENTIFIER}"
    return None


def validate_repository(root: Path = ROOT) -> list[str]:
    failures: list[str] = []
    expected_digests = {
        "LICENSE": SOFTWARE_LICENSE_SHA256,
        "LICENSE-DOCUMENTATION": DOCUMENTATION_SHA256,
    }
    for relative, expected in expected_digests.items():
        path = root / relative
        if not path.is_file():
            failures.append(f"{relative}: canonical license text is missing")
        elif sha256(path) != expected:
            failures.append(f"{relative}: canonical license text digest differs")

    failures.extend(validate_package_manifests(root))

    for relative in (
        "sdks/typescript/package.json",
        "integrations/javascript/package.json",
    ):
        package = json.loads((root / relative).read_text(encoding="utf-8"))
        packaged_files = package.get("files", [])
        for required in PACKAGE_LICENSE_DOCUMENTS:
            if required not in packaged_files:
                failures.append(f"{relative}: files omits {required}")

    distribution_roots = sorted(
        path.parent for path in (root / "crates").glob("*/Cargo.toml")
    ) + [
        root / "integrations" / "pliegors",
        root / "sdks" / "python",
        root / "sdks" / "typescript",
        root / "integrations" / "javascript",
    ]
    for distribution_root in distribution_roots:
        for relative in PACKAGE_LICENSE_DOCUMENTS:
            packaged_license = distribution_root / relative
            if not packaged_license.is_file():
                failures.append(f"{packaged_license}: package license file is missing")
            elif packaged_license.read_bytes().rstrip() != (root / relative).read_bytes().rstrip():
                failures.append(f"{packaged_license}: package license file differs from root")

    for relative in (
        "contracts/openapi/hyphae-v1.yaml",
        "contracts/openapi/hyphae-v2.yaml",
        "crates/hyphae-contracts/assets/openapi/hyphae-v1.yaml",
        "crates/hyphae-contracts/assets/openapi/hyphae-v2.yaml",
    ):
        failure = validate_openapi_license(root / relative)
        if failure is not None:
            failures.append(failure)

    for path in source_files(root):
        failure = validate_spdx_file(path)
        if failure is not None:
            failures.append(failure)

    for path in schema_files(root):
        failure = validate_schema_file(path)
        if failure is not None:
            failures.append(failure)

    generator = (root / "tools" / "generate_sdk_models.py").read_text(encoding="utf-8")
    for marker in (
        f"// SPDX-License-Identifier: {SOFTWARE_IDENTIFIER}",
        f"# SPDX-License-Identifier: {SOFTWARE_IDENTIFIER}",
    ):
        if marker not in generator:
            failures.append(f"tools/generate_sdk_models.py: missing generated marker {marker}")
    if "SPDX-License-Identifier: Apache-2.0" in generator:
        failures.append("tools/generate_sdk_models.py: stale generated Apache-2.0 marker")

    packaged = literal_tuple(root / "packaging" / "package.py", "INCLUDED_DOCUMENTS")
    if packaged != ARCHIVE_DOCUMENTS:
        failures.append(
            "packaging/package.py: release archive licensing documents differ from policy"
        )

    policy = (root / "LICENSE-POLICY.md").read_text(encoding="utf-8")
    for identifier in (SOFTWARE_IDENTIFIER, DOCUMENTATION_IDENTIFIER):
        if identifier not in policy:
            failures.append(f"LICENSE-POLICY.md: missing {identifier}")
    deny_policy = (root / "deny.toml").read_text(encoding="utf-8")
    if f'"{SOFTWARE_IDENTIFIER}"' not in deny_policy:
        failures.append("deny.toml: workspace AGPL-3.0-only license is not allowed")
    return failures


def main() -> int:
    failures = validate_repository()
    if failures:
        for failure in failures:
            print(f"error: {failure}", file=sys.stderr)
        return 1
    print(
        "license policy passed: canonical AGPL-3.0-only code, "
        "CC-BY-SA-4.0 documentation, and complete source SPDX coverage"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
