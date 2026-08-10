#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only
"""Review dependency changes without requiring a hosted dependency-graph API."""

from __future__ import annotations

import argparse
import io
import json
import re
import shutil
import subprocess
import tarfile
import tempfile
import tomllib
from pathlib import Path
from pathlib import PurePosixPath
from typing import Any


ROOT = Path(__file__).resolve().parents[1]
HEX_COMMIT = re.compile(r"[0-9a-f]{40}")
CARGO_MANIFESTS = (
    "Cargo.toml",
    "conformance/g6/runners/rust/Cargo.toml",
    "conformance/g7/runners/rust/Cargo.toml",
    "conformance/g8/independent-backup-verifier/Cargo.toml",
    "conformance/rust/Cargo.toml",
    "crates/hyphae-cli/Cargo.toml",
    "crates/hyphae-client/Cargo.toml",
    "crates/hyphae-contracts/Cargo.toml",
    "crates/hyphae-core/Cargo.toml",
    "crates/hyphae-engine/Cargo.toml",
    "crates/hyphae-native-ann/Cargo.toml",
    "crates/hyphae-native-blobs/Cargo.toml",
    "crates/hyphae-native-btree/Cargo.toml",
    "crates/hyphae-native-catalog/Cargo.toml",
    "crates/hyphae-native-daemon/Cargo.toml",
    "crates/hyphae-native-manifest/Cargo.toml",
    "crates/hyphae-native-mvcc/Cargo.toml",
    "crates/hyphae-native-pages/Cargo.toml",
    "crates/hyphae-native-product/Cargo.toml",
    "crates/hyphae-native-protocol/Cargo.toml",
    "crates/hyphae-native-records/Cargo.toml",
    "crates/hyphae-native-runtime/Cargo.toml",
    "crates/hyphae-native-types/Cargo.toml",
    "crates/hyphae-native-wal/Cargo.toml",
    "crates/hyphae-query/Cargo.toml",
    "crates/hyphae-retrieval/Cargo.toml",
    "crates/hyphae-server/Cargo.toml",
    "crates/hyphae-storage/Cargo.toml",
    "fuzz/Cargo.toml",
    "integrations/pliegors/Cargo.toml",
)
CARGO_LOCKS = (
    "Cargo.lock",
    "conformance/g6/runners/rust/Cargo.lock",
    "conformance/g7/runners/rust/Cargo.lock",
    "fuzz/Cargo.lock",
)
ISOLATED_CARGO_MANIFESTS = (
    "conformance/g6/runners/rust/Cargo.toml",
    "conformance/g7/runners/rust/Cargo.toml",
)
NPM_PROJECTS = {
    "sdks/typescript/package.json": "sdks/typescript/package-lock.json",
    "integrations/javascript/package.json": "integrations/javascript/package-lock.json",
    "integrations/host-smoke/package.json": "integrations/host-smoke/package-lock.json",
}
NPM_LOCKS = tuple(NPM_PROJECTS.values())
PYTHON_MANIFESTS = ("sdks/python/pyproject.toml",)
REGISTERED_DEPENDENCY_FILES = (
    set(CARGO_MANIFESTS)
    | set(CARGO_LOCKS)
    | set(NPM_PROJECTS)
    | set(NPM_LOCKS)
    | set(PYTHON_MANIFESTS)
)
DEPENDENCY_BASENAMES = frozenset(
    {
        ".gitmodules",
        "build.gradle",
        "build.gradle.kts",
        "bun.lock",
        "bun.lockb",
        "cargo.lock",
        "cargo.toml",
        "composer.json",
        "composer.lock",
        "conan.lock",
        "conanfile.py",
        "conanfile.txt",
        "deno.json",
        "deno.jsonc",
        "deno.lock",
        "deps.edn",
        "directory.packages.props",
        "flake.lock",
        "flake.nix",
        "gemfile",
        "gemfile.lock",
        "go.mod",
        "go.sum",
        "go.work",
        "go.work.sum",
        "gradle.lockfile",
        "mix.exs",
        "mix.lock",
        "npm-shrinkwrap.json",
        "nuget.config",
        "package-lock.json",
        "package.json",
        "package.resolved",
        "package.swift",
        "packages.config",
        "packages.lock.json",
        "pdm.lock",
        "pipfile",
        "pipfile.lock",
        "pnpm-lock.yaml",
        "pnpm-workspace.yaml",
        "podfile",
        "podfile.lock",
        "poetry.lock",
        "pom.xml",
        "project.clj",
        "pubspec.lock",
        "pubspec.yaml",
        "pylock.toml",
        "pyproject.toml",
        "setup.cfg",
        "setup.py",
        "settings.gradle",
        "settings.gradle.kts",
        "uv.lock",
        "vcpkg-configuration.json",
        "vcpkg.json",
        "yarn.lock",
    }
)
PIP_REQUIREMENT_FILE = re.compile(
    r"(?:requirements|constraints).*\.(?:in|txt)"
)
PIP_REQUIREMENT_DIRECTORIES = frozenset({"constraints", "requirements"})
PIP_REQUIREMENT_SUFFIXES = frozenset({".in", ".txt"})


