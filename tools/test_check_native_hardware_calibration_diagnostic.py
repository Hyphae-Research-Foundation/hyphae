#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""Tests for the non-authoritative thread-scaling diagnostic contract."""

from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from tools.check_native_hardware_calibration_diagnostic import (
    DiagnosticValidationError,
    validate_receipt,
)
from tools.run_native_hardware_calibration_diagnostic import (
    producer_blake3,
    run_diagnostic,
)


COMMIT = "1" * 40
TREE = "2" * 40
HARDWARE = "3" * 64
EXECUTABLE = "4" * 64
COMPILER = "rustc test"
BUILD = "hyphae diagnostic test"
SAMPLES = [
    value * 225
    for value in [
        950,
        1020,
        980,
        1050,
        1010,
        970,
        1040,
        1000,
        960,
        1030,
        990,
        950,
        1020,
        980,
        1050,
        1010,
        970,
        1040,
        1000,
        960,
        1030,
        990,
        950,
        1020,
        980,
        1050,
        1010,
        970,
        1040,
        1000,
        960,
    ]
]


def statistics(bytes_per_operation: int) -> dict[str, int | str]:
    return {
        "unit": "picoseconds_per_operation",
        "minimum": 213_750,
        "median": 225_000,
        "maximum": 236_250,
        "median_absolute_deviation": 6_750,
        "relative_mad_ppm": 30_000,
        "relative_range_ppm": 100_000,
        "median_bytes_per_second": bytes_per_operation * 1_000_000_000_000 // 225_000,
    }


def worker_point(worker_count: int) -> dict[str, object]:
    bytes_per_operation = 1_048_576 * worker_count
    return {
        "worker_count": worker_count,
        "variant": "persistent-workers-physical-range-linux-affinity",
        "bytes_per_operation": bytes_per_operation,
        "operations_per_sample": 1_000_000,
        "maximum_operations_per_sample": 1_048_576,
        "batch_calibration_status": "converged",
        "samples_picoseconds_per_operation": list(SAMPLES),
        "statistics": statistics(bytes_per_operation),
        "correctness": {
            "status": "passed",
            "result_digest_blake3": "5" * 64,
            "reference_digest_blake3": "5" * 64,
        },
        "status": "stable",
    }


def valid_receipt() -> dict[str, object]:
    return {
        "schema": "hyphae-native-hardware-calibration-diagnostic-v1",
        "authority": False,
        "evidence_class": "diagnostic-only",
        "claims": [],
        "closure_declared": False,
        "source": {"commit": COMMIT, "tree": TREE},
        "platform": "linux",
        "identity": {
            "hardware_fingerprint": HARDWARE,
            "producer_executable_blake3": EXECUTABLE,
            "compiler_identity": COMPILER,
            "hyphae_build_identity": BUILD,
        },
        "policy": {
            "mode": "thorough",
            "warmup_batches": 4,
            "samples_per_measurement": 31,
            "target_sample_duration_ms": 225,
            "maximum_relative_mad_ppm": 40_000,
            "operation_calibration_target_lower_ppm": 900_000,
            "operation_calibration_target_upper_ppm": 1_100_000,
            "operation_calibration_confirmations": 2,
            "operation_calibration_max_refinements": 6,
        },
        "surface": {
            "primitive": "thread-scaling-memory-scan",
            "binding": "linux-sched-affinity",
            "worker_points": [worker_point(1), worker_point(2)],
        },
    }


def validate(value: object) -> dict[str, object]:
    return validate_receipt(
        value,
        expected_source_commit=COMMIT,
        expected_source_tree=TREE,
        expected_platform="linux",
        expected_hardware_fingerprint=HARDWARE,
        expected_producer_executable_blake3=EXECUTABLE,
        expected_compiler_identity=COMPILER,
        expected_hyphae_build_identity=BUILD,
        expected_worker_counts=[1, 2],
    )


