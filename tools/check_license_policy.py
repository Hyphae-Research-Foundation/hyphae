#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Verify Hyphae's software and documentation licensing boundary."""

from __future__ import annotations

import ast
import hashlib
import json
import os
import re
import subprocess
import sys
import tomllib
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
SOFTWARE_IDENTIFIER = "Apache-2.0"
DOCUMENTATION_IDENTIFIER = "CC-BY-SA-4.0"
SOFTWARE_LICENSE_SHA256 = (
    "cfc7749b96f63bd31c3c42b5c471bf756814053e847c10f3eb003417bc523d30"
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
SOURCE_SUFFIXES = frozenset({".rs", ".py", ".ts", ".js", ".mjs", ".sh"})
COMMENTABLE_MACHINE_SUFFIXES = frozenset(
    {
        ".rs",
        ".py",
        ".ts",
        ".js",
        ".mjs",
        ".sh",
        ".yml",
        ".yaml",
        ".astro",
        ".html",
        ".toml",
    }
)
JSON_EXCEPTION_PATH_COUNT = 91
JSON_EXCEPTION_PATHS_SHA256 = (
    "e50c00d1a14ccb859f8201b30584c1709f6855b6a0cd7187e981e8177c22b39b"
)
IGNORED_GENERATED_DIRECTORIES = frozenset(
    {"build", "dist", "node_modules", "target"}
)
IGNORED_GENERATED_ROOTS = frozenset({"website"})
ARCHIVE_DOCUMENTS = (
    "LICENSE",
    "LICENSE-DOCUMENTATION",
    "LICENSE-POLICY.md",
    "NOTICE",
    "README.md",
    "THIRD_PARTY_NOTICES.md",
    "THIRD_PARTY_LICENSES.txt",
)
PACKAGE_LICENSE_DOCUMENTS = (
    "LICENSE",
    "LICENSE-DOCUMENTATION",
    "LICENSE-POLICY.md",
)
NPM_ARCHIVE_DOCUMENTS = (*PACKAGE_LICENSE_DOCUMENTS, "THIRD_PARTY_NOTICES.md")
PUBLISHABLE_NPM_PROJECTS = (
    "sdks/typescript",
    "integrations/javascript",
)
EXTENSIONLESS_MACHINE_FILES = {
    ".editorconfig": "# SPDX-License-Identifier: Apache-2.0",
    ".gitattributes": "# SPDX-License-Identifier: Apache-2.0",
    ".github/CODEOWNERS": "# SPDX-License-Identifier: Apache-2.0",
    ".gitignore": "# SPDX-License-Identifier: Apache-2.0",
    "fuzz/.gitignore": "# SPDX-License-Identifier: Apache-2.0",
}
SLT_MACHINE_SUFFIX = ".slt"


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


def machine_files(root: Path) -> list[Path]:
    files: list[Path] = []
    for directory, names, filenames in os.walk(root):
        relative_directory = Path(directory).relative_to(root)
        names[:] = sorted(
            name
            for name in names
            if name not in IGNORED_GENERATED_DIRECTORIES
            and not (
                relative_directory == Path(".") and name in IGNORED_GENERATED_ROOTS
            )
            and not (relative_directory == Path(".") and name == ".git")
        )
        for filename in sorted(filenames):
            path = Path(directory) / filename
            relative = path.relative_to(root).as_posix()
            if (
                path.suffix in COMMENTABLE_MACHINE_SUFFIXES
                or path.suffix == SLT_MACHINE_SUFFIX
                or relative in EXTENSIONLESS_MACHINE_FILES
            ):
                files.append(path)
    return sorted(files)


def repository_machine_files(root: Path) -> list[Path]:
    try:
        names = subprocess.run(
            [
                "git",
                "-C",
                str(root),
                "ls-files",
                "--cached",
                "--others",
                "--exclude-standard",
                "-z",
            ],
            check=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=30,
        ).stdout.decode("utf-8").split("\0")
    except (OSError, UnicodeError, subprocess.SubprocessError):
        return machine_files(root)
    files: list[Path] = []
    for relative in names:
        if not relative:
            continue
        path = root / relative
        if (
            path.is_file()
            and (
                path.suffix in COMMENTABLE_MACHINE_SUFFIXES
                or path.suffix == SLT_MACHINE_SUFFIX
                or relative in EXTENSIONLESS_MACHINE_FILES
            )
        ):
            files.append(path)
    return sorted(files)


def normative_markdown_files(root: Path) -> list[Path]:
    contract = json.loads(
        (root / "config/relicensing-1.2.0-classification.json").read_text(
            encoding="utf-8"
        )
    )
    rules = contract["classification"]["rules"]

    def category(relative: str) -> str | None:
        matches: list[dict[str, object]] = []
        for rule in rules:
            match = rule["match"]
            kind = match["kind"]
            values = match["paths"]
            if (
                (kind == "exact" and relative in values)
                or (kind == "prefix" and any(relative.startswith(value) for value in values))
                or (kind == "suffix" and any(relative.endswith(value) for value in values))
                or (kind == "basename" and Path(relative).name in values)
            ):
                matches.append(rule)
        if not matches:
            return None
        winner = min(matches, key=lambda rule: rule["priority"])
        return str(winner["category"])

    return sorted(
        path
        for path in root.rglob("*.md")
        if path.is_file()
        and not IGNORED_GENERATED_DIRECTORIES.intersection(path.parts)
        and category(path.relative_to(root).as_posix()) == "normative-specification"
    )


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
        and path.relative_to(root).parts[0] not in IGNORED_GENERATED_ROOTS
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
            or not set((*PACKAGE_LICENSE_DOCUMENTS, "THIRD_PARTY_NOTICES.md")).issubset(
                license_files
            )
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

    for project in PUBLISHABLE_NPM_PROJECTS:
        relative = f"{project}/package.json"
        package = json.loads((root / relative).read_text(encoding="utf-8"))
        packaged_files = package.get("files", [])
        for required in NPM_ARCHIVE_DOCUMENTS:
            if required not in packaged_files:
                failures.append(f"{relative}: files omits {required}")
        if package.get("scripts", {}).get("prepack") != "rm -rf dist && npm run build":
            failures.append(f"{relative}: prepack must remove stale dist and rebuild")

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

    notice = (root / "THIRD_PARTY_NOTICES.md").read_text(encoding="utf-8")
    for required in (
        "DCO 1.1",
        "LGPL-3.0-or-later",
        "@img/sharp-libvips-*",
        "@anthropic-ai/claude-code",
        "must never be included in Hyphae",
        "setuptools==84.0.0",
    ):
        if required not in notice:
            failures.append(f"THIRD_PARTY_NOTICES.md: missing boundary {required}")

    for relative in (
        "contracts/openapi/hyphae-v1.yaml",
        "contracts/openapi/hyphae-v2.yaml",
        "crates/hyphae-contracts/assets/openapi/hyphae-v1.yaml",
        "crates/hyphae-contracts/assets/openapi/hyphae-v2.yaml",
    ):
        failure = validate_openapi_license(root / relative)
        if failure is not None:
            failures.append(failure)

    for path in repository_machine_files(root):
        failure = validate_spdx_file(path)
        if failure is not None:
            failures.append(failure)

    observed_extensionless = {
        path.relative_to(root).as_posix()
        for path in repository_machine_files(root)
        if path.relative_to(root).as_posix() in EXTENSIONLESS_MACHINE_FILES
    }
    if observed_extensionless != set(EXTENSIONLESS_MACHINE_FILES):
        failures.append("extensionless machine-file inventory differs from exact policy")

    for path in schema_files(root):
        failure = validate_schema_file(path)
        if failure is not None:
            failures.append(failure)

    normative_marker = (
        f"<!-- SPDX-License-Identifier: {SOFTWARE_IDENTIFIER} -->"
    )
    for path in normative_markdown_files(root):
        if normative_marker not in path.read_text(encoding="utf-8").splitlines()[:3]:
            failures.append(f"{path}: normative Markdown lacks explicit Apache-2.0 marker")

    classification = json.loads(
        (root / "config/relicensing-1.2.0-classification.json").read_text(
            encoding="utf-8"
        )
    )
    rules = classification["classification"]["rules"]
    machine_json_prefixes = (
        ".agents/",
        ".claude-plugin/",
        "compatibility/",
        "config/",
        "conformance/",
        "contracts/",
        "crates/",
        "examples/",
        "integrations/",
        "packaging/",
        "plugins/",
        "sdks/",
    )
    json_marker_exceptions = {
        "package.json": "npm manifest format",
        "package-lock.json": "npm lock format",
        "tsconfig.json": "TypeScript config format",
    }
    json_exception_prefixes = {
        ".agents/": "tool marketplace format",
        ".claude-plugin/": "tool marketplace format",
        "compatibility/": "immutable compatibility fixture",
        "config/": "policy-governed machine data",
        "conformance/": "policy-governed conformance data",
        "contracts/": "policy-governed public contract data",
        "crates/": "policy-governed packaged fixture data",
        "docs/gates/evidence/": "immutable or source-bound evidence",
        "examples/": "literal protocol example payload",
        "plugins/": "host-defined plugin manifest format",
    }
    observed_json_exceptions: list[str] = []
    for path in manifest_paths(root, "*.json"):
        relative = manifest_name(path, root)
        if not relative.startswith(machine_json_prefixes):
            continue
        try:
            document = json.loads(path.read_text(encoding="utf-8"))
        except json.JSONDecodeError:
            failures.append(f"{relative}: JSON is malformed")
            continue
        if isinstance(document, dict) and document.get("$comment") == (
            f"SPDX-License-Identifier: {SOFTWARE_IDENTIFIER}"
        ):
            continue
        if path.name in json_marker_exceptions or any(
            relative.startswith(prefix) for prefix in json_exception_prefixes
        ):
            observed_json_exceptions.append(relative)
            continue
        failures.append(f"{relative}: JSON lacks canonical SPDX $comment")
    encoded_json_exceptions = (
        "\n".join(sorted(observed_json_exceptions)) + "\n"
    ).encode("utf-8")
    if len(observed_json_exceptions) != JSON_EXCEPTION_PATH_COUNT or hashlib.sha256(
        encoded_json_exceptions
    ).hexdigest() != JSON_EXCEPTION_PATHS_SHA256:
        failures.append(
            "strict JSON SPDX exceptions differ from the frozen exact path inventory"
        )

    generator = (root / "tools" / "generate_sdk_models.py").read_text(encoding="utf-8")
    for marker in (
        f"// SPDX-License-Identifier: {SOFTWARE_IDENTIFIER}",
        f"# SPDX-License-Identifier: {SOFTWARE_IDENTIFIER}",
    ):
        if marker not in generator:
            failures.append(f"tools/generate_sdk_models.py: missing generated marker {marker}")
    if "SPDX-License-Identifier: AGPL-3.0-only" in generator:
        failures.append("tools/generate_sdk_models.py: stale generated AGPL-3.0-only marker")

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
        failures.append("deny.toml: workspace Apache-2.0 license is not allowed")
    return failures


def main() -> int:
    failures = validate_repository()
    if failures:
        for failure in failures:
            print(f"error: {failure}", file=sys.stderr)
        return 1
    print(
        "license policy passed: canonical Apache-2.0 software, "
        "CC-BY-SA-4.0 documentation, and policy-backed machine-file SPDX coverage"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
