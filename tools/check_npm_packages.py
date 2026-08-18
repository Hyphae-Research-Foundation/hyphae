#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Build clean npm packages and inspect exact tarball legal contents."""

from __future__ import annotations

import argparse
import json
import shutil
import subprocess
import sys
import tarfile
import tempfile
from pathlib import Path

if __package__ in {None, ""}:
    sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from tools.check_registry_publish import validate_publish_authority


ROOT = Path(__file__).resolve().parents[1]
PROJECTS = ("sdks/typescript", "integrations/javascript")
REQUIRED = {
    "package/LICENSE",
    "package/LICENSE-DOCUMENTATION",
    "package/LICENSE-POLICY.md",
    "package/THIRD_PARTY_NOTICES.md",
    "package/package.json",
}


def validate_project(project: Path) -> list[str]:
    failures: list[str] = []
    npm = shutil.which("npm")
    if npm is None:
        return ["npm executable is unavailable"]
    subprocess.run([npm, "ci", "--ignore-scripts"], cwd=project, check=True)
    shutil.rmtree(project / "dist", ignore_errors=True)
    with tempfile.TemporaryDirectory(prefix="hyphae-npm-pack-") as directory:
        destination = Path(directory)
        subprocess.run(
            [npm, "pack", "--pack-destination", str(destination)],
            cwd=project,
            check=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        archives = list(destination.glob("*.tgz"))
        if len(archives) != 1:
            return [f"{project}: npm pack produced {len(archives)} archives"]
        with tarfile.open(archives[0], "r:gz") as archive:
            names = set(archive.getnames())
            missing = REQUIRED - names
            if missing:
                failures.append(f"{project}: tarball lacks {sorted(missing)!r}")
            manifest = archive.extractfile("package/package.json")
            if manifest is None:
                failures.append(f"{project}: tarball package.json is unreadable")
            else:
                package = json.load(manifest)
                if package.get("license") != "Apache-2.0":
                    failures.append(f"{project}: tarball license is not Apache-2.0")
            forbidden = [
                name
                for name in names
                if "node_modules/" in name
                or "@anthropic-ai/claude-code" in name
                or "sharp-libvips" in name
            ]
            if forbidden:
                failures.append(f"{project}: tarball bundles tooling {forbidden!r}")
    return failures


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--project", action="append", choices=PROJECTS)
    arguments = parser.parse_args()
    projects = arguments.project or list(PROJECTS)
    failures = [
        failure
        for relative in projects
        for failure in validate_project(ROOT / relative)
    ]
    failures.extend(validate_publish_authority("npm", ROOT, dry_run=True))
    if failures:
        for failure in failures:
            print(f"error: {failure}", file=sys.stderr)
        return 1
    print(f"npm package audit passed: {len(projects)} clean-build tarballs")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