class HardwareCalibrationDiagnosticTests(unittest.TestCase):
    def test_accepts_only_explicit_diagnostic_evidence(self) -> None:
        receipt = validate(valid_receipt())
        self.assertIs(receipt["authority"], False)
        self.assertEqual(receipt["evidence_class"], "diagnostic-only")
        self.assertEqual(receipt["claims"], [])
        self.assertIs(receipt["closure_declared"], False)

    def test_recomputes_statistics_from_exact_chronological_samples(self) -> None:
        for field, value in (
            ("median", 1001),
            ("relative_mad_ppm", 29_999),
            ("median_bytes_per_second", 1),
        ):
            with self.subTest(field=field):
                receipt = valid_receipt()
                receipt["surface"]["worker_points"][0]["statistics"][field] = value
                with self.assertRaisesRegex(DiagnosticValidationError, "do not recompute"):
                    validate(receipt)

    def test_requires_exactly_31_samples_for_every_worker_point(self) -> None:
        receipt = valid_receipt()
        receipt["surface"]["worker_points"][0][
            "samples_picoseconds_per_operation"
        ].pop()
        with self.assertRaisesRegex(DiagnosticValidationError, "exactly 31"):
            validate(receipt)

    def test_rejects_authority_closure_claims_and_authority_shaped_extras(self) -> None:
        for field, value, message in (
            ("authority", True, "authority"),
            ("closure_declared", True, "closure"),
            ("claims", ["stable scaling"], "claims"),
            ("accepted_for_scheduling", False, "keys differ"),
            ("cache_status", "disabled", "keys differ"),
            ("governor_policy", {}, "keys differ"),
        ):
            with self.subTest(field=field):
                receipt = valid_receipt()
                receipt[field] = value
                with self.assertRaisesRegex(DiagnosticValidationError, message):
                    validate(receipt)

    def test_binds_exact_source_hardware_executable_and_worker_points(self) -> None:
        mutations = (
            (("source", "commit"), "f" * 40, "exact source"),
            (("source", "tree"), "e" * 40, "exact source"),
            (("identity", "hardware_fingerprint"), "d" * 64, "another hardware"),
            (("identity", "producer_executable_blake3"), "c" * 64, "another producer"),
            (("identity", "compiler_identity"), "rustc drift", "another compiler"),
            (("identity", "hyphae_build_identity"), "build drift", "another Hyphae"),
        )
        for path, value, message in mutations:
            with self.subTest(path=path):
                receipt = valid_receipt()
                receipt[path[0]][path[1]] = value
                with self.assertRaisesRegex(DiagnosticValidationError, message):
                    validate(receipt)
        receipt = valid_receipt()
        receipt["platform"] = "darwin"
        with self.assertRaisesRegex(DiagnosticValidationError, "another platform"):
            validate(receipt)
        receipt = valid_receipt()
        receipt["surface"]["worker_points"].reverse()
        with self.assertRaisesRegex(DiagnosticValidationError, "exact ordered request"):
            validate(receipt)

    def test_rejects_other_surfaces_and_inconsistent_binding(self) -> None:
        receipt = valid_receipt()
        receipt["surface"]["primitive"] = "queue-depth-random-read"
        with self.assertRaisesRegex(DiagnosticValidationError, "another measurement surface"):
            validate(receipt)
        receipt = valid_receipt()
        receipt["surface"]["binding"] = "unbound"
        with self.assertRaisesRegex(DiagnosticValidationError, "variants disagree"):
            validate(receipt)

    def test_recomputes_convergence_and_stability(self) -> None:
        receipt = valid_receipt()
        point = receipt["surface"]["worker_points"][0]
        point["operations_per_sample"] = point["maximum_operations_per_sample"]
        with self.assertRaisesRegex(DiagnosticValidationError, "outside.*target window"):
            validate(receipt)
        receipt = valid_receipt()
        receipt["surface"]["worker_points"][0]["status"] = "unstable"
        with self.assertRaisesRegex(DiagnosticValidationError, "status must be stable"):
            validate(receipt)

        receipt = valid_receipt()
        point = receipt["surface"]["worker_points"][0]
        point["batch_calibration_status"] = "not-converged"
        point["status"] = "unstable"
        self.assertEqual(validate(receipt)["surface"]["worker_points"][0], point)

    def test_requires_differential_correctness_for_every_point(self) -> None:
        for field, value in (
            ("status", "failed"),
            ("result_digest_blake3", "6" * 64),
        ):
            with self.subTest(field=field):
                receipt = valid_receipt()
                receipt["surface"]["worker_points"][0]["correctness"][field] = value
                with self.assertRaisesRegex(
                    DiagnosticValidationError, "differential validation"
                ):
                    validate(receipt)

    def test_schema_freezes_non_authority_and_31_samples(self) -> None:
        schema_path = Path(__file__).parents[1] / "contracts" / "json-schema" / (
            "native-hardware-calibration-diagnostic-v1.schema.json"
        )
        schema = json.loads(schema_path.read_text(encoding="utf-8"))
        self.assertEqual(schema["properties"]["authority"], {"const": False})
        self.assertEqual(schema["properties"]["closure_declared"], {"const": False})
        samples = schema["$defs"]["worker_point"]["properties"][
            "samples_picoseconds_per_operation"
        ]
        self.assertEqual((samples["minItems"], samples["maxItems"]), (31, 31))

    def test_orchestrator_only_persists_validated_producer_output(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            producer = root / "producer"
            producer.write_text(
                "#!/usr/bin/env python3\n"
                "import json\n"
                f"print(json.dumps({valid_receipt()!r}))\n",
                encoding="utf-8",
            )
            producer.chmod(0o755)
            profile = root / "profile.json"
            profile.write_text(json.dumps({"fingerprint": HARDWARE}), encoding="utf-8")
            output = root / "diagnostic.json"
            with patch(
                "tools.run_native_hardware_calibration_diagnostic.producer_blake3",
                return_value=EXECUTABLE,
            ):
                receipt = run_diagnostic(
                    producer,
                    source_commit=COMMIT,
                    source_tree=TREE,
                    platform="linux",
                    hardware_profile=profile,
                    producer_executable_blake3=EXECUTABLE,
                    compiler_identity=COMPILER,
                    hyphae_build_identity=BUILD,
                    worker_counts=[1, 2],
                    output=output,
                    timeout_seconds=5,
                )
            self.assertEqual(json.loads(output.read_text(encoding="utf-8")), receipt)

            invalid = valid_receipt()
            invalid["authority"] = True
            producer.write_text(
                "#!/usr/bin/env python3\n"
                "import json\n"
                f"print(json.dumps({invalid!r}))\n",
                encoding="utf-8",
            )
            output.unlink()
            with patch(
                "tools.run_native_hardware_calibration_diagnostic.producer_blake3",
                return_value=EXECUTABLE,
            ), self.assertRaisesRegex(DiagnosticValidationError, "authority"):
                run_diagnostic(
                    producer,
                    source_commit=COMMIT,
                    source_tree=TREE,
                    platform="linux",
                    hardware_profile=profile,
                    producer_executable_blake3=EXECUTABLE,
                    compiler_identity=COMPILER,
                    hyphae_build_identity=BUILD,
                    worker_counts=[1, 2],
                    output=output,
                    timeout_seconds=5,
                )
            self.assertFalse(output.exists())

    def test_orchestrator_rejects_a_claimed_digest_for_other_producer_bytes(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            producer = root / "producer"
            producer.write_bytes(b"different producer bytes")
            self.assertNotEqual(producer_blake3(producer), EXECUTABLE)
            profile = root / "profile.json"
            profile.write_text(json.dumps({"fingerprint": HARDWARE}), encoding="utf-8")
            output = root / "diagnostic.json"
            with self.assertRaisesRegex(RuntimeError, "bytes differ"):
                run_diagnostic(
                    producer,
                    source_commit=COMMIT,
                    source_tree=TREE,
                    platform="linux",
                    hardware_profile=profile,
                    producer_executable_blake3=EXECUTABLE,
                    compiler_identity=COMPILER,
                    hyphae_build_identity=BUILD,
                    worker_counts=[1, 2],
                    output=output,
                    timeout_seconds=5,
                )
            self.assertFalse(output.exists())


if __name__ == "__main__":
    unittest.main()
