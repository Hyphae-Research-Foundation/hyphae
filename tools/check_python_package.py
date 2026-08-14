#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""Fail-closed static contract for the publishable Python SDK."""

from __future__ import annotations

import json
import re
import tomllib
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[1]
MANIFEST = Path("sdks/python/pyproject.toml")
WORKFLOW = Path(".github/workflows/python-publish.yml")
EXPECTED_NAME = "hyphae-sdk"
REQUIRED_URLS = {"Homepage", "Documentation", "Repository", "Issues", "Changelog"}
PYPI_ACTION = "pypa/gh-action-pypi-publish@dc37677b2e1c63e2034f94d8a5b11f265b73ba33"


class PythonPackageValidationError(ValueError):
    """The Python distribution contract is incomplete or unsafe."""


def fail(message: str) -> None:
    raise PythonPackageValidationError(message)


def load_manifest(root: Path) -> dict[str, Any]:
    path = root / MANIFEST
    try:
        value = tomllib.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, tomllib.TOMLDecodeError) as error:
        fail(f"{MANIFEST} is not valid UTF-8 TOML: {error}")
    return value


def validate(root: Path = ROOT) -> dict[str, object]:
    manifest = load_manifest(root)
    project = manifest.get("project")
    if not isinstance(project, dict):
        fail("Python manifest must contain one project table")
    if project.get("name") != EXPECTED_NAME:
        fail("Python distribution must use the reserved hyphae-sdk name")
    version = project.get("version")
    if not isinstance(version, str) or re.fullmatch(r"\d+\.\d+\.\d+", version) is None:
        fail("Python distribution version must be strict semver")
    if project.get("requires-python") != ">=3.11":
        fail("Python support floor must remain explicit")
    if project.get("dependencies") != []:
        fail("Python SDK runtime must remain standard-library only")
    if manifest.get("build-system", {}).get("requires") != ["setuptools==80.9.0"]:
        fail("Python build backend dependency must remain exactly pinned")
    if project.get("license") != "AGPL-3.0-only":
        fail("Python SDK license expression must match the repository")
    readme = project.get("readme")
    if readme != {"file": "README.md", "content-type": "text/markdown"}:
        fail("Python long description must be bound to its checked-in README")
    urls = project.get("urls")
    if not isinstance(urls, dict) or set(urls) != REQUIRED_URLS:
        fail("Python project URLs are incomplete or contain an unreviewed entry")
    package_data = manifest.get("tool", {}).get("setuptools", {}).get("package-data", {})
    if package_data.get("hyphae_sdk") != ["py.typed"]:
        fail("typed-distribution marker is not included in package data")
    for relative in (
        "sdks/python/README.md",
        "sdks/python/LICENSE",
        "sdks/python/LICENSE-DOCUMENTATION",
        "sdks/python/LICENSE-POLICY.md",
        "sdks/python/src/hyphae_sdk/py.typed",
    ):
        if not (root / relative).is_file():
            fail(f"Python distribution input is missing: {relative}")
    workflow = (root / WORKFLOW).read_text(encoding="utf-8")
    required_workflow = {
        "workflow_dispatch:",
        "id-token: write",
        "environment: ${{ inputs.repository }}",
        PYPI_ACTION,
        "tools/python_distribution_receipt.py verify",
    }
    if any(fragment not in workflow for fragment in required_workflow):
        fail("Python Trusted Publishing workflow is incomplete")
    forbidden = {"skip-existing: true", "PYPI_TOKEN", "TWINE_PASSWORD", "password:"}
    if any(fragment in workflow for fragment in forbidden):
        fail("Python publication must use fail-closed OIDC without stored credentials")
    if workflow.count("id-token: write") != 1:
        fail("only the Python publish job may mint an OIDC identity")
    return {"name": EXPECTED_NAME, "status": "passed", "version": version}


def main() -> int:
    print(json.dumps(validate(), sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
