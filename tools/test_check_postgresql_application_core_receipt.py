# SPDX-License-Identifier: Apache-2.0
from __future__ import annotations

import base64
import copy
import hashlib
import json
import unittest
from pathlib import Path
from unittest import mock

from tools import check_postgresql_application_core_receipt as receipt_checker
from tools.check_postgresql_application_core import GateFailure, validate_profile
from tools.check_postgresql_application_core_receipt import (
    RECEIPT_SCHEMA,
    RECEIPT_EVIDENCE_CLASS,
    build_execution_admission,
    code_authorities,
    expected_observation,
    repository_source_identity,
    validate_oci_container,
    validate_receipt,
)


ROOT = Path(__file__).resolve().parents[1]
PROFILE = ROOT / "conformance/postgresql/application-core-v1.json"
IMAGE_DIGEST = "sha256:" + "b" * 64
MANIFEST_MEDIA_TYPE = "application/vnd.oci.image.manifest.v1+json"
INDEX_MEDIA_TYPE = "application/vnd.oci.image.index.v1+json"
CONFIG_MEDIA_TYPE = "application/vnd.oci.image.config.v1+json"
CHILD_DOCUMENT = json.dumps(
    {
        "schemaVersion": 2,
        "mediaType": MANIFEST_MEDIA_TYPE,
        "config": {"mediaType": CONFIG_MEDIA_TYPE, "digest": IMAGE_DIGEST, "size": 123},
        "layers": [],
    },
    sort_keys=True,
    separators=(",", ":"),
).encode()
CHILD_DIGEST = "sha256:" + hashlib.sha256(CHILD_DOCUMENT).hexdigest()
PLATFORM = {"os": "linux", "architecture": "amd64"}
INDEX_DOCUMENT = json.dumps(
    {
        "schemaVersion": 2,
        "mediaType": INDEX_MEDIA_TYPE,
        "manifests": [
            {
                "mediaType": MANIFEST_MEDIA_TYPE,
                "digest": CHILD_DIGEST,
                "size": len(CHILD_DOCUMENT),
                "platform": PLATFORM,
            }
        ],
    },
    sort_keys=True,
    separators=(",", ":"),
).encode()
INDEX_DIGEST = "sha256:" + hashlib.sha256(INDEX_DOCUMENT).hexdigest()


def expected_container() -> dict:
    return {
        "reference": f"docker.io/library/postgres:test@{INDEX_DIGEST}",
        "manifest_digest": INDEX_DIGEST,
        "registry": "registry-1.docker.io",
        "repository": "library/postgres",
    }


def oci_evidence() -> dict:
    return {
        "reference": expected_container()["reference"],
        "registry": "registry-1.docker.io",
        "repository": "library/postgres",
        "index": {
            "digest": INDEX_DIGEST,
            "media_type": INDEX_MEDIA_TYPE,
            "size": len(INDEX_DOCUMENT),
            "document_base64": base64.b64encode(INDEX_DOCUMENT).decode(),
        },
        "resolved_manifest": {
            "digest": CHILD_DIGEST,
            "media_type": MANIFEST_MEDIA_TYPE,
            "size": len(CHILD_DOCUMENT),
            "document_base64": base64.b64encode(CHILD_DOCUMENT).decode(),
            "config": {
                "digest": IMAGE_DIGEST,
                "media_type": CONFIG_MEDIA_TYPE,
                "size": 123,
            },
        },
        "image_digest": IMAGE_DIGEST,
        "platform": copy.deepcopy(PLATFORM),
    }


