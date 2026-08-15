#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only

from __future__ import annotations

import copy
import tempfile
import unittest
from pathlib import Path

from tools.check_relicensing_preflight import (
    CONTRACT_PATH,
    EXPECTED_GENERATED_COPIES,
    MAX_CONTRACT_BYTES,
    ROOT,
    classify_path,
    load_contract,
    repository_paths,
    validate_contract,
    validate_historical_release_evidence,
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

    def test_current_repository_contract_passes_with_truthful_blockers(self) -> None:
        result = validate_contract(self.contract, repository_paths(ROOT), ROOT)
        self.assertEqual(result.failures, [])
        self.assertEqual(
            result.blockers,
            [
                "counsel-approval",
                "copyright-relicensing-authority",
                "prior-commitments",
                "dependency-license-exact-sha",
                "contribution-governance",
            ],
        )

    def test_historical_tags_match_the_frozen_apache_evidence(self) -> None:
        self.assertEqual(validate_historical_release_evidence(ROOT), [])

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
            "NOTICE": "software",
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

    def test_only_specification_decision_may_be_closed(self) -> None:
        false_claim = copy.deepcopy(self.contract)
        false_claim["preflight"]["evidence_categories"][0]["status"] = (
            "accepted-decision"
        )
        false_claim["preflight"]["evidence_categories"][0]["evidence"] = [
            "docs/fake-counsel-receipt.md"
        ]
        self.assertTrue(
            any("truthful frozen statuses" in error for error in self.validate(false_claim))
        )

        complete = copy.deepcopy(self.contract)
        complete["preflight"]["overall_status"] = "complete"
        complete["preflight"]["completion_claim"] = True
        self.assertTrue(
            any("truthfully remain blocked" in error for error in self.validate(complete))
        )


if __name__ == "__main__":
    unittest.main()
