#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Fail-closed static contract for the publishable Python SDK."""

from __future__ import annotations

import argparse
import json
import re
from pathlib import Path
from typing import Any

import tomllib

ROOT = Path(__file__).resolve().parents[1]
MANIFEST = Path("sdks/python/pyproject.toml")
WORKSPACE_MANIFEST = Path("Cargo.toml")
WORKFLOW = Path(".github/workflows/python-publish.yml")
EXPECTED_NAME = "hyphae-sdk"
REQUIRED_URLS = {"Homepage", "Documentation", "Repository", "Issues", "Changelog"}
PYPI_ACTION = "pypa/gh-action-pypi-publish@dc37677b2e1c63e2034f94d8a5b11f265b73ba33"
APACHE_RELEASE_VERSION = "1.2.1"


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


def load_workspace_manifest(root: Path) -> dict[str, Any]:
    path = root / WORKSPACE_MANIFEST
    try:
        value = tomllib.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, tomllib.TOMLDecodeError) as error:
        fail(f"{WORKSPACE_MANIFEST} is not valid UTF-8 TOML: {error}")
    return value


def workflow_job(workflow: str, name: str) -> str:
    match = re.search(
        rf"(?ms)^  {re.escape(name)}:\n.*?(?=^  [a-z0-9-]+:\n|\Z)", workflow
    )
    if match is None:
        fail(f"Python publication workflow is missing the {name} job")
    return match.group(0)


