#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only

from __future__ import annotations

import json
import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from tools.check_native_g7_receipt import GateFailure
from tools.check_native_performance_receipt import validate_progress
from tools.run_native_g7 import (
    G7_SURFACES,
    PILOT_OBSERVATIONS,
    PILOT_WARMUP,
    ProcessMetrics,
    ProgressStalled,
    ProgressWatchdog,
    RuntimeBudgetExceeded,
    derive_cell_runtime_budget,
    derive_matrix_runtime_plan,
    parse_macos_counter_export,
    persist_validated_cell_checkpoint,
    run_calibration_pilot,
    run_cell,
    validate_completed_ann_progress,
    validate_completed_cell_progress,
    validate_cross_artifact_dataset,
    validate_execution_authority_evidence,
    validate_initial_ann_bulk_evidence,
    validate_partial_receipt,
    validate_pilot_search_evidence,
    validate_warm_control_pilot_latency,
    write_matrix_progress,
)


class NativeG7ControllerTests(unittest.TestCase):
    SOURCE_COMMIT = "1" * 40

    @classmethod
    def initial_ann_bulk(cls) -> dict[str, object]:
        return {
            "schema": "hyphae-native-g7-initial-ann-bulk-v1",
            "source_commit": cls.SOURCE_COMMIT,
            "dataset_digest": "3" * 64,
            "builder": "partitioned-hnsw-v1",
            "partition_policy": "g7-fixed-64-logical-partitions-v1",
            "input_identity": "4" * 64,
            "aggregate_identity": "5" * 64,
            "planned_vectors": 1_000_000,
            "planned_partitions": 64,
            "planned_workers": 44,
            "planned_memory_bytes": 4_000_000_000,
            "worker_batches": 48,
            "total_time_nanos": 1,
            "hardware_profile_fingerprint": "6" * 64,
            "governor_policy_schema": "hyphae-native-governor-policy-v1",
            "governor_mode": "mixed",
            "calibration_cache_key": "test-calibration",
            "topology_digest": "7" * 64,
            "topology_workers": 48,
            "hard_affinity": True,
            "governor_execution": {
                "class": "bulk",
                "compute_threads": 44,
                "io_slots": 0,
                "memory_bytes": 4_000_000_000,
                "queue_ticket": None,
                "initial_queue_depth": 0,
                "queue_time_nanos": 0,
                "execution_time_nanos": 1,
            },
        }

    @staticmethod
    def execution_authority(*, background: bool = False) -> dict[str, object]:
        surfaces = {
            "search-fixture", "embedded-structure", "embedded-sql",
            "local-structure-seed", "local-structure-migration",
            "local-structure-daemon", "local-sql-daemon", "indexed-sql",
            "join-sql", "group-commit", "physical-observation",
        }
        if background:
            surfaces.add("background-maintenance")
        return {
            "status": "measured",
            "database_queue_wait_millis": 60_000,
            "topology_digest": "9" * 64,
            "runner_executable_blake3": "8" * 64,
            "calibration_executable_blake3": "8" * 64,
            "installations": len(surfaces),
            "installed_surfaces": sorted(surfaces),
            "registered_pools": 1,
            "local_dispatches": 9,
            "stolen_dispatches": 0,
            "completed_jobs": 9,
            "numa_steal_status": "disabled",
        }

    def test_execution_authority_requires_exact_database_queue_wait(self) -> None:
        validate_execution_authority_evidence(
            self.execution_authority(),
            calibration_executable_blake3="8" * 64,
            topology_digest="9" * 64,
            background=False,
        )
        for mutation, value in (
            ("missing", None),
            ("bool", True),
            ("zero", 0),
            ("wrong", 59_999),
            ("extra", 60_000),
        ):
            with self.subTest(mutation=mutation):
                evidence = self.execution_authority()
                if mutation == "missing":
                    del evidence["database_queue_wait_millis"]
                elif mutation == "extra":
                    evidence["unexpected_queue_wait"] = value
                else:
                    evidence["database_queue_wait_millis"] = value
                with self.assertRaisesRegex(RuntimeError, "authority.*(fields|queue wait)"):
                    validate_execution_authority_evidence(
                        evidence,
                        calibration_executable_blake3="8" * 64,
                        topology_digest="9" * 64,
                        background=False,
                    )

    def test_execution_authority_requires_exact_runner_and_every_surface(self) -> None:
        evidence = self.execution_authority(background=True)
        validate_execution_authority_evidence(
            evidence,
            calibration_executable_blake3="8" * 64,
            topology_digest="9" * 64,
            background=True,
        )
        evidence["installed_surfaces"].remove("local-sql-daemon")
        with self.assertRaisesRegex(RuntimeError, "omitted"):
            validate_execution_authority_evidence(
                evidence,
                calibration_executable_blake3="8" * 64,
                topology_digest="9" * 64,
                background=True,
            )

    def test_execution_authority_rejects_cli_calibration_and_counter_drift(self) -> None:
        evidence = self.execution_authority()
        evidence["runner_executable_blake3"] = "7" * 64
        with self.assertRaisesRegex(RuntimeError, "another runner"):
            validate_execution_authority_evidence(
                evidence,
                calibration_executable_blake3="8" * 64,
                topology_digest="9" * 64,
                background=False,
            )
        evidence = self.execution_authority()
        evidence["completed_jobs"] += 1
        with self.assertRaisesRegex(RuntimeError, "reconcile"):
            validate_execution_authority_evidence(
                evidence,
                calibration_executable_blake3="8" * 64,
                topology_digest="9" * 64,
                background=False,
            )
        evidence = self.execution_authority()
        evidence["local_dispatches"] = 7
        evidence["stolen_dispatches"] = 2
        with self.assertRaisesRegex(RuntimeError, "disabled"):
            validate_execution_authority_evidence(
                evidence,
                calibration_executable_blake3="8" * 64,
                topology_digest="9" * 64,
                background=False,
            )

    @classmethod
    def completed_progress(cls) -> dict[str, object]:
        details = cls.initial_ann_bulk()
        details["eta"] = {
            "status": "completed",
            "estimated_remaining_nanos": 0,
        }
        return {
            "schema": "hyphae-native-performance-progress-v1",
            "source_commit": cls.SOURCE_COMMIT,
            "source_tree": "2" * 40,
            "dataset_digest": "3" * 64,
            "operation": "ann-bulk-build",
            "stage": "ann-published",
            "sequence": 4,
            "completed_units": 1_000_000,
            "total_units": 1_000_000,
            "unit": "vectors",
            "elapsed_nanos": 10,
            "status": "completed",
            "checkpoint_digest": "4" * 64,
            "details": details,
        }

    @classmethod
    def pilot_receipt(cls, throughput: float = 10_000.0) -> dict[str, object]:
        from tools.test_check_native_g7_receipt import _strict_group_commit_evidence

        pilot = {
            "source_commit": cls.SOURCE_COMMIT,
            "platform": "linux",
            "state": "warm",
            "concurrency": 1,
            "dataset": {
                "observations": PILOT_OBSERVATIONS,
                "warmup": PILOT_WARMUP,
                "search_documents": 1_000_000,
                "vector_count": 1_000_000,
                "vector_dimension": 384,
                "digest": "3" * 64,
            },
            "cells": {
                name: {
                    "throughput_per_second": throughput,
                    "p99": 100_000,
                }
                for name in G7_SURFACES
            },
            "controller": {"wall_seconds": 20.0},
        }
        pilot["cells"]["strict-group-commit"].update({
            "group_commit_evidence": _strict_group_commit_evidence(
                observations=PILOT_OBSERVATIONS,
                concurrency=1,
            ),
        })
        return pilot

    @classmethod
    def warm_control_pilot(cls) -> dict[str, object]:
        pilot = cls.pilot_receipt()
        pilot["background_interference"] = {"status": "control"}
        for cell in pilot["cells"].values():
            cell["p50"] = 1
            cell["p99"] = 1
        return pilot

    @classmethod
    def partial_receipt(cls) -> dict[str, object]:
        return {
            "schema": "hyphae-native-g7-partial-receipt-v1",
            "source_commit": cls.SOURCE_COMMIT,
            "source_tree": "2" * 40,
            "dataset_digest": "3" * 64,
            "platform": "linux",
            "state": "warm",
            "concurrency": 1,
            "sequence": 2,
            "status": "running",
            "completed_count": 1,
            "total_cells": len(G7_SURFACES),
            "current_cell": G7_SURFACES[1],
            "cells": {G7_SURFACES[0]: {"status": "measured"}},
        }

    @classmethod
    def completed_cell_progress(cls) -> dict[str, object]:
        return {
            "schema": "hyphae-native-performance-progress-v1",
            "source_commit": cls.SOURCE_COMMIT,
            "source_tree": "2" * 40,
            "dataset_digest": "3" * 64,
            "operation": "g7-cell",
            "stage": "cell-completed",
            "sequence": 12,
            "completed_units": 13_100_000,
            "total_units": 13_100_000,
            "unit": "work-units",
            "elapsed_nanos": 10,
            "status": "completed",
            "checkpoint_digest": "4" * 64,
            "details": {
                "eta": {
                    "status": "completed",
                    "estimated_remaining_nanos": 0,
                },
            },
        }

    def test_parses_macos_counter_rows_and_references(self) -> None:
        document = """<?xml version="1.0"?>
<trace-query-result><node><schema name="MetricTable"/>
<row><string id="1">Cycles</string><fixed-decimal id="2">120.0</fixed-decimal></row>
<row><string id="3">L1D Cache Load Misses</string><fixed-decimal id="4">7.0</fixed-decimal></row>
<row><string id="5">L1D Cache Store Misses</string><fixed-decimal id="6">3.0</fixed-decimal></row>
<row><string ref="1"/><fixed-decimal ref="2"/></row>
</node></trace-query-result>
"""
        with tempfile.TemporaryDirectory() as directory:
            export = Path(directory) / "metrics.xml"
            export.write_text(document, encoding="utf-8")
            counters = parse_macos_counter_export(export)
        self.assertEqual(counters, {"cpu_cycles": 240, "cache_misses": 10})

    def test_rejects_export_without_cycles(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            export = Path(directory) / "metrics.xml"
            export.write_text("<trace-query-result/>", encoding="utf-8")
            with self.assertRaisesRegex(RuntimeError, "did not contain"):
                parse_macos_counter_export(export)

    def test_accepts_durably_published_ann_progress(self) -> None:
        validate_completed_ann_progress(self.completed_progress(), self.SOURCE_COMMIT)

    def test_accepts_only_terminal_whole_cell_progress(self) -> None:
        validate_completed_cell_progress(
            self.completed_cell_progress(), self.SOURCE_COMMIT
        )
        progress = self.completed_cell_progress()
        progress["stage"] = "ann-published"
        with self.assertRaisesRegex(RuntimeError, "complete cell publication"):
            validate_completed_cell_progress(progress, self.SOURCE_COMMIT)

    def test_progress_watchdog_fails_closed_on_source_commit_drift(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "runner-progress.json"
            watchdog = ProgressWatchdog(
                path,
                timeout_seconds=5.0,
                started=10.0,
                expected_commit=self.SOURCE_COMMIT,
            )
            progress = self.completed_cell_progress()
            progress["source_commit"] = "f" * 40
            path.write_text(json.dumps(progress))
            with self.assertRaisesRegex(ValueError, "differs from expected"):
                watchdog.observe(11.0)

    def test_progress_schema_covers_runner_details_and_eta(self) -> None:
        schema_path = Path(__file__).parents[1] / "contracts" / "json-schema" / (
            "native-performance-progress-v1.schema.json"
        )
        schema = json.loads(schema_path.read_text(encoding="utf-8"))
        self.assertIn("details", schema["required"])
        self.assertEqual(schema["properties"]["details"]["required"], ["eta"])
        self.assertEqual(
            set(schema["$defs"]["eta"]["required"]),
            {"status", "estimated_remaining_nanos"},
        )

    def test_rejects_progress_that_stops_before_publication(self) -> None:
        progress = self.completed_progress()
        progress["stage"] = "ann-publication"
        progress["status"] = "running"
        progress["checkpoint_digest"] = None
        with self.assertRaisesRegex(RuntimeError, "durable publication"):
            validate_completed_ann_progress(progress, self.SOURCE_COMMIT)

    def test_rejects_parallel_bulk_without_multiple_worker_batches(self) -> None:
        evidence = self.initial_ann_bulk()
        evidence["worker_batches"] = 1
        with self.assertRaisesRegex(RuntimeError, "parallel worker batches"):
            validate_initial_ann_bulk_evidence(evidence, self.SOURCE_COMMIT)

    def test_accepts_compute_only_initial_bulk_governor_request(self) -> None:
        evidence = self.initial_ann_bulk()
        self.assertEqual(evidence["governor_execution"]["io_slots"], 0)
        validate_initial_ann_bulk_evidence(evidence, self.SOURCE_COMMIT)

    def test_rejects_initial_bulk_governor_resource_mismatch(self) -> None:
        for field, value in (("compute_threads", 43), ("memory_bytes", 3_999_999_999)):
            with self.subTest(field=field):
                evidence = self.initial_ann_bulk()
                evidence["governor_execution"][field] = value
                with self.assertRaisesRegex(RuntimeError, "governor execution"):
                    validate_initial_ann_bulk_evidence(evidence, self.SOURCE_COMMIT)

    def test_rejects_negative_initial_bulk_governor_resources(self) -> None:
        for field in ("compute_threads", "io_slots", "memory_bytes"):
            with self.subTest(field=field):
                evidence = self.initial_ann_bulk()
                evidence["governor_execution"][field] = -1
                with self.assertRaisesRegex(RuntimeError, "governor execution"):
                    validate_initial_ann_bulk_evidence(evidence, self.SOURCE_COMMIT)

    def test_rejects_invented_initial_bulk_io_reservation(self) -> None:
        evidence = self.initial_ann_bulk()
        evidence["governor_execution"]["io_slots"] = 1
        with self.assertRaisesRegex(RuntimeError, "governor execution"):
            validate_initial_ann_bulk_evidence(evidence, self.SOURCE_COMMIT)

    def test_rejects_bulk_above_durable_partition_limit(self) -> None:
        evidence = self.initial_ann_bulk()
        evidence["planned_partitions"] = 112
        with self.assertRaisesRegex(RuntimeError, "depends on hardware"):
            validate_initial_ann_bulk_evidence(evidence, self.SOURCE_COMMIT)

    def test_accepts_more_topology_workers_than_logical_partitions(self) -> None:
        evidence = self.initial_ann_bulk()
        evidence["topology_workers"] = 256
        evidence["planned_workers"] = 64
        evidence["worker_batches"] = 64
        evidence["governor_execution"]["compute_threads"] = 64
        validate_initial_ann_bulk_evidence(evidence, self.SOURCE_COMMIT)

    def test_rejects_progress_details_for_another_dataset(self) -> None:
        progress = self.completed_progress()
        progress["details"]["dataset_digest"] = "8" * 64
        with self.assertRaisesRegex(RuntimeError, "another dataset"):
            validate_completed_ann_progress(progress, self.SOURCE_COMMIT)

    def test_stage_progress_preserves_vector_identity(self) -> None:
        previous = self.completed_progress()
        previous.update({
            "stage": "ann-private-build",
            "sequence": 1,
            "completed_units": 0,
            "status": "running",
            "checkpoint_digest": None,
            "details": {
                "builder": "partitioned-hnsw-v1",
                "eta": {
                    "status": "pending",
                    "estimated_remaining_nanos": None,
                },
            },
        })
        current = dict(previous)
        current.update({
            "stage": "ann-child-build",
            "sequence": 2,
            "completed_units": 250_000,
            "elapsed_nanos": 20,
            "details": {
                "builder": "partitioned-hnsw-v1",
                "stage_completed": 1,
                "stage_total": 4,
                "eta": {
                    "status": "estimated",
                    "estimated_remaining_nanos": 60,
                },
            },
        })
        validate_progress(current, self.SOURCE_COMMIT, previous)

    def test_matrix_progress_is_atomic_diagnostic_and_exact_sha(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "matrix.progress.json"
            completed = [{
                "state": "warm",
                "background_mode": "control",
                "concurrency": 1,
            }]
            current = {
                "state": "warm",
                "background_mode": "control",
                "concurrency": 8,
            }
            write_matrix_progress(
                path,
                self.SOURCE_COMMIT,
                "linux",
                completed,
                6,
                current,
                "running",
                1,
            )
            progress = json.loads(path.read_text(encoding="utf-8"))
            self.assertEqual(progress["source_commit"], self.SOURCE_COMMIT)
            self.assertEqual(progress["completed_count"], 1)
            self.assertEqual(progress["total_cells"], 6)
            self.assertEqual(progress["current_cell"], current)
            self.assertEqual(progress["status"], "running")
            self.assertFalse(list(path.parent.glob("*.tmp")))

    def test_progress_watchdog_fails_after_configured_stall(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "runner-progress.json"
            watchdog = ProgressWatchdog(path, timeout_seconds=5.0, started=10.0)
            watchdog.observe(14.9)
            with self.assertRaisesRegex(ProgressStalled, "stalled for 5s"):
                watchdog.observe(15.0)

    @unittest.skipUnless(os.name == "posix", "requires Linux procfs semantics")
    def test_process_metrics_preserves_runner_counters_when_proc_becomes_inaccessible(
        self,
    ) -> None:
        metrics = ProcessMetrics(12345)
        runner_counters = {
            name: {
                "status": "measured",
                "value": value,
                "unit": "count" if name == "page_faults" else "bytes",
                "provider": f"runner-{name}",
            }
            for name, value in (
                ("bytes_read", 101),
                ("bytes_written", 202),
                ("rss", 303),
                ("page_faults", 404),
            )
        }
        payload = {
            "counters": {
                name: counter.copy() for name, counter in runner_counters.items()
            }
        }
        proc_io = "read_bytes: 10\nwrite_bytes: 20\n"
        denied = PermissionError(13, "permission denied", "/proc/12345/io")

        with (
            patch("tools.run_native_g7.sys.platform", "linux"),
            patch.object(
                metrics,
                "_status",
                side_effect=({"rss": 4096}, {"rss": 8192}),
            ),
            patch.object(metrics, "_faults", side_effect=(2, 5)),
            patch.object(Path, "read_text", side_effect=(proc_io, denied)),
        ):
            metrics.sample()
            metrics.sample()

        metrics.inject(payload)
        self.assertEqual(payload["counters"], runner_counters)

    @unittest.skipUnless(os.name == "posix", "requires a POSIX executable fixture")
    def test_run_cell_preserves_runner_counters_when_proc_sampling_fails(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            runner = Path(directory) / "runner"
            counters = {
                name: {
                    "status": "measured",
                    "value": value,
                    "unit": "bytes" if name != "page_faults" else "count",
                    "provider": f"runner-{name}",
                }
                for name, value in (
                    ("rss", 101),
                    ("page_faults", 202),
                    ("bytes_read", 303),
                    ("bytes_written", 404),
                )
            }
            runner.write_text(
                "#!/bin/sh\n"
                "sleep 0.05\n"
                f"printf '%s\\n' '{json.dumps({'counters': counters})}'\n"
            )
            runner.chmod(0o755)
            original_read_text = Path.read_text

            def deny_proc_io(path: Path, *args, **kwargs) -> str:
                if str(path).startswith("/proc/") and path.name == "io":
                    raise PermissionError(13, "permission denied", str(path))
                return original_read_text(path, *args, **kwargs)

            with (
                patch("tools.run_native_g7.sys.platform", "linux"),
                patch.object(Path, "read_text", deny_proc_io),
            ):
                payload = run_cell(
                    runner,
                    self.SOURCE_COMMIT,
                    "linux",
                    "warm",
                    1,
                    timeout_seconds=2.0,
                )

        self.assertEqual(payload["counters"], counters)

    @unittest.skipUnless(os.name == "posix", "requires a POSIX executable fixture")
    def test_run_cell_distinguishes_stall_from_runtime_budget(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            runner = root / "runner"
            runner.write_text("#!/bin/sh\nwhile :; do sleep 1; done\n")
            runner.chmod(0o755)
            progress = root / "progress.json"
            with self.assertRaisesRegex(RuntimeBudgetExceeded, "runtime budget"):
                run_cell(
                    runner,
                    self.SOURCE_COMMIT,
                    "linux",
                    "warm",
                    1,
                    timeout_seconds=0.05,
                    progress_path=progress,
                    stall_timeout_seconds=10.0,
                )
            with self.assertRaisesRegex(ProgressStalled, "cell stalled"):
                run_cell(
                    runner,
                    self.SOURCE_COMMIT,
                    "linux",
                    "warm",
                    1,
                    timeout_seconds=2.0,
                    progress_path=progress,
                    stall_timeout_seconds=0.05,
                )

    @unittest.skipUnless(os.name == "posix", "requires POSIX executable fixtures")
    def test_run_cell_uses_only_effective_child_environment_for_perf(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            marker = root / "perf-used"
            perf = root / "perf"
            perf.write_text(
                "#!/bin/sh\n"
                "touch \"$PERF_MARKER\"\n"
                "while [ \"$1\" != \"--\" ]; do shift; done\n"
                "shift\n"
                "exec \"$@\"\n"
            )
            perf.chmod(0o755)
            runner = root / "runner"
            runner.write_text("#!/bin/sh\nprintf '{\"counters\": {}}\\n'\n")
            runner.chmod(0o755)
            parent_path = f"{root}{os.pathsep}{os.environ.get('PATH', '')}"
            for parent_perf, child_perf, expected in (
                (True, False, False),
                (False, True, True),
            ):
                with self.subTest(parent_perf=parent_perf, child_perf=child_perf):
                    marker.unlink(missing_ok=True)
                    with patch.dict(
                        os.environ,
                        {"PATH": parent_path},
                        clear=False,
                    ), patch("tools.run_native_g7.sys.platform", "linux"):
                        if parent_perf:
                            os.environ["HYPHAE_G7_PERF"] = "1"
                        else:
                            os.environ.pop("HYPHAE_G7_PERF", None)
                        child_environment = os.environ.copy()
                        if child_perf:
                            child_environment["HYPHAE_G7_PERF"] = "1"
                        else:
                            child_environment.pop("HYPHAE_G7_PERF", None)
                        child_environment["PERF_MARKER"] = str(marker)
                        run_cell(
                            runner,
                            self.SOURCE_COMMIT,
                            "linux",
                            "warm",
                            1,
                            environment=child_environment,
                        )
                    self.assertEqual(marker.exists(), expected)

    @unittest.skipUnless(os.name == "posix", "requires a POSIX executable fixture")
    def test_run_cell_does_not_block_on_large_final_json(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            runner = Path(directory) / "runner"
            runner.write_text(
                "#!/usr/bin/env python3\n"
                "import json\n"
                "print(json.dumps({'counters': {}, 'padding': 'x' * 1_100_000}))\n"
            )
            runner.chmod(0o755)
            payload = run_cell(
                runner,
                self.SOURCE_COMMIT,
                "linux",
                "warm",
                1,
                timeout_seconds=2.0,
            )
            self.assertEqual(len(payload["padding"]), 1_100_000)

    def test_calibration_pilot_strips_smoke_corpus_overrides(self) -> None:
        captured: dict[str, str] = {}

        def capture_environment(*args, **kwargs):
            child_environment = (
                kwargs["environment"] if "environment" in kwargs else args[5]
            )
            captured.update(child_environment)
            raise RuntimeError("captured")

        with tempfile.TemporaryDirectory() as directory, patch(
            "tools.run_native_g7.run_cell",
            side_effect=capture_environment,
        ):
            root = Path(directory)
            with self.assertRaisesRegex(RuntimeError, "captured"):
                run_calibration_pilot(
                    root / "runner",
                    commit=self.SOURCE_COMMIT,
                    source_tree="2" * 40,
                    platform="linux",
                    state="warm",
                    concurrency=1,
                    environment={
                        "HYPHAE_G7_SMOKE": "1",
                        "HYPHAE_G7_SEARCH_DOCUMENTS": "128",
                        "HYPHAE_G7_VECTOR_DIMENSION": "16",
                    },
                    receipt_path=root / "receipt.json",
                    progress_path=root / "progress.json",
                    partial_path=root / "partial.json",
                    timeout_seconds=1,
                    stall_timeout_seconds=1,
                )
        self.assertNotIn("HYPHAE_G7_SMOKE", captured)
        self.assertNotIn("HYPHAE_G7_SEARCH_DOCUMENTS", captured)
        self.assertNotIn("HYPHAE_G7_VECTOR_DIMENSION", captured)

    def test_pilot_budget_rejects_nonclosure_corpus(self) -> None:
        for field, value in (
            ("search_documents", 128),
            ("vector_count", 128),
            ("vector_dimension", 16),
        ):
            with self.subTest(field=field):
                pilot = self.pilot_receipt()
                pilot["dataset"][field] = value
                with self.assertRaisesRegex(
                    RuntimeBudgetExceeded,
                    "exact closure corpus",
                ):
                    derive_cell_runtime_budget(
                        pilot,
                        expected_commit=self.SOURCE_COMMIT,
                        expected_platform="linux",
                        expected_state="warm",
                        expected_concurrency=1,
                        observations=1_000_000,
                        warmup=100_000,
                        hard_cap_seconds=7_200,
                        seed_primed=True,
                    )

    def test_pilot_search_evidence_is_validated_before_budget_projection(self) -> None:
        from tools.test_check_native_g7_receipt import (
            _strict_group_commit_evidence,
            receipt as closure_receipt,
        )

        pilot = closure_receipt()
        pilot["dataset"]["observations"] = PILOT_OBSERVATIONS
        ann = pilot["cells"]["ann-top10-recall-095"]
        ann["ann_routing_interval"]["observations"] = PILOT_OBSERVATIONS
        ann["ann_routing_interval"]["selected_certified"] = PILOT_OBSERVATIONS
        ann["ann_routing_interval"]["next_partition_lower_bound_present"] = (
            PILOT_OBSERVATIONS
        )
        ann["ann_routing_interval"]["targeted_single_batches"] = PILOT_OBSERVATIONS
        ann["ann_routing_interval"]["generic_single_fallback_batches"] = 0
        bm25 = pilot["cells"]["bm25-top10"]
        bm25_interval = bm25["lexical_read_view_query_interval"]
        bm25_interval["observations"] = PILOT_OBSERVATIONS
        bm25_interval["postings_evaluated"] = PILOT_OBSERVATIONS
        bm25_interval["execution_sequence_last"] = (
            bm25_interval["execution_sequence_first"] + PILOT_OBSERVATIONS - 1
        )
        filtered = pilot["cells"]["filtered-bm25-top10"]
        filtered_interval = filtered["filtered_lexical_read_view_query_interval"]
        for field in (
            "observations",
            "postings_scored",
            "filter_records_evaluated",
            "filter_records_matched",
        ):
            filtered_interval[field] = PILOT_OBSERVATIONS
        filtered_interval["execution_sequence_last"] = (
            filtered_interval["execution_sequence_first"] + PILOT_OBSERVATIONS - 1
        )
        hybrid = pilot["cells"]["hybrid-top10"]
        hybrid_interval = hybrid["hybrid_read_view_query_interval"]
        hybrid_interval["observations"] = PILOT_OBSERVATIONS
        for field in (
            "peak_admission_executions",
            "result_retention_executions",
            "fusion_executions",
        ):
            hybrid_interval[field] = PILOT_OBSERVATIONS
        hybrid["hybrid_ann_routing_interval"]["observations"] = PILOT_OBSERVATIONS
        hybrid["hybrid_ann_routing_interval"]["selected_certified"] = PILOT_OBSERVATIONS
        hybrid["hybrid_ann_routing_interval"]["next_partition_lower_bound_present"] = (
            PILOT_OBSERVATIONS
        )
        hybrid["hybrid_ann_routing_interval"][
            "targeted_single_batches"
        ] = PILOT_OBSERVATIONS
        hybrid["hybrid_ann_routing_interval"]["generic_single_fallback_batches"] = 0
        pilot["cells"]["strict-group-commit"]["group_commit_evidence"] = (
            _strict_group_commit_evidence(observations=PILOT_OBSERVATIONS)
        )
        validate_pilot_search_evidence(pilot, "a" * 40)

        strict = pilot["cells"]["strict-group-commit"]["group_commit_evidence"]
        strict["submission_mode"] = "natural-timed-collection-v1"
        with self.assertRaisesRegex(GateFailure, "group-commit configuration"):
            validate_pilot_search_evidence(pilot, "a" * 40)
        strict["submission_mode"] = "explicit-bounded-cohort-v1"

        for cell_name, interval_name in (
            ("ann-top10-recall-095", "ann_routing_interval"),
            ("hybrid-top10", "hybrid_ann_routing_interval"),
        ):
            interval = pilot["cells"][cell_name][interval_name]
            interval["targeted_single_batches"] -= 1
            interval["generic_single_fallback_batches"] = 1
            with self.assertRaisesRegex(GateFailure, "targeted routing evidence"):
                validate_pilot_search_evidence(pilot, "a" * 40)
            interval["targeted_single_batches"] = PILOT_OBSERVATIONS
            interval["generic_single_fallback_batches"] = 0

        pilot["schema"] = "hyphae-native-g7-receipt-v3"
        with self.assertRaisesRegex(RuntimeError, "identity or open state"):
            validate_pilot_search_evidence(pilot, "a" * 40)
        pilot["schema"] = "hyphae-native-g7-receipt-v4"
        bm25_interval["process_physical_page_reads"] = 1
        with self.assertRaisesRegex(GateFailure, "BM25 read-view interval"):
            validate_pilot_search_evidence(pilot, "a" * 40)
        bm25_interval["process_physical_page_reads"] = 0

        filtered_interval["filter_records_matched"] = PILOT_OBSERVATIONS - 1
        with self.assertRaisesRegex(GateFailure, "filtered BM25 read-view interval"):
            validate_pilot_search_evidence(pilot, "a" * 40)
        filtered_interval["filter_records_matched"] = PILOT_OBSERVATIONS

        hybrid["hybrid_read_view_query_interval"]["physical_page_reads"] = 1
        with self.assertRaisesRegex(GateFailure, "storage or materialization"):
            validate_pilot_search_evidence(pilot, "a" * 40)

    def test_receipt_progress_and_partial_must_bind_same_dataset(self) -> None:
        receipt = self.pilot_receipt()
        progress = self.completed_cell_progress()
        partial = self.partial_receipt()
        partial.update({
            "status": "completed",
            "completed_count": len(G7_SURFACES),
            "current_cell": None,
            "cells": json.loads(json.dumps(receipt["cells"])),
        })
        validate_cross_artifact_dataset(
            receipt,
            progress,
            partial,
            expected_observations=PILOT_OBSERVATIONS,
            expected_warmup=PILOT_WARMUP,
        )
        for artifact in (progress, partial):
            with self.subTest(artifact=artifact["schema"]):
                drifted = dict(artifact)
                drifted["dataset_digest"] = "f" * 64
                with self.assertRaisesRegex(RuntimeError, "another dataset"):
                    validate_cross_artifact_dataset(
                        receipt,
                        drifted if artifact is progress else progress,
                        drifted if artifact is partial else partial,
                        expected_observations=PILOT_OBSERVATIONS,
                        expected_warmup=PILOT_WARMUP,
                    )

    def test_terminal_partial_must_preserve_strict_commit_evidence(self) -> None:
        receipt = self.pilot_receipt()
        progress = self.completed_cell_progress()
        partial = self.partial_receipt()
        partial.update({
            "status": "completed",
            "completed_count": len(G7_SURFACES),
            "current_cell": None,
            "cells": json.loads(json.dumps(receipt["cells"])),
        })
        validate_cross_artifact_dataset(
            receipt,
            progress,
            partial,
            expected_observations=PILOT_OBSERVATIONS,
            expected_warmup=PILOT_WARMUP,
        )
        partial["cells"]["strict-group-commit"]["group_commit_evidence"][
            "commit_receipt_digest"
        ] = "f" * 64
        with self.assertRaisesRegex(RuntimeError, "terminal partial differs"):
            validate_cross_artifact_dataset(
                receipt,
                progress,
                partial,
                expected_observations=PILOT_OBSERVATIONS,
                expected_warmup=PILOT_WARMUP,
            )

    def test_terminal_partial_must_preserve_strict_maintenance_evidence(self) -> None:
        receipt = self.pilot_receipt()
        progress = self.completed_cell_progress()
        partial = self.partial_receipt()
        partial.update({
            "status": "completed",
            "completed_count": len(G7_SURFACES),
            "current_cell": None,
            "cells": json.loads(json.dumps(receipt["cells"])),
        })
        partial["cells"]["strict-group-commit"]["group_commit_evidence"][
            "maintenance"
        ]["checkpoint"]["manifest_digest"] = "f" * 64
        with self.assertRaisesRegex(RuntimeError, "terminal partial differs"):
            validate_cross_artifact_dataset(
                receipt,
                progress,
                partial,
                expected_observations=PILOT_OBSERVATIONS,
                expected_warmup=PILOT_WARMUP,
            )

    def test_terminal_partial_status_must_be_completed(self) -> None:
        receipt = self.pilot_receipt()
        progress = self.completed_cell_progress()
        partial = self.partial_receipt()
        partial.update({
            "status": "running",
            "completed_count": len(G7_SURFACES),
            "current_cell": None,
            "cells": json.loads(json.dumps(receipt["cells"])),
        })
        with self.assertRaisesRegex(RuntimeError, "terminal partial differs"):
            validate_cross_artifact_dataset(
                receipt,
                progress,
                partial,
                expected_observations=PILOT_OBSERVATIONS,
                expected_warmup=PILOT_WARMUP,
            )

    def test_short_pilot_derives_a_bounded_full_cell_budget(self) -> None:
        budget = derive_cell_runtime_budget(
            self.pilot_receipt(),
            expected_commit=self.SOURCE_COMMIT,
            expected_platform="linux",
            expected_state="warm",
            expected_concurrency=1,
            observations=1_000_000,
            warmup=100_000,
            hard_cap_seconds=7_200,
            seed_primed=True,
        )
        self.assertEqual(
            budget["method"],
            "exact-runner-short-pilot-with-matched-c1-warmup-v4",
        )
        self.assertEqual(
            budget["warmup_projection"],
            {
                "method": "matched-state-background-c1-p99-linear-v1",
                "source_concurrency": 1,
            },
        )
        self.assertEqual(
            budget["seed_treatment"],
            "measured-after-identical-seed-prime",
        )
        self.assertEqual(budget["full_observations"], 1_000_000)
        self.assertEqual(budget["full_warmup"], 100_000)
        self.assertGreater(budget["timeout_seconds"], budget["expected_seconds"])
        self.assertLessEqual(budget["timeout_seconds"], 7_200)
        projection = budget["strict_group_commit_correctness_projection"]
        self.assertEqual(
            projection["method"],
            "linear-pilot-maintenance-reopen-full-key-verification-v2",
        )
        self.assertAlmostEqual(
            projection["full_seconds"],
            projection["pilot_seconds"] * 100,
        )

    def test_budget_rejects_missing_commit_maintenance_or_reopen_cost(self) -> None:
        for field in ("maintenance", "reopen"):
            with self.subTest(field=field):
                pilot = self.pilot_receipt()
                del pilot["cells"]["strict-group-commit"]["group_commit_evidence"][field]
                with self.assertRaisesRegex(
                    RuntimeBudgetExceeded, "maintenance/reopen evidence"
                ):
                    derive_cell_runtime_budget(
                        pilot,
                        expected_commit=self.SOURCE_COMMIT,
                        expected_platform="linux",
                        expected_state="warm",
                        expected_concurrency=1,
                        observations=1_000_000,
                        warmup=100_000,
                        hard_cap_seconds=7_200,
                        seed_primed=True,
                    )

    def test_high_tail_does_not_project_every_observation_at_p99(self) -> None:
        pilot = self.pilot_receipt(throughput=10_000.0)
        for cell in pilot["cells"].values():
            cell["p99"] = 1_000_000
        budget = derive_cell_runtime_budget(
            pilot,
            expected_commit=self.SOURCE_COMMIT,
            expected_platform="linux",
            expected_state="warm",
            expected_concurrency=1,
            observations=1_000_000,
            warmup=100_000,
            hard_cap_seconds=7_200,
            seed_primed=True,
        )
        self.assertAlmostEqual(
            budget["surface_seconds"]["strict-group-commit"],
            100.0005,
        )
        self.assertLessEqual(budget["timeout_seconds"], 7_200)

    def test_p99_does_not_change_measurement_runtime_projection(self) -> None:
        lower_tail = self.pilot_receipt(throughput=10_000.0)
        higher_tail = self.pilot_receipt(throughput=10_000.0)
        lower_tail["cells"]["strict-group-commit"]["p99"] = 100_000
        higher_tail["cells"]["strict-group-commit"]["p99"] = 10_000_000
        arguments = {
            "expected_commit": self.SOURCE_COMMIT,
            "expected_platform": "linux",
            "expected_state": "warm",
            "expected_concurrency": 1,
            "observations": 1_000_000,
            "warmup": 100_000,
            "hard_cap_seconds": 7_200,
            "seed_primed": True,
        }
        lower_budget = derive_cell_runtime_budget(lower_tail, **arguments)
        higher_budget = derive_cell_runtime_budget(higher_tail, **arguments)
        self.assertEqual(
            lower_budget["surface_seconds"]["strict-group-commit"],
            higher_budget["surface_seconds"]["strict-group-commit"],
        )

    def test_budget_projects_serial_warmup_from_the_matched_c1_pilot(self) -> None:
        pilot = self.pilot_receipt(throughput=1_000_000_000.0)
        for cell in pilot["cells"].values():
            cell["p99"] = 1_000_000
        pilot["concurrency"] = 32
        pilot["cells"]["strict-group-commit"]["group_commit_evidence"][
            "producer_concurrency"
        ] = 32
        pilot["cells"]["strict-group-commit"]["group_commit_evidence"][
            "maximum_active_producers"
        ] = 32
        serial_pilot = self.pilot_receipt(throughput=1_000_000_000.0)
        for cell in serial_pilot["cells"].values():
            cell["p99"] = 100_000
        budget = derive_cell_runtime_budget(
            pilot,
            serial_warmup_pilot=serial_pilot,
            expected_commit=self.SOURCE_COMMIT,
            expected_platform="linux",
            expected_state="warm",
            expected_concurrency=32,
            observations=1_000_000,
            warmup=100_000,
            hard_cap_seconds=7_200,
            seed_primed=True,
        )
        self.assertAlmostEqual(
            budget["surface_seconds"]["embedded-structure-point-get"],
            10.001,
        )

    def test_c32_queue_tail_does_not_inflate_the_serial_warmup_projection(self) -> None:
        pilot = self.pilot_receipt(throughput=4_000.0)
        pilot["concurrency"] = 32
        for cell in pilot["cells"].values():
            cell["p99"] = 12_000_000
        pilot["cells"]["strict-group-commit"]["group_commit_evidence"].update({
            "producer_concurrency": 32,
            "maximum_active_producers": 32,
        })
        serial_pilot = self.pilot_receipt(throughput=500_000.0)
        for cell in serial_pilot["cells"].values():
            cell["p99"] = 2_000
        budget = derive_cell_runtime_budget(
            pilot,
            serial_warmup_pilot=serial_pilot,
            expected_commit=self.SOURCE_COMMIT,
            expected_platform="linux",
            expected_state="warm",
            expected_concurrency=32,
            observations=1_000_000,
            warmup=100_000,
            hard_cap_seconds=7_200,
            seed_primed=True,
        )
        self.assertAlmostEqual(
            budget["surface_seconds"]["embedded-structure-point-get"],
            250.2,
        )
        self.assertLessEqual(budget["timeout_seconds"], 7_200)

    def test_concurrent_budget_requires_exact_matched_c1_warmup_evidence(self) -> None:
        pilot = self.pilot_receipt()
        pilot["concurrency"] = 32
        pilot["cells"]["strict-group-commit"]["group_commit_evidence"].update({
            "producer_concurrency": 32,
            "maximum_active_producers": 32,
        })
        with self.assertRaisesRegex(RuntimeBudgetExceeded, "matched C1"):
            derive_cell_runtime_budget(
                pilot,
                expected_commit=self.SOURCE_COMMIT,
                expected_platform="linux",
                expected_state="warm",
                expected_concurrency=32,
                observations=1_000_000,
                warmup=100_000,
                hard_cap_seconds=7_200,
                seed_primed=True,
            )

    def test_short_pilot_fails_early_when_projection_exceeds_hard_cap(self) -> None:
        with self.assertRaisesRegex(
            RuntimeBudgetExceeded,
            "above the authorized cap.*slowest_surface",
        ):
            derive_cell_runtime_budget(
                self.pilot_receipt(throughput=100.0),
                expected_commit=self.SOURCE_COMMIT,
                expected_platform="linux",
                expected_state="warm",
                expected_concurrency=1,
                observations=1_000_000,
                warmup=100_000,
                hard_cap_seconds=7_200,
                seed_primed=True,
            )

    def test_warm_control_pilot_fails_fast_on_normative_target_miss(self) -> None:
        for percentile, value, target in (
            ("p50", 250_001, 250_000),
            ("p99", 900_001, 900_000),
        ):
            with self.subTest(percentile=percentile):
                pilot = self.warm_control_pilot()
                pilot["cells"]["ann-top10-recall-095"][percentile] = value
                with self.assertRaisesRegex(
                    RuntimeBudgetExceeded,
                    f"ann-top10-recall-095.*{percentile}={value}.*target={target}",
                ):
                    validate_warm_control_pilot_latency(pilot)

    def test_warm_control_pilot_reports_every_normative_target_miss(self) -> None:
        pilot = self.warm_control_pilot()
        pilot["cells"]["ann-top10-recall-095"]["p50"] = 286_523
        pilot["cells"]["local-structure-point-get"]["p50"] = 27_507
        with self.assertRaises(RuntimeBudgetExceeded) as raised:
            validate_warm_control_pilot_latency(pilot)
        message = str(raised.exception)
        ann = "ann-top10-recall-095.p50=286523, target=250000"
        local = "local-structure-point-get.p50=27507, target=25000"
        self.assertIn(ann, message)
        self.assertIn(local, message)
        self.assertLess(message.index(ann), message.index(local))

    def test_warm_control_pilot_excludes_advisory_commit_target(self) -> None:
        pilot = self.warm_control_pilot()
        pilot["cells"]["strict-group-commit"]["p50"] = 20_000_000
        pilot["cells"]["strict-group-commit"]["p99"] = 40_000_000
        validate_warm_control_pilot_latency(pilot)

    def test_warm_control_pilot_rejects_noncontrol_identity(self) -> None:
        pilot = self.warm_control_pilot()
        pilot["background_interference"] = {"status": "measured"}
        with self.assertRaisesRegex(RuntimeBudgetExceeded, "identity"):
            validate_warm_control_pilot_latency(pilot)

    def test_short_pilot_is_exact_sha_and_complete_surface_evidence(self) -> None:
        pilot = self.pilot_receipt()
        pilot["source_commit"] = "f" * 40
        with self.assertRaisesRegex(RuntimeBudgetExceeded, "identity or coverage"):
            derive_cell_runtime_budget(
                pilot,
                expected_commit=self.SOURCE_COMMIT,
                expected_platform="linux",
                expected_state="warm",
                expected_concurrency=1,
                observations=1_000_000,
                warmup=100_000,
                hard_cap_seconds=7_200,
                seed_primed=True,
            )

    def test_budget_rejects_a_pilot_that_includes_one_time_seed_cost(self) -> None:
        with self.assertRaisesRegex(RuntimeBudgetExceeded, "runtime bounds"):
            derive_cell_runtime_budget(
                self.pilot_receipt(),
                expected_commit=self.SOURCE_COMMIT,
                expected_platform="linux",
                expected_state="warm",
                expected_concurrency=1,
                observations=1_000_000,
                warmup=100_000,
                hard_cap_seconds=7_200,
                seed_primed=False,
            )

    def test_matrix_budget_is_accepted_before_measurements_start(self) -> None:
        budget = derive_cell_runtime_budget(
            self.pilot_receipt(),
            expected_commit=self.SOURCE_COMMIT,
            expected_platform="linux",
            expected_state="warm",
            expected_concurrency=1,
            observations=1_000_000,
            warmup=100_000,
            hard_cap_seconds=7_200,
            seed_primed=True,
        )
        plan = derive_matrix_runtime_plan(
            calibration_seconds=600,
            cell_budgets=[budget] * 6,
            hard_cap_seconds=39_600,
            expected_cell_count=6,
        )
        self.assertEqual(plan["status"], "accepted")
        self.assertEqual(plan["cell_count"], 6)
        self.assertLessEqual(plan["planned_total_seconds"], 39_600)

    def test_matrix_budget_fails_before_measurement_when_plan_cannot_fit(self) -> None:
        budget = {
            "schema": "hyphae-native-g7-runtime-budget-v4",
            "timeout_seconds": 7_200,
        }
        with self.assertRaisesRegex(
            RuntimeBudgetExceeded,
            "exceed the matrix cap.*total=.*cap=39600",
        ):
            derive_matrix_runtime_plan(
                calibration_seconds=600,
                cell_budgets=[budget] * 6,
                hard_cap_seconds=39_600,
                expected_cell_count=6,
            )

    def test_matrix_runtime_plan_rejects_legacy_v3_cell_budget(self) -> None:
        budget = {
            "schema": "hyphae-native-g7-runtime-budget-v3",
            "timeout_seconds": 1,
        }
        with self.assertRaisesRegex(RuntimeBudgetExceeded, "invalid cell budget"):
            derive_matrix_runtime_plan(
                calibration_seconds=0,
                cell_budgets=[budget],
                hard_cap_seconds=2,
                expected_cell_count=1,
            )

    def test_invalid_runner_output_never_becomes_a_validated_cell_artifact(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "validated.json"
            with self.assertRaisesRegex(RuntimeError, "identity or normative"):
                persist_validated_cell_checkpoint(
                    path,
                    {"source_commit": "f" * 40},
                    expected_commit=self.SOURCE_COMMIT,
                    expected_tree="2" * 40,
                    expected_platform="linux",
                    expected_state="warm",
                    expected_concurrency=1,
                    background_mode="control",
                    hardware={},
                    build={},
                    calibration_executable_blake3="8" * 64,
                    runtime_budget={},
                )
            self.assertFalse(path.exists())

    def test_partial_receipt_preserves_exact_sha_and_completed_surfaces(self) -> None:
        receipt = self.partial_receipt()
        validated = validate_partial_receipt(
            receipt,
            expected_commit=self.SOURCE_COMMIT,
            expected_tree="2" * 40,
            expected_platform="linux",
            expected_state="warm",
            expected_concurrency=1,
        )
        self.assertEqual(validated["completed_count"], 1)
        self.assertEqual(set(validated["cells"]), {G7_SURFACES[0]})

    def test_partial_receipt_rejects_identity_or_count_drift(self) -> None:
        for field, value in (
            ("source_commit", "f" * 40),
            ("completed_count", 2),
        ):
            with self.subTest(field=field):
                receipt = self.partial_receipt()
                receipt[field] = value
                with self.assertRaisesRegex(RuntimeError, "identity|inconsistent"):
                    validate_partial_receipt(
                        receipt,
                        expected_commit=self.SOURCE_COMMIT,
                        expected_tree="2" * 40,
                        expected_platform="linux",
                        expected_state="warm",
                        expected_concurrency=1,
                    )

    def test_progress_watchdog_stall_reports_last_stage_progress_and_eta(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "runner-progress.json"
            watchdog = ProgressWatchdog(path, timeout_seconds=5.0, started=5.0)
            path.write_text(json.dumps({
                "sequence": 7,
                "status": "running",
                "stage": "ann-child-build",
                "completed_units": 250_000,
                "total_units": 1_000_000,
                "details": {
                    "eta": {
                        "status": "estimated",
                        "estimated_remaining_nanos": 123,
                    },
                },
            }))
            watchdog.observe(10.0)
            with self.assertRaises(RuntimeError) as context:
                watchdog.observe(15.0)
            message = str(context.exception)
            self.assertIn("stage='ann-child-build'", message)
            self.assertIn("completed=250000/1000000", message)
            self.assertIn(
                'eta={"estimated_remaining_nanos":123,"status":"estimated"}',
                message,
            )

    def test_progress_watchdog_tracks_sequence_and_stops_after_completion(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "runner-progress.json"
            watchdog = ProgressWatchdog(path, timeout_seconds=5.0, started=10.0)
            path.write_text(json.dumps({
                "operation": "g7-cell",
                "stage": "surface-measure",
                "sequence": 1,
                "status": "running",
            }))
            watchdog.observe(14.0)
            path.write_text(json.dumps({
                "operation": "g7-cell",
                "stage": "cell-completed",
                "sequence": 2,
                "status": "completed",
            }))
            watchdog.observe(18.0)
            watchdog.observe(100.0)

    def test_progress_watchdog_keeps_watching_after_ann_publication(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "runner-progress.json"
            watchdog = ProgressWatchdog(path, timeout_seconds=5.0, started=10.0)
            path.write_text(json.dumps({
                "operation": "ann-bulk-build",
                "stage": "ann-published",
                "sequence": 2,
                "status": "completed",
            }))
            watchdog.observe(12.0)
            with self.assertRaisesRegex(RuntimeError, "stalled for 5s"):
                watchdog.observe(17.0)


if __name__ == "__main__":
    unittest.main()
