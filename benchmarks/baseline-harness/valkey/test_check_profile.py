#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

from __future__ import annotations

import copy
import json
import tempfile
import unittest
from pathlib import Path

import check_profile


class ProfileMutationTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.profile = json.loads(check_profile.DEFAULT_PROFILE.read_text(encoding="utf-8"))
        cls.runner = (
            check_profile.ROOT
            / "benchmarks/baseline-harness/scripts/run-metal.sh"
        ).read_text(encoding="utf-8")

    def check_mutation(self, mutate, expected: str) -> None:
        profile = copy.deepcopy(self.profile)
        mutate(profile)
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "mutated.json"
            path.write_text(json.dumps(profile), encoding="utf-8")
            with self.assertRaisesRegex(check_profile.ProfileError, expected):
                check_profile.check_profile(path, check_profile.ROOT)

    def test_checked_in_profile_is_valid(self) -> None:
        report = check_profile.check_profile()
        self.assertEqual(report["operations"], 72)
        self.assertFalse(report["compatibility_claim"])
        self.assertEqual(report["lanes"], ["no", "always", "everysec"])
        self.assertEqual(report["profile_sha256"], check_profile.EXPECTED_PROFILE_SHA256)

    def test_rejects_valkey_compatibility_claim(self) -> None:
        self.check_mutation(
            lambda profile: profile["claim_policy"].update(valkey_compatible=True),
            r"claim_policy\.valkey_compatible must be false",
        )

    def test_rejects_promoting_an_unsupported_operation(self) -> None:
        def mutate(profile) -> None:
            operation = next(item for item in profile["inventory"] if item["operation"] == "EVAL")
            operation["classification"] = "exact"
            operation["native"] = ["FakeEval"]
            operation["scope"] = "all"
            operation["reason"] = "unsupported promotion"
            operation.pop("gap")
            profile["summary"]["excluded"] -= 1
            profile["summary"]["exact"] += 1

        self.check_mutation(mutate, "claim semantics changed without a reviewed full-seal")

    def test_rejects_omitting_an_excluded_gap(self) -> None:
        def mutate(profile) -> None:
            operation = next(item for item in profile["inventory"] if item["operation"] == "XREAD")
            operation.pop("gap")

        self.check_mutation(mutate, "XREAD: excluded operations require an explicit gap")

    def test_rejects_oracle_sha_drift(self) -> None:
        self.check_mutation(
            lambda profile: profile["oracle"].update(source_commit="0" * 40),
            "oracle.source_commit must be",
        )

    def test_full_seal_rejects_scope_drift(self) -> None:
        def mutate(profile) -> None:
            operation = next(item for item in profile["inventory"] if item["operation"] == "GET")
            operation["scope"] = "Any key in any protocol."

        self.check_mutation(mutate, "claim semantics changed without a reviewed full-seal")

    def test_full_seal_rejects_reason_drift(self) -> None:
        def mutate(profile) -> None:
            operation = next(item for item in profile["inventory"] if item["operation"] == "SET")
            operation["reason"] = "Unreviewed semantic explanation."

        self.check_mutation(mutate, "claim semantics changed without a reviewed full-seal")

    def test_full_seal_rejects_native_mapping_drift(self) -> None:
        def mutate(profile) -> None:
            operation = next(item for item in profile["inventory"] if item["operation"] == "HGET")
            operation["native"] = ["UnreviewedMapping"]

        self.check_mutation(mutate, "claim semantics changed without a reviewed full-seal")

    def test_full_seal_rejects_unsupported_note_drift(self) -> None:
        def mutate(profile) -> None:
            operation = next(item for item in profile["inventory"] if item["operation"] == "EVAL")
            operation["gap"] = "Unreviewed unsupported-operation note."

        self.check_mutation(mutate, "claim semantics changed without a reviewed full-seal")

    def test_full_seal_rejects_classification_definition_drift(self) -> None:
        self.check_mutation(
            lambda profile: profile["classification_definitions"].update(exact="broader"),
            "claim semantics changed without a reviewed full-seal",
        )

    def test_full_seal_rejects_profile_identity_drift(self) -> None:
        def mutate(profile) -> None:
            profile["profile"] = "Broader profile"
            profile["profile_version"] = 2
            profile["status"] = "complete"
            profile["foundation_only"] = False

        self.check_mutation(mutate, "claim semantics changed without a reviewed full-seal")

    def test_full_seal_rejects_source_and_evidence_authority_drift(self) -> None:
        def mutate(profile) -> None:
            profile["oracle"]["operation_authority"] = "unreviewed"
            profile["evidence_authorities"]["product-surface"] = "README.md"

        self.check_mutation(mutate, "claim semantics changed without a reviewed full-seal")

    def test_full_seal_rejects_executed_source_binding_drift(self) -> None:
        self.check_mutation(
            lambda profile: profile["executed_source_binding"].update(
                policy="unsealed-descendant-source"
            ),
            "claim semantics changed without a reviewed full-seal",
        )

    def test_full_seal_rejects_omitting_build_or_workload_sources(self) -> None:
        def mutate(profile) -> None:
            profile["executed_source_binding"]["producer_validator_paths"].remove(
                "benchmarks/baseline-harness/Cargo.lock"
            )

        self.check_mutation(mutate, "claim semantics changed without a reviewed full-seal")

    def test_semantic_bundle_changes_when_authority_bytes_change(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "authority").write_bytes(b"one")
            original = check_profile.semantic_bundle_sha256(root, ["authority"])
            (root / "authority").write_bytes(b"two")
            changed = check_profile.semantic_bundle_sha256(root, ["authority"])
        self.assertNotEqual(original, changed)

    def test_semantic_implementation_change_requires_profile_revision(self) -> None:
        relative = "crates/hyphae-native-runtime/src/model.rs"
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            target = root / relative
            target.parent.mkdir(parents=True)
            target.write_bytes((check_profile.ROOT / relative).read_bytes())
            original = check_profile.semantic_bundle_sha256(root, [relative])
            target.write_bytes(target.read_bytes() + b"\n// semantic mutation\n")
            changed = check_profile.semantic_bundle_sha256(root, [relative])
        self.assertNotEqual(original, changed)

    def test_full_seal_rejects_operation_evidence_drift(self) -> None:
        def mutate(profile) -> None:
            operation = next(item for item in profile["inventory"] if item["operation"] == "GET")
            operation["evidence"] = ["product-surface"]

        self.check_mutation(mutate, "claim semantics changed without a reviewed full-seal")

    def test_rejects_unsealed_top_level_profile_fields(self) -> None:
        self.check_mutation(
            lambda profile: profile.update(unreviewed_claim=True),
            "profile field set must be exactly",
        )

    def test_rejects_duplicate_profile_keys(self) -> None:
        encoded = check_profile.DEFAULT_PROFILE.read_text(encoding="utf-8").replace(
            '"schema": "hyphae-external-valkey-application-core-v1",',
            '"schema": "hyphae-external-valkey-application-core-v1",\n'
            '  "schema": "hyphae-external-valkey-application-core-v1",',
            1,
        )
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "duplicate.json"
            path.write_text(encoded, encoding="utf-8")
            with self.assertRaisesRegex(check_profile.ProfileError, "duplicate JSON key"):
                check_profile.check_profile(path, check_profile.ROOT)

    def test_rejects_config_authority_drift(self) -> None:
        self.check_mutation(
            lambda profile: profile["configuration_authority"]["lanes"][0].update(
                sha256="0" * 64
            ),
            "lane no: config sha256 mismatch",
        )

    def test_rejects_everysec_equivalence_claim(self) -> None:
        self.check_mutation(
            lambda profile: profile["configuration_authority"]["lanes"][2].update(
                comparison="Hyphae Memory equivalent"
            ),
            "everysec lane must explicitly deny",
        )

    def test_zpop_lifecycle_mismatch_stays_different(self) -> None:
        def mutate(profile) -> None:
            operation = next(
                item for item in profile["inventory"] if item["operation"] == "ZPOPMIN"
            )
            operation["classification"] = "exact"
            operation["reason"] = "All lifecycle cases align."
            profile["summary"]["different"] -= 1
            profile["summary"]["exact"] += 1

        self.check_mutation(mutate, "ZPOPMIN: typed lifecycle mismatch requires different")

    def test_getrange_missing_key_mismatch_stays_different(self) -> None:
        def mutate(profile) -> None:
            operation = next(
                item for item in profile["inventory"] if item["operation"] == "GETRANGE"
            )
            operation["classification"] = "exact"
            operation["reason"] = "Missing keys align."
            profile["summary"]["different"] -= 1
            profile["summary"]["exact"] += 1

        self.check_mutation(mutate, "GETRANGE: missing-key mismatch requires different")

    def test_set_exact_scope_excludes_live_typed_collections(self) -> None:
        def mutate(profile) -> None:
            operation = next(item for item in profile["inventory"] if item["operation"] == "SET")
            operation["scope"] = "Unconditional replacement of any key family."

        self.check_mutation(mutate, "SET: exact scope must remain missing-or-live-scalar only")

    def test_setrange_missing_key_lifecycle_stays_different(self) -> None:
        def mutate(profile) -> None:
            operation = next(
                item for item in profile["inventory"] if item["operation"] == "SETRANGE"
            )
            operation["classification"] = "exact"
            operation["reason"] = "Missing keys align."
            profile["summary"]["different"] -= 1
            profile["summary"]["exact"] += 1

        self.check_mutation(mutate, "SETRANGE: missing-key lifecycle mismatch requires different")

    def test_setrange_reason_retains_empty_patch_missing_key_behavior(self) -> None:
        def mutate(profile) -> None:
            operation = next(
                item for item in profile["inventory"] if item["operation"] == "SETRANGE"
            )
            operation["reason"] = "Valkey always creates a missing scalar."

        self.check_mutation(mutate, "SETRANGE: reason must retain the empty-patch")

    def test_expireat_shared_timestamp_domains_are_exact(self) -> None:
        def mutate_seconds(profile) -> None:
            operation = next(
                item for item in profile["inventory"] if item["operation"] == "EXPIREAT"
            )
            operation["scope"] = operation["scope"].replace("9223372036854", "9223372036855")

        def mutate_milliseconds(profile) -> None:
            operation = next(
                item for item in profile["inventory"] if item["operation"] == "PEXPIREAT"
            )
            operation["reason"] = operation["reason"].replace("* 1000", "* 1000000")

        self.check_mutation(mutate_seconds, "EXPIREAT: shared signed-seconds bounds differ")
        self.check_mutation(
            mutate_milliseconds,
            "PEXPIREAT: milliseconds-to-microseconds formula differs",
        )

    def test_ttl_equivalence_excludes_nonpositive_boundaries(self) -> None:
        def mutate_ttl(profile) -> None:
            operation = next(item for item in profile["inventory"] if item["operation"] == "TTL")
            operation["scope"] = "One known-family key at any expiry boundary."
            operation["reason"] = "Missing and expired values are equivalent."

        def mutate_pttl(profile) -> None:
            operation = next(item for item in profile["inventory"] if item["operation"] == "PTTL")
            operation["scope"] = operation["scope"].replace(">= 1000", ">= 0")

        self.check_mutation(mutate_ttl, "TTL: equivalent scope must require a positive future")
        self.check_mutation(
            mutate_pttl,
            "PTTL: equivalent scope must require a positive future",
        )

    def test_set_algebra_equivalence_keeps_64_position_bound(self) -> None:
        for operation_name in ("SINTER", "SUNION", "SDIFF"):
            def mutate(profile, operation_name=operation_name) -> None:
                operation = next(
                    item for item in profile["inventory"] if item["operation"] == operation_name
                )
                operation["scope"] = operation["scope"].replace(
                    "1..=64 input key positions, ", ""
                )

            self.check_mutation(
                mutate,
                f"{operation_name}: set algebra input-key-position bound differs",
            )

    def test_set_algebra_uses_product_debug_response_admission(self) -> None:
        def mutate(profile) -> None:
            operation = next(
                item for item in profile["inventory"] if item["operation"] == "SUNION"
            )
            operation["scope"] = operation["scope"].replace(
                "response_debug_bytes = format!",
                "encoded_wire_bytes = format!",
            )

        self.check_mutation(mutate, "SUNION: set algebra Debug-byte authority differs")

    def test_zrange_equivalence_remains_live_set_scoped(self) -> None:
        def mutate(profile) -> None:
            operation = next(
                item for item in profile["inventory"] if item["operation"] == "ZRANGE"
            )
            operation["scope"] = "Any sorted set, including missing and expired sets."
            operation["reason"] = "All collection lifecycle states align."

        self.check_mutation(mutate, "ZRANGE: equivalent scope must require a live pre-existing")

    def test_zrange_rank_and_score_cases_keep_distinct_bounds(self) -> None:
        def mutate_rank(profile) -> None:
            operation = next(
                item
                for item in profile["inventory"]
                if item["operation"] == "ZRANGE" and item["case"] == "rank"
            )
            operation["scope"] = operation["scope"].replace(
                "no BYSCORE, BYLEX, LIMIT, or OFFSET shape",
                "optional LIMIT offset/count",
            )

        def mutate_score(profile) -> None:
            operation = next(
                item
                for item in profile["inventory"]
                if item["operation"] == "ZRANGE" and item["case"] == "score"
            )
            operation["scope"] = operation["scope"].replace(
                "explicit LIMIT offset/count", "optional unbounded output"
            )

        self.check_mutation(mutate_rank, "ZRANGE rank: LIMIT/OFFSET must remain outside scope")
        self.check_mutation(mutate_score, "ZRANGE score: bounded LIMIT/OFFSET mapping differs")

    def test_zrange_cases_keep_product_response_envelope(self) -> None:
        def mutate_rank(profile) -> None:
            operation = next(
                item
                for item in profile["inventory"]
                if item["operation"] == "ZRANGE" and item["case"] == "rank"
            )
            operation["scope"] = operation["scope"].replace(
                "at most 4096 items", "at most 4097 items"
            )

        def mutate_score(profile) -> None:
            operation = next(
                item
                for item in profile["inventory"]
                if item["operation"] == "ZRANGE" and item["case"] == "score"
            )
            operation["scope"] = operation["scope"].replace("<= 16777216", "<= 16777217")

        self.check_mutation(mutate_rank, "ZRANGE rank: normalized rank span bound differs")
        self.check_mutation(mutate_score, "ZRANGE score: ProductLimits Debug-byte authority differs")

    def test_zrange_requires_both_sealed_cases(self) -> None:
        def mutate(profile) -> None:
            profile["inventory"] = [
                item
                for item in profile["inventory"]
                if not (item["operation"] == "ZRANGE" and item.get("case") == "score")
            ]
            profile["summary"]["operations"] -= 1
            profile["summary"]["equivalent"] -= 1

        self.check_mutation(mutate, "exactly the rank and score ZRANGE cases")

    def test_runner_binds_archive_build_binary_and_configs(self) -> None:
        lanes = self.profile["configuration_authority"]["lanes"]
        errors = check_profile.runner_source_errors(
            self.runner, self.profile["oracle"]["artifact"]["url"], lanes
        )
        self.assertEqual(errors, [])

    def test_runner_rejects_missing_binary_digest_binding(self) -> None:
        mutated = self.runner.replace(
            'HYPHAE_VALKEY_SERVER_SHA256=$(sha256sum "$VALKEY_SERVER"',
            'HYPHAE_VALKEY_SERVER_SHA256=$(untrusted "$VALKEY_SERVER"',
        )
        errors = check_profile.runner_source_errors(
            mutated,
            self.profile["oracle"]["artifact"]["url"],
            self.profile["configuration_authority"]["lanes"],
        )
        self.assertTrue(any("SERVER_SHA256" in error for error in errors))

    def test_runner_rejects_missing_clean_and_fresh_setup(self) -> None:
        mutated = self.runner.replace(
            'test -z "$(git status --porcelain=v1 --untracked-files=all)"',
            "true",
        ).replace(
            "rm -rf /mnt/nvme/valkey-no /mnt/nvme/valkey-always /mnt/nvme/valkey-everysec",
            "true",
        )
        errors = check_profile.runner_source_errors(
            mutated,
            self.profile["oracle"]["artifact"]["url"],
            self.profile["configuration_authority"]["lanes"],
        )
        self.assertTrue(any("git status" in error for error in errors))
        self.assertTrue(any("rm -rf" in error for error in errors))

    def test_runner_rejects_missing_stale_daemon_and_retained_artifact_guards(self) -> None:
        mutated = self.runner.replace("pgrep -x valkey-server", "true").replace(
            'install -m 0555 "$VALKEY_SERVER" "$RETAINED_VALKEY_SERVER"',
            "true",
        )
        errors = check_profile.runner_source_errors(
            mutated,
            self.profile["oracle"]["artifact"]["url"],
            self.profile["configuration_authority"]["lanes"],
        )
        self.assertTrue(any("pgrep" in error for error in errors))
        self.assertTrue(any("install -m" in error for error in errors))

    def test_runner_rejects_weakened_i7i_topology_or_quota_guards(self) -> None:
        mutated = self.runner.replace(
            'test "$PHYSICAL_CORES" -eq 48', "true"
        ).replace('test "$CPU_QUOTA" = max', "true")
        errors = check_profile.runner_source_errors(
            mutated,
            self.profile["oracle"]["artifact"]["url"],
            self.profile["configuration_authority"]["lanes"],
        )
        self.assertTrue(any("PHYSICAL_CORES" in error for error in errors))
        self.assertTrue(any("CPU_QUOTA" in error for error in errors))


if __name__ == "__main__":
    unittest.main()