def git(*arguments: str, check: bool = True) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ("git", *arguments),
        cwd=ROOT,
        check=check,
        text=True,
        capture_output=True,
    )


def git_bytes(*arguments: str) -> bytes:
    result = subprocess.run(
        ("git", *arguments),
        cwd=ROOT,
        check=True,
        capture_output=True,
    )
    return result.stdout


def resolve_commit(revision: str, label: str) -> str:
    if HEX_COMMIT.fullmatch(revision) is None:
        raise ValueError(f"{label} must be a lowercase 40-character Git commit ID")
    result = git("rev-parse", "--verify", f"{revision}^{{commit}}", check=False)
    if result.returncode != 0 or result.stdout.strip() != revision:
        raise ValueError(f"{label} is not an available canonical Git commit object")
    return revision


def read_revision(revision: str, path: str) -> str | None:
    result = git("show", f"{revision}:{path}", check=False)
    return result.stdout if result.returncode == 0 else None


def cargo_dependencies(text: str | None) -> dict[str, dict[str, Any]]:
    if text is None:
        return {}
    parsed = tomllib.loads(text)
    dependencies: dict[str, dict[str, Any]] = {}
    for package in parsed.get("package", []):
        source = package.get("source")
        if source is None:
            continue
        name = package["name"]
        version = package["version"]
        key = f"{name}@{version}|{source}"
        checksum = package.get("checksum")
        if source.startswith("registry+") and not checksum:
            raise ValueError(f"registry dependency lacks checksum: {name}@{version}")
        dependencies[key] = {"checksum": checksum, "source": source}
    return dependencies


def npm_dependencies(text: str | None) -> dict[str, dict[str, Any]]:
    if text is None:
        return {}
    parsed = json.loads(text)
    dependencies: dict[str, dict[str, Any]] = {}
    for location, package in parsed.get("packages", {}).items():
        marker = "node_modules/"
        if marker not in location or package.get("link") is True:
            continue
        name = package.get("name") or location.rsplit(marker, maxsplit=1)[1]
        version = package.get("version")
        if not version:
            raise ValueError(f"npm dependency lacks version: {location}")
        resolved = package.get("resolved")
        integrity = package.get("integrity")
        if resolved and resolved.startswith("http") and not integrity:
            raise ValueError(f"npm dependency lacks integrity: {name}@{version}")
        key = f"{name}@{version}|{location}"
        dependencies[key] = {
            "dev": bool(package.get("dev", False)),
            "integrity": integrity,
            "resolved": resolved,
        }
    return dependencies


def python_dependencies(text: str | None) -> dict[str, dict[str, Any]]:
    if text is None:
        return {}
    parsed = tomllib.loads(text)
    project = parsed.get("project", {})
    groups: dict[str, list[str]] = {"runtime": project.get("dependencies", [])}
    groups.update(project.get("optional-dependencies", {}))
    build = parsed.get("build-system", {}).get("requires", [])
    groups["build"] = build
    dependencies: dict[str, dict[str, Any]] = {}
    for group, requirements in groups.items():
        for requirement in requirements:
            dependencies[f"{group}|{requirement}"] = {"group": group}
    return dependencies


