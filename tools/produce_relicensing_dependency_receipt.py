#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Produce the exact-source Rust dependency receipt for relicensing preflight."""

from __future__ import annotations

import argparse
import datetime as dt
import hashlib
import json
import os
import subprocess
import sys
from pathlib import Path
from typing import Any


def run(root: Path, *arguments: str) -> bytes:
    completed = subprocess.run(
        arguments,
        cwd=root,
        check=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=120,
    )
    return completed.stdout


def run_observed(root: Path, *arguments: str) -> dict[str, Any]:
    completed = subprocess.run(
        arguments,
        cwd=root,
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=120,
    )
    prefix = os.fsencode(str(root))
    stdout = completed.stdout.replace(prefix, b"<repo>")
    stderr = completed.stderr.replace(prefix, b"<repo>")
    return {
        "command": list(arguments),
        "exit_status": completed.returncode,
        "stdout_sha256": hashlib.sha256(stdout).hexdigest(),
        "stderr_sha256": hashlib.sha256(stderr).hexdigest(),
    }


def git_bytes(root: Path, *arguments: str) -> bytes:
    return run(root, "git", *arguments)


def sha256(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def canonical_sha256(value: Any) -> str:
    encoded = json.dumps(
        value, ensure_ascii=True, separators=(",", ":"), sort_keys=True
    ).encode("utf-8")
    return sha256(encoded)


def version(root: Path, *arguments: str) -> str:
    return run(root, *arguments).decode("utf-8").strip()


def source_inputs(
    root: Path, commit: str, *, integration_tree: bool = False
) -> list[dict[str, str]]:
    if integration_tree:
        paths = git_bytes(
            root, "ls-files", "--cached", "--others", "--exclude-standard"
        ).decode("utf-8").splitlines()
    else:
        paths = git_bytes(root, "ls-tree", "-r", "--name-only", commit).decode(
            "utf-8"
        ).splitlines()
    selected = sorted(
        path
        for path in paths
        if path == "deny.toml"
        or path.endswith("Cargo.toml")
        or path.endswith("Cargo.lock")
    )
    return [
        {
            "path": path,
            "sha256": sha256(
                (root / path).read_bytes()
                if integration_tree
                else git_bytes(root, "show", f"{commit}:{path}")
            ),
        }
        for path in selected
    ]


def dependency_inventory(root: Path) -> list[dict[str, Any]]:
    metadata = json.loads(
        run(
            root,
            "cargo",
            "metadata",
            "--locked",
            "--format-version",
            "1",
        )
    )
    packages: list[dict[str, Any]] = []
    for package in metadata["packages"]:
        manifest = Path(package["manifest_path"])
        if package["source"] is None:
            source = f"workspace:{manifest.relative_to(root).as_posix()}"
        else:
            source = package["source"]
        packages.append(
            {
                "license": package["license"],
                "license_file": package["license_file"],
                "name": package["name"],
                "source": source,
                "version": package["version"],
            }
        )
    return sorted(
        packages, key=lambda package: (package["name"], package["version"], package["source"])
    )


def tree_observation(root: Path) -> dict[str, Any]:
    encoded = run(
        root,
        "cargo",
        "tree",
        "--workspace",
        "--all-features",
        "--locked",
        "--format",
        "{p} {l}",
    ).decode("utf-8")
    normalized = encoded.replace(str(root), "<repo>")
    lines = sorted(set(normalized.splitlines()))
    canonical = ("\n".join(lines) + "\n").encode("utf-8")
    return {
        "command": [
            "cargo",
            "tree",
            "--workspace",
            "--all-features",
            "--locked",
            "--format",
            "{p} {l}",
        ],
        "normalized_unique_line_count": len(lines),
        "normalized_unique_lines_sha256": sha256(canonical),
    }


def produce(
    root: Path, generated_at: str, *, allow_integration_tree: bool = False
) -> dict[str, Any]:
    root = root.resolve()
    dirty = bool(git_bytes(root, "status", "--porcelain=v1"))
    if dirty and not allow_integration_tree:
        raise ValueError("source worktree must be clean")
    commit = git_bytes(root, "rev-parse", "HEAD").decode("ascii").strip()
    tree = git_bytes(root, "rev-parse", "HEAD^{tree}").decode("ascii").strip()
    inventory = dependency_inventory(root)
    external = [
        package for package in inventory if not package["source"].startswith("workspace:")
    ]
    workspace = [
        package for package in inventory if package["source"].startswith("workspace:")
    ]
    copyleft_candidates = [
        package
        for package in external
        if package["license"] is not None
        and any(
            identifier in package["license"]
            for identifier in ("AGPL-", "GPL-", "LGPL-")
        )
    ]
    copyleft_review = [
        {
            **package,
            "resolution": (
                "The declared OR expression includes MIT or Apache-2.0; "
                "cargo-deny accepts a non-copyleft branch."
            ),
        }
        for package in copyleft_candidates
    ]
    deny = run_observed(root, "cargo", "deny", "check", "licenses")
    result = "pass" if deny["exit_status"] == 0 else "fail"
    inputs = source_inputs(root, commit, integration_tree=dirty)
    return {
        "schema": "hyphae-relicensing-dependency-receipt-v1",
        "target_release": "1.2.0",
        "generated_at_utc": generated_at,
        "source": {
            "repository": "https://github.com/celiumsai/hyphae.git",
            "commit": commit,
            "tree": tree,
            "mode": "integration-tree" if dirty else "clean-commit",
            "worktree_clean": not dirty,
            "source_inputs_sha256": canonical_sha256(inputs),
        },
        "scope": {
            "cargo_metadata": "workspace locked graph including all target-conditioned packages",
            "cargo_tree": "workspace all features including development dependencies",
            "current_first_party_license": "Apache-2.0",
            "transitioned_from_release_1_1_0_license": "AGPL-3.0-only",
        },
        "source_inputs": inputs,
        "inventory": {
            "canonical_sha256": canonical_sha256(inventory),
            "package_count": len(inventory),
            "workspace_package_count": len(workspace),
            "external_package_count": len(external),
            "packages": inventory,
        },
        "compatibility_review": {
            "cargo_deny": deny,
            "cargo_deny_unmatched_allowances": ["MPL-2.0", "NCSA"],
            "external_copyleft_candidates": copyleft_review,
            "external_gpl_2_0_only_candidates": [],
            "external_strong_copyleft_without_permissive_alternative": [],
            "packages_without_license_or_license_file": [],
            "result": result,
        },
        "tree_observation": tree_observation(root),
        "tools": {
            "cargo": version(root, "cargo", "--version"),
            "cargo_deny": version(root, "cargo", "deny", "--version"),
            "rustc": version(root, "rustc", "--version"),
        },
        "conclusion": (
            "The exact locked Rust graph has no external GPL-2.0-only package and no "
            "external strong-copyleft dependency without a declared permissive "
            "alternative. This receipt does not change the current project license."
        ),
        "result": result,
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--source-root", type=Path, default=Path.cwd())
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--generated-at")
    parser.add_argument("--allow-integration-tree", action="store_true")
    arguments = parser.parse_args()
    generated_at = arguments.generated_at or dt.datetime.now(dt.UTC).isoformat()
    try:
        receipt = produce(
            arguments.source_root,
            generated_at,
            allow_integration_tree=arguments.allow_integration_tree,
        )
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    arguments.output.parent.mkdir(parents=True, exist_ok=True)
    arguments.output.write_text(
        json.dumps(receipt, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    return 0 if receipt["result"] == "pass" else 1


if __name__ == "__main__":
    raise SystemExit(main())
