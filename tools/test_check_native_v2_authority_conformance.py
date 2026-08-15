#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""Tests for the managed Native v2 authority conformance corpus."""

from __future__ import annotations

import copy
import json
import tempfile
import unittest
from pathlib import Path

from tools.check_native_v2_authority_conformance import (
    AuthorityConformanceError,
    CORPUS,
    ROOT,
    validate,
)


CONTRACT = ROOT / "contracts/native-access-control-v1.json"


def payload() -> dict:
    return json.loads(CORPUS.read_text(encoding="utf-8"))


class NativeV2AuthorityConformanceTests(unittest.TestCase):
    def test_checked_in_corpus_is_complete_and_source_bound(self) -> None:
        result = validate(payload(), CONTRACT, ROOT)
        self.assertEqual(result["status"], "passed")
        self.assertEqual(result["operations"], 12)
        self.assertEqual(result["authentication_denials"], 5)
        self.assertEqual(result["evidence_rows"], 9)

    def test_operation_drift_from_access_control_contract_fails_closed(self) -> None:
        corpus = payload()
        row = next(
            item for item in corpus["operations"] if item["id"] == "security.audit_read"
        )
        row["allowed_roles"].remove("operator")
        row["denied_roles"].append("operator")
        row["denied_roles"].sort()
        with self.assertRaisesRegex(AuthorityConformanceError, "operation matrix"):
            validate(corpus, CONTRACT, ROOT)

    def test_unknown_or_missing_security_operation_fails_closed(self) -> None:
        for mutate in (
            lambda rows: rows.pop(),
            lambda rows: rows.append(
                {
                    "id": "security.future",
                    "source_variant": "SecurityFuture",
                    "kind": "read",
                    "minimum_minor": 1,
                    "permission": "security.read",
                    "scope": "instance",
                    "allowed_roles": ["owner"],
                    "denied_roles": [
                        "admin",
                        "auditor",
                        "developer",
                        "operator",
                        "reader",
                        "writer",
                    ],
                }
            ),
        ):
            with self.subTest(mutate=mutate):
                corpus = payload()
                mutate(corpus["operations"])
                corpus["operations"].sort(key=lambda row: row["id"])
                with self.assertRaisesRegex(
                    AuthorityConformanceError, "exact twelve operations"
                ):
                    validate(corpus, CONTRACT, ROOT)

    def test_authentication_denials_are_uniform_and_digest_bound(self) -> None:
        corpus = payload()
        corpus["authentication_denial"][0]["error"]["message"] = "credential absent"
        with self.assertRaisesRegex(AuthorityConformanceError, "uniform"):
            validate(corpus, CONTRACT, ROOT)

        corpus = payload()
        corpus["digests"]["authentication_error_sha256"] = "0" * 64
        with self.assertRaisesRegex(AuthorityConformanceError, "authentication digest"):
            validate(corpus, CONTRACT, ROOT)

    def test_minor_rejections_must_precede_dispatch(self) -> None:
        corpus = payload()
        corpus["protocol"]["rejections_before_dispatch"][1]["dispatch_reached"] = True
        with self.assertRaisesRegex(AuthorityConformanceError, "before dispatch"):
            validate(corpus, CONTRACT, ROOT)

    def test_protocol_source_minor_drift_fails_closed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for relative in (
                "crates/hyphae-native-protocol/src/handshake.rs",
                "crates/hyphae-native-protocol/src/product.rs",
                "crates/hyphae-native-protocol/tests/golden_vectors.rs",
                "crates/hyphae-native-product/src/error.rs",
            ):
                target = root / relative
                target.parent.mkdir(parents=True, exist_ok=True)
                source = (ROOT / relative).read_text(encoding="utf-8")
                if relative.endswith("handshake.rs"):
                    source = source.replace(
                        "pub const PROTOCOL_MINOR: u16 = 2;",
                        "pub const PROTOCOL_MINOR: u16 = 3;",
                    )
                target.write_text(source, encoding="utf-8")
            with self.assertRaisesRegex(
                AuthorityConformanceError, "protocol version"
            ):
                validate(payload(), CONTRACT, root)

    def test_cursor_epoch_and_limits_are_normative(self) -> None:
        corpus = payload()
        corpus["pagination"]["metadata"]["cursor"] = "opaque-offset"
        with self.assertRaisesRegex(AuthorityConformanceError, "metadata pagination"):
            validate(corpus, CONTRACT, ROOT)

        corpus = payload()
        corpus["limits"]["security_result_rows"] += 1
        with self.assertRaisesRegex(AuthorityConformanceError, "limits"):
            validate(corpus, CONTRACT, ROOT)

    def test_redaction_cannot_drop_a_secret_bearing_field(self) -> None:
        corpus = payload()
        corpus["redaction"]["forbidden_fields"].remove("verifier")
        with self.assertRaisesRegex(AuthorityConformanceError, "redaction"):
            validate(corpus, CONTRACT, ROOT)

    def test_section_and_contract_digests_detect_mutation(self) -> None:
        corpus = payload()
        corpus["role_matrix"]["reader"]["read"] = "allow"
        with self.assertRaisesRegex(AuthorityConformanceError, "role matrix"):
            validate(corpus, CONTRACT, ROOT)

        corpus = payload()
        corpus["digests"]["authority_contract_sha256"] = "f" * 64
        with self.assertRaisesRegex(AuthorityConformanceError, "contract digest"):
            validate(corpus, CONTRACT, ROOT)

    def test_evidence_anchors_and_requirement_coverage_fail_closed(self) -> None:
        corpus = payload()
        corpus["evidence"][0]["anchors"] = ["not_a_real_test_anchor"]
        with self.assertRaisesRegex(AuthorityConformanceError, "evidence anchor"):
            validate(corpus, CONTRACT, ROOT)

        corpus = payload()
        corpus["evidence"][0]["covers"] = []
        with self.assertRaisesRegex(AuthorityConformanceError, "coverage"):
            validate(corpus, CONTRACT, ROOT)

    def test_role_matrix_requires_the_exact_write_plane_evidence(self) -> None:
        corpus = payload()
        role_evidence = next(
            row for row in corpus["evidence"] if "roles.allow-deny" in row["covers"]
        )
        role_evidence.update(
            {
                "anchors": [
                    "exact_instance_permissions_partition_the_managed_read_plane"
                ],
                "command": (
                    "cargo test --locked -p hyphae-native-product "
                    "--test security_read_plane "
                    "exact_instance_permissions_partition_the_managed_read_plane"
                ),
                "source": (
                    "crates/hyphae-native-product/tests/security_read_plane.rs"
                ),
            }
        )
        with self.assertRaisesRegex(AuthorityConformanceError, "role matrix evidence"):
            validate(corpus, CONTRACT, ROOT)

    def test_python_managed_live_evidence_is_fixed_and_cross_platform(self) -> None:
        corpus = payload()
        evidence = next(
            row for row in corpus["evidence"] if row["id"] == "python-managed-live"
        )
        evidence["platforms"].remove("windows")
        with self.assertRaisesRegex(
            AuthorityConformanceError, "Python managed live evidence"
        ):
            validate(corpus, CONTRACT, ROOT)

        corpus = payload()
        evidence = next(
            row for row in corpus["evidence"] if row["id"] == "python-managed-live"
        )
        evidence["command"] = "cargo test --locked -p hyphae-client"
        with self.assertRaisesRegex(
            AuthorityConformanceError, "Python managed live evidence"
        ):
            validate(corpus, CONTRACT, ROOT)

    def test_unexpected_fields_fail_closed(self) -> None:
        corpus = copy.deepcopy(payload())
        corpus["escape_hatch"] = True
        with self.assertRaisesRegex(AuthorityConformanceError, "corpus fields"):
            validate(corpus, CONTRACT, ROOT)


if __name__ == "__main__":
    unittest.main()
