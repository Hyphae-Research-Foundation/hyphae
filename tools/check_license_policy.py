#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only

"""Verify Hyphae's software and documentation licensing boundary."""

from __future__ import annotations

import ast
import hashlib
import json
import re
import sys
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


def toml_string(path: Path, section: str, key: str) -> str | None:
    current_section = ""
    assignment = re.compile(rf'^{re.escape(key)}\s*=\s*"([^"]+)"\s*$')
    for raw_line in path.read_text(encoding="utf-8").splitlines():
        line = raw_line.strip()
        if line.startswith("[") and line.endswith("]"):
            current_section = line[1:-1]
            continue
        if current_section == section and (match := assignment.fullmatch(line)):
            return match.group(1)
    return None


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

    if toml_string(root / "Cargo.toml", "workspace.package", "license") != SOFTWARE_IDENTIFIER:
        failures.append("Cargo.toml: workspace license differs from AGPL-3.0-only")
    if toml_string(root / "fuzz" / "Cargo.toml", "package", "license") != SOFTWARE_IDENTIFIER:
        failures.append("fuzz/Cargo.toml: package license differs from AGPL-3.0-only")
    if (
        toml_string(
            root / "sdks" / "python" / "pyproject.toml",
            "project",
            "license",
        )
        != SOFTWARE_IDENTIFIER
    ):
        failures.append("sdks/python/pyproject.toml: license differs from AGPL-3.0-only")
    python_project = (root / "sdks" / "python" / "pyproject.toml").read_text(
        encoding="utf-8"
    )
    if (
        'license-files = ["LICENSE", "LICENSE-DOCUMENTATION", '
        '"LICENSE-POLICY.md"]'
        not in python_project
    ):
        failures.append("sdks/python/pyproject.toml: license-files are incomplete")

    json_licenses = (
        ("sdks/typescript/package.json", ("license",)),
        ("sdks/typescript/package-lock.json", ("packages", "", "license")),
        ("integrations/javascript/package.json", ("license",)),
        ("integrations/javascript/package-lock.json", ("packages", "", "license")),
        ("integrations/host-smoke/package.json", ("license",)),
        ("integrations/host-smoke/package-lock.json", ("packages", "", "license")),
        (
            "integrations/javascript/package-lock.json",
            ("packages", "../../sdks/typescript", "license"),
        ),
    )
    for relative, keys in json_licenses:
        value: object = json.loads((root / relative).read_text(encoding="utf-8"))
        for key in keys:
            if not isinstance(value, dict) or key not in value:
                value = None
                break
            value = value[key]
        if value != SOFTWARE_IDENTIFIER:
            failures.append(
                f"{relative}:{'.'.join(keys)} differs from {SOFTWARE_IDENTIFIER}"
            )

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
