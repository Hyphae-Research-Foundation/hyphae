#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Unit tests for exact-source Python publication receipts."""

from __future__ import annotations

import base64
import copy
import json
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from tools.python_distribution_receipt import (
    CRYPTOGRAPHIC_VERIFIER,
    INSTALLATION_EVIDENCE_SCHEMA,
    WORKFLOW_REF,
    PythonReceiptError,
    build_receipt,
    check_local_distributions,
    load_local_json,
    sha256_bytes,
    validate_receipt,
    verify_distribution_cryptographically,
    verify_provenance,
    verify_registry,
)

COMMIT = "a" * 40
TREE = "b" * 40
WORKFLOW_SHA = "c" * 40
ROOT = Path(__file__).resolve().parents[1]


class PythonDistributionReceiptTests(unittest.TestCase):
    def fixture(
        self, *, wheel: bytes = b"wheel", sdist: bytes = b"sdist"
    ) -> tempfile.TemporaryDirectory[str]:
        directory = tempfile.TemporaryDirectory()
        root = Path(directory.name)
        (root / "hyphae_sdk-1.2.0-py3-none-any.whl").write_bytes(wheel)
        (root / "hyphae_sdk-1.2.0.tar.gz").write_bytes(sdist)
        return directory

    def build(
        self,
        directory: Path,
        repeated: Path,
        *,
        repository: str = "testpypi",
        testpypi_receipt: Path | None = None,
        testpypi_run_metadata: Path | None = None,
        testpypi_receipt_sha256: str | None = None,
        testpypi_run_id: int | None = None,
        workflow_ref: str = WORKFLOW_REF,
    ) -> dict[str, object]:
        with tempfile.TemporaryDirectory() as evidence:
            receipts, authority = self.publication_contract(
                Path(evidence), directory, repeated, repository
            )
            try:
                return build_receipt(
                    directory,
                    "1.2.0",
                    "v1.2.0",
                    COMMIT,
                    reproducible_directory=repeated,
                    independent_build_receipts=receipts,
                    publication_authority=authority,
                    source_tree=TREE,
                    workflow_repository="celiumsai/hyphae",
                    workflow_ref=workflow_ref,
                    workflow_sha=WORKFLOW_SHA,
                    workflow_run_id=124 if repository == "pypi" else 123,
                    workflow_run_attempt=1,
                    repository=repository,
                    testpypi_receipt=testpypi_receipt,
                    testpypi_run_metadata=testpypi_run_metadata,
                    testpypi_receipt_sha256=testpypi_receipt_sha256,
                    testpypi_run_id=testpypi_run_id,
                )
            finally:
                for path in receipts:
                    path.unlink(missing_ok=True)

    def publication_contract(
        self, root: Path, first: Path, second: Path, repository: str
    ) -> tuple[tuple[Path, Path], Path]:
        source = {"tag": "v1.2.0", "commit": COMMIT, "tree": TREE}
        receipts = []
        receipt_values = []
        for builder, directory in (("a", first), ("b", second)):
            distributions = [
                {
                    "filename": path.name,
                    "sha256": sha256_bytes(path.read_bytes()),
                    "bytes": path.stat().st_size,
                }
                for path in sorted(directory.iterdir())
            ]
            value = {
                "schema": "hyphae-python-independent-build-v1",
                "builder": builder,
                "source": source,
                "version": "1.2.0",
                "toolchain": {"python": "3.11.15", "uv": "uv 0.12.3"},
                "runner": {
                    "os": "Linux",
                    "arch": "X64",
                    "image_os": "ubuntu24",
                    "image_version": "20260810.1",
                },
                "distributions": distributions,
            }
            path = directory / "builder-receipt.json"
            path.write_text(json.dumps(value, sort_keys=True), encoding="utf-8")
            receipts.append(path)
            receipt_values.append(value)
        builds = [
            {
                "builder": value["builder"],
                "artifact": {
                    "id": 100 + index,
                    "name": (f"hyphae-python-independent-{value['builder']}-v1.2.0"),
                    "digest": f"sha256:{str(index + 1) * 64}",
                },
                "receipt_sha256": sha256_bytes(path.read_bytes()),
                "toolchain": value["toolchain"],
                "runner": value["runner"],
                "distributions": value["distributions"],
            }
            for index, (path, value) in enumerate(zip(receipts, receipt_values))
        ]
        release_authority = None
        if repository == "pypi":
            release_authority = self.release_authority(source)
        authority = root / "publication-authority.json"
        authority.write_text(
            json.dumps(
                {
                    "schema": "hyphae-python-publication-authority-v1",
                    "source": source,
                    "independent_builds": builds,
                    "release_authority": release_authority,
                },
                sort_keys=True,
            ),
            encoding="utf-8",
        )
        return (receipts[0], receipts[1]), authority

    def release_authority(self, source: dict[str, str]) -> dict[str, object]:
        def run(
            run_id: int, workflow: str, event: str, branch: str
        ) -> dict[str, object]:
            return {
                "id": run_id,
                "attempt": 1,
                "event": event,
                "status": "completed",
                "conclusion": "success",
                "head_branch": branch,
                "head_sha": source["commit"],
                "path": workflow,
                "repository": "celiumsai/hyphae",
            }

        return {
            "run": run(200, ".github/workflows/release.yml", "push", source["tag"]),
            "artifact": {
                "id": 300,
                "name": "hyphae-release-candidate",
                "digest": f"sha256:{'3' * 64}",
            },
            "release_evidence": {
                "filename": f"hyphae-{source['tag']}.release-evidence.json",
                "sha256": "4" * 64,
            },
            "sboms": {
                "spdx": {
                    "filename": f"hyphae-{source['tag']}.spdx.json",
                    "sha256": "5" * 64,
                },
                "cyclonedx": {
                    "filename": f"hyphae-{source['tag']}.cdx.json",
                    "sha256": "6" * 64,
                },
            },
            "g8_closure": {
                "run": run(
                    201,
                    ".github/workflows/native-g8-closure.yml",
                    "workflow_dispatch",
                    "main",
                ),
                "artifact": {
                    "id": 301,
                    "name": f"native-g8-aggregate-{source['commit']}",
                    "digest": f"sha256:{'7' * 64}",
                },
                "aggregate": {
                    "filename": "native-g8-aggregate.json",
                    "sha256": "8" * 64,
                    "claims": ["G8"],
                    "closure_declared": True,
                },
            },
        }

    def run_metadata(self, **overrides: object) -> dict[str, object]:
        metadata: dict[str, object] = {
            "id": 123,
            "run_attempt": 1,
            "event": "workflow_dispatch",
            "status": "completed",
            "conclusion": "success",
            "head_branch": "main",
            "head_sha": WORKFLOW_SHA,
            "path": ".github/workflows/python-publish.yml",
            "repository": {"full_name": "celiumsai/hyphae"},
            "head_repository": {"full_name": "celiumsai/hyphae"},
        }
        metadata.update(overrides)
        return metadata

    def release(self, receipt: dict[str, object]) -> dict[str, object]:
        distributions = receipt["distributions"]
        assert isinstance(distributions, dict)
        return {
            "info": {"name": "hyphae-sdk", "version": "1.2.0"},
            "urls": [
                {
                    "filename": entry["filename"],
                    "digests": {"sha256": entry["sha256"]},
                }
                for entry in distributions.values()
            ],
        }

    def provenance(
        self, filename: str, digest: str, *, repository: str = "testpypi"
    ) -> dict[str, object]:
        statement = {
            "_type": "https://in-toto.io/Statement/v1",
            "subject": [{"name": filename, "digest": {"sha256": digest}}],
            "predicateType": "https://docs.pypi.org/attestations/publish/v1",
            "predicate": None,
        }
        encoded = base64.b64encode(
            json.dumps(statement, separators=(",", ":")).encode()
        ).decode()
        return {
            "version": 1,
            "attestation_bundles": [
                {
                    "publisher": {
                        "kind": "GitHub",
                        "repository": "celiumsai/hyphae",
                        "workflow": "python-publish.yml",
                        "environment": repository,
                        "claims": None,
                    },
                    "attestations": [
                        {
                            "envelope": {
                                "statement": encoded,
                                "signature": "signature",
                            },
                            "verification_material": {
                                "certificate": "observed-by-pypi"
                            },
                            "version": 1,
                        }
                    ],
                }
            ],
        }

    def publish(
        self, receipt: dict[str, object], *, repository: str = "testpypi"
    ) -> tuple[dict[str, object], bytes]:
        receipt_bytes = (json.dumps(receipt, sort_keys=True) + "\n").encode()
        distributions = receipt["distributions"]
        assert isinstance(distributions, dict)
        provenances = {
            entry["filename"]: self.provenance(
                entry["filename"], entry["sha256"], repository=repository
            )
            for entry in distributions.values()
        }
        with self.fixture() as directory:
            evidence = self.installation_evidence(Path(directory), receipt)
            published = verify_registry(
                receipt,
                receipt_bytes,
                self.release(receipt),
                provenances,
                repository,
                evidence,
                321,
                "d" * 64,
                distribution_directory=Path(directory),
                cryptographic_verifier=lambda *_: dict(CRYPTOGRAPHIC_VERIFIER),
            )
        return published, receipt_bytes

    def installation_evidence(
        self,
        root: Path,
        receipt: dict[str, object],
    ) -> tuple[Path, ...]:
        directory = root / "installation-evidence"
        directory.mkdir()
        distributions = receipt["distributions"]
        assert isinstance(distributions, dict)
        paths = []
        for boundary, patch in (("3.11", "9"), ("3.14", "0")):
            for kind in ("wheel", "sdist"):
                distribution = distributions[kind]
                value = {
                    "schema": INSTALLATION_EVIDENCE_SCHEMA,
                    "python_boundary": boundary,
                    "kind": kind,
                    "version": receipt["version"],
                    "distribution_filename": distribution["filename"],
                    "distribution_sha256": distribution["sha256"],
                    "implementation": "CPython",
                    "interpreter_version": f"{boundary}.{patch}",
                    "status": "passed",
                }
                path = directory / f"{boundary}-{kind}.json"
                path.write_text(
                    json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n",
                    encoding="utf-8",
                )
                paths.append(path)
        return tuple(paths)

    def test_build_receipt_v2_binds_source_run_and_reproducible_bytes(self) -> None:
        with self.fixture() as directory, self.fixture() as repeated:
            receipt = self.build(Path(directory), Path(repeated))
        self.assertEqual(receipt["schema"], "hyphae-python-distribution-receipt-v2")
        self.assertEqual(receipt["source"]["tree"], TREE)
        self.assertEqual(receipt["run"]["id"], 123)
        self.assertEqual(receipt["reproducibility"], {"builds": 2, "matched": True})
        self.assertEqual(
            [
                item["builder"]
                for item in receipt["publication_authority"]["independent_builds"]
            ],
            ["a", "b"],
        )

    def test_publication_authority_binds_exact_builder_receipt_bytes(self) -> None:
        with (
            self.fixture() as directory,
            self.fixture() as repeated,
            tempfile.TemporaryDirectory() as evidence,
        ):
            receipts, authority = self.publication_contract(
                Path(evidence), Path(directory), Path(repeated), "testpypi"
            )
            receipts[0].write_text(
                receipts[0].read_text(encoding="utf-8") + "\n", encoding="utf-8"
            )
            with self.assertRaisesRegex(
                PythonReceiptError, "differs from independent builder receipts"
            ):
                build_receipt(
                    Path(directory),
                    "1.2.0",
                    "v1.2.0",
                    COMMIT,
                    reproducible_directory=Path(repeated),
                    independent_build_receipts=receipts,
                    publication_authority=authority,
                    source_tree=TREE,
                    workflow_repository="celiumsai/hyphae",
                    workflow_ref=WORKFLOW_REF,
                    workflow_sha=WORKFLOW_SHA,
                    workflow_run_id=123,
                    workflow_run_attempt=1,
                    repository="testpypi",
                )

    def test_local_json_rejects_recursive_duplicate_keys(self) -> None:
        with tempfile.NamedTemporaryFile() as source:
            source.write(b'{"authority":{"run":1,"run":2}}')
            source.flush()
            with self.assertRaisesRegex(PythonReceiptError, "duplicate.*run"):
                load_local_json(Path(source.name), "test authority")

    def test_builder_and_publication_authorities_reject_duplicate_keys(self) -> None:
        with (
            self.fixture() as directory,
            self.fixture() as repeated,
            tempfile.TemporaryDirectory() as evidence,
        ):
            receipts, authority = self.publication_contract(
                Path(evidence), Path(directory), Path(repeated), "testpypi"
            )
            cases = (
                (
                    receipts[0],
                    '"runner": {',
                    '"runner": {"os": "duplicate",',
                ),
                (
                    authority,
                    '"source": {',
                    '"source": {"tree": "duplicate",',
                ),
            )
            for path, before, after in cases:
                with self.subTest(path=path.name):
                    original = path.read_text(encoding="utf-8")
                    self.assertIn(before, original)
                    path.write_text(
                        original.replace(before, after, 1), encoding="utf-8"
                    )
                    with self.assertRaisesRegex(PythonReceiptError, "duplicate"):
                        build_receipt(
                            Path(directory),
                            "1.2.0",
                            "v1.2.0",
                            COMMIT,
                            reproducible_directory=Path(repeated),
                            independent_build_receipts=receipts,
                            publication_authority=authority,
                            source_tree=TREE,
                            workflow_repository="celiumsai/hyphae",
                            workflow_ref=WORKFLOW_REF,
                            workflow_sha=WORKFLOW_SHA,
                            workflow_run_id=123,
                            workflow_run_attempt=1,
                            repository="testpypi",
                        )
                    path.write_text(original, encoding="utf-8")

    def test_verify_rejects_duplicate_keys_in_exact_build_receipt_bytes(self) -> None:
        with self.fixture() as directory, self.fixture() as repeated:
            receipt = self.build(Path(directory), Path(repeated))
            encoded = json.dumps(receipt).replace(
                '"source": {"tag": "v1.2.0",',
                '"source": {"tag": "v1.2.0", "tag": "v1.2.0",',
                1,
            )
            with self.assertRaisesRegex(PythonReceiptError, "duplicate.*tag"):
                verify_registry(
                    receipt,
                    encoded.encode(),
                    self.release(receipt),
                    {},
                    "testpypi",
                    (),
                    321,
                    "d" * 64,
                )

    def test_testpypi_rejects_release_authority_and_pypi_requires_closed_g8(
        self,
    ) -> None:
        with self.fixture() as directory, self.fixture() as repeated:
            testpypi = self.build(Path(directory), Path(repeated))
            testpypi["publication_authority"]["release_authority"] = (
                self.release_authority(testpypi["source"])
            )
            with self.assertRaisesRegex(PythonReceiptError, "cannot claim"):
                validate_receipt(testpypi)

            testpypi["publication_authority"]["release_authority"] = None
            published, _ = self.publish(testpypi)
            with (
                tempfile.NamedTemporaryFile() as authority,
                tempfile.NamedTemporaryFile() as metadata,
            ):
                authority_bytes = (
                    json.dumps(published, sort_keys=True) + "\n"
                ).encode()
                authority.write(authority_bytes)
                authority.flush()
                metadata.write(json.dumps(self.run_metadata()).encode())
                metadata.flush()
                pypi = self.build(
                    Path(directory),
                    Path(repeated),
                    repository="pypi",
                    testpypi_receipt=Path(authority.name),
                    testpypi_run_metadata=Path(metadata.name),
                    testpypi_receipt_sha256=sha256_bytes(authority_bytes),
                    testpypi_run_id=123,
                )
            pypi["publication_authority"]["release_authority"]["g8_closure"][
                "aggregate"
            ]["closure_declared"] = False
            with self.assertRaisesRegex(PythonReceiptError, "G8 aggregate"):
                validate_receipt(pypi)

    def test_second_build_drift_is_rejected(self) -> None:
        with (
            self.fixture() as directory,
            self.fixture(wheel=b"different") as repeated,
            self.assertRaisesRegex(PythonReceiptError, "reproducible"),
        ):
            self.build(Path(directory), Path(repeated))

    def test_source_tree_and_workflow_identity_are_fail_closed(self) -> None:
        with (
            self.fixture() as directory,
            self.fixture() as repeated,
            self.assertRaisesRegex(PythonReceiptError, "canonical"),
        ):
            self.build(
                Path(directory),
                Path(repeated),
                workflow_ref=WORKFLOW_REF.replace("main", "feature"),
            )

    def test_pypi_requires_exact_published_testpypi_authority(self) -> None:
        with (
            self.fixture() as directory,
            self.fixture() as repeated,
            tempfile.NamedTemporaryFile() as authority,
            tempfile.NamedTemporaryFile() as metadata,
        ):
            built = self.build(Path(directory), Path(repeated))
            published, _ = self.publish(built)
            authority_bytes = (json.dumps(published, sort_keys=True) + "\n").encode()
            authority.write(authority_bytes)
            authority.flush()
            metadata.write(json.dumps(self.run_metadata()).encode())
            metadata.flush()
            pypi = self.build(
                Path(directory),
                Path(repeated),
                repository="pypi",
                testpypi_receipt=Path(authority.name),
                testpypi_run_metadata=Path(metadata.name),
                testpypi_receipt_sha256=sha256_bytes(authority_bytes),
                testpypi_run_id=123,
            )
        self.assertEqual(pypi["testpypi_authority"]["run"]["id"], 123)
        self.assertEqual(
            pypi["testpypi_authority"]["run_metadata"]["conclusion"], "success"
        )

    def test_pypi_rejects_authority_digest_and_source_drift(self) -> None:
        with (
            self.fixture() as directory,
            self.fixture() as repeated,
            tempfile.NamedTemporaryFile() as authority,
            tempfile.NamedTemporaryFile() as metadata,
        ):
            built = self.build(Path(directory), Path(repeated))
            published, _ = self.publish(built)
            published["source"]["tree"] = "d" * 40
            authority_bytes = json.dumps(published).encode()
            authority.write(authority_bytes)
            authority.flush()
            metadata.write(json.dumps(self.run_metadata()).encode())
            metadata.flush()
            with self.assertRaisesRegex(
                PythonReceiptError, "exact source|publication authority"
            ):
                self.build(
                    Path(directory),
                    Path(repeated),
                    repository="pypi",
                    testpypi_receipt=Path(authority.name),
                    testpypi_run_metadata=Path(metadata.name),
                    testpypi_receipt_sha256=sha256_bytes(authority_bytes),
                    testpypi_run_id=123,
                )

    def test_github_run_authority_metadata_is_fail_closed(self) -> None:
        mutations = {
            "conclusion": {"conclusion": "failure"},
            "workflow": {"path": ".github/workflows/other.yml"},
            "branch": {"head_branch": "feature"},
            "sha": {"head_sha": "e" * 40},
            "attempt": {"run_attempt": 2},
            "repository": {"repository": {"full_name": "attacker/fork"}},
        }
        with (
            self.fixture() as directory,
            self.fixture() as repeated,
            tempfile.NamedTemporaryFile() as authority,
            tempfile.NamedTemporaryFile() as metadata,
        ):
            built = self.build(Path(directory), Path(repeated))
            published, _ = self.publish(built)
            authority_bytes = (json.dumps(published, sort_keys=True) + "\n").encode()
            authority.write(authority_bytes)
            authority.flush()
            for name, mutation in mutations.items():
                with self.subTest(name=name):
                    metadata.seek(0)
                    metadata.truncate()
                    metadata.write(json.dumps(self.run_metadata(**mutation)).encode())
                    metadata.flush()
                    with self.assertRaisesRegex(PythonReceiptError, "successful main"):
                        self.build(
                            Path(directory),
                            Path(repeated),
                            repository="pypi",
                            testpypi_receipt=Path(authority.name),
                            testpypi_run_metadata=Path(metadata.name),
                            testpypi_receipt_sha256=sha256_bytes(authority_bytes),
                            testpypi_run_id=123,
                        )

    def test_local_distribution_mutation_is_rejected_before_oidc(self) -> None:
        with self.fixture() as directory, self.fixture() as repeated:
            receipt = self.build(Path(directory), Path(repeated))
            Path(directory, "hyphae_sdk-1.2.0.tar.gz").write_bytes(b"mutated")
            with self.assertRaisesRegex(PythonReceiptError, "local publication files"):
                check_local_distributions(
                    receipt,
                    Path(directory),
                    source_commit=COMMIT,
                    source_tree=TREE,
                    workflow_run_id=123,
                    workflow_run_attempt=1,
                    repository="testpypi",
                )

    def test_registry_and_pep740_provenance_are_exact(self) -> None:
        with self.fixture() as directory, self.fixture() as repeated:
            receipt = self.build(Path(directory), Path(repeated))
            published, _ = self.publish(receipt)
        self.assertEqual(published["status"], "published")
        provenance = published["registry_verification"]["provenance"]
        self.assertEqual(
            published["registry_verification"]["publication_artifact"],
            {"id": 321, "sha256": "d" * 64},
        )
        self.assertEqual(
            provenance["wheel"]["publisher"]["workflow"], "python-publish.yml"
        )
        self.assertEqual(
            provenance["sdist"]["subject"]["sha256"],
            receipt["distributions"]["sdist"]["sha256"],
        )
        self.assertEqual(
            provenance["wheel"]["cryptographic_verifier"],
            CRYPTOGRAPHIC_VERIFIER,
        )
        installation = published["registry_verification"]["installation_evidence"]
        self.assertEqual(
            [
                (item["observed"]["python_boundary"], item["observed"]["kind"])
                for item in installation
            ],
            [
                ("3.11", "wheel"),
                ("3.11", "sdist"),
                ("3.14", "wheel"),
                ("3.14", "sdist"),
            ],
        )
        self.assertTrue(
            all(len(item["evidence_sha256"]) == 64 for item in installation)
        )

    def test_installation_evidence_is_complete_unique_and_source_bound(self) -> None:
        mutations = {
            "version": ("version", "1.2.1"),
            "kind": ("kind", "zip"),
            "digest": ("distribution_sha256", "0" * 64),
            "filename": ("distribution_filename", "other.whl"),
        }
        with self.fixture() as directory, self.fixture() as repeated:
            receipt = self.build(Path(directory), Path(repeated))
            distributions = receipt["distributions"]
            provenances = {
                entry["filename"]: self.provenance(entry["filename"], entry["sha256"])
                for entry in distributions.values()
            }
            evidence = self.installation_evidence(Path(directory), receipt)

            for name, (field, value) in mutations.items():
                with self.subTest(name=name):
                    original = evidence[0].read_text(encoding="utf-8")
                    changed = json.loads(original)
                    changed[field] = value
                    evidence[0].write_text(
                        json.dumps(changed, sort_keys=True, separators=(",", ":"))
                        + "\n",
                        encoding="utf-8",
                    )
                    with self.assertRaisesRegex(
                        PythonReceiptError, "unknown|exact distribution"
                    ):
                        verify_registry(
                            receipt,
                            json.dumps(receipt).encode(),
                            self.release(receipt),
                            provenances,
                            "testpypi",
                            evidence,
                            321,
                            "d" * 64,
                            distribution_directory=Path(directory),
                            cryptographic_verifier=lambda *_: dict(
                                CRYPTOGRAPHIC_VERIFIER
                            ),
                        )
                    evidence[0].write_text(original, encoding="utf-8")

            with self.assertRaisesRegex(PythonReceiptError, "duplicate"):
                verify_registry(
                    receipt,
                    json.dumps(receipt).encode(),
                    self.release(receipt),
                    provenances,
                    "testpypi",
                    (evidence[0], evidence[0], evidence[2], evidence[3]),
                    321,
                    "d" * 64,
                    distribution_directory=Path(directory),
                    cryptographic_verifier=lambda *_: dict(CRYPTOGRAPHIC_VERIFIER),
                )

    def test_installation_evidence_rejects_duplicate_keys_and_noncanonical_json(
        self,
    ) -> None:
        with self.fixture() as directory, self.fixture() as repeated:
            receipt = self.build(Path(directory), Path(repeated))
            evidence = self.installation_evidence(Path(directory), receipt)
            original = evidence[0].read_text(encoding="utf-8")
            duplicate = original.replace(
                '"kind":"wheel",', '"kind":"wheel","kind":"wheel",', 1
            )
            evidence[0].write_text(duplicate, encoding="utf-8")
            with self.assertRaisesRegex(PythonReceiptError, "duplicate.*kind"):
                verify_registry(
                    receipt,
                    json.dumps(receipt).encode(),
                    self.release(receipt),
                    {},
                    "testpypi",
                    evidence,
                    321,
                    "d" * 64,
                )
            evidence[0].write_text(json.dumps(json.loads(original)), encoding="utf-8")
            with self.assertRaisesRegex(PythonReceiptError, "canonical"):
                verify_registry(
                    receipt,
                    json.dumps(receipt).encode(),
                    self.release(receipt),
                    {},
                    "testpypi",
                    evidence,
                    321,
                    "d" * 64,
                )

    def test_pep740_wrong_publisher_or_subject_is_rejected(self) -> None:
        filename = "hyphae_sdk-1.2.0.tar.gz"
        digest = "1" * 64
        provenance = self.provenance(filename, digest)
        provenance["attestation_bundles"][0]["publisher"]["repository"] = (
            "attacker/fork"
        )
        with self.assertRaisesRegex(PythonReceiptError, "matching Trusted Publisher"):
            verify_provenance(provenance, filename, digest, "testpypi")
        provenance = self.provenance(filename, "2" * 64)
        with self.assertRaisesRegex(PythonReceiptError, "matching Trusted Publisher"):
            verify_provenance(provenance, filename, digest, "testpypi")

    def test_crypto_verifier_receives_only_the_exact_selected_attestation(self) -> None:
        with self.fixture() as directory, self.fixture() as repeated:
            receipt = self.build(Path(directory), Path(repeated))
            distributions = receipt["distributions"]
            provenances = {
                entry["filename"]: self.provenance(entry["filename"], entry["sha256"])
                for entry in distributions.values()
            }
            wheel = provenances[distributions["wheel"]["filename"]]
            different = copy.deepcopy(wheel["attestation_bundles"][0])
            different["publisher"]["workflow"] = "other.yml"
            wheel["attestation_bundles"].append(different)
            evidence = self.installation_evidence(Path(directory), receipt)

            def exact_selection(_: Path, selected: object, __: str) -> dict[str, str]:
                bundles = selected["attestation_bundles"]
                self.assertEqual(len(bundles), 1)
                self.assertEqual(len(bundles[0]["attestations"]), 1)
                self.assertEqual(
                    bundles[0]["publisher"]["workflow"], "python-publish.yml"
                )
                return dict(CRYPTOGRAPHIC_VERIFIER)

            verify_registry(
                receipt,
                json.dumps(receipt).encode(),
                self.release(receipt),
                provenances,
                "testpypi",
                evidence,
                321,
                "d" * 64,
                distribution_directory=Path(directory),
                cryptographic_verifier=exact_selection,
            )

    def test_official_pep740_verifier_uses_production_roots_for_both_registries(
        self,
    ) -> None:
        with self.fixture() as directory:
            artifact = Path(directory, "hyphae_sdk-1.2.0.tar.gz")
            provenance = self.provenance(artifact.name, "1" * 64)

            def completed(
                command: list[str], **kwargs: object
            ) -> subprocess.CompletedProcess[bytes]:
                self.assertIn("pypi-attestations==0.0.30", command)
                self.assertIn("https://github.com/celiumsai/hyphae", command)
                self.assertIn("--isolated", command)
                self.assertIn("--no-config", command)
                self.assertIn("--no-env-file", command)
                self.assertIn("https://pypi.org/simple", command)
                self.assertNotIn("--staging", command)
                evidence_path = Path(command[command.index("--provenance-file") + 1])
                self.assertEqual(
                    json.loads(evidence_path.read_text(encoding="utf-8")),
                    provenance,
                )
                self.assertEqual(Path(command[-1]), artifact.resolve())
                self.assertNotIn("GH_TOKEN", kwargs["env"])
                self.assertNotIn("UV_FIND_LINKS", kwargs["env"])
                self.assertNotIn("UV_NO_INDEX", kwargs["env"])
                return subprocess.CompletedProcess(command, 0, b"", b"")

            for repository in ("testpypi", "pypi"):
                with (
                    self.subTest(repository=repository),
                    mock.patch.dict(
                        "os.environ",
                        {
                            "GH_TOKEN": "secret",
                            "PYTHONPATH": "untrusted",
                            "UV_FIND_LINKS": "/attacker",
                            "UV_NO_INDEX": "1",
                        },
                    ),
                    mock.patch(
                        "tools.python_distribution_receipt.subprocess.run",
                        side_effect=completed,
                    ),
                ):
                    evidence = verify_distribution_cryptographically(
                        artifact, provenance, repository
                    )
                    self.assertEqual(evidence, CRYPTOGRAPHIC_VERIFIER)

    def test_official_pep740_verifier_rejects_tampered_crypto_evidence(self) -> None:
        with self.fixture() as directory:
            artifact = Path(directory, "hyphae_sdk-1.2.0.tar.gz")
            original = self.provenance(artifact.name, "1" * 64)
            statement = {
                "_type": "https://in-toto.io/Statement/v1",
                "subject": [{"name": artifact.name, "digest": {"sha256": "2" * 64}}],
                "predicateType": "https://docs.pypi.org/attestations/publish/v1",
                "predicate": None,
            }
            mutations = {
                "signature": (
                    "attestation_bundles",
                    0,
                    "attestations",
                    0,
                    "envelope",
                    "signature",
                    "tampered",
                ),
                "material": (
                    "attestation_bundles",
                    0,
                    "attestations",
                    0,
                    "verification_material",
                    "certificate",
                    "tampered",
                ),
                "subject": (
                    "attestation_bundles",
                    0,
                    "attestations",
                    0,
                    "envelope",
                    "statement",
                    base64.b64encode(json.dumps(statement).encode()).decode(),
                ),
            }
            for name, mutation in mutations.items():
                with self.subTest(name=name):
                    provenance = copy.deepcopy(original)
                    target: object = provenance
                    for part in mutation[:-2]:
                        target = target[part]
                    target[mutation[-2]] = mutation[-1]

                    def rejected(
                        command: list[str], expected: object = provenance, **_: object
                    ) -> subprocess.CompletedProcess[bytes]:
                        evidence_path = Path(
                            command[command.index("--provenance-file") + 1]
                        )
                        self.assertEqual(
                            json.loads(evidence_path.read_text(encoding="utf-8")),
                            expected,
                        )
                        return subprocess.CompletedProcess(command, 1, b"", b"")

                    with (
                        mock.patch(
                            "tools.python_distribution_receipt.subprocess.run",
                            side_effect=rejected,
                        ),
                        self.assertRaisesRegex(
                            PythonReceiptError, "cryptographic verification failed"
                        ),
                    ):
                        verify_distribution_cryptographically(
                            artifact, provenance, "testpypi"
                        )

    def test_registry_cannot_claim_crypto_evidence_when_verifier_fails(self) -> None:
        with self.fixture() as directory, self.fixture() as repeated:
            receipt = self.build(Path(directory), Path(repeated))
            distributions = receipt["distributions"]
            provenances = {
                entry["filename"]: self.provenance(entry["filename"], entry["sha256"])
                for entry in distributions.values()
            }
            evidence = self.installation_evidence(Path(directory), receipt)

            def rejected(*_: object) -> dict[str, str]:
                raise PythonReceiptError("PEP 740 cryptographic verification failed")

            with self.assertRaisesRegex(
                PythonReceiptError, "cryptographic verification failed"
            ):
                verify_registry(
                    receipt,
                    json.dumps(receipt).encode(),
                    self.release(receipt),
                    provenances,
                    "testpypi",
                    evidence,
                    321,
                    "d" * 64,
                    distribution_directory=Path(directory),
                    cryptographic_verifier=rejected,
                )

    def test_schema_metaschema_and_built_published_samples_are_semantic(self) -> None:
        schema = ROOT / "docs/release/schema/python-distribution-receipt-v2.schema.json"
        with (
            self.fixture() as directory,
            self.fixture() as repeated,
            tempfile.TemporaryDirectory() as samples,
        ):
            built = self.build(Path(directory), Path(repeated))
            published, _ = self.publish(built)
            sample_paths = []
            for name, value in (("built", built), ("published", published)):
                path = Path(samples, f"{name}.json")
                path.write_text(json.dumps(value), encoding="utf-8")
                sample_paths.append(path)
            invalid = copy.deepcopy(published)
            del invalid["registry_verification"]["provenance"]["wheel"][
                "cryptographic_verifier"
            ]
            invalid_path = Path(samples, "invalid.json")
            invalid_path.write_text(json.dumps(invalid), encoding="utf-8")
            script = (
                "import json,sys; "
                "from jsonschema import Draft202012Validator; "
                "schema=json.load(open(sys.argv[1], encoding='utf-8')); "
                "Draft202012Validator.check_schema(schema); "
                "validator=Draft202012Validator(schema); "
                "[validator.validate(json.load(open(path, encoding='utf-8'))) "
                "for path in sys.argv[2:]]"
            )
            result = subprocess.run(
                [
                    "uv",
                    "run",
                    "--quiet",
                    "--no-project",
                    "--with",
                    "jsonschema==4.25.1",
                    "python",
                    "-c",
                    script,
                    str(schema),
                    *(str(path) for path in sample_paths),
                ],
                check=False,
                capture_output=True,
                timeout=120,
            )
            invalid_result = subprocess.run(
                [
                    "uv",
                    "run",
                    "--quiet",
                    "--no-project",
                    "--with",
                    "jsonschema==4.25.1",
                    "python",
                    "-c",
                    script,
                    str(schema),
                    str(invalid_path),
                ],
                check=False,
                capture_output=True,
                timeout=120,
            )
        self.assertEqual(result.returncode, 0, result.stderr.decode())
        self.assertNotEqual(invalid_result.returncode, 0)

    def test_registry_digest_mismatch_is_rejected(self) -> None:
        with self.fixture() as directory, self.fixture() as repeated:
            receipt = self.build(Path(directory), Path(repeated))
            release = self.release(receipt)
            release["urls"][0]["digests"]["sha256"] = "0" * 64
            distributions = receipt["distributions"]
            provenances = {
                entry["filename"]: self.provenance(entry["filename"], entry["sha256"])
                for entry in distributions.values()
            }
            evidence = self.installation_evidence(Path(directory), receipt)
            with self.assertRaisesRegex(PythonReceiptError, "SHA-256"):
                verify_registry(
                    receipt,
                    json.dumps(receipt).encode(),
                    release,
                    provenances,
                    "testpypi",
                    evidence,
                    321,
                    "d" * 64,
                )

    def test_unknown_receipt_field_and_missing_installation_evidence_are_rejected(
        self,
    ) -> None:
        with self.fixture() as directory, self.fixture() as repeated:
            receipt = self.build(Path(directory), Path(repeated))
            malformed = copy.deepcopy(receipt)
            malformed["unreviewed"] = True
            with self.assertRaisesRegex(PythonReceiptError, "unknown"):
                validate_receipt(malformed)
            evidence = self.installation_evidence(Path(directory), receipt)
            with self.assertRaisesRegex(PythonReceiptError, "four"):
                verify_registry(
                    receipt,
                    json.dumps(receipt).encode(),
                    self.release(receipt),
                    {},
                    "testpypi",
                    evidence[:-1],
                    321,
                    "d" * 64,
                )

    def test_checked_in_schema_freezes_v2_publisher_identity(self) -> None:
        schema = json.loads(
            (
                ROOT / "docs/release/schema/python-distribution-receipt-v2.schema.json"
            ).read_text(encoding="utf-8")
        )
        self.assertFalse(schema["additionalProperties"])
        self.assertEqual(
            schema["$defs"]["run"]["properties"]["workflow_ref"]["const"],
            WORKFLOW_REF,
        )
        self.assertEqual(
            schema["$defs"]["publisher"]["properties"]["workflow"]["const"],
            "python-publish.yml",
        )
        self.assertEqual(
            schema["$defs"]["distributions"]["properties"]["wheel"]["$ref"],
            "#/$defs/wheel_distribution",
        )
        installation = schema["$defs"]["installation_evidence"]
        self.assertEqual(installation["minItems"], 4)
        self.assertEqual(installation["maxItems"], 4)
        self.assertIn(
            "py3-none-any",
            schema["$defs"]["wheel_distribution"]["allOf"][1]["properties"]["filename"][
                "pattern"
            ],
        )
        description = schema["description"]
        self.assertIn("Structural validation only", description)
        self.assertIn("tools/python_distribution_receipt.py", description)
        self.assertIn("mandatory semantic validator", description)


if __name__ == "__main__":
    unittest.main()
