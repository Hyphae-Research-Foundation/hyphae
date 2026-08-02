#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Audit the exact non-dev dependency closure of the native Hyphae runtime."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import platform
import re
import shutil
import subprocess
import sys
import tomllib
from collections import defaultdict, deque
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


POLICY_SCHEMA = "hyphae-native-dependency-policy-v1"
RECEIPT_SCHEMA = "hyphae-native-dependency-audit-v1"
GATE_VERSION = 1
PACKAGE_PATH = re.compile(
    r"[\\/](?P<name>[A-Za-z0-9_.-]+)-"
    r"(?P<version>[0-9]+\.[0-9]+\.[0-9]+"
    r"(?:[-+][A-Za-z0-9_.-]+)?)[\\/]"
)


class GateFailure(RuntimeError):
    """A dependency fact differs from the reviewed native policy."""


def _require_mapping(value: object, label: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise GateFailure(f"{label} must be a JSON object")
    return value


def _require_list(value: object, label: str) -> list[Any]:
    if not isinstance(value, list):
        raise GateFailure(f"{label} must be a JSON array")
    return value


def _reviewed_external(policy: dict[str, Any]) -> dict[str, dict[str, str]]:
    reviewed: dict[str, dict[str, str]] = {}
    for raw in _require_list(policy.get("external_packages"), "external_packages"):
        entry = _require_mapping(raw, "external package entry")
        required = ("name", "version", "source", "license", "category", "rationale")
        for field in required:
            if not isinstance(entry.get(field), str) or not entry[field].strip():
                raise GateFailure(
                    f"external package entry requires nonempty string field {field}"
                )
        name = entry["name"]
        if name in reviewed:
            raise GateFailure(f"duplicate external package inventory entry {name}")
        reviewed[name] = {field: entry[field] for field in required}
    return reviewed


def _dependency_kinds(raw_dependency: dict[str, Any]) -> set[str]:
    raw_kinds = raw_dependency.get("dep_kinds")
    if not isinstance(raw_kinds, list) or not raw_kinds:
        return {"normal"}
    kinds: set[str] = set()
    for raw in raw_kinds:
        item = _require_mapping(raw, "dependency kind")
        kind = item.get("kind")
        if kind != "dev":
            kinds.add("normal" if kind is None else str(kind))
    return kinds


def audit_metadata(
    metadata: dict[str, Any],
    policy: dict[str, Any],
) -> dict[str, Any]:
    """Return the reviewed non-dev closure or fail on any policy drift."""

    if policy.get("schema") != POLICY_SCHEMA:
        raise GateFailure("unsupported native dependency policy schema")
    root_name = policy.get("root_package")
    if not isinstance(root_name, str) or not root_name:
        raise GateFailure("root_package must be a nonempty string")

    packages = [
        _require_mapping(item, "metadata package")
        for item in _require_list(metadata.get("packages"), "metadata packages")
    ]
    package_by_id = {str(item.get("id")): item for item in packages}
    roots = [item for item in packages if item.get("name") == root_name]
    if len(roots) != 1:
        raise GateFailure(
            f"root package {root_name} must resolve exactly once; found {len(roots)}"
        )

    resolve = _require_mapping(metadata.get("resolve"), "metadata resolve")
    nodes = [
        _require_mapping(item, "metadata resolve node")
        for item in _require_list(resolve.get("nodes"), "metadata resolve nodes")
    ]
    node_by_id = {str(item.get("id")): item for item in nodes}
    root_id = str(roots[0].get("id"))
    if root_id not in node_by_id:
        raise GateFailure(f"root package {root_name} has no resolve node")

    queue: deque[str] = deque([root_id])
    reachable_ids: set[str] = set()
    kinds_by_id: dict[str, set[str]] = defaultdict(set)
    kinds_by_id[root_id].add("root")
    while queue:
        package_id = queue.popleft()
        if package_id in reachable_ids:
            continue
        reachable_ids.add(package_id)
        node = node_by_id.get(package_id)
        if node is None:
            raise GateFailure(f"reachable package {package_id} has no resolve node")
        for raw_dependency in _require_list(node.get("deps"), "resolve dependencies"):
            dependency_record = _require_mapping(raw_dependency, "resolve dependency")
            kinds = _dependency_kinds(dependency_record)
            if not kinds:
                continue
            dependency_id = str(dependency_record.get("pkg"))
            if dependency_id not in package_by_id:
                raise GateFailure(
                    f"reachable dependency {dependency_id} has no package metadata"
                )
            kinds_by_id[dependency_id].update(kinds)
            queue.append(dependency_id)

    reachable = [package_by_id[package_id] for package_id in reachable_ids]
    names: dict[str, dict[str, Any]] = {}
    for item in reachable:
        name = str(item.get("name"))
        if name in names:
            raise GateFailure(
                f"native closure contains multiple versions of package {name}"
            )
        names[name] = item

    forbidden = {
        str(item)
        for item in _require_list(
            policy.get("forbidden_packages"), "forbidden_packages"
        )
    }
    for name in sorted(names):
        if name in forbidden:
            raise GateFailure(f"forbidden package {name} is reachable from {root_name}")

    reviewed_workspace = {
        str(item)
        for item in _require_list(
            policy.get("workspace_packages"), "workspace_packages"
        )
    }
    actual_workspace = {
        str(item.get("name")) for item in reachable if item.get("source") is None
    }
    unreviewed_workspace = actual_workspace - reviewed_workspace
    if unreviewed_workspace:
        raise GateFailure(
            "unreviewed workspace package "
            + ", ".join(sorted(unreviewed_workspace))
        )
    stale_workspace = reviewed_workspace - actual_workspace
    if stale_workspace:
        raise GateFailure(
            "stale workspace package inventory entry "
            + ", ".join(sorted(stale_workspace))
        )

    reviewed_external = _reviewed_external(policy)
    actual_external = {
        str(item.get("name")) for item in reachable if item.get("source") is not None
    }
    unreviewed_external = actual_external - reviewed_external.keys()
    if unreviewed_external:
        raise GateFailure(
            "unreviewed external package " + ", ".join(sorted(unreviewed_external))
        )
    stale_external = reviewed_external.keys() - actual_external
    if stale_external:
        raise GateFailure(
            "stale external inventory entry " + ", ".join(sorted(stale_external))
        )

    for name in sorted(actual_external):
        actual = names[name]
        reviewed = reviewed_external[name]
        for field in ("version", "source", "license"):
            found = actual.get(field)
            expected = reviewed[field]
            if found != expected:
                raise GateFailure(
                    f"external package {name} {field} differs: "
                    f"expected {expected!r}, found {found!r}"
                )

    ordered_packages: list[dict[str, Any]] = []
    dependency_kinds: dict[str, list[str]] = {}
    for item in sorted(
        reachable,
        key=lambda package: (str(package.get("name")), str(package.get("version"))),
    ):
        package_id = str(item.get("id"))
        name = str(item.get("name"))
        workspace = item.get("source") is None
        kinds = sorted(kinds_by_id[package_id])
        dependency_kinds[name] = kinds
        record: dict[str, Any] = {
            "name": name,
            "version": str(item.get("version")),
            "workspace": workspace,
            "source": item.get("source"),
            "license": item.get("license"),
            "dependency_kinds": kinds,
            "manifest_path": item.get("manifest_path"),
        }
        if not workspace:
            record["category"] = reviewed_external[name]["category"]
            record["rationale"] = reviewed_external[name]["rationale"]
        ordered_packages.append(record)

    return {
        "root_package": root_name,
        "package_count": len(ordered_packages),
        "workspace_package_count": len(actual_workspace),
        "external_package_count": len(actual_external),
        "packages": ordered_packages,
        "dependency_kinds": dict(sorted(dependency_kinds.items())),
        "forbidden_packages_present": [],
    }


def validate_lint_policy(
    workspace_manifest: dict[str, Any],
    crate_manifests: dict[str, dict[str, Any]],
    workspace_package_names: list[str],
) -> None:
    """Require workspace unsafe denial and inheritance by every native crate."""

    workspace = _require_mapping(workspace_manifest.get("workspace"), "workspace")
    lints = _require_mapping(workspace.get("lints"), "workspace lints")
    rust_lints = _require_mapping(lints.get("rust"), "workspace Rust lints")
    if rust_lints.get("unsafe_code") != "forbid":
        raise GateFailure("workspace unsafe_code lint must remain forbid")
    for name in workspace_package_names:
        manifest = crate_manifests.get(name)
        if manifest is None:
            raise GateFailure(f"missing manifest for workspace package {name}")
        crate_lints = _require_mapping(manifest.get("lints"), f"{name} lints")
        if crate_lints.get("workspace") is not True:
            raise GateFailure(f"workspace package {name} must inherit workspace lints")


def _unsafe_count(value: object) -> int:
    if isinstance(value, dict):
        return sum(
            int(child) if key == "unsafe_" and isinstance(child, int) else _unsafe_count(child)
            for key, child in value.items()
        )
    if isinstance(value, list):
        return sum(_unsafe_count(child) for child in value)
    return 0


def _parse_failure_packages(stderr: str) -> list[str]:
    packages: set[str] = set()
    for line in stderr.splitlines():
        if "Failed to parse file:" not in line:
            continue
        match = PACKAGE_PATH.search(line)
        if match is not None:
            packages.add(f"{match.group('name')}@{match.group('version')}")
    return sorted(packages)


def audit_unsafe(
    geiger_report: dict[str, Any],
    geiger_stderr: str,
    closure: list[dict[str, Any]],
) -> dict[str, Any]:
    """Validate native unsafe absence and report reviewed external unsafety."""

    metrics: dict[tuple[str, str], dict[str, Any]] = {}
    for raw in _require_list(geiger_report.get("packages"), "geiger packages"):
        package_metric = _require_mapping(raw, "geiger package")
        package = _require_mapping(package_metric.get("package"), "geiger package identity")
        identity = _require_mapping(package.get("id"), "geiger package ID")
        key = (str(identity.get("name")), str(identity.get("version")))
        if key in metrics:
            raise GateFailure(f"duplicate cargo-geiger metrics for {key[0]}@{key[1]}")
        metrics[key] = package_metric

    parse_failures = _parse_failure_packages(geiger_stderr)
    closure_labels = {
        f"{entry['name']}@{entry['version']}" for entry in closure
    }
    closure_parse_failures = sorted(closure_labels.intersection(parse_failures))
    if closure_parse_failures:
        raise GateFailure(
            "cargo-geiger could not parse closure package "
            + ", ".join(closure_parse_failures)
        )

    unscanned_files = [
        str(item)
        for item in _require_list(
            geiger_report.get("used_but_not_scanned_files"),
            "geiger used_but_not_scanned_files",
        )
    ]
    for entry in closure:
        if not entry["workspace"]:
            continue
        marker = f"/crates/{entry['name']}/".lower()
        if any(marker in path.replace("\\", "/").lower() for path in unscanned_files):
            raise GateFailure(
                f"cargo-geiger left native workspace files unscanned for {entry['name']}"
            )

    packages: list[dict[str, Any]] = []
    for entry in sorted(closure, key=lambda item: (item["name"], item["version"])):
        key = (str(entry["name"]), str(entry["version"]))
        metric = metrics.get(key)
        if metric is None:
            if entry["workspace"]:
                raise GateFailure(
                    f"missing cargo-geiger metrics for native package {entry['name']}"
                )
            packages.append(
                {
                    "name": entry["name"],
                    "version": entry["version"],
                    "workspace": False,
                    "status": "not-scanned-on-host",
                    "unsafe_count": None,
                }
            )
            continue
        unsafety = _require_mapping(metric.get("unsafety"), "geiger unsafety")
        used_unsafe_count = _unsafe_count(unsafety.get("used"))
        unused_unsafe_count = _unsafe_count(unsafety.get("unused"))
        unsafe_count = used_unsafe_count + unused_unsafe_count
        if entry["workspace"] and unsafe_count != 0:
            raise GateFailure(
                f"direct unsafe usage in native package {entry['name']}: "
                f"{unsafe_count} findings"
            )
        packages.append(
            {
                "name": entry["name"],
                "version": entry["version"],
                "workspace": bool(entry["workspace"]),
                "status": "scanned",
                "unsafe_count": unsafe_count,
                "used_unsafe_count": used_unsafe_count,
                "unused_unsafe_count": unused_unsafe_count,
            }
        )

    return {
        "packages": packages,
        "native_unsafe_count": sum(
            int(entry["unsafe_count"] or 0)
            for entry in packages
            if entry["workspace"]
        ),
        "external_unsafe_count_on_host": sum(
            int(entry["unsafe_count"] or 0)
            for entry in packages
            if not entry["workspace"]
        ),
        "external_used_unsafe_count_on_host": sum(
            int(entry.get("used_unsafe_count") or 0)
            for entry in packages
            if not entry["workspace"]
        ),
        "packages_without_metrics": geiger_report.get("packages_without_metrics", []),
        "used_but_not_scanned_files": unscanned_files,
        "out_of_closure_parse_failures": sorted(
            set(parse_failures) - closure_labels
        ),
    }


def _run(
    command: list[str],
    cwd: Path,
    environment: dict[str, str] | None = None,
) -> subprocess.CompletedProcess[str]:
    process_environment = os.environ.copy()
    if environment is not None:
        process_environment.update(environment)
    return subprocess.run(
        command,
        cwd=cwd,
        env=process_environment,
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        encoding="utf-8",
        errors="replace",
    )


def _run_required(
    command: list[str],
    cwd: Path,
    command_receipts: list[dict[str, Any]],
    environment: dict[str, str] | None = None,
) -> subprocess.CompletedProcess[str]:
    completed = _run(command, cwd, environment)
    receipt = {
        "command": command,
        "exit_status": completed.returncode,
        "stdout_sha256": hashlib.sha256(completed.stdout.encode()).hexdigest(),
        "stderr_sha256": hashlib.sha256(completed.stderr.encode()).hexdigest(),
    }
    if environment is not None:
        receipt["environment"] = dict(sorted(environment.items()))
    command_receipts.append(receipt)
    if completed.returncode != 0:
        detail = completed.stderr.strip() or completed.stdout.strip()
        raise GateFailure(f"command failed ({' '.join(command)}): {detail}")
    return completed


def _version(command: list[str], cwd: Path) -> str:
    completed = _run(command, cwd)
    if completed.returncode != 0:
        raise GateFailure(f"cannot determine tool version: {' '.join(command)}")
    return completed.stdout.strip() or completed.stderr.strip()


def _load_toml(path: Path) -> dict[str, Any]:
    with path.open("rb") as handle:
        return tomllib.load(handle)


def _git_value(arguments: list[str], root: Path) -> str:
    completed = _run(["git", *arguments], root)
    if completed.returncode != 0:
        raise GateFailure(f"git {' '.join(arguments)} failed")
    return completed.stdout.strip()


def sanitize_receipt_paths(
    value: Any,
    *,
    repo_paths: list[str],
    cargo_home: str,
    home: str,
) -> Any:
    """Replace machine-specific path prefixes in one receipt value."""

    replacements: dict[str, str] = {}
    for path, label in (
        *((path, "<repo>") for path in repo_paths),
        (cargo_home, "<cargo-home>"),
        (home, "<home>"),
    ):
        stripped = path.rstrip("/\\")
        if not stripped:
            continue
        replacements[stripped] = label
        replacements[stripped.replace("\\", "/")] = label
        replacements[stripped.replace("/", "\\")] = label
    ordered = sorted(replacements.items(), key=lambda item: len(item[0]), reverse=True)

    def sanitize(item: Any) -> Any:
        if isinstance(item, dict):
            return {key: sanitize(child) for key, child in item.items()}
        if isinstance(item, list):
            return [sanitize(child) for child in item]
        if isinstance(item, str):
            result = item
            for prefix, replacement in ordered:
                result = result.replace(prefix, replacement)
            return result
        return item

    return sanitize(value)


def build_receipt(
    root: Path,
    policy_path: Path,
    require_clean: bool,
) -> dict[str, Any]:
    """Execute the complete native dependency gate and return its receipt."""

    policy_bytes = policy_path.read_bytes()
    policy = _require_mapping(json.loads(policy_bytes), "policy")
    commands: list[dict[str, Any]] = []

    metadata_command = ["cargo", "metadata", "--locked", "--format-version", "1"]
    metadata_result = _run_required(metadata_command, root, commands)
    metadata = _require_mapping(json.loads(metadata_result.stdout), "cargo metadata")
    closure = audit_metadata(metadata, policy)

    crate_manifests: dict[str, dict[str, Any]] = {}
    for entry in closure["packages"]:
        if not entry["workspace"]:
            continue
        manifest_path = Path(str(entry["manifest_path"])).absolute()
        try:
            manifest_path.relative_to(root.absolute())
        except ValueError as error:
            raise GateFailure(
                f"native workspace manifest escapes repository: {manifest_path}"
            ) from error
        crate_manifests[entry["name"]] = _load_toml(manifest_path)
    workspace_names = [
        entry["name"] for entry in closure["packages"] if entry["workspace"]
    ]
    validate_lint_policy(
        _load_toml(root / "Cargo.toml"),
        crate_manifests,
        workspace_names,
    )

    _run_required(["cargo", "deny", "check"], root, commands)
    runtime_manifest = next(
        Path(str(entry["manifest_path"])).absolute()
        for entry in closure["packages"]
        if entry["name"] == closure["root_package"]
    )
    host_identity = f"{sys.platform}-{platform.machine().lower()}"
    geiger_target = root / "target" / "native-dependency-geiger" / host_identity
    geiger_environment = {"CARGO_TARGET_DIR": str(geiger_target.absolute())}
    geiger_command = [
        "cargo",
        "geiger",
        "--manifest-path",
        str(runtime_manifest),
        "--all-features",
        "--build-dependencies",
        "--locked",
        "--output-format",
        "Json",
        "--color",
        "never",
        "--quiet",
    ]
    geiger_result = _run_required(
        geiger_command,
        root,
        commands,
        geiger_environment,
    )
    try:
        geiger_report = _require_mapping(
            json.loads(geiger_result.stdout), "cargo-geiger report"
        )
    except json.JSONDecodeError as error:
        raise GateFailure(f"cargo-geiger did not emit one JSON report: {error}") from error
    unsafe_report = audit_unsafe(
        geiger_report,
        geiger_result.stderr,
        closure["packages"],
    )

    status = _git_value(["status", "--porcelain"], root)
    clean = not status
    if require_clean and not clean:
        raise GateFailure("clean evidence run requested from a dirty worktree")

    receipt = {
        "schema": RECEIPT_SCHEMA,
        "gate_version": GATE_VERSION,
        "generated_at_utc": datetime.now(timezone.utc).isoformat(),
        "result": "pass",
        "git": {
            "commit": _git_value(["rev-parse", "HEAD"], root),
            "tree": _git_value(["rev-parse", "HEAD^{tree}"], root),
            "worktree_clean_before_receipt": clean,
        },
        "host": {
            "system": platform.system(),
            "release": platform.release(),
            "machine": platform.machine(),
            "python": platform.python_version(),
        },
        "tools": {
            "git": _version(["git", "--version"], root),
            "git_executable": shutil.which("git"),
            "rustc": _version(["rustc", "--version", "--verbose"], root),
            "cargo": _version(["cargo", "--version"], root),
            "cargo_deny": _version(["cargo", "deny", "--version"], root),
            "cargo_geiger": _version(["cargo", "geiger", "--version"], root),
        },
        "policy": {
            "path": policy_path.relative_to(root).as_posix(),
            "sha256": hashlib.sha256(policy_bytes).hexdigest(),
        },
        "closure": closure,
        "unsafe": unsafe_report,
        "commands": commands,
    }
    cargo_home = os.environ.get("CARGO_HOME", str(Path.home() / ".cargo"))
    return sanitize_receipt_paths(
        receipt,
        repo_paths=[str(root.absolute()), str(root.resolve())],
        cargo_home=cargo_home,
        home=str(Path.home()),
    )


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--policy",
        type=Path,
        default=Path("config/native-dependency-policy.json"),
    )
    parser.add_argument("--output", type=Path)
    parser.add_argument("--require-clean", action="store_true")
    arguments = parser.parse_args()
    root = Path(__file__).absolute().parent.parent
    policy_path = arguments.policy
    if not policy_path.is_absolute():
        policy_path = root / policy_path
    try:
        receipt = build_receipt(root, policy_path.absolute(), arguments.require_clean)
    except (GateFailure, OSError, json.JSONDecodeError, tomllib.TOMLDecodeError) as error:
        print(f"native dependency gate failed: {error}", file=sys.stderr)
        return 1

    encoded = json.dumps(receipt, indent=2, sort_keys=True) + "\n"
    if arguments.output is None:
        sys.stdout.write(encoded)
    else:
        output = arguments.output
        if not output.is_absolute():
            output = root / output
        output.parent.mkdir(parents=True, exist_ok=True)
        output.write_text(encoded, encoding="utf-8")
        print(
            "native dependency gate passed: "
            f"{receipt['closure']['workspace_package_count']} workspace + "
            f"{receipt['closure']['external_package_count']} external packages"
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
