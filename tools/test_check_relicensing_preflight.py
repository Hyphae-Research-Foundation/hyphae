#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

from __future__ import annotations

import copy
import hashlib
import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from tools.check_relicensing_preflight import (
    CONTRACT_PATH,
    DEPENDABOT_REVIEW_PATH,
    DEPENDENCY_RECEIPT_PATH,
    EXPECTED_GENERATED_COPIES,
    MAX_CONTRACT_BYTES,
    REPRESENTATIVE_ATTESTATION_PATH,
    REPOSITORY_AUDIT_PATH,
    ROOT,
    _git_read,
    _git_read_unbounded,
    classify_path,
    load_contract,
    repository_paths,
    validate_contract,
    validate_historical_release_evidence,
    validate_preflight_evidence,
)


class RelicensingPreflightTests(unittest.TestCase):
    def setUp(self) -> None:
        self.contract = load_contract(CONTRACT_PATH)

    def validate(self, document: dict[str, object]) -> list[str]:
        return validate_contract(document, repository_paths(ROOT)).failures

    def materialize_contract_tree(self, root: Path) -> None:
        for entry in EXPECTED_GENERATED_COPIES:
            source = root / entry["source"]
            destination = root / entry["copy"]
            if entry["mode"] == "packaged-subset":
                source.mkdir(parents=True, exist_ok=True)
                destination.mkdir(parents=True, exist_ok=True)
                (source / "contract.json").write_text("{}\n", encoding="utf-8")
                (destination / "contract.json").write_text("{}\n", encoding="utf-8")
            else:
                source.parent.mkdir(parents=True, exist_ok=True)
                destination.parent.mkdir(parents=True, exist_ok=True)
                source.write_text("{}\n", encoding="utf-8")
                destination.write_text("{}\n", encoding="utf-8")
        for category in self.contract["preflight"]["evidence_categories"]:
            for path in category["evidence"]:
                evidence_path = root / path
                evidence_path.parent.mkdir(parents=True, exist_ok=True)
                evidence_path.write_text("evidence\n", encoding="utf-8")

    def validate_evidence_documents(
        self, mutations: dict[str, dict[str, object]]
    ) -> list[str]:
        paths = (
            DEPENDENCY_RECEIPT_PATH,
            REPOSITORY_AUDIT_PATH,
            REPRESENTATIVE_ATTESTATION_PATH,
            DEPENDABOT_REVIEW_PATH,
        )
        documents = {
            path: json.loads((ROOT / path).read_text(encoding="utf-8"))
            for path in paths
        }
        documents.update(mutations)
        with patch(
            "tools.check_relicensing_preflight._load_json_evidence",
            side_effect=lambda _root, relative: copy.deepcopy(documents[relative]),
        ), patch(
            "tools.check_dependency_license_aggregate.validate_aggregate",
            return_value=[],
        ), patch(
            "tools.check_relicensing_transition.validate_current_transition_content",
            return_value=[],
        ):
            return validate_preflight_evidence(ROOT)

    def test_current_repository_contract_passes_as_effective(self) -> None:
        result = validate_contract(self.contract, repository_paths(ROOT), ROOT)
        self.assertEqual(result.failures, [])
        self.assertEqual(result.blockers, [])

    def test_historical_tags_match_the_frozen_apache_evidence(self) -> None:
        self.assertEqual(validate_historical_release_evidence(ROOT), [])

    def test_source_bound_preflight_evidence_is_accepted(self) -> None:
        self.assertEqual(validate_preflight_evidence(ROOT), [])

    def test_unknown_top_level_and_nested_keys_fail_closed(self) -> None:
        top_level = copy.deepcopy(self.contract)
        top_level["unexpected"] = True
        self.assertTrue(any("unknown keys" in error for error in self.validate(top_level)))

        nested = copy.deepcopy(self.contract)
        nested["preflight"]["evidence_categories"][0]["receipt"] = "invented"
        self.assertTrue(any("unknown keys" in error for error in self.validate(nested)))

        generated = copy.deepcopy(self.contract)
        generated["classification"]["generated_copies"][0]["unknown"] = True
        self.assertTrue(any("unknown keys" in error for error in self.validate(generated)))

        malformed = copy.deepcopy(self.contract)
        malformed["classification"]["rules"][0]["match"] = "not-an-object"
        self.assertTrue(any("must be an object" in error for error in self.validate(malformed)))

        wrong_type = copy.deepcopy(self.contract)
        wrong_type["classification"]["rules"][0]["category"] = []
        self.assertTrue(any("unknown category" in error for error in self.validate(wrong_type)))

        preflight_type = copy.deepcopy(self.contract)
        preflight_type["preflight"] = []
        self.assertTrue(any("must be an object" in error for error in self.validate(preflight_type)))

        trademark_type = copy.deepcopy(self.contract)
        trademark_type["boundaries"]["trademarks"]["reserved_asset_paths"] = "logo"
        self.assertTrue(any("must be an array" in error for error in self.validate(trademark_type)))

    def test_schema_identifiers_release_and_history_are_frozen(self) -> None:
        mutations = []
        schema = copy.deepcopy(self.contract)
        schema["schema"] = "hyphae-relicensing-classification-v2"
        mutations.append(schema)
        release = copy.deepcopy(self.contract)
        release["target_release"] = "1.2.1"
        mutations.append(release)
        history = copy.deepcopy(self.contract)
        history["historical_releases"][1]["software"] = "Apache-2.0"
        mutations.append(history)
        category = copy.deepcopy(self.contract)
        category["categories"][0]["id"] = "Software"
        mutations.append(category)
        for mutation in mutations:
            with self.subTest(mutation=mutation):
                self.assertNotEqual(self.validate(mutation), [])

    def test_loader_rejects_recursive_duplicates_nonstandard_json_and_oversize(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            duplicate = root / "duplicate.json"
            duplicate.write_text(
                '{"outer":{"license":"Apache-2.0","license":"CC0-1.0"}}',
                encoding="utf-8",
            )
            with self.assertRaisesRegex(ValueError, "duplicate JSON key: license"):
                load_contract(duplicate)

            nonstandard = root / "nonstandard.json"
            nonstandard.write_text('{"value":NaN}', encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "nonstandard JSON constant: NaN"):
                load_contract(nonstandard)

            invalid_utf8 = root / "invalid-utf8.json"
            invalid_utf8.write_bytes(b'{"value":"\xff"}')
            with self.assertRaises(UnicodeDecodeError):
                load_contract(invalid_utf8)

            oversize = root / "oversize.json"
            oversize.write_bytes(b" " * (MAX_CONTRACT_BYTES + 1))
            with self.assertRaisesRegex(ValueError, "exceeds 1048576 bytes"):
                load_contract(oversize)

    def test_precedence_and_unclassified_paths_fail_closed(self) -> None:
        tied = copy.deepcopy(self.contract)
        tied["classification"]["rules"][1]["priority"] = tied["classification"][
            "rules"
        ][0]["priority"]
        self.assertTrue(any("duplicate priority" in error for error in self.validate(tied)))

        expanded = copy.deepcopy(self.contract)
        packaged_rule = next(
            rule
            for rule in expanded["classification"]["rules"]
            if rule["id"] == "packaged-public-contract-copies"
        )
        packaged_rule["match"]["paths"].append("unreviewed-contracts/")
        self.assertTrue(
            any("frozen path rules differ" in error for error in self.validate(expanded))
        )

        rules = self.contract["classification"]["rules"]
        category, ties = classify_path("crates/example/README.md", rules)
        self.assertEqual(category, "narrative-documentation")
        self.assertEqual(ties, [])

        result = validate_contract(self.contract, ["unbounded-new-root-file"])
        self.assertTrue(any("unclassified" in error for error in result.failures))

    def test_normative_roots_machine_contracts_and_readmes_are_exact(self) -> None:
        rules = self.contract["classification"]["rules"]
        expected = {
            "DCO": "third-party-material",
            "NOTICE": "software",
            "THIRD_PARTY_NOTICES.md": "software",
            "crates/hyphae-core/THIRD_PARTY_NOTICES.md": "software",
            "contracts/openapi/hyphae-v2.yaml": "normative-specification",
            "crates/hyphae-contracts/assets/openapi/hyphae-v2.yaml": (
                "normative-specification"
            ),
            "docs/native/types-v1.md": "normative-specification",
            "docs/gates/native-local-phase-1.md": "normative-specification",
            "docs/security/threat-model.md": "narrative-documentation",
            "docs/security/server-threat-model.md": "narrative-documentation",
            "docs/security/native-access-control-threat-model.md": (
                "normative-specification"
            ),
            "docs/gates/evidence/receipt.json": "software",
            "docs/gates/evidence/receipt.md": "narrative-documentation",
            "docs/release/receipts/release.json": "software",
            "docs/release/receipts/release.md": "narrative-documentation",
            "crates/hyphae-core/README.md": "narrative-documentation",
            "crates/hyphae-native-product/assets/product-error-v1.md": (
                "normative-specification"
            ),
            "config/native-specification-profile.json": "software",
        }
        for path, expected_category in expected.items():
            with self.subTest(path=path):
                category, ties = classify_path(path, rules)
                self.assertEqual(ties, [])
                self.assertEqual(category, expected_category)

    def test_canonical_dco_is_exact_third_party_material(self) -> None:
        rules = self.contract["classification"]["rules"]
        category, ties = classify_path("DCO", rules)
        self.assertEqual(ties, [])
        self.assertEqual(category, "third-party-material")
        self.assertEqual(
            self.contract["boundaries"]["third_party"]["exact_paths"],
            ["DCO", "THIRD_PARTY_LICENSES.txt"],
        )

    def test_generated_contract_copies_inherit_apache_and_exact_bytes(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.materialize_contract_tree(root)

            result = validate_contract(self.contract, [], root)
            self.assertEqual(result.failures, [])

            copied = root / EXPECTED_GENERATED_COPIES[0]["copy"] / "contract.json"
            copied.write_text('{"mutated":true}\n', encoding="utf-8")
            result = validate_contract(self.contract, [], root)
            self.assertTrue(any("copy bytes differ" in error for error in result.failures))

    def test_generated_contract_copies_reject_symlink_roots_and_files(self) -> None:
        cases = (
            ("exact source", 2, "source", None),
            ("exact copy", 2, "copy", None),
            ("directory source", 0, "source", None),
            ("directory copy", 0, "copy", None),
            ("source file", 0, "source", "contract.json"),
            ("copied file", 0, "copy", "contract.json"),
        )
        for name, entry_index, side, child in cases:
            with self.subTest(name=name), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                self.materialize_contract_tree(root)
                endpoint = root / EXPECTED_GENERATED_COPIES[entry_index][side]
                if child is not None:
                    endpoint /= child
                real = endpoint.with_name(f"{endpoint.name}-real")
                endpoint.rename(real)
                endpoint.symlink_to(real, target_is_directory=real.is_dir())

                result = validate_contract(self.contract, [], root)
                self.assertTrue(
                    any("symlink" in error for error in result.failures),
                    result.failures,
                )

    def test_generated_contract_copies_reject_symlink_path_components(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.materialize_contract_tree(root)
            contracts = root / "contracts"
            real = root / "contracts-real"
            contracts.rename(real)
            contracts.symlink_to(real, target_is_directory=True)

            result = validate_contract(self.contract, [], root)
            self.assertTrue(
                any("symlink" in error for error in result.failures),
                result.failures,
            )

    def test_accepted_legal_evidence_cannot_be_reopened_silently(self) -> None:
        false_claim = copy.deepcopy(self.contract)
        false_claim["preflight"]["evidence_categories"][0]["status"] = (
            "prepared-unverified"
        )
        self.assertTrue(
            any("truthful frozen statuses" in error for error in self.validate(false_claim))
        )

        complete = copy.deepcopy(self.contract)
        complete["preflight"]["overall_status"] = "blocked"
        complete["preflight"]["completion_claim"] = False
        self.assertTrue(
            any("accepted completion state" in error for error in self.validate(complete))
        )

    def test_dependency_receipt_fails_closed_on_digest_drift(self) -> None:
        receipt_path = ROOT / (
            "docs/gates/evidence/relicensing-1.2.0-dependencies-fcf2f918.json"
        )
        original = receipt_path.read_text(encoding="utf-8")
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            evidence = root / "docs" / "gates" / "evidence"
            evidence.mkdir(parents=True)
            for name in (
                "relicensing-1.2.0-dependencies-fcf2f918.json",
                "relicensing-1.2.0-dependency-license-aggregate.json",
                "relicensing-1.2.0-repository-audit-fcf2f918.json",
                "relicensing-1.2.0-representative-attestation.json",
                "relicensing-1.2.0-dependabot-review.json",
            ):
                (evidence / name).write_bytes(
                    (ROOT / "docs" / "gates" / "evidence" / name).read_bytes()
                )
            dependency = evidence / receipt_path.name
            document = json.loads(original)
            document["inventory"]["canonical_sha256"] = "0" * 64
            dependency.write_text(
                json.dumps(document),
                encoding="utf-8",
            )
            with patch(
                "tools.check_relicensing_preflight._git_read",
                side_effect=lambda _root, *arguments: _git_read(ROOT, *arguments),
            ), patch(
                "tools.check_relicensing_preflight._git_read_unbounded",
                side_effect=lambda _root, *arguments: _git_read_unbounded(
                    ROOT, *arguments
                ),
            ):
                failures = validate_preflight_evidence(root)
            self.assertTrue(any("digest differs" in error for error in failures))

    def test_legal_evidence_must_remain_bound_to_the_exact_base(self) -> None:
        cases = (
            (REPOSITORY_AUDIT_PATH, "audit differs from exact legal base"),
            (
                REPRESENTATIVE_ATTESTATION_PATH,
                "attestation differs from exact legal base",
            ),
        )
        for path, expected in cases:
            with self.subTest(path=path):
                document = json.loads((ROOT / path).read_text(encoding="utf-8"))
                current = json.loads(
                    (ROOT / DEPENDENCY_RECEIPT_PATH).read_text(encoding="utf-8")
                )["source"]
                document["source"]["commit"] = current["commit"]
                document["source"]["tree"] = current["tree"]
                failures = self.validate_evidence_documents({path: document})
                self.assertTrue(any(expected in error for error in failures))

    def test_dependabot_review_uses_only_its_legal_base_history(self) -> None:
        calls: list[tuple[str, ...]] = []

        def git_read(root: Path, *arguments: str) -> bytes:
            calls.append(arguments)
            return _git_read(root, *arguments)

        with patch(
            "tools.check_relicensing_preflight._git_read", side_effect=git_read
        ):
            self.assertEqual(self.validate_evidence_documents({}), [])
        log_calls = [arguments for arguments in calls if arguments[:1] == ("log",)]
        self.assertEqual(len(log_calls), 1)
        self.assertEqual(
            log_calls[0][1],
            json.loads((ROOT / DEPENDABOT_REVIEW_PATH).read_text(encoding="utf-8"))[
                "source"
            ]["commit"],
        )
        self.assertNotIn("--all", log_calls[0])

    def test_dependabot_review_method_and_digest_fail_closed(self) -> None:
        original = json.loads(
            (ROOT / DEPENDABOT_REVIEW_PATH).read_text(encoding="utf-8")
        )
        cases = (
            ("scope", "All Dependabot commits in every local ref", "review method differs"),
            ("ordered_commit_ids_sha256", "not-a-digest", "invalid SHA-256"),
        )
        for field, value, expected in cases:
            with self.subTest(field=field):
                review = copy.deepcopy(original)
                review["method"][field] = value
                failures = self.validate_evidence_documents(
                    {DEPENDABOT_REVIEW_PATH: review}
                )
                self.assertTrue(any(expected in error for error in failures), failures)

        review = copy.deepcopy(original)
        current = json.loads(
            (ROOT / DEPENDENCY_RECEIPT_PATH).read_text(encoding="utf-8")
        )["source"]
        review["source"]["commit"] = current["commit"]
        review["source"]["tree"] = current["tree"]
        failures = self.validate_evidence_documents({DEPENDABOT_REVIEW_PATH: review})
        self.assertTrue(
            any("Dependabot review differs from exact legal base" in error for error in failures),
            failures,
        )

    def test_dependency_receipt_rejects_unrelated_commit_and_tree(self) -> None:
        original = json.loads(
            (ROOT / DEPENDENCY_RECEIPT_PATH).read_text(encoding="utf-8")
        )
        cases = (
            (
                "commit",
                "e88f2ea2c3455a393e3ac0cd69e25486cc26888e",
                "c131ab057c8ab05ed2e2389954f0e8145a71dbdb",
                "must descend from the legal base",
            ),
            (
                "tree",
                original["source"]["commit"],
                "51b283d27d0c0f5d194680de1d3e273b57f2ff95",
                "does not match source commit",
            ),
        )
        for name, commit, tree, expected in cases:
            with self.subTest(name=name):
                receipt = copy.deepcopy(original)
                receipt["source"]["commit"] = commit
                receipt["source"]["tree"] = tree
                failures = self.validate_evidence_documents(
                    {DEPENDENCY_RECEIPT_PATH: receipt}
                )
                self.assertTrue(any(expected in error for error in failures), failures)

    def test_dependency_receipt_rejects_stale_current_cargo_inputs(self) -> None:
        receipt = json.loads(
            (ROOT / DEPENDENCY_RECEIPT_PATH).read_text(encoding="utf-8")
        )
        receipt["source_inputs"][0]["sha256"] = "0" * 64
        receipt["source"]["source_inputs_sha256"] = hashlib.sha256(
            json.dumps(
                receipt["source_inputs"],
                ensure_ascii=True,
                separators=(",", ":"),
                sort_keys=True,
            ).encode("utf-8")
        ).hexdigest()
        failures = self.validate_evidence_documents({DEPENDENCY_RECEIPT_PATH: receipt})
        self.assertTrue(
            any(
                "current Cargo input set or digest differs" in error
                for error in failures
            )
        )

    def test_dependency_receipt_rejects_false_clean_claim(self) -> None:
        receipt = json.loads(
            (ROOT / DEPENDENCY_RECEIPT_PATH).read_text(encoding="utf-8")
        )
        receipt["source"]["mode"] = "clean-commit"
        receipt["source"]["worktree_clean"] = True

        def git_read(root: Path, *arguments: str) -> bytes:
            if arguments == (
                "status",
                "--porcelain=v1",
                "--untracked-files=all",
            ):
                return b" M Cargo.toml\n"
            return _git_read(root, *arguments)

        with patch(
            "tools.check_relicensing_preflight._git_read", side_effect=git_read
        ):
            failures = self.validate_evidence_documents(
                {DEPENDENCY_RECEIPT_PATH: receipt}
            )
        self.assertTrue(
            any("clean source claim is false" in error for error in failures)
        )

    def test_preflight_rejects_stale_transition_content(self) -> None:
        with patch(
            "tools.check_relicensing_transition.validate_current_transition_content",
            return_value=["transition receipt content digest differs"],
        ):
            failures = validate_preflight_evidence(ROOT)
        self.assertIn("transition receipt content digest differs", failures)

    def test_each_decisive_attestation_boolean_fails_closed_when_false(self) -> None:
        source = ROOT / "docs/gates/evidence"
        mutations = (
            ("statements_prepared_for_attestation", "transition_authorization", "confirmed"),
            ("statements_prepared_for_attestation", "copyright_authority", "confirmed"),
            ("statements_prepared_for_attestation", "counsel_approval", "qualified_open_source_counsel"),
            ("statements_prepared_for_attestation", "counsel_approval", "written_approval_received"),
            ("statements_prepared_for_attestation", "counsel_approval", "confidential_record_not_embedded"),
            ("statements_prepared_for_attestation", "prior_commitments", "absence_confirmed"),
            ("identity_dispositions", "ec2_user_commits", "authority_confirmed"),
            ("identity_dispositions", "dependabot_commits", "authority_confirmed"),
        )
        for section, item, field in mutations:
            with self.subTest(field=f"{section}.{item}.{field}"), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                target = root / "docs/gates/evidence"
                target.mkdir(parents=True)
                for path in (
                    "relicensing-1.2.0-dependencies-fcf2f918.json",
                    "relicensing-1.2.0-dependency-license-aggregate.json",
                    "relicensing-1.2.0-repository-audit-fcf2f918.json",
                    "relicensing-1.2.0-representative-attestation.json",
                    "relicensing-1.2.0-dependabot-review.json",
                ):
                    (target / path).write_bytes((source / path).read_bytes())
                attestation_path = target / "relicensing-1.2.0-representative-attestation.json"
                attestation = json.loads(attestation_path.read_text(encoding="utf-8"))
                attestation[section][item][field] = False
                attestation_path.write_text(json.dumps(attestation), encoding="utf-8")
                with patch(
                    "tools.check_relicensing_preflight._git_read",
                    side_effect=lambda _root, *arguments: _git_read(ROOT, *arguments),
                ), patch(
                    "tools.check_relicensing_preflight._git_read_unbounded",
                    side_effect=lambda _root, *arguments: _git_read_unbounded(ROOT, *arguments),
                ):
                    failures = validate_preflight_evidence(root)
                self.assertTrue(any("must be explicitly true" in error for error in failures))


if __name__ == "__main__":
    unittest.main()