class PostgreSQLApplicationCoreReceiptTests(unittest.TestCase):
    def profile(self) -> dict:
        return json.loads(PROFILE.read_text(encoding="utf-8"))

    def receipt(self) -> dict:
        profile = self.profile()
        results = []
        for case in profile["cases"]:
            stdout = f"ok:{case['id']}\n"
            results.append(
                {
                    "id": case["id"],
                    "status": "observed-passed",
                    "exit_code": 0,
                    "oracle_sha256": case["oracle_sha256"],
                    "stdout": stdout,
                    "stdout_sha256": hashlib.sha256(stdout.encode()).hexdigest(),
                    "diagnostic": "",
                    "diagnostic_sha256": hashlib.sha256(b"").hexdigest(),
                }
            )
        return {
            "schema": RECEIPT_SCHEMA,
            "status": "observed-passed",
            "profile_sha256": hashlib.sha256(PROFILE.read_bytes()).hexdigest(),
            "postgresql_version": profile["postgresql"]["version"],
            "evidence_class": RECEIPT_EVIDENCE_CLASS,
            "claim_authority": False,
            "claims": [],
            "closure_declared": False,
            "execution_admission": build_execution_admission("Hyphae test operator", True),
            "source": repository_source_identity(ROOT),
            "code_authorities": code_authorities(ROOT),
            "host": {"os": "linux", "architecture": "x86_64"},
            "container": oci_evidence(),
            "configuration": copy.deepcopy(profile["postgresql"]["configuration"]),
            "observed_configuration": expected_observation(profile["postgresql"]),
            "case_count": len(results),
            "passed_count": len(results),
            "profile_claim_status": "blocked",
            "cleanup": "container-removed",
            "results": results,
        }

    def validate(self, receipt: dict) -> dict[str, object]:
        with mock.patch.object(
            receipt_checker,
            "validate_oci_container",
            return_value=(CHILD_DIGEST, IMAGE_DIGEST),
        ):
            return validate_receipt(ROOT, PROFILE, receipt)

    def test_synthetic_markers_validate_only_as_nonclaim_observation(self) -> None:
        audit = self.validate(self.receipt())

        self.assertEqual(audit["status"], "validated-non-claim-authoritative")
        self.assertEqual(audit["observation_status"], "observed-passed")
        self.assertFalse(audit["claim_authority"])
        self.assertEqual(audit["claims"], [])
        self.assertFalse(audit["closure_declared"])
        self.assertNotIn("parity_eligible", audit)
        self.assertFalse(validate_profile(ROOT, self.profile())["parity_eligible"])
        self.assertEqual(audit["passed_count"], 11)
        self.assertEqual(audit["resolved_manifest_digest"], CHILD_DIGEST)
        self.assertEqual(audit["image_digest"], IMAGE_DIGEST)

    def test_consistent_failed_oracle_receipt_is_valid_evidence_of_failure(self) -> None:
        receipt = self.receipt()
        failed = receipt["results"][0]
        failed["status"] = "observed-failed"
        failed["exit_code"] = 3
        failed["stdout"] = ""
        failed["stdout_sha256"] = hashlib.sha256(b"").hexdigest()
        failed["diagnostic"] = "oracle failed"
        failed["diagnostic_sha256"] = hashlib.sha256(b"oracle failed").hexdigest()
        receipt["status"] = "observed-failed"
        receipt["passed_count"] = 10

        audit = self.validate(receipt)
        self.assertEqual(audit["observation_status"], "observed-failed")

    def test_oci_documents_authenticate_membership_platform_and_image_config(self) -> None:
        child, image = validate_oci_container(oci_evidence(), expected_container())

        self.assertEqual(child, CHILD_DIGEST)
        self.assertEqual(image, IMAGE_DIGEST)

    def test_oci_document_tampering_and_platform_mismatch_fail_closed(self) -> None:
        mutations = []

        def mutate_index_bytes(evidence: dict) -> None:
            evidence["index"]["document_base64"] = base64.b64encode(b"{}").decode()

        mutations.append(("index bytes", mutate_index_bytes, "bytes do not match"))

        def mutate_child_bytes(evidence: dict) -> None:
            evidence["resolved_manifest"]["document_base64"] = base64.b64encode(b"{}").decode()

        mutations.append(("child bytes", mutate_child_bytes, "bytes do not match"))

        def mutate_platform(evidence: dict) -> None:
            evidence["platform"]["architecture"] = "arm64"

        mutations.append(("platform", mutate_platform, "descriptor differs"))

        def mutate_child_digest(evidence: dict) -> None:
            evidence["resolved_manifest"]["digest"] = "sha256:" + "c" * 64

        mutations.append(("membership", mutate_child_digest, "not a unique member"))

        def mutate_config_digest(evidence: dict) -> None:
            evidence["resolved_manifest"]["config"]["digest"] = "sha256:" + "c" * 64

        mutations.append(("config", mutate_config_digest, "config is not bound"))

        def mutate_image_digest(evidence: dict) -> None:
            evidence["image_digest"] = "sha256:" + "c" * 64

        mutations.append(("image", mutate_image_digest, "config is not bound"))

        for label, mutate, message in mutations:
            with self.subTest(tampering=label):
                evidence = oci_evidence()
                mutate(evidence)
                with self.assertRaisesRegex(GateFailure, message):
                    validate_oci_container(evidence, expected_container())

    def test_receipt_tampering_fails_closed(self) -> None:
        mutations = []

        def mutate_profile_digest(receipt: dict) -> None:
            receipt["profile_sha256"] = "0" * 64

        mutations.append(("profile digest", mutate_profile_digest, "profile binding"))

        def grant_claim_authority(receipt: dict) -> None:
            receipt["claim_authority"] = True

        mutations.append(("claim authority", grant_claim_authority, "identity, profile binding"))

        def declare_closure(receipt: dict) -> None:
            receipt["closure_declared"] = True

        mutations.append(("closure", declare_closure, "identity, profile binding"))

        def mutate_admission(receipt: dict) -> None:
            receipt["execution_admission"]["claim_authority"] = True

        mutations.append(("operator admission", mutate_admission, "execution admission"))

        def mutate_observed_setting(receipt: dict) -> None:
            receipt["observed_configuration"]["settings"]["fsync"] = "off"

        mutations.append(("observed setting", mutate_observed_setting, "observed PostgreSQL"))

        def mutate_expected_setting(receipt: dict) -> None:
            receipt["configuration"]["settings"]["fsync"] = "off"

        mutations.append(("configuration", mutate_expected_setting, "configuration differs"))

        def mutate_source(receipt: dict) -> None:
            receipt["source"]["tree"] = "0" * 40

        mutations.append(("source tree", mutate_source, "source identity differs"))

        for authority in ["runner", "profile_checker", "receipt_validator"]:
            def mutate_code(receipt: dict, name: str = authority) -> None:
                receipt["code_authorities"][name]["sha256"] = "0" * 64

            mutations.append(
                (f"{authority} bytes", mutate_code, "runner or checker bytes differ")
            )

        def mutate_oracle(receipt: dict) -> None:
            receipt["results"][0]["oracle_sha256"] = "0" * 64

        mutations.append(("oracle bytes", mutate_oracle, "malformed or tampered"))

        def mutate_stdout(receipt: dict) -> None:
            receipt["results"][0]["stdout"] = "tampered\n"

        mutations.append(("stdout", mutate_stdout, "malformed or tampered"))

        def mutate_diagnostic(receipt: dict) -> None:
            receipt["results"][0]["diagnostic"] = "tampered"

        mutations.append(("diagnostic", mutate_diagnostic, "malformed or tampered"))

        def mutate_result_status(receipt: dict) -> None:
            receipt["results"][0]["status"] = "failed"

        mutations.append(("result status", mutate_result_status, "status is inconsistent"))

        def mutate_count(receipt: dict) -> None:
            receipt["passed_count"] = 10

        mutations.append(("count", mutate_count, "counts or status"))

        def mutate_cleanup(receipt: dict) -> None:
            receipt["cleanup"] = "pending"

        mutations.append(("cleanup", mutate_cleanup, "cleanup is invalid"))

        def add_field(receipt: dict) -> None:
            receipt["unreviewed"] = True

        mutations.append(("extra field", add_field, "unknown or missing fields"))

        for label, mutate, message in mutations:
            with self.subTest(tampering=label):
                receipt = self.receipt()
                mutate(receipt)
                with self.assertRaisesRegex(GateFailure, message):
                    self.validate(receipt)


if __name__ == "__main__":
    unittest.main()