def merge_base(base: str, head: str) -> str:
    result = git("merge-base", "--all", base, head, check=False)
    candidates = result.stdout.splitlines()
    if (
        result.returncode != 0
        or len(candidates) != 1
        or HEX_COMMIT.fullmatch(candidates[0]) is None
    ):
        raise ValueError("base and head do not have one canonical merge-base")
    return resolve_commit(candidates[0], "merge-base")


def changed_dependency_files(merge_base_commit: str, head: str) -> set[str]:
    result = git("diff", "--name-only", f"{merge_base_commit}..{head}", "--")
    return {line.strip().replace("\\", "/") for line in result.stdout.splitlines() if line.strip()}


def is_dependency_manifest_or_lock(path: str) -> bool:
    candidate = PurePosixPath(path.replace("\\", "/"))
    basename = candidate.name.casefold()
    if basename in DEPENDENCY_BASENAMES:
        return True
    if PIP_REQUIREMENT_FILE.fullmatch(basename) is not None:
        return True
    parent_directories = {part.casefold() for part in candidate.parts[:-1]}
    return (
        bool(parent_directories & PIP_REQUIREMENT_DIRECTORIES)
        and candidate.suffix.casefold() in PIP_REQUIREMENT_SUFFIXES
    )


def extract_git_archive(archive: bytes, destination: Path) -> None:
    with tarfile.open(fileobj=io.BytesIO(archive), mode="r:") as source:
        for member in source.getmembers():
            relative = PurePosixPath(member.name)
            if (
                relative.is_absolute()
                or not relative.parts
                or any(part in ("", ".", "..") for part in relative.parts)
                or "\\" in member.name
            ):
                raise ValueError(f"unsafe path in Git archive: {member.name!r}")
            target = destination.joinpath(*relative.parts)
            if member.isdir():
                target.mkdir(parents=True, exist_ok=True)
                continue
            if not member.isfile():
                raise ValueError(f"unsupported entry in Git archive: {member.name!r}")
            extracted = source.extractfile(member)
            if extracted is None:
                raise ValueError(f"cannot read Git archive entry: {member.name!r}")
            target.parent.mkdir(parents=True, exist_ok=True)
            with target.open("wb") as output:
                shutil.copyfileobj(extracted, output)


def validate_cargo_lock(head: str, manifest_path: str | None = None) -> None:
    head_commit = resolve_commit(head, "head")
    command = ["cargo", "metadata", "--locked", "--format-version", "1"]
    if manifest_path is not None:
        command.extend(("--manifest-path", manifest_path))
    with tempfile.TemporaryDirectory(prefix="hyphae-dependency-review-") as temporary:
        snapshot = Path(temporary)
        extract_git_archive(
            git_bytes("archive", "--format=tar", head_commit),
            snapshot,
        )
        result = subprocess.run(
            command,
            cwd=snapshot,
            check=False,
            text=True,
            capture_output=True,
        )
    if result.returncode != 0:
        detail = result.stderr.strip().splitlines()
        suffix = f": {detail[-1]}" if detail else ""
        label = manifest_path or "Cargo.toml"
        raise ValueError(f"{label} requires an updated Cargo.lock{suffix}")


def validate_npm_lock(head: str, manifest_path: str) -> None:
    head_commit = resolve_commit(head, "head")
    project = PurePosixPath(manifest_path).parent
    npm = shutil.which("npm")
    if npm is None:
        raise ValueError("npm is required to validate package-lock.json")
    with tempfile.TemporaryDirectory(prefix="hyphae-dependency-review-") as temporary:
        snapshot = Path(temporary)
        extract_git_archive(
            git_bytes("archive", "--format=tar", head_commit),
            snapshot,
        )
        result = subprocess.run(
            (npm, "ci", "--ignore-scripts", "--dry-run"),
            cwd=snapshot.joinpath(*project.parts),
            check=False,
            text=True,
            capture_output=True,
        )
    if result.returncode != 0:
        detail = result.stderr.strip().splitlines()
        suffix = f": {detail[-1]}" if detail else ""
        raise ValueError(
            f"{manifest_path} requires an updated {NPM_PROJECTS[manifest_path]}"
            f"{suffix}"
        )