def validate(
    root: Path = ROOT, *, workflow_root: Path | None = None
) -> dict[str, object]:
    manifest = load_manifest(root)
    project = manifest.get("project")
    if not isinstance(project, dict):
        fail("Python manifest must contain one project table")
    if project.get("name") != EXPECTED_NAME:
        fail("Python distribution must use the reserved hyphae-sdk name")
    version = project.get("version")
    if not isinstance(version, str) or re.fullmatch(r"\d+\.\d+\.\d+", version) is None:
        fail("Python distribution version must be strict semver")
    workspace_version = (
        load_workspace_manifest(root)
        .get("workspace", {})
        .get("package", {})
        .get("version")
    )
    if version != workspace_version:
        fail("Python distribution version must equal workspace.package.version")
    if project.get("requires-python") != ">=3.11":
        fail("Python support floor must remain explicit")
    if project.get("dependencies") != []:
        fail("Python SDK runtime must remain standard-library only")
    if manifest.get("build-system", {}).get("requires") != ["setuptools==80.9.0"]:
        fail("Python build backend dependency must remain exactly pinned")
    if project.get("license") != "Apache-2.0":
        fail("Python SDK license expression must match the repository")
    classifiers = project.get("classifiers")
    if not isinstance(classifiers, list) or any(
        isinstance(classifier, str) and classifier.startswith("License ::")
        for classifier in classifiers
    ):
        fail("PEP 639 license expression must not be duplicated by a classifier")
    readme = project.get("readme")
    if readme != {"file": "README.md", "content-type": "text/markdown"}:
        fail("Python long description must be bound to its checked-in README")
    urls = project.get("urls")
    if not isinstance(urls, dict) or set(urls) != REQUIRED_URLS:
        fail("Python project URLs are incomplete or contain an unreviewed entry")
    package_data = (
        manifest.get("tool", {}).get("setuptools", {}).get("package-data", {})
    )
    if package_data.get("hyphae_sdk") != ["py.typed"]:
        fail("typed-distribution marker is not included in package data")
    for relative in (
        "sdks/python/README.md",
        "sdks/python/LICENSE",
        "sdks/python/LICENSE-DOCUMENTATION",
        "sdks/python/LICENSE-POLICY.md",
        "sdks/python/THIRD_PARTY_NOTICES.md",
        "sdks/python/build-dependencies.json",
        "sdks/python/src/hyphae_sdk/py.typed",
    ):
        if not (root / relative).is_file():
            fail(f"Python distribution input is missing: {relative}")
    workflow = ((workflow_root or root) / WORKFLOW).read_text(encoding="utf-8")
    required_workflow = {
        "workflow_dispatch:",
        "id-token: write",
        "environment: ${{ inputs.repository }}",
        PYPI_ACTION,
        "tools/python_distribution_receipt.py verify",
        "tools/python_distribution_receipt.py check-local",
        "--reproducible-directory",
        "testpypi_receipt_sha256",
        "refs/heads/main",
        "actions/download-artifact",
        "run-id: ${{ inputs.testpypi_run_id }}",
        "github-token: ${{ github.token }}",
        "hyphae-python-testpypi-${{ steps.source.outputs.commit }}",
        "testpypi-authority/release/publish-dist",
        "uv build source/sdks/python --out-dir dist",
        "packages-dir: release/publish-dist/",
        "--only-binary hyphae-sdk",
        "--no-binary hyphae-sdk",
        "mkdir artifact",
        "cp dist/*.whl dist/*.tar.gz artifact/",
        'Path("artifact/builder-receipt.json")',
        "path: artifact/*",
        "mkdir installation-evidence",
        '"schema": "hyphae-python-installation-evidence-v1"',
        "observed_sha256 = hashlib.sha256(distribution.read_bytes()).hexdigest()",
        "if observed_sha256 != expected_sha256:",
        '"distribution_sha256": observed_sha256',
        "WHEEL_SHA256: ${{ needs.build.outputs.wheel-sha256 }}",
        "SDIST_SHA256: ${{ needs.build.outputs.sdist-sha256 }}",
        "--installation-evidence installation-evidence/3.11-wheel.json",
        "--installation-evidence installation-evidence/3.11-sdist.json",
        "--installation-evidence installation-evidence/3.14-wheel.json",
        "--installation-evidence installation-evidence/3.14-sdist.json",
        "installation-evidence/*.json",
        "artifact-ids: ${{ needs.build.outputs.publication-artifact-id }}",
        "--testpypi-run-metadata testpypi-authority/github-run.json",
        "actions/runs/$TESTPYPI_RUN_ID",
        "--no-cache",
        "--publication-artifact-sha256",
        "release_run_id",
        "release_run_attempt",
        "release_evidence_sha256",
        "release_spdx_sha256",
        "release_cyclonedx_sha256",
        "g8_closure_run_id",
        "g8_closure_run_attempt",
        "g8_closure_sha256",
        "hyphae-release-candidate",
        "native-g8-aggregate-${{ steps.source.outputs.commit }}",
        "--publication-authority python-publication-authority.json",
        "independent-build:",
        "candidate-validation:",
        "matrix:\n        builder: [a, b]",
        "python-version: '3.11.15'",
        "hyphae-python-independent-${{ matrix.builder }}-${{ inputs.source_tag }}",
        "actions/runs/${{ github.run_id }}/artifacts?per_page=100",
        "--independent-build-receipt independent-a/builder-receipt.json",
        "--independent-build-receipt independent-b/builder-receipt.json",
        'test "$version" = "1.2.1"',
    }
    if any(fragment not in workflow for fragment in required_workflow):
        fail("Python Trusted Publishing workflow is incomplete")
    forbidden = {
        "skip-existing: true",
        "PYPI_TOKEN",
        "TWINE_PASSWORD",
        "password:",
        "--installed-distribution",
        "--python-version",
    }
    if any(fragment in workflow for fragment in forbidden):
        fail("Python publication must use fail-closed OIDC without stored credentials")
    if workflow.count("id-token: write") != 1:
        fail("only the Python publish job may mint an OIDC identity")
    if workflow.count("uv build source/sdks/python --out-dir dist") != 1:
        fail("Python publication must use only the two-job independent build matrix")
    if workflow.count("path: artifact/*") != 1:
        fail("independent builders must upload one flat artifact staging directory")
    if workflow.count("packages-dir: release/publish-dist/") != 2:
        fail("both registries must publish only the selected exact bytes")
    if workflow.count("--no-cache") < 3:
        fail("registry wheel and sdist installations must bypass shared caches")
    candidate_job = workflow_job(workflow, "candidate-validation")
    candidate_requirements = {
        "needs: independent-build",
        "name: hyphae-python-independent-a-${{ inputs.source_tag }}",
        "name: hyphae-python-independent-b-${{ inputs.source_tag }}",
        "python -m unittest discover -s source/sdks/python/tests -v",
        "control/tools/check_python_distributions.py",
        '"independent-a/hyphae_sdk-$version-py3-none-any.whl"',
        '"independent-a/hyphae_sdk-$version.tar.gz"',
        'cp "independent-$builder"/*.whl "candidate-$builder/"',
        "import hyphae_sdk",
    }
    if any(fragment not in candidate_job for fragment in candidate_requirements):
        fail("candidate-validation must exercise both named independent artifacts")
    if "actions/upload-artifact" in candidate_job or "\n    outputs:" in candidate_job:
        fail("candidate-validation must not expose artifacts or outputs to authority")
    build_job = workflow_job(workflow, "build")
    build_requirements = {
        "needs:\n      - independent-build\n      - candidate-validation",
        "name: hyphae-python-independent-a-${{ inputs.source_tag }}",
        "name: hyphae-python-independent-b-${{ inputs.source_tag }}",
    }
    if any(fragment not in build_job for fragment in build_requirements):
        fail("build authority must depend on validation and inspect original artifacts")
    distribution_check = re.search(
        r"python control/tools/check_python_distributions\.py \\\n"
        r'\s+--directory "inspected-\$builder" \\\n'
        r'\s+--expected-version "\$VERSION"',
        build_job,
    )
    if distribution_check is None:
        fail("build authority must inspect original artifacts canonically")
    validation = distribution_check.start()
    trusted_use = build_job.find("Verify independent builder identities")
    if validation < 0 or trusted_use < 0 or validation > trusted_use:
        fail("build authority must inspect candidate archives before trusting them")
    forbidden_build_execution = {
        "astral-sh/setup-uv",
        "uv build",
        "uv pip",
        "uv venv",
        "pip install",
        "pip wheel",
        "python -m build",
        "python -m unittest",
        "import hyphae_sdk",
        "source/sdks/python/src",
        "source/sdks/python/tests",
        "python source/",
        "python ./source/",
        "needs.candidate-validation.outputs",
    }
    if any(fragment in build_job for fragment in forbidden_build_execution):
        fail("build authority must not install, import, build, or test candidate code")
    publish_job = workflow_job(workflow, "publish")
    if "actions/checkout" in publish_job or "\n        run:" in publish_job:
        fail("OIDC publish job must not execute repository code or shell commands")
    # The checked-in source remains 1.1.0 until release preparation, but every
    # workflow path that can reach OIDC must reject it first.
    if f'test "$version" = "{APACHE_RELEASE_VERSION}"' not in workflow:
        fail("Apache Python publication must be gated on version 1.2.1")
    contract_root = workflow_root or root
    if not (
        contract_root / "docs/release/schema/python-distribution-receipt-v2.schema.json"
    ).is_file():
        fail("Python publication receipt schema is missing")
    return {"name": EXPECTED_NAME, "status": "passed", "version": version}


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", type=Path, default=ROOT)
    parser.add_argument("--workflow-root", type=Path)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    print(
        json.dumps(
            validate(args.root, workflow_root=args.workflow_root), sort_keys=True
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
