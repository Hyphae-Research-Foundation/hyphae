#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""Mutation tests for the publishable Python SDK contract."""

from __future__ import annotations

import shutil
import tempfile
import unittest
from pathlib import Path

from tools.check_python_package import (
    ROOT,
    PythonPackageValidationError,
    validate,
)


class PythonPackageContractTests(unittest.TestCase):
    def fixture(self) -> tempfile.TemporaryDirectory[str]:
        directory = tempfile.TemporaryDirectory()
        root = Path(directory.name)
        shutil.copytree(ROOT / "sdks/python", root / "sdks/python")
        shutil.copy2(ROOT / "Cargo.toml", root / "Cargo.toml")
        (root / ".github/workflows").mkdir(parents=True)
        shutil.copy2(
            ROOT / ".github/workflows/python-publish.yml",
            root / ".github/workflows/python-publish.yml",
        )
        schema = "docs/release/schema/python-distribution-receipt-v2.schema.json"
        (root / "docs/release/schema").mkdir(parents=True)
        shutil.copy2(ROOT / schema, root / schema)
        return directory

    def replace(self, root: Path, before: str, after: str) -> None:
        path = root / "sdks/python/pyproject.toml"
        text = path.read_text(encoding="utf-8")
        self.assertIn(before, text)
        path.write_text(text.replace(before, after, 1), encoding="utf-8")

    def remove_from_workflow(self, root: Path, fragment: str) -> None:
        path = root / ".github/workflows/python-publish.yml"
        text = path.read_text(encoding="utf-8")
        self.assertIn(fragment, text)
        path.write_text(text.replace(fragment, ""), encoding="utf-8")

    def replace_in_workflow(self, root: Path, before: str, after: str) -> None:
        path = root / ".github/workflows/python-publish.yml"
        text = path.read_text(encoding="utf-8")
        self.assertIn(before, text)
        path.write_text(text.replace(before, after, 1), encoding="utf-8")

    def test_checked_in_package_is_publishable(self) -> None:
        self.assertEqual(
            validate(),
            {"name": "hyphae-sdk", "status": "passed", "version": "1.1.0"},
        )

    def test_distribution_name_cannot_collide_with_unrelated_project(self) -> None:
        with self.fixture() as directory:
            root = Path(directory)
            self.replace(root, 'name = "hyphae-sdk"', 'name = "hyphae"')
            with self.assertRaisesRegex(PythonPackageValidationError, "hyphae-sdk"):
                validate(root)

    def test_runtime_dependency_cannot_be_added_silently(self) -> None:
        with self.fixture() as directory:
            root = Path(directory)
            self.replace(root, "dependencies = []", 'dependencies = ["requests"]')
            with self.assertRaisesRegex(
                PythonPackageValidationError, "standard-library"
            ):
                validate(root)

    def test_python_version_must_equal_the_workspace_version(self) -> None:
        with self.fixture() as directory:
            root = Path(directory)
            self.replace(root, 'version = "1.1.0"', 'version = "1.1.1"')
            with self.assertRaisesRegex(PythonPackageValidationError, "workspace"):
                validate(root)

    def test_typed_marker_cannot_be_omitted(self) -> None:
        with self.fixture() as directory:
            root = Path(directory)
            self.replace(root, 'hyphae_sdk = ["py.typed"]', "hyphae_sdk = []")
            with self.assertRaisesRegex(
                PythonPackageValidationError, "typed-distribution"
            ):
                validate(root)

    def test_publisher_cannot_fall_back_to_a_stored_token(self) -> None:
        with self.fixture() as directory:
            root = Path(directory)
            path = root / ".github/workflows/python-publish.yml"
            path.write_text(
                path.read_text(encoding="utf-8") + "\n# PYPI_TOKEN\n",
                encoding="utf-8",
            )
            with self.assertRaisesRegex(PythonPackageValidationError, "OIDC"):
                validate(root)

    def test_double_build_and_pre_oidc_rehash_cannot_be_removed(self) -> None:
        for fragment in (
            "--reproducible-directory",
            "check-local",
            "uv build source/sdks/python --out-dir dist",
            "--only-binary hyphae-sdk",
            "--no-binary hyphae-sdk",
            "--installation-evidence installation-evidence/3.14-sdist.json",
            "artifact-ids: ${{ needs.build.outputs.publication-artifact-id }}",
            "--no-cache",
            "--publication-artifact-sha256",
        ):
            with self.subTest(fragment=fragment), self.fixture() as directory:
                root = Path(directory)
                self.remove_from_workflow(root, fragment)
                with self.assertRaises(PythonPackageValidationError):
                    validate(root)

    def test_testpypi_authority_cannot_be_removed(self) -> None:
        for fragment in (
            "testpypi_receipt_sha256",
            "run-id: ${{ inputs.testpypi_run_id }}",
            "github-token: ${{ github.token }}",
            "testpypi-authority/release/publish-dist",
            "--testpypi-run-metadata testpypi-authority/github-run.json",
        ):
            with self.subTest(fragment=fragment), self.fixture() as directory:
                root = Path(directory)
                self.remove_from_workflow(root, fragment)
                with self.assertRaisesRegex(PythonPackageValidationError, "workflow"):
                    validate(root)

    def test_pypi_release_and_g8_authority_cannot_be_removed(self) -> None:
        for fragment in (
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
        ):
            with self.subTest(fragment=fragment), self.fixture() as directory:
                root = Path(directory)
                self.remove_from_workflow(root, fragment)
                with self.assertRaisesRegex(PythonPackageValidationError, "workflow"):
                    validate(root)

    def test_reproducibility_requires_two_independent_builder_jobs(self) -> None:
        for fragment in (
            "independent-build:",
            "matrix:\n        builder: [a, b]",
            "python-version: '3.11.15'",
            "hyphae-python-independent-${{ matrix.builder }}-${{ inputs.source_tag }}",
            "actions/runs/${{ github.run_id }}/artifacts?per_page=100",
            "--independent-build-receipt independent-a/builder-receipt.json",
            "--independent-build-receipt independent-b/builder-receipt.json",
            "mkdir artifact",
            "cp dist/*.whl dist/*.tar.gz artifact/",
            'Path("artifact/builder-receipt.json")',
            "path: artifact/*",
            'cp "independent-$builder"/*.whl "candidate-$builder/"',
        ):
            with self.subTest(fragment=fragment), self.fixture() as directory:
                root = Path(directory)
                self.remove_from_workflow(root, fragment)
                with self.assertRaises(PythonPackageValidationError):
                    validate(root)

    def test_installation_evidence_cannot_be_replaced_by_declared_flags(self) -> None:
        mutations = (
            (
                "--installation-evidence installation-evidence/3.11-wheel.json",
                "--installed-distribution 3.11:wheel",
            ),
            (
                "if observed_sha256 != expected_sha256:",
                "if False:",
            ),
            (
                '"schema": "hyphae-python-installation-evidence-v1"',
                '"schema": "unreviewed"',
            ),
            ("installation-evidence/*.json", "installation-evidence-disabled/*.json"),
        )
        for before, after in mutations:
            with self.subTest(before=before), self.fixture() as directory:
                root = Path(directory)
                self.replace_in_workflow(root, before, after)
                with self.assertRaisesRegex(PythonPackageValidationError, "workflow"):
                    validate(root)

    def test_independent_artifact_cannot_restore_nested_upload_paths(self) -> None:
        with self.fixture() as directory:
            root = Path(directory)
            self.replace_in_workflow(
                root,
                "path: artifact/*",
                "path: |\n            dist/*.whl\n            dist/*.tar.gz\n            builder-receipt.json",
            )
            with self.assertRaisesRegex(PythonPackageValidationError, "workflow|flat"):
                validate(root)

    def test_candidate_execution_must_remain_in_an_isolated_job(self) -> None:
        mutations = (
            (
                "  candidate-validation:\n",
                "  candidate-check-disabled:\n",
            ),
            (
                "  candidate-validation:\n    name: Validate candidate Python code and distributions\n    needs: independent-build",
                "  candidate-validation:\n    name: Validate candidate Python code and distributions\n    needs: []",
            ),
            (
                "      - independent-build\n      - candidate-validation",
                "      - independent-build",
            ),
            (
                "  build:\n    name: Compare independent Python distributions and bind authority",
                "  build:\n    name: Compare independent Python distributions and bind authority\n    # import hyphae_sdk",
            ),
            (
                "  build:\n    name: Compare independent Python distributions and bind authority",
                "  build:\n    name: Compare independent Python distributions and bind authority\n    # needs.candidate-validation.outputs.dist",
            ),
            (
                "  candidate-validation:\n    name: Validate candidate Python code and distributions",
                "  candidate-validation:\n    name: Validate candidate Python code and distributions\n    # actions/upload-artifact",
            ),
        )
        for before, after in mutations:
            with self.subTest(after=after), self.fixture() as directory:
                root = Path(directory)
                self.replace_in_workflow(root, before, after)
                with self.assertRaises(PythonPackageValidationError):
                    validate(root)

    def test_build_authority_must_validate_downloads_before_trusting_them(self) -> None:
        marker = "python control/tools/check_python_distributions.py"
        with self.fixture() as directory:
            root = Path(directory)
            path = root / ".github/workflows/python-publish.yml"
            text = path.read_text(encoding="utf-8")
            build_start = text.index("\n  build:\n")
            check = text.index(marker, build_start)
            path.write_text(
                text[:check]
                + "python control/tools/unchecked_distributions.py"
                + text[check + len(marker) :],
                encoding="utf-8",
            )
            with self.assertRaisesRegex(
                PythonPackageValidationError, "inspect|incomplete"
            ):
                validate(root)

    def test_oidc_job_cannot_execute_repository_code(self) -> None:
        with self.fixture() as directory:
            root = Path(directory)
            path = root / ".github/workflows/python-publish.yml"
            text = path.read_text(encoding="utf-8")
            marker = "  verify:\n"
            self.assertIn(marker, text)
            injected = "      - name: Unsafe repository code\n        run: python tools/unsafe.py\n\n"
            path.write_text(
                text.replace(marker, injected + marker, 1), encoding="utf-8"
            )
            with self.assertRaisesRegex(PythonPackageValidationError, "OIDC"):
                validate(root)


if __name__ == "__main__":
    unittest.main()