def validate_registered_dependency_files(changed: set[str]) -> None:
    normalized = {path.replace("\\", "/") for path in changed}
    unknown = sorted(
        path
        for path in normalized
        if is_dependency_manifest_or_lock(path)
        and path not in REGISTERED_DEPENDENCY_FILES
    )
    if unknown:
        raise ValueError(f"unregistered dependency manifests or locks: {unknown!r}")


def validate_manifest_lock_pairs(changed: set[str], head: str) -> None:
    validate_registered_dependency_files(changed)
    rust_manifests = {path for path in changed if path.endswith("Cargo.toml")}
    root_manifests = rust_manifests.difference(ISOLATED_CARGO_MANIFESTS, {"fuzz/Cargo.toml"})
    if root_manifests and "Cargo.lock" not in changed:
        validate_cargo_lock(head)
    if "fuzz/Cargo.toml" in changed and "fuzz/Cargo.lock" not in changed:
        validate_cargo_lock(head, "fuzz/Cargo.toml")
    for runner in ISOLATED_CARGO_MANIFESTS:
        lock = str(PurePosixPath(runner).with_name("Cargo.lock"))
        if runner in changed and lock not in changed:
            validate_cargo_lock(head, runner)
    for manifest in (path for path in changed if path in NPM_PROJECTS):
        lock = NPM_PROJECTS[manifest]
        if lock not in changed:
            validate_npm_lock(head, manifest)


def dependency_diff(
    base: dict[str, dict[str, Any]], current: dict[str, dict[str, Any]]
) -> dict[str, Any]:
    shared = base.keys() & current.keys()
    return {
        "added": sorted(current.keys() - base.keys()),
        "removed": sorted(base.keys() - current.keys()),
        "metadata_changed": sorted(key for key in shared if base[key] != current[key]),
    }


def review(base: str, head: str) -> dict[str, Any]:
    base_commit = resolve_commit(base, "base")
    head_commit = resolve_commit(head, "head")
    merge_base_commit = merge_base(base_commit, head_commit)
    changed = changed_dependency_files(merge_base_commit, head_commit)
    validate_manifest_lock_pairs(changed, head_commit)
    ecosystems: dict[str, Any] = {}
    for path in CARGO_LOCKS:
        ecosystems[path] = dependency_diff(
            cargo_dependencies(read_revision(merge_base_commit, path)),
            cargo_dependencies(read_revision(head_commit, path)),
        )
    for path in NPM_LOCKS:
        ecosystems[path] = dependency_diff(
            npm_dependencies(read_revision(merge_base_commit, path)),
            npm_dependencies(read_revision(head_commit, path)),
        )
    for path in PYTHON_MANIFESTS:
        ecosystems[path] = dependency_diff(
            python_dependencies(read_revision(merge_base_commit, path)),
            python_dependencies(read_revision(head_commit, path)),
        )
    return {
        "version": 2,
        "base": base_commit,
        "merge_base": merge_base_commit,
        "head": head_commit,
        "changed_dependency_files": sorted(
            path
            for path in changed
            if path in REGISTERED_DEPENDENCY_FILES
        ),
        "ecosystems": ecosystems,
        "status": "ok",
    }


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--base", required=True)
    parser.add_argument("--head", required=True)
    parser.add_argument("--output", type=Path, required=True)
    arguments = parser.parse_args()
    report = review(arguments.base, arguments.head)
    arguments.output.write_text(
        json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    counts = {
        path: {key: len(value) for key, value in result.items()}
        for path, result in report["ecosystems"].items()
    }
    print(json.dumps({"status": "ok", "changes": counts}, sort_keys=True))


if __name__ == "__main__":
    main()
