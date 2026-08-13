#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only

"""Run the complete controlled G7 state/concurrency matrix for one platform."""

from __future__ import annotations

import argparse
import ctypes
from dataclasses import dataclass
import hashlib
import json
import math
import os
import platform as platform_module
import signal
import shutil
import subprocess
import sys
import tempfile
import time
import xml.etree.ElementTree as ElementTree
from pathlib import Path
from typing import TextIO

if os.name == "posix":
    import resource

try:
    from tools.prepare_native_g7_macos_template import prepare as prepare_macos_template
    from tools.check_native_g7_receipt import (
        DEFAULT_AUTHORITY,
        validate_ann_read_view_cell,
        validate_bm25_read_view_cell,
        validate_filtered_bm25_read_view_cell,
        validate_hybrid_read_view_cell,
        validate_strict_group_commit_cell,
    )
    from tools.check_native_performance_receipt import validate_progress
except ModuleNotFoundError:
    from prepare_native_g7_macos_template import prepare as prepare_macos_template
    from check_native_g7_receipt import (
        DEFAULT_AUTHORITY,
        validate_ann_read_view_cell,
        validate_bm25_read_view_cell,
        validate_filtered_bm25_read_view_cell,
        validate_hybrid_read_view_cell,
        validate_strict_group_commit_cell,
    )
    from check_native_performance_receipt import validate_progress


ROOT = Path(__file__).resolve().parents[1]
STATES = ("warm",)
CONCURRENCIES = (1, 8, 32)
BACKGROUND_MODES = ("control", "interference")
ACTIVE_PROCESS: subprocess.Popen[str] | None = None
MAX_INITIAL_ANN_BULK_PARTITIONS = 111
G7_LOGICAL_ANN_PARTITIONS = 64
G7_PREFERRED_ANN_PARTITIONS = 32
G7_ANN_PARTITION_POLICY = "g7-fixed-64-logical-partitions-v1"
G7_RECEIPT_SCHEMA = "hyphae-native-g7-receipt-v4"
G7_SURFACES = (
    "embedded-structure-point-get",
    "embedded-prepared-sql-primary-key",
    "local-structure-point-get",
    "local-prepared-sql-primary-key",
    "indexed-sql-bounded-read",
    "two-index-join-bounded-read",
    "bm25-top10",
    "filtered-bm25-top10",
    "ann-top10-recall-095",
    "hybrid-top10",
    "strict-group-commit",
)
PILOT_OBSERVATIONS = 10_000
PILOT_WARMUP = 1_000
PILOT_BUDGET_MULTIPLIER = 1.1
PILOT_BUDGET_RESERVE_SECONDS = 300.0
PROCESS_ERROR_TAIL_CHARS = 4_096
CLOSURE_SEARCH_DOCUMENTS = 1_000_000
CLOSURE_VECTOR_COUNT = 1_000_000
CLOSURE_VECTOR_DIMENSION = 384
CLOSURE_OVERRIDE_VARIABLES = (
    "HYPHAE_G7_SMOKE",
    "HYPHAE_G7_SEARCH_DOCUMENTS",
    "HYPHAE_G7_VECTOR_DIMENSION",
)


class ProgressStalled(RuntimeError):
    """The runner stopped producing measurable progress."""


class RuntimeBudgetExceeded(RuntimeError):
    """Measured work cannot complete inside the authorized runtime cap."""


@dataclass
class MatrixCellPlan:
    state: str
    background_mode: str
    concurrency: int
    pilot_receipt_path: Path
    pilot_progress_path: Path
    pilot_partial_path: Path
    partial_receipt_path: Path
    runner_receipt_path: Path
    validated_receipt_path: Path
    runtime_budget: dict[str, object] | None = None

    def diagnostic(self, phase: str) -> dict[str, object]:
        diagnostic: dict[str, object] = {
            "state": self.state,
            "background_mode": self.background_mode,
            "concurrency": self.concurrency,
            "phase": phase,
            "pilot_receipt": str(self.pilot_receipt_path),
            "partial_receipt": str(self.partial_receipt_path),
            "runner_receipt": str(self.runner_receipt_path),
            "validated_cell_receipt": str(self.validated_receipt_path),
        }
        if self.runtime_budget is not None:
            diagnostic["runtime_budget"] = self.runtime_budget
        return diagnostic


class ProgressWatchdog:
    def __init__(
        self,
        path: Path,
        timeout_seconds: float,
        started: float,
        expected_commit: str | None = None,
    ) -> None:
        self.path = path
        self.timeout_seconds = timeout_seconds
        self.expected_commit = expected_commit
        self.last_activity = started
        self.last_sequence: int | None = None
        self.last_payload: dict[str, object] | None = None
        self.last_progress_summary = "no progress payload observed"
        self.completed = False

    def observe(self, now: float) -> None:
        if self.path.is_file():
            payload = json.loads(self.path.read_text(encoding="utf-8"))
            if not isinstance(payload, dict):
                raise RuntimeError("G7 runner progress must be an object")
            sequence = payload.get("sequence")
            if not isinstance(sequence, int) or isinstance(sequence, bool) or sequence <= 0:
                raise RuntimeError("G7 runner progress has an invalid sequence")
            if self.expected_commit is not None:
                validate_progress(payload, self.expected_commit)
            if self.last_sequence is None or sequence > self.last_sequence:
                if self.expected_commit is not None and self.last_payload is not None:
                    validate_progress(payload, self.expected_commit, self.last_payload)
                self.last_sequence = sequence
                self.last_activity = now
                self.last_payload = payload
            elif sequence < self.last_sequence:
                raise RuntimeError("G7 runner progress sequence regressed")
            details = payload.get("details")
            eta = details.get("eta") if isinstance(details, dict) else None
            self.last_progress_summary = (
                f"sequence={sequence}, stage={payload.get('stage')!r}, "
                f"completed={payload.get('completed_units')!r}/"
                f"{payload.get('total_units')!r}, eta="
                f"{json.dumps(eta, sort_keys=True, separators=(',', ':'))}"
            )
            self.completed = (
                payload.get("operation") == "g7-cell"
                and payload.get("stage") == "cell-completed"
                and payload.get("status") == "completed"
            )
        if not self.completed and now - self.last_activity >= self.timeout_seconds:
            raise ProgressStalled(
                f"G7 runner progress stalled for {self.timeout_seconds:.0f}s; "
                f"last progress: {self.last_progress_summary}"
            )


def write_json_atomic(path: Path, payload: dict[str, object]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    descriptor, temporary_name = tempfile.mkstemp(
        prefix=f".{path.name}.", suffix=".tmp", dir=path.parent
    )
    temporary = Path(temporary_name)
    try:
        with os.fdopen(descriptor, "w", encoding="utf-8") as output:
            json.dump(payload, output, indent=2, sort_keys=True)
            output.write("\n")
            output.flush()
            os.fsync(output.fileno())
        temporary.replace(path)
    except BaseException:
        temporary.unlink(missing_ok=True)
        raise


def closure_environment(environment: dict[str, str]) -> dict[str, str]:
    sanitized = environment.copy()
    for name in CLOSURE_OVERRIDE_VARIABLES:
        sanitized.pop(name, None)
    return sanitized


def validate_receipt_dataset(
    dataset: object,
    *,
    expected_observations: int,
    expected_warmup: int,
) -> str:
    if (
        not isinstance(dataset, dict)
        or dataset.get("observations") != expected_observations
        or dataset.get("warmup") != expected_warmup
        or dataset.get("search_documents") != CLOSURE_SEARCH_DOCUMENTS
        or dataset.get("vector_count") != CLOSURE_VECTOR_COUNT
        or dataset.get("vector_dimension") != CLOSURE_VECTOR_DIMENSION
    ):
        raise RuntimeError("G7 receipt dataset differs from the exact closure corpus")
    digest = dataset.get("digest")
    if not isinstance(digest, str) or len(digest) != 64 or any(
        character not in "0123456789abcdef" for character in digest
    ):
        raise RuntimeError("G7 receipt dataset digest is invalid")
    return digest


def validate_cross_artifact_dataset(
    receipt: object,
    progress: object,
    partial: object,
    *,
    expected_observations: int,
    expected_warmup: int,
) -> None:
    if not isinstance(receipt, dict):
        raise RuntimeError("G7 runner receipt must be an object")
    digest = validate_receipt_dataset(
        receipt.get("dataset"),
        expected_observations=expected_observations,
        expected_warmup=expected_warmup,
    )
    if (
        not isinstance(progress, dict)
        or not isinstance(partial, dict)
        or progress.get("dataset_digest") != digest
        or partial.get("dataset_digest") != digest
    ):
        raise RuntimeError("G7 receipt, progress, or partial targets another dataset")
    receipt_cells = receipt.get("cells")
    partial_cells = partial.get("cells")
    if (
        partial.get("status") != "completed"
        or partial.get("completed_count") != len(G7_SURFACES)
        or partial.get("current_cell") is not None
        or not isinstance(receipt_cells, dict)
        or not isinstance(partial_cells, dict)
        or partial_cells != receipt_cells
    ):
        raise RuntimeError("G7 terminal partial differs from the final measured cells")


def validate_partial_receipt(
    payload: object,
    *,
    expected_commit: str,
    expected_tree: str,
    expected_platform: str,
    expected_state: str,
    expected_concurrency: int,
) -> dict[str, object]:
    fields = {
        "schema", "source_commit", "source_tree", "dataset_digest", "platform",
        "state", "concurrency", "sequence", "status", "completed_count",
        "total_cells", "current_cell", "cells",
    }
    if not isinstance(payload, dict) or set(payload) != fields:
        raise RuntimeError("G7 partial receipt fields mismatch")
    if (
        payload["schema"] != "hyphae-native-g7-partial-receipt-v1"
        or payload["source_commit"] != expected_commit
        or payload["source_tree"] != expected_tree
        or payload["platform"] != expected_platform
        or payload["state"] != expected_state
        or payload["concurrency"] != expected_concurrency
    ):
        raise RuntimeError("G7 partial receipt identity mismatch")
    digest = payload["dataset_digest"]
    if not isinstance(digest, str) or len(digest) != 64 or any(
        character not in "0123456789abcdef" for character in digest
    ):
        raise RuntimeError("G7 partial receipt dataset digest is invalid")
    sequence = payload["sequence"]
    cells = payload["cells"]
    completed_count = payload["completed_count"]
    if (
        not isinstance(sequence, int)
        or isinstance(sequence, bool)
        or sequence <= 0
        or not isinstance(cells, dict)
        or not set(cells).issubset(G7_SURFACES)
        or not isinstance(completed_count, int)
        or isinstance(completed_count, bool)
        or completed_count != len(cells)
        or payload["total_cells"] != len(G7_SURFACES)
    ):
        raise RuntimeError("G7 partial receipt progress is inconsistent")
    status = payload["status"]
    current_cell = payload["current_cell"]
    if status == "running":
        if current_cell is not None and current_cell not in G7_SURFACES:
            raise RuntimeError("G7 partial receipt current cell is invalid")
    elif status == "completed":
        if completed_count != len(G7_SURFACES) or current_cell is not None:
            raise RuntimeError("completed G7 partial receipt is incomplete")
    else:
        raise RuntimeError("G7 partial receipt status is invalid")
    return payload


def derive_cell_runtime_budget(
    pilot: object,
    *,
    expected_commit: str,
    expected_platform: str,
    expected_state: str,
    expected_concurrency: int,
    observations: int,
    warmup: int,
    hard_cap_seconds: float,
    seed_primed: bool,
) -> dict[str, object]:
    if not isinstance(pilot, dict):
        raise RuntimeBudgetExceeded("G7 pilot receipt must be an object")
    dataset = pilot.get("dataset")
    cells = pilot.get("cells")
    controller = pilot.get("controller")
    if (
        pilot.get("source_commit") != expected_commit
        or pilot.get("platform") != expected_platform
        or pilot.get("state") != expected_state
        or pilot.get("concurrency") != expected_concurrency
        or not isinstance(cells, dict)
        or set(cells) != set(G7_SURFACES)
        or not isinstance(controller, dict)
    ):
        raise RuntimeBudgetExceeded("G7 pilot receipt identity or coverage mismatch")
    try:
        validate_receipt_dataset(
            dataset,
            expected_observations=PILOT_OBSERVATIONS,
            expected_warmup=PILOT_WARMUP,
        )
    except RuntimeError as error:
        raise RuntimeBudgetExceeded(f"G7 pilot {error}") from error
    wall_seconds = controller.get("wall_seconds")
    if (
        not isinstance(wall_seconds, (int, float))
        or isinstance(wall_seconds, bool)
        or not math.isfinite(wall_seconds)
        or wall_seconds <= 0
        or observations < PILOT_OBSERVATIONS
        or warmup < PILOT_WARMUP
        or hard_cap_seconds <= 0
        or seed_primed is not True
    ):
        raise RuntimeBudgetExceeded("G7 pilot or requested runtime bounds are invalid")
    strict_cell = cells.get("strict-group-commit")
    strict_evidence = (
        strict_cell.get("group_commit_evidence")
        if isinstance(strict_cell, dict)
        else None
    )
    reopen = strict_evidence.get("reopen") if isinstance(strict_evidence, dict) else None
    maintenance = (
        strict_evidence.get("maintenance") if isinstance(strict_evidence, dict) else None
    )
    if (
        not isinstance(reopen, dict)
        or not isinstance(maintenance, dict)
        or strict_evidence.get("logical_commits") != PILOT_OBSERVATIONS
        or not isinstance(maintenance.get("total_time_nanos"), int)
        or isinstance(maintenance["total_time_nanos"], bool)
        or maintenance["total_time_nanos"] <= 0
        or any(
            not isinstance(reopen.get(field), int)
            or isinstance(reopen[field], bool)
            or reopen[field] <= 0
            for field in ("open_time_nanos", "verification_time_nanos")
        )
    ):
        raise RuntimeBudgetExceeded(
            "G7 pilot omitted validated strict group-commit maintenance/reopen evidence"
        )
    pilot_commit_correctness_seconds = (
        maintenance["total_time_nanos"]
        + reopen["open_time_nanos"]
        + reopen["verification_time_nanos"]
    ) / 1_000_000_000
    full_commit_correctness_seconds = pilot_commit_correctness_seconds * (
        observations / PILOT_OBSERVATIONS
    )
    full_surface_seconds: dict[str, float] = {}
    pilot_surface_seconds = 0.0
    for name in G7_SURFACES:
        cell = cells[name]
        throughput = cell.get("throughput_per_second") if isinstance(cell, dict) else None
        p99_nanos = cell.get("p99") if isinstance(cell, dict) else None
        if (
            not isinstance(throughput, (int, float))
            or isinstance(throughput, bool)
            or not math.isfinite(throughput)
            or throughput <= 0
            or not isinstance(p99_nanos, int)
            or isinstance(p99_nanos, bool)
            or p99_nanos <= 0
        ):
            raise RuntimeBudgetExceeded(f"G7 pilot timing is invalid for {name}")
        pilot_measurement_seconds = PILOT_OBSERVATIONS / throughput
        throughput_seconds = observations / throughput
        pilot_warmup_seconds = 0.0
        full_warmup_seconds = 0.0
        pilot_correctness_seconds = 0.0
        full_correctness_seconds = 0.0
        if name != "strict-group-commit" and expected_state == "warm":
            pilot_warmup_seconds = PILOT_WARMUP * p99_nanos / 1_000_000_000
            full_warmup_seconds = warmup * p99_nanos / 1_000_000_000
        elif name == "strict-group-commit":
            pilot_correctness_seconds = pilot_commit_correctness_seconds
            full_correctness_seconds = full_commit_correctness_seconds
        pilot_surface_seconds += (
            pilot_measurement_seconds
            + pilot_warmup_seconds
            + pilot_correctness_seconds
        )
        full_surface_seconds[name] = (
            throughput_seconds + full_warmup_seconds + full_correctness_seconds
        )
    fixed_overhead_seconds = max(0.0, wall_seconds - pilot_surface_seconds)
    expected_seconds = fixed_overhead_seconds + sum(full_surface_seconds.values())
    derived_seconds = (
        expected_seconds * PILOT_BUDGET_MULTIPLIER
        + PILOT_BUDGET_RESERVE_SECONDS
    )
    if derived_seconds > hard_cap_seconds:
        slowest = max(full_surface_seconds, key=full_surface_seconds.get)
        raise RuntimeBudgetExceeded(
            "G7 pilot projects a runtime budget above the authorized cap: "
            f"expected={expected_seconds:.1f}s, derived={derived_seconds:.1f}s, "
            f"cap={hard_cap_seconds:.1f}s, slowest_surface={slowest}, "
            f"slowest_seconds={full_surface_seconds[slowest]:.1f}s"
        )
    return {
        "schema": "hyphae-native-g7-runtime-budget-v3",
        "method": "exact-runner-short-pilot-with-bounded-recovery-v3",
        "seed_treatment": "measured-after-identical-seed-prime",
        "pilot_observations": PILOT_OBSERVATIONS,
        "pilot_warmup": PILOT_WARMUP,
        "full_observations": observations,
        "full_warmup": warmup,
        "pilot_wall_seconds": round(float(wall_seconds), 6),
        "fixed_overhead_seconds": round(fixed_overhead_seconds, 6),
        "strict_group_commit_correctness_projection": {
            "method": "linear-pilot-maintenance-reopen-full-key-verification-v2",
            "pilot_seconds": round(pilot_commit_correctness_seconds, 9),
            "full_seconds": round(full_commit_correctness_seconds, 9),
        },
        "expected_seconds": round(expected_seconds, 6),
        "multiplier": PILOT_BUDGET_MULTIPLIER,
        "reserve_seconds": PILOT_BUDGET_RESERVE_SECONDS,
        "timeout_seconds": math.ceil(derived_seconds),
        "hard_cap_seconds": hard_cap_seconds,
        "surface_seconds": {
            name: round(value, 6) for name, value in full_surface_seconds.items()
        },
    }


def validate_warm_control_pilot_latency(pilot: object) -> None:
    """Fail before matrix execution when the normative control pilot misses."""
    if not isinstance(pilot, dict):
        raise RuntimeBudgetExceeded("G7 warm control pilot must be an object")
    background = pilot.get("background_interference")
    if (
        pilot.get("state") != "warm"
        or pilot.get("concurrency") != 1
        or not isinstance(background, dict)
        or background.get("status") != "control"
    ):
        raise RuntimeBudgetExceeded("G7 warm control pilot identity is invalid")
    cells = pilot.get("cells")
    if not isinstance(cells, dict) or set(cells) != set(DEFAULT_AUTHORITY.cells):
        raise RuntimeBudgetExceeded("G7 warm control pilot coverage is invalid")
    misses: list[str] = []
    for name, (target_p50, target_p99) in sorted(
        DEFAULT_AUTHORITY.warm_targets.items()
    ):
        cell = cells.get(name)
        if not isinstance(cell, dict):
            raise RuntimeBudgetExceeded(
                f"G7 warm control pilot omitted normative surface {name}"
            )
        for percentile, target in (("p50", target_p50), ("p99", target_p99)):
            value = cell.get(percentile)
            if not isinstance(value, int) or isinstance(value, bool) or value <= 0:
                raise RuntimeBudgetExceeded(
                    f"G7 warm control pilot timing is invalid: {name}.{percentile}"
                )
            if value > target:
                misses.append(f"{name}.{percentile}={value}, target={target}")
    if misses:
        raise RuntimeBudgetExceeded(
            "G7 warm control pilot missed normative latency targets: "
            + "; ".join(misses)
        )


def derive_matrix_runtime_plan(
    *,
    calibration_seconds: float,
    cell_budgets: list[dict[str, object]],
    hard_cap_seconds: float,
    expected_cell_count: int,
) -> dict[str, object]:
    if (
        not math.isfinite(calibration_seconds)
        or calibration_seconds < 0
        or not math.isfinite(hard_cap_seconds)
        or hard_cap_seconds <= 0
        or not cell_budgets
        or expected_cell_count <= 0
        or len(cell_budgets) != expected_cell_count
    ):
        raise RuntimeBudgetExceeded("G7 matrix runtime planning inputs are invalid")
    timeouts: list[float] = []
    for budget in cell_budgets:
        timeout = budget.get("timeout_seconds")
        if (
            budget.get("schema") != "hyphae-native-g7-runtime-budget-v3"
            or not isinstance(timeout, (int, float))
            or isinstance(timeout, bool)
            or not math.isfinite(timeout)
            or timeout <= 0
        ):
            raise RuntimeBudgetExceeded("G7 matrix contains an invalid cell budget")
        timeouts.append(float(timeout))
    planned_measurement_seconds = sum(timeouts)
    planned_total_seconds = calibration_seconds + planned_measurement_seconds
    if planned_total_seconds > hard_cap_seconds:
        raise RuntimeBudgetExceeded(
            "G7 pilots and evidence-derived cell budgets exceed the matrix cap: "
            f"calibration={calibration_seconds:.1f}s, "
            f"measurement={planned_measurement_seconds:.1f}s, "
            f"total={planned_total_seconds:.1f}s, cap={hard_cap_seconds:.1f}s"
        )
    return {
        "schema": "hyphae-native-g7-matrix-runtime-plan-v1",
        "status": "accepted",
        "calibration_seconds": round(calibration_seconds, 6),
        "planned_measurement_seconds": round(planned_measurement_seconds, 6),
        "planned_total_seconds": round(planned_total_seconds, 6),
        "hard_cap_seconds": hard_cap_seconds,
        "cell_count": len(cell_budgets),
    }


def write_matrix_progress(
    path: Path,
    source_commit: str,
    platform: str,
    completed: list[dict[str, object]],
    total_cells: int,
    current: dict[str, object] | None,
    status: str,
    started_unix_nanos: int,
) -> None:
    write_json_atomic(path, {
        "schema": "hyphae-native-g7-matrix-progress-v1",
        "source_commit": source_commit,
        "platform": platform,
        "status": status,
        "completed_cells": completed,
        "completed_count": len(completed),
        "total_cells": total_cells,
        "current_cell": current,
        "started_unix_nanos": started_unix_nanos,
        "updated_unix_nanos": time.time_ns(),
    })


def stop_process(process: subprocess.Popen[str]) -> None:
    if process.poll() is not None:
        return
    if os.name == "posix":
        try:
            os.killpg(process.pid, signal.SIGTERM)
        except ProcessLookupError:
            return
    else:
        process.terminate()
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        if os.name == "posix":
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                return
        else:
            process.kill()
        process.wait(timeout=5)


def collect_process_output(stdout: TextIO, stderr: TextIO) -> tuple[str, str]:
    try:
        stdout.flush()
        stdout.seek(0)
        output = stdout.read()
        stderr.flush()
        stderr.seek(0)
        errors = stderr.read()
        return output, errors
    finally:
        stdout.close()
        stderr.close()


def process_error_tail(stderr: str) -> str:
    return stderr[-PROCESS_ERROR_TAIL_CHARS:].strip()


def handle_controller_signal(signum: int, _frame: object) -> None:
    process = ACTIVE_PROCESS
    if process is not None:
        stop_process(process)
    raise SystemExit(128 + signum)


class MacRusageInfoV4(ctypes.Structure):
    _fields_ = [("uuid", ctypes.c_ubyte * 16)] + [
        (name, ctypes.c_uint64)
        for name in (
            "user_time", "system_time", "package_idle_wakeups", "interrupt_wakeups",
            "pageins", "wired_size", "resident_size", "physical_footprint",
            "process_start", "process_exit", "child_user_time", "child_system_time",
            "child_package_idle_wakeups", "child_interrupt_wakeups", "child_pageins",
            "child_elapsed", "disk_bytes_read", "disk_bytes_written", "qos_default",
            "qos_maintenance", "qos_background", "qos_utility", "qos_legacy",
            "qos_user_initiated", "qos_user_interactive", "billed_system_time",
            "serviced_system_time", "logical_writes", "lifetime_max_footprint",
            "instructions", "cycles", "billed_energy", "serviced_energy",
            "interval_max_footprint", "runnable_time",
        )
    ]


def macos_rusage(process_id: int) -> MacRusageInfoV4 | None:
    if sys.platform != "darwin":
        return None
    library = ctypes.CDLL("/usr/lib/libproc.dylib", use_errno=True)
    library.proc_pid_rusage.argtypes = (
        ctypes.c_int,
        ctypes.c_int,
        ctypes.POINTER(MacRusageInfoV4),
    )
    library.proc_pid_rusage.restype = ctypes.c_int
    usage = MacRusageInfoV4()
    return usage if library.proc_pid_rusage(process_id, 4, ctypes.byref(usage)) == 0 else None


def verify_source(expected_commit: str) -> str:
    if len(expected_commit) != 40 or any(
        character not in "0123456789abcdef" for character in expected_commit
    ):
        raise ValueError("source commit must be a canonical lowercase SHA-1")
    head = subprocess.run(
        ("git", "rev-parse", "HEAD"), cwd=ROOT, check=True,
        capture_output=True, text=True,
    ).stdout.strip()
    if head != expected_commit:
        raise RuntimeError("source commit differs from checked-out HEAD")
    dirty = subprocess.run(
        ("git", "status", "--porcelain", "--untracked-files=all"), cwd=ROOT,
        check=True, capture_output=True, text=True,
    ).stdout.strip()
    if dirty:
        raise RuntimeError("source worktree, including untracked files, must be clean")
    return subprocess.run(
        ("git", "rev-parse", "HEAD^{tree}"), cwd=ROOT, check=True,
        capture_output=True, text=True,
    ).stdout.strip()


def build_metadata(binary: Path, source_tree: str) -> dict[str, str]:
    rustc = subprocess.run(
        ("rustc", "-vV"), check=True, capture_output=True, text=True, timeout=30
    ).stdout.strip()
    cargo = subprocess.run(
        ("cargo", "-V"), check=True, capture_output=True, text=True, timeout=30
    ).stdout.strip()
    host = next(
        (line.removeprefix("host: ") for line in rustc.splitlines() if line.startswith("host: ")),
        "",
    )
    if not host:
        raise RuntimeError("rustc did not disclose its host target")
    return {
        "rustc": rustc,
        "cargo": cargo,
        "profile": "release",
        "target": host,
        "os": platform_module.platform(),
        "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
        "source_tree": source_tree,
    }


def run_cell(
    binary: Path,
    commit: str,
    platform: str,
    state: str,
    concurrency: int,
    environment: dict[str, str] | None = None,
    macos_counter_template: Path | None = None,
    timeout_seconds: float | None = None,
    progress_path: Path | None = None,
    stall_timeout_seconds: float | None = None,
) -> dict:
    global ACTIVE_PROCESS
    environment = dict(os.environ if environment is None else environment)
    base_command = [str(binary), commit, platform, state, str(concurrency)]
    command = base_command
    perf_output: Path | None = None
    if (
        sys.platform.startswith("linux")
        and environment.get("HYPHAE_G7_PERF") == "1"
        and shutil.which("perf")
    ):
        descriptor = tempfile.NamedTemporaryFile(
            prefix="hyphae-g7-perf-",
            suffix=".csv",
            delete=False,
        )
        descriptor.close()
        perf_output = Path(descriptor.name)
        command = [
            "perf",
            "stat",
            "-x,",
            "-e",
            "cycles,cache-misses,minor-faults,major-faults",
            "-o",
            str(perf_output),
            "--",
            *base_command,
        ]
    started = time.monotonic()
    watchdog = (
        ProgressWatchdog(
            progress_path,
            stall_timeout_seconds,
            started,
            expected_commit=commit,
        )
        if progress_path is not None and stall_timeout_seconds is not None
        else None
    )
    next_progress_check = started
    environment["RUST_BACKTRACE"] = "1"
    child_usage_before = (
        resource.getrusage(resource.RUSAGE_CHILDREN) if os.name == "posix" else None
    )
    stdout_capture = tempfile.TemporaryFile(mode="w+", encoding="utf-8")
    stderr_capture = tempfile.TemporaryFile(mode="w+", encoding="utf-8")
    try:
        process = subprocess.Popen(
            command,
            cwd=ROOT,
            env=environment,
            stdout=stdout_capture,
            stderr=stderr_capture,
            text=True,
            start_new_session=os.name == "posix",
        )
    except BaseException:
        stdout_capture.close()
        stderr_capture.close()
        raise
    ACTIVE_PROCESS = process
    metrics = ProcessMetrics(process.pid)
    while process.poll() is None:
        metrics.sample()
        now = time.monotonic()
        if watchdog is not None and now >= next_progress_check:
            try:
                watchdog.observe(now)
            except ProgressStalled as error:
                stop_process(process)
                _, stderr = collect_process_output(stdout_capture, stderr_capture)
                ACTIVE_PROCESS = None
                if perf_output is not None:
                    perf_output.unlink(missing_ok=True)
                raise ProgressStalled(
                    f"G7 cell stalled ({state}/{concurrency}): {error}; "
                    f"runner stderr tail: {process_error_tail(stderr)}"
                ) from error
            except (OSError, UnicodeError, RuntimeError, ValueError) as error:
                stop_process(process)
                _, stderr = collect_process_output(stdout_capture, stderr_capture)
                ACTIVE_PROCESS = None
                if perf_output is not None:
                    perf_output.unlink(missing_ok=True)
                raise RuntimeError(
                    f"G7 cell progress watchdog failed ({state}/{concurrency}): {error}; "
                    f"runner stderr tail: {process_error_tail(stderr)}"
                ) from error
            next_progress_check = now + 1.0
        if timeout_seconds is not None and now - started >= timeout_seconds:
            stop_process(process)
            _, stderr = collect_process_output(stdout_capture, stderr_capture)
            ACTIVE_PROCESS = None
            if perf_output is not None:
                perf_output.unlink(missing_ok=True)
            progress = (
                watchdog.last_progress_summary
                if watchdog is not None
                else "progress watchdog disabled"
            )
            raise RuntimeBudgetExceeded(
                f"G7 cell exceeded its evidence-derived runtime budget of "
                f"{timeout_seconds:.0f}s ({state}/{concurrency}); "
                f"last progress: {progress}; "
                f"runner stderr tail: {process_error_tail(stderr)}"
            )
        time.sleep(0.01)
    stdout, stderr = collect_process_output(stdout_capture, stderr_capture)
    ACTIVE_PROCESS = None
    metrics.sample()
    if child_usage_before is not None:
        metrics.record_child_usage(
            child_usage_before,
            resource.getrusage(resource.RUSAGE_CHILDREN),
        )
    completed = subprocess.CompletedProcess(command, process.returncode, stdout, stderr)
    if completed.returncode != 0:
        if perf_output is not None:
            perf_output.unlink(missing_ok=True)
        raise RuntimeError(
            f"G7 cell failed ({state}/{concurrency}); "
            f"runner stderr tail: {process_error_tail(completed.stderr)}"
        )
    payload = json.loads(completed.stdout)
    if perf_output is None:
        metrics.inject(payload)
    if perf_output is not None:
        augment_perf_counters(payload, perf_output)
        perf_output.unlink(missing_ok=True)
    if macos_counter_template is not None:
        cache_misses = measure_macos_cache_misses(
            binary,
            [commit, platform, state, str(concurrency)],
            environment,
            macos_counter_template,
        )
        payload["counters"]["cache_misses"] = {
            "status": "measured",
            "value": cache_misses,
            "unit": "count",
            "provider": "macos-instruments-l1d-paired-hot-path-pass",
            "pairing": "same-build-corpus-workload-after-uninstrumented-seed",
        }
    payload["controller"] = {
        "wall_seconds": round(time.monotonic() - started, 6),
        "host": platform_module.platform(),
        "machine": platform_module.machine(),
    }
    return payload


def run_calibration_pilot(
    binary: Path,
    *,
    commit: str,
    source_tree: str,
    platform: str,
    state: str,
    concurrency: int,
    environment: dict[str, str],
    receipt_path: Path,
    progress_path: Path,
    partial_path: Path,
    timeout_seconds: float,
    stall_timeout_seconds: float,
) -> dict[str, object]:
    pilot_environment = closure_environment(environment)
    pilot_environment["HYPHAE_G7_OBSERVATIONS"] = str(PILOT_OBSERVATIONS)
    pilot_environment["HYPHAE_G7_WARMUP"] = str(PILOT_WARMUP)
    pilot_environment["HYPHAE_G7_PROGRESS_FILE"] = str(progress_path.resolve())
    pilot_environment["HYPHAE_G7_PARTIAL_RECEIPT_FILE"] = str(partial_path.resolve())
    pilot_environment.pop("HYPHAE_G7_PERF", None)
    for artifact in (receipt_path, progress_path, partial_path):
        artifact.unlink(missing_ok=True)
    pilot = run_cell(
        binary,
        commit,
        platform,
        state,
        concurrency,
        environment=pilot_environment,
        macos_counter_template=None,
        timeout_seconds=timeout_seconds,
        progress_path=progress_path,
        stall_timeout_seconds=stall_timeout_seconds,
    )
    if not progress_path.is_file():
        raise RuntimeError("G7 pilot did not produce mandatory progress")
    progress = validate_completed_cell_progress(
        json.loads(progress_path.read_text(encoding="utf-8")), commit
    )
    if not partial_path.is_file():
        raise RuntimeError("G7 pilot did not persist a partial receipt")
    partial = validate_partial_receipt(
        json.loads(partial_path.read_text(encoding="utf-8")),
        expected_commit=commit,
        expected_tree=source_tree,
        expected_platform=platform,
        expected_state=state,
        expected_concurrency=concurrency,
    )
    validate_cross_artifact_dataset(
        pilot,
        progress,
        partial,
        expected_observations=PILOT_OBSERVATIONS,
        expected_warmup=PILOT_WARMUP,
    )
    validate_pilot_search_evidence(pilot, commit)
    write_json_atomic(receipt_path, pilot)
    return pilot


def validate_pilot_search_evidence(
    pilot: dict[str, object], expected_commit: str
) -> None:
    """Bind the runtime projection to honest ANN and hybrid hot-path evidence."""
    if (
        pilot.get("schema") != G7_RECEIPT_SCHEMA
        or pilot.get("gate") != "G7"
        or pilot.get("status") != "passed"
        or pilot.get("evidence_class") != "closure-candidate"
        or pilot.get("source_commit") != expected_commit
        or pilot.get("claims") != []
        or pilot.get("closure_declared") is not False
    ):
        raise RuntimeError("G7 pilot receipt identity or open state mismatch")
    dataset = pilot.get("dataset")
    cells = pilot.get("cells")
    initial_ann_bulk = pilot.get("initial_ann_bulk")
    if not isinstance(dataset, dict) or not isinstance(cells, dict):
        raise RuntimeError("G7 pilot omitted its dataset or measured cells")
    observations = dataset.get("observations")
    if not isinstance(observations, int) or isinstance(observations, bool):
        raise RuntimeError("G7 pilot observations are invalid")
    validate_initial_ann_bulk_evidence(initial_ann_bulk, expected_commit)
    if not isinstance(initial_ann_bulk, dict):
        raise RuntimeError("G7 pilot omitted initial ANN bulk evidence")
    ann_cell = cells.get("ann-top10-recall-095")
    hybrid_cell = cells.get("hybrid-top10")
    concurrency = pilot.get("concurrency")
    if not isinstance(concurrency, int) or isinstance(concurrency, bool):
        raise RuntimeError("G7 pilot concurrency is invalid")
    validate_ann_read_view_cell(ann_cell, initial_ann_bulk, observations, concurrency)
    validate_hybrid_read_view_cell(
        hybrid_cell,
        ann_cell,
        observations,
        concurrency,
    )
    validate_bm25_read_view_cell(
        cells.get("bm25-top10"), hybrid_cell, observations
    )
    validate_filtered_bm25_read_view_cell(
        cells.get("filtered-bm25-top10"), hybrid_cell, observations
    )
    validate_strict_group_commit_cell(
        cells.get("strict-group-commit"), observations, pilot.get("concurrency")
    )


class ProcessMetrics:
    def __init__(self, process_id: int) -> None:
        self.process_id = process_id
        self.peak_rss: int | None = None
        self.initial_io: dict[str, int] | None = None
        self.final_io: dict[str, int] | None = None
        self.linux_proc_sample_complete = True
        self.page_faults: int | None = None
        self.cpu_cycles: int | None = None
        self.macos_bytes_read: int | None = None
        self.macos_bytes_written: int | None = None

    def _status(self) -> dict[str, int]:
        if os.name == "nt":
            return self._windows_status()
        if not sys.platform.startswith("linux"):
            completed = subprocess.run(
                ("ps", "-o", "rss=", "-p", str(self.process_id)),
                capture_output=True,
                text=True,
                check=False,
            )
            value = completed.stdout.strip()
            return {"rss": int(value) * 1024} if value.isdigit() else {}
        contents = self._read_linux_proc("status")
        if contents is None:
            return {}
        values: dict[str, int] = {}
        for line in contents.splitlines():
            if line.startswith("VmHWM:"):
                values["rss"] = int(line.split()[1]) * 1024
        return values

    def _io(self) -> dict[str, int]:
        if os.name != "posix" or not sys.platform.startswith("linux"):
            return {}
        contents = self._read_linux_proc("io")
        if contents is None:
            return {}
        values: dict[str, int] = {}
        for line in contents.splitlines():
            name, _, value = line.partition(":")
            if name in {"read_bytes", "write_bytes"}:
                values[name] = int(value.strip())
        return values

    def _faults(self) -> int | None:
        if os.name != "posix" or not sys.platform.startswith("linux"):
            return None
        contents = self._read_linux_proc("stat")
        if contents is None:
            return None
        fields = contents.split()
        if len(fields) <= 14:
            return None
        return int(fields[9]) + int(fields[11])

    def _read_linux_proc(self, name: str) -> str | None:
        try:
            return Path(f"/proc/{self.process_id}/{name}").read_text(
                encoding="ascii",
                errors="ignore",
            )
        except OSError:
            # The process can exit or cross a ptrace/dumpability boundary
            # between poll() and this optional controller-side sample. Never
            # publish a truncated interval over the runner's own counters.
            self.linux_proc_sample_complete = False
            return None

    def sample(self) -> None:
        status = self._status()
        rss = status.get("rss")
        if rss is not None:
            self.peak_rss = max(self.peak_rss or 0, rss)
        io = self._io()
        if io and self.initial_io is None:
            self.initial_io = io
        if io:
            self.final_io = io
        faults = self._faults()
        if faults is not None:
            self.page_faults = faults
        usage = macos_rusage(self.process_id)
        if usage is not None:
            self.peak_rss = max(self.peak_rss or 0, usage.resident_size)
            self.cpu_cycles = usage.cycles
            self.macos_bytes_read = usage.disk_bytes_read
            self.macos_bytes_written = usage.disk_bytes_written

    def record_child_usage(self, before: object, after: object) -> None:
        self.page_faults = max(
            0,
            int(after.ru_minflt - before.ru_minflt + after.ru_majflt - before.ru_majflt),
        )

    def inject(self, payload: dict) -> None:
        counters = payload["counters"]
        linux_proc_usable = self.linux_proc_sample_complete
        if self.peak_rss is not None and linux_proc_usable:
            counters["rss"] = {
                "status": "measured", "value": self.peak_rss, "unit": "bytes",
                "provider": "macos-proc-pid-rusage-v4" if sys.platform == "darwin" else "linux-proc-vmhwm",
            }
        if self.page_faults is not None and linux_proc_usable:
            counters["page_faults"] = {
                "status": "measured", "value": self.page_faults, "unit": "count",
                "provider": "macos-getrusage-child-faults" if sys.platform == "darwin" else "linux-proc-stat",
            }
        if self.cpu_cycles is not None:
            counters["cpu_cycles"] = {
                "status": "measured", "value": self.cpu_cycles, "unit": "cycles",
                "provider": "macos-proc-pid-rusage-v4",
            }
        for name, value in (
            ("bytes_read", self.macos_bytes_read),
            ("bytes_written", self.macos_bytes_written),
        ):
            if value is not None:
                counters[name] = {
                    "status": "measured", "value": value, "unit": "bytes",
                    "provider": "macos-proc-pid-rusage-v4",
                }
        if (
            linux_proc_usable
            and self.initial_io is not None
            and self.final_io is not None
        ):
            for source, target in (("read_bytes", "bytes_read"), ("write_bytes", "bytes_written")):
                if source in self.initial_io and source in self.final_io:
                    counters[target] = {
                        "status": "measured",
                        "value": max(0, self.final_io[source] - self.initial_io[source]),
                        "unit": "bytes",
                        "provider": "linux-proc-io",
                    }

    def _windows_status(self) -> dict[str, int]:
        if os.name != "nt":
            return {}
        import ctypes

        class Counters(ctypes.Structure):
            _fields_ = [
                ("cb", ctypes.c_ulong),
                ("PageFaultCount", ctypes.c_ulong),
                ("PeakWorkingSetSize", ctypes.c_size_t),
                ("WorkingSetSize", ctypes.c_size_t),
                ("QuotaPeakPagedPoolUsage", ctypes.c_size_t),
                ("QuotaPagedPoolUsage", ctypes.c_size_t),
                ("QuotaPeakNonPagedPoolUsage", ctypes.c_size_t),
                ("QuotaNonPagedPoolUsage", ctypes.c_size_t),
                ("PagefileUsage", ctypes.c_size_t),
                ("PeakPagefileUsage", ctypes.c_size_t),
            ]

        kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
        psapi = ctypes.WinDLL("psapi", use_last_error=True)
        handle = kernel32.OpenProcess(0x1000 | 0x0010, False, self.process_id)
        if not handle:
            return {}
        try:
            counters = Counters()
            counters.cb = ctypes.sizeof(counters)
            if not psapi.GetProcessMemoryInfo(handle, ctypes.byref(counters), counters.cb):
                return {}
            self.page_faults = int(counters.PageFaultCount)
            return {"rss": int(counters.PeakWorkingSetSize)}
        finally:
            kernel32.CloseHandle(handle)


def augment_perf_counters(payload: dict, path: Path) -> None:
    names = {
        "cycles": "cpu_cycles",
        "cache-misses": "cache_misses",
        "minor-faults": "page_faults",
        "major-faults": "page_faults",
    }
    values: dict[str, int] = {}
    for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
        fields = line.split(",")
        if len(fields) < 3:
            continue
        raw, event = fields[0].strip(), fields[2].strip()
        if raw.startswith("<") or event not in names:
            continue
        try:
            value = int(raw.replace(".", ""))
        except ValueError:
            continue
        target = names[event]
        values[target] = values.get(target, 0) + value
    for name in ("cpu_cycles", "cache_misses"):
        if name in values:
            payload["counters"][name] = {
                "status": "measured",
                "value": values[name],
                "unit": "cycles" if name == "cpu_cycles" else "count",
                "provider": "linux-perf-stat",
            }
    if "page_faults" in values:
        payload["counters"]["page_faults"] = {
            "status": "measured",
            "value": values["page_faults"],
            "unit": "count",
            "provider": "linux-perf-stat",
        }


def parse_macos_counter_export(path: Path) -> dict[str, int]:
    root = ElementTree.parse(path).getroot()
    references: dict[str, str] = {}
    totals = {"cpu_cycles": 0, "cache_misses": 0}
    metric_names = {
        "Cycles": "cpu_cycles",
        "L1D Cache Load Misses": "cache_misses",
        "L1D Cache Store Misses": "cache_misses",
    }
    for row in root.iter("row"):
        strings: list[str] = []
        metric_value: int | None = None
        for field in row:
            reference = field.get("ref")
            value = references.get(reference, "") if reference is not None else (field.text or "")
            identity = field.get("id")
            if identity is not None:
                references[identity] = value
            if field.tag == "string":
                strings.append(value)
            elif field.tag == "fixed-decimal" and value:
                metric_value = int(float(value))
        if metric_value is None:
            continue
        metric = next((metric_names[name] for name in strings if name in metric_names), None)
        if metric is not None:
            totals[metric] += metric_value
    if totals["cpu_cycles"] <= 0 or totals["cache_misses"] < 0:
        raise RuntimeError("Instruments export did not contain Native G7 counters")
    return totals


def validate_completed_ann_progress(
    progress: dict[str, object], expected_commit: str
) -> None:
    validate_progress(progress, expected_commit)
    if (
        progress.get("operation") not in {"ann-bulk-build", "ann-seed-verify"}
        or progress.get("stage") != "ann-published"
        or progress.get("status") != "completed"
        or progress.get("unit") != "vectors"
    ):
        raise RuntimeError("G7 ANN progress did not reach durable publication")
    details = progress.get("details")
    if not isinstance(details, dict):
        raise RuntimeError("G7 ANN progress omitted its details")
    validate_progress_eta(details.get("eta"), completed=True)
    evidence = {name: value for name, value in details.items() if name != "eta"}
    validate_initial_ann_bulk_evidence(evidence, expected_commit)
    if evidence["dataset_digest"] != progress["dataset_digest"]:
        raise RuntimeError("G7 ANN progress details target another dataset")


def validate_completed_cell_progress(
    progress: dict[str, object], expected_commit: str
) -> dict[str, object]:
    validate_progress(progress, expected_commit)
    if (
        progress.get("operation") != "g7-cell"
        or progress.get("stage") != "cell-completed"
        or progress.get("status") != "completed"
        or progress.get("unit") != "work-units"
        or progress.get("completed_units") != progress.get("total_units")
    ):
        raise RuntimeError("G7 progress did not reach complete cell publication")
    details = progress.get("details")
    if not isinstance(details, dict):
        raise RuntimeError("completed G7 cell progress omitted its details")
    validate_progress_eta(details.get("eta"), completed=True)
    return progress


def validate_progress_eta(value: object, *, completed: bool) -> None:
    if not isinstance(value, dict) or set(value) != {
        "status", "estimated_remaining_nanos"
    }:
        raise RuntimeError("G7 runner progress ETA fields mismatch")
    status = value["status"]
    remaining = value["estimated_remaining_nanos"]
    if completed:
        if status != "completed" or remaining != 0:
            raise RuntimeError("completed G7 progress has an invalid ETA")
        return
    if status not in {"pending", "estimated"}:
        raise RuntimeError("running G7 progress has an invalid ETA status")
    if status == "pending" and remaining is not None:
        raise RuntimeError("pending G7 progress ETA must be unknown")
    if status == "estimated" and (
        not isinstance(remaining, int) or isinstance(remaining, bool) or remaining < 0
    ):
        raise RuntimeError("estimated G7 progress ETA is invalid")


def validate_initial_ann_bulk_evidence(
    evidence: object, expected_commit: str
) -> None:
    fields = {
        "schema", "source_commit", "dataset_digest", "builder", "partition_policy", "input_identity",
        "aggregate_identity", "planned_vectors", "planned_partitions", "planned_workers",
        "planned_memory_bytes", "worker_batches", "total_time_nanos",
        "hardware_profile_fingerprint", "governor_policy_schema", "governor_mode",
        "calibration_cache_key", "topology_digest", "topology_workers", "hard_affinity",
        "governor_execution",
    }
    if not isinstance(evidence, dict) or set(evidence) != fields:
        raise RuntimeError("G7 initial ANN bulk evidence fields mismatch")
    if (
        evidence["schema"] != "hyphae-native-g7-initial-ann-bulk-v1"
        or evidence["source_commit"] != expected_commit
        or evidence["builder"] != "partitioned-hnsw-v1"
        or evidence["partition_policy"] != G7_ANN_PARTITION_POLICY
        or evidence["governor_mode"] not in {"bulk", "mixed"}
        or not isinstance(evidence["hard_affinity"], bool)
    ):
        raise RuntimeError("G7 initial ANN bulk evidence identity mismatch")
    for name in (
        "dataset_digest", "input_identity", "aggregate_identity",
        "hardware_profile_fingerprint", "topology_digest",
    ):
        value = evidence[name]
        if not isinstance(value, str) or len(value) != 64 or any(
            character not in "0123456789abcdef" for character in value
        ):
            raise RuntimeError(f"G7 initial ANN bulk {name} is not a canonical digest")
    for name in (
        "planned_vectors", "planned_partitions", "planned_workers",
        "planned_memory_bytes", "worker_batches", "topology_workers",
    ):
        value = evidence[name]
        if not isinstance(value, int) or isinstance(value, bool) or value <= 0:
            raise RuntimeError(f"G7 initial ANN bulk {name} is invalid")
    if evidence["planned_partitions"] > evidence["planned_vectors"]:
        raise RuntimeError("G7 initial ANN bulk partition plan exceeds its vectors")
    if evidence["planned_partitions"] != min(
        G7_LOGICAL_ANN_PARTITIONS, evidence["planned_vectors"]
    ):
        raise RuntimeError("G7 initial ANN bulk partition plan depends on hardware")
    if evidence["planned_partitions"] > MAX_INITIAL_ANN_BULK_PARTITIONS:
        raise RuntimeError("G7 initial ANN bulk exceeds its durable partition limit")
    if evidence["planned_workers"] > evidence["topology_workers"]:
        raise RuntimeError("G7 initial ANN bulk worker plan exceeds its topology")
    if evidence["planned_workers"] > evidence["planned_partitions"]:
        raise RuntimeError("G7 initial ANN bulk worker plan exceeds logical partitions")
    if (
        min(evidence["topology_workers"], evidence["planned_partitions"]) > 1
        and evidence["planned_workers"] <= 1
    ):
        raise RuntimeError("G7 initial ANN bulk ignored its multi-worker topology")
    if evidence["planned_workers"] > 1 and evidence["worker_batches"] <= 1:
        raise RuntimeError("G7 initial ANN bulk did not prove parallel worker batches")
    execution = evidence["governor_execution"]
    execution_fields = {
        "class", "compute_threads", "io_slots", "memory_bytes", "queue_ticket",
        "initial_queue_depth", "queue_time_nanos", "execution_time_nanos",
    }
    if (
        not isinstance(execution, dict)
        or set(execution) != execution_fields
        or execution["class"] != "bulk"
        or not isinstance(execution["compute_threads"], int)
        or isinstance(execution["compute_threads"], bool)
        or execution["compute_threads"] <= 0
        or execution["compute_threads"] != evidence["planned_workers"]
        or not isinstance(execution["memory_bytes"], int)
        or isinstance(execution["memory_bytes"], bool)
        or execution["memory_bytes"] <= 0
        or execution["memory_bytes"] != evidence["planned_memory_bytes"]
        or not isinstance(execution["io_slots"], int)
        or isinstance(execution["io_slots"], bool)
        or execution["io_slots"] != 0
        or not isinstance(execution["initial_queue_depth"], int)
        or isinstance(execution["initial_queue_depth"], bool)
        or execution["initial_queue_depth"] < 0
        or not isinstance(execution["queue_time_nanos"], int)
        or isinstance(execution["queue_time_nanos"], bool)
        or execution["queue_time_nanos"] < 0
        or not isinstance(execution["execution_time_nanos"], int)
        or isinstance(execution["execution_time_nanos"], bool)
        or execution["execution_time_nanos"] <= 0
        or (
            execution["queue_ticket"] is not None
            and (
                not isinstance(execution["queue_ticket"], int)
                or isinstance(execution["queue_ticket"], bool)
                or execution["queue_ticket"] < 0
            )
        )
    ):
        raise RuntimeError("G7 initial ANN bulk governor execution evidence mismatch")


def validate_execution_authority_evidence(
    evidence: object,
    *,
    calibration_executable_blake3: str,
    topology_digest: str,
    background: bool,
) -> None:
    fields = {
        "status", "database_queue_wait_millis", "topology_digest",
        "runner_executable_blake3",
        "calibration_executable_blake3", "installations", "installed_surfaces",
        "registered_pools", "local_dispatches", "stolen_dispatches",
        "completed_jobs", "numa_steal_status",
    }
    if not isinstance(evidence, dict) or set(evidence) != fields:
        raise RuntimeError("G7 execution authority evidence fields mismatch")
    if evidence["status"] != "measured":
        raise RuntimeError("G7 execution authority was not measured")
    if (
        not isinstance(evidence["database_queue_wait_millis"], int)
        or isinstance(evidence["database_queue_wait_millis"], bool)
        or evidence["database_queue_wait_millis"] != 60_000
    ):
        raise RuntimeError("G7 execution authority database queue wait differs")
    if (
        evidence["runner_executable_blake3"] != calibration_executable_blake3
        or evidence["calibration_executable_blake3"] != calibration_executable_blake3
    ):
        raise RuntimeError("G7 execution authority targets another runner executable")
    if evidence["topology_digest"] != topology_digest:
        raise RuntimeError("G7 execution authority targets another topology")
    if evidence["numa_steal_status"] not in {"calibrated", "disabled", "not-applicable"}:
        raise RuntimeError("G7 execution authority has an invalid NUMA steal status")
    surfaces = evidence["installed_surfaces"]
    if not isinstance(surfaces, list) or surfaces != sorted(set(surfaces)):
        raise RuntimeError("G7 execution authority surfaces are not canonical")
    required = {
        "search-fixture", "embedded-structure", "embedded-sql",
        "local-structure-seed", "local-structure-migration",
        "local-structure-daemon", "local-sql-daemon", "indexed-sql",
        "join-sql", "group-commit", "physical-observation",
    }
    if background:
        required.add("background-maintenance")
    allowed = required | {"search-seed-builder"}
    if not required.issubset(surfaces) or not set(surfaces).issubset(allowed):
        raise RuntimeError("G7 execution authority omitted or invented a measured surface")
    if (
        evidence["installations"] != len(surfaces)
        or evidence["registered_pools"] != 1
    ):
        raise RuntimeError("G7 execution authority installation counts mismatch")
    counters = [
        evidence["local_dispatches"], evidence["stolen_dispatches"],
        evidence["completed_jobs"],
    ]
    if any(
        not isinstance(value, int) or isinstance(value, bool) or value < 0
        for value in counters
    ):
        raise RuntimeError("G7 execution authority counters are invalid")
    if evidence["completed_jobs"] != (
        evidence["local_dispatches"] + evidence["stolen_dispatches"]
    ):
        raise RuntimeError("G7 execution authority dispatch counters do not reconcile")
    if evidence["numa_steal_status"] != "calibrated" and evidence["stolen_dispatches"] != 0:
        raise RuntimeError("G7 execution authority stole work while NUMA stealing was disabled")


def bind_and_validate_cell_receipt(
    receipt: dict[str, object],
    *,
    expected_commit: str,
    expected_platform: str,
    expected_state: str,
    expected_concurrency: int,
    background_mode: str,
    hardware: dict[str, object],
    build: dict[str, str],
    calibration_executable_blake3: str,
) -> None:
    dataset = receipt.get("dataset")
    if (
        receipt.get("schema") != G7_RECEIPT_SCHEMA
        or receipt.get("gate") != "G7"
        or receipt.get("status") != "passed"
        or receipt.get("evidence_class") != "closure-candidate"
        or receipt.get("source_commit") != expected_commit
        or receipt.get("platform") != expected_platform
        or receipt.get("state") != expected_state
        or receipt.get("concurrency") != expected_concurrency
        or receipt.get("claims") != []
        or receipt.get("closure_declared") is not False
    ):
        raise RuntimeError("G7 runner receipt identity or normative workload mismatch")
    validate_receipt_dataset(
        dataset,
        expected_observations=1_000_000,
        expected_warmup=100_000,
    )
    receipt["background_mode"] = background_mode
    receipt["hardware"] = hardware
    receipt["build"] = build
    initial_ann_bulk = receipt.get("initial_ann_bulk")
    validate_initial_ann_bulk_evidence(initial_ann_bulk, expected_commit)
    receipt_cells = receipt.get("cells")
    if not isinstance(receipt_cells, dict):
        raise RuntimeError("G7 runner omitted its measured cells or dataset")
    observations = dataset.get("observations")
    if not isinstance(observations, int) or isinstance(observations, bool):
        raise RuntimeError("G7 runner dataset observations are invalid")
    ann_cell = receipt_cells.get("ann-top10-recall-095")
    hybrid_cell = receipt_cells.get("hybrid-top10")
    validate_ann_read_view_cell(
        ann_cell, initial_ann_bulk, observations, expected_concurrency
    )
    validate_hybrid_read_view_cell(
        hybrid_cell,
        ann_cell,
        observations,
        expected_concurrency,
    )
    validate_bm25_read_view_cell(
        receipt_cells.get("bm25-top10"), hybrid_cell, observations
    )
    validate_filtered_bm25_read_view_cell(
        receipt_cells.get("filtered-bm25-top10"), hybrid_cell, observations
    )
    validate_strict_group_commit_cell(
        receipt_cells.get("strict-group-commit"),
        observations,
        expected_concurrency,
    )
    if not isinstance(initial_ann_bulk, dict):
        raise RuntimeError("G7 runner omitted initial ANN bulk evidence")
    validate_execution_authority_evidence(
        receipt.get("execution_authority"),
        calibration_executable_blake3=calibration_executable_blake3,
        topology_digest=initial_ann_bulk["topology_digest"],
        background=background_mode == "interference",
    )
    if initial_ann_bulk["dataset_digest"] != dataset.get("digest"):
        raise RuntimeError("G7 ANN evidence targets another receipt dataset")


def persist_validated_cell_checkpoint(
    path: Path,
    receipt: dict[str, object],
    *,
    expected_commit: str,
    expected_tree: str,
    expected_platform: str,
    expected_state: str,
    expected_concurrency: int,
    background_mode: str,
    hardware: dict[str, object],
    build: dict[str, str],
    calibration_executable_blake3: str,
    runtime_budget: dict[str, object],
) -> None:
    bind_and_validate_cell_receipt(
        receipt,
        expected_commit=expected_commit,
        expected_platform=expected_platform,
        expected_state=expected_state,
        expected_concurrency=expected_concurrency,
        background_mode=background_mode,
        hardware=hardware,
        build=build,
        calibration_executable_blake3=calibration_executable_blake3,
    )
    write_json_atomic(path, {
        "schema": "hyphae-native-g7-validated-cell-checkpoint-v1",
        "status": "validated-pre-sweep",
        "source_commit": expected_commit,
        "source_tree": expected_tree,
        "platform": expected_platform,
        "state": expected_state,
        "concurrency": expected_concurrency,
        "background_mode": background_mode,
        "runtime_budget": runtime_budget,
        "receipt": receipt,
    })


def load_contract_path(path: Path, label: str) -> dict[str, object]:
    if path.is_symlink() or not path.is_file():
        raise RuntimeError(f"{label} must be a regular non-symlink file")
    payload = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(payload, dict):
        raise RuntimeError(f"{label} must contain one JSON object")
    return payload


def prepare_macos_counter_template(directory: Path) -> Path:
    bootstrap = directory / "bootstrap.trace"
    completed = subprocess.run(
        (
            "xcrun", "xctrace", "record", "--quiet", "--no-prompt",
            "--template", "CPU Counters", "--time-limit", "1s",
            "--output", str(bootstrap), "--launch", "--", "/usr/bin/yes",
        ),
        capture_output=True,
        text=True,
        check=False,
        timeout=30,
    )
    source = bootstrap / "form.template"
    if not source.is_file():
        raise RuntimeError(f"failed to bootstrap Instruments CPU Counters: {completed.stderr.strip()}")
    template = directory / "L1D.tracetemplate"
    prepare_macos_template(source, template)
    return template


def measure_macos_cache_misses(
    binary: Path,
    arguments: list[str],
    environment: dict[str, str],
    template: Path,
) -> int:
    with tempfile.TemporaryDirectory(prefix="hyphae-g7-xctrace-") as directory:
        temporary = Path(directory)
        trace = temporary / "counters.trace"
        ready = temporary / "ready"
        start = temporary / "start"
        profiled_environment = environment.copy()
        profiled_environment["HYPHAE_G7_PROFILE_READY_FILE"] = str(ready)
        profiled_environment["HYPHAE_G7_PROFILE_START_FILE"] = str(start)
        runner = subprocess.Popen(
            [str(binary), *arguments],
            cwd=ROOT,
            env=profiled_environment,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        while not ready.is_file():
            if runner.poll() is not None:
                stdout, stderr = runner.communicate()
                raise RuntimeError(f"macOS counter runner failed before profiling: {stderr.strip()} {stdout.strip()}")
            time.sleep(0.05)
        if ready.read_text(encoding="ascii").strip() != str(runner.pid):
            runner.terminate()
            raise RuntimeError("macOS counter runner disclosed a different process ID")
        profiler = subprocess.Popen(
            [
                "xcrun", "xctrace", "record", "--quiet", "--no-prompt",
                "--template", str(template), "--output", str(trace),
                "--attach", str(runner.pid),
            ],
            cwd=ROOT,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        profiler_deadline = time.monotonic() + 30
        while not trace.is_dir():
            if profiler.poll() is not None:
                _, profiler_stderr = profiler.communicate()
                start.touch()
                runner.communicate()
                raise RuntimeError(f"macOS profiler failed to attach: {profiler_stderr.strip()}")
            if time.monotonic() >= profiler_deadline:
                profiler.terminate()
                start.touch()
                runner.communicate()
                raise RuntimeError("macOS profiler did not start within 30 seconds")
            time.sleep(0.05)
        time.sleep(0.5)
        start.touch()
        stdout, stderr = runner.communicate()
        _, profiler_stderr = profiler.communicate()
        if runner.returncode != 0:
            raise RuntimeError(f"macOS counter runner failed: {stderr.strip()}")
        if profiler.returncode != 0:
            raise RuntimeError(f"macOS profiler failed: {profiler_stderr.strip()}")
        json.loads(stdout)
        export = temporary / "metrics.xml"
        subprocess.run(
            (
                "xcrun", "xctrace", "export", "--input", str(trace),
                "--xpath", '/trace-toc/run/data/table[@schema="MetricTable"]',
                "--output", str(export),
            ),
            capture_output=True,
            text=True,
            check=True,
        )
        return parse_macos_counter_export(export)["cache_misses"]


def main() -> int:
    if os.name == "posix":
        signal.signal(signal.SIGTERM, handle_controller_signal)
        signal.signal(signal.SIGINT, handle_controller_signal)
    parser = argparse.ArgumentParser()
    parser.add_argument("--source-commit", required=True)
    parser.add_argument("--platform", default=sys.platform)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--skip-build", action="store_true")
    parser.add_argument("--observations", type=int, default=1_000_000)
    parser.add_argument("--warmup", type=int, default=100_000)
    parser.add_argument("--background", action="store_true")
    parser.add_argument("--hardware-file", type=Path)
    parser.add_argument("--hardware-profile", type=Path)
    parser.add_argument("--governor-policy", type=Path)
    parser.add_argument("--hardware-calibration", type=Path)
    parser.add_argument("--execution-topology", type=Path)
    parser.add_argument("--cell-timeout-seconds", type=int, default=7_200)
    parser.add_argument("--matrix-timeout-seconds", type=int, default=39_600)
    parser.add_argument("--stall-timeout-seconds", type=int, default=1_800)
    arguments = parser.parse_args()
    if (
        arguments.cell_timeout_seconds <= 0
        or arguments.matrix_timeout_seconds <= 0
        or arguments.stall_timeout_seconds <= 0
    ):
        raise ValueError("G7 timeout bounds must be positive")
    if arguments.observations != 1_000_000 or arguments.warmup != 100_000:
        raise ValueError(
            "G7 closure requires exactly 1,000,000 observations and 100,000 warmups"
        )
    source_tree = verify_source(arguments.source_commit)
    hardware_path = arguments.hardware_file or (
        Path(os.environ["HYPHAE_G7_HARDWARE_FILE"])
        if "HYPHAE_G7_HARDWARE_FILE" in os.environ else None
    )
    if arguments.background and hardware_path is None:
        raise RuntimeError("complete G7 matrix requires --hardware-file")
    hardware = (
        json.loads(hardware_path.read_text(encoding="utf-8"))
        if hardware_path is not None else {
            "dedicated": False,
            "cpu": platform_module.processor() or "undisclosed",
            "topology": "undisclosed",
            "ram_bytes": 0,
            "storage": "undisclosed",
            "filesystem": "undisclosed",
            "governor": "undisclosed",
            "affinity": "uncontrolled",
            "priority": "normal",
            "background_services": "uncontrolled",
            "virtualization": "unknown",
        }
    )
    contract_arguments = (
        (
            "hardware profile",
            arguments.hardware_profile,
            "HYPHAE_G7_HARDWARE_PROFILE_FILE",
        ),
        (
            "hardware calibration",
            arguments.hardware_calibration,
            "HYPHAE_G7_HARDWARE_CALIBRATION_FILE",
        ),
        (
            "governor policy",
            arguments.governor_policy,
            "HYPHAE_G7_GOVERNOR_POLICY_FILE",
        ),
        (
            "execution topology",
            arguments.execution_topology,
            "HYPHAE_G7_EXECUTION_TOPOLOGY_FILE",
        ),
    )
    contract_paths: dict[str, Path] = {}
    contract_values: dict[str, dict[str, object]] = {}
    for label, argument_path, environment_name in contract_arguments:
        path = argument_path or (
            Path(os.environ[environment_name])
            if environment_name in os.environ else None
        )
        if path is None:
            raise RuntimeError(f"G7 runner requires --{label.replace(' ', '-')}")
        contract_values[environment_name] = load_contract_path(path, label)
        contract_paths[environment_name] = path.resolve()
    binary = ROOT / "conformance" / "g7" / "runners" / "rust" / "target" / "release" / "hyphae-native-g7-runner"
    if os.name == "nt":
        binary = binary.with_suffix(".exe")
    if not arguments.skip_build:
        subprocess.run(
            [
                "cargo",
                "build",
                "--manifest-path",
                "conformance/g7/runners/rust/Cargo.toml",
                "--locked",
                "--release",
            ],
            cwd=ROOT,
            check=True,
        )
    if not binary.is_file():
        raise RuntimeError(f"G7 runner not found: {binary}")
    build = build_metadata(binary, source_tree)
    macos_counter_workspace: Path | None = None
    macos_counter_template: Path | None = None
    if sys.platform == "darwin":
        macos_counter_workspace = Path(tempfile.mkdtemp(prefix="hyphae-g7-template-"))
        macos_counter_template = prepare_macos_counter_template(macos_counter_workspace)
    environment = closure_environment(dict(os.environ))
    environment["HYPHAE_G7_OBSERVATIONS"] = str(arguments.observations)
    environment["HYPHAE_G7_WARMUP"] = str(arguments.warmup)
    environment["HYPHAE_G7_SOURCE_TREE"] = source_tree
    for environment_name, path in contract_paths.items():
        environment[environment_name] = str(path)
    progress_path = arguments.output.with_name(
        f"{arguments.output.stem}.progress.json"
    )
    runner_progress_path = arguments.output.with_name(
        f"{arguments.output.stem}.runner-progress.json"
    )
    matrix_runtime_plan_path = arguments.output.with_name(
        f"{arguments.output.stem}.runtime-plan.json"
    )
    progress_path.unlink(missing_ok=True)
    runner_progress_path.unlink(missing_ok=True)
    matrix_runtime_plan_path.unlink(missing_ok=True)
    arguments.output.unlink(missing_ok=True)
    pilot_directory = arguments.output.with_name(f"{arguments.output.stem}.pilots")
    partial_directory = arguments.output.with_name(f"{arguments.output.stem}.partials")
    cell_directory = arguments.output.with_name(f"{arguments.output.stem}.cells")
    for directory in (pilot_directory, partial_directory, cell_directory):
        directory.mkdir(parents=True, exist_ok=True)
    environment["HYPHAE_G7_PROGRESS_FILE"] = str(runner_progress_path.resolve())
    transient_seed_workspace: Path | None = None
    if arguments.background:
        data_root_value = environment.get("HYPHAE_G7_DATA_ROOT")
        if data_root_value is None:
            raise RuntimeError("complete G7 matrix requires HYPHAE_G7_DATA_ROOT")
        data_root = Path(data_root_value)
        if not data_root.is_absolute() or data_root.is_symlink() or not data_root.is_dir():
            raise RuntimeError("G7 data root must be an existing absolute real directory")
    if "HYPHAE_G7_SEARCH_SEED_ROOT" not in environment:
        data_root = environment.get("HYPHAE_G7_DATA_ROOT")
        if data_root is None:
            transient_seed_workspace = Path(
                tempfile.mkdtemp(prefix="hyphae-g7-search-seeds-")
            )
            seed_root = transient_seed_workspace
        else:
            seed_root = Path(data_root) / "shared-search-seeds"
        environment["HYPHAE_G7_SEARCH_SEED_ROOT"] = str(seed_root)
    calibration_identity = contract_values[
        "HYPHAE_G7_HARDWARE_CALIBRATION_FILE"
    ].get("identity")
    if not isinstance(calibration_identity, dict):
        raise RuntimeError("G7 hardware calibration omitted its identity")
    calibration_executable = calibration_identity.get("executable_blake3")
    if not isinstance(calibration_executable, str):
        raise RuntimeError("G7 hardware calibration omitted its executable digest")
    receipts = []
    completed_cells: list[dict[str, object]] = []
    matrix_started = time.monotonic()
    started_unix_nanos = time.time_ns()
    background_modes = BACKGROUND_MODES if arguments.background else ("control",)
    total_cells = len(STATES) * len(background_modes) * len(CONCURRENCIES)
    write_matrix_progress(
        progress_path,
        arguments.source_commit,
        arguments.platform,
        completed_cells,
        total_cells,
        None,
        "running",
        started_unix_nanos,
    )
    plans: list[MatrixCellPlan] = []
    for state in STATES:
        for background_mode in background_modes:
            for concurrency in CONCURRENCIES:
                artifact_name = f"{state}-{background_mode}-{concurrency}"
                plan = MatrixCellPlan(
                    state=state,
                    background_mode=background_mode,
                    concurrency=concurrency,
                    pilot_receipt_path=pilot_directory / f"{artifact_name}.json",
                    pilot_progress_path=pilot_directory / f"{artifact_name}.progress.json",
                    pilot_partial_path=pilot_directory / f"{artifact_name}.partial.json",
                    partial_receipt_path=partial_directory / f"{artifact_name}.json",
                    runner_receipt_path=cell_directory / f"{artifact_name}.runner.json",
                    validated_receipt_path=(
                        cell_directory / f"{artifact_name}.validated.json"
                    ),
                )
                for artifact in (
                    plan.pilot_receipt_path,
                    plan.pilot_progress_path,
                    plan.pilot_partial_path,
                    plan.partial_receipt_path,
                    plan.runner_receipt_path,
                    plan.validated_receipt_path,
                ):
                    artifact.unlink(missing_ok=True)
                plans.append(plan)

    prime_plan = plans[0]
    prime_receipt_path = pilot_directory / "global-seed-prime.json"
    prime_progress_path = pilot_directory / "global-seed-prime.progress.json"
    prime_partial_path = pilot_directory / "global-seed-prime.partial.json"
    current_cell = prime_plan.diagnostic("global-seed-prime")
    current_cell["seed_prime_receipt"] = str(prime_receipt_path)
    write_matrix_progress(
        progress_path,
        arguments.source_commit,
        arguments.platform,
        completed_cells,
        total_cells,
        current_cell,
        "running",
        started_unix_nanos,
    )
    prime_environment = environment.copy()
    prime_environment.pop("HYPHAE_G7_BACKGROUND", None)
    try:
        run_calibration_pilot(
            binary,
            commit=arguments.source_commit,
            source_tree=source_tree,
            platform=arguments.platform,
            state=prime_plan.state,
            concurrency=prime_plan.concurrency,
            environment=prime_environment,
            receipt_path=prime_receipt_path,
            progress_path=prime_progress_path,
            partial_path=prime_partial_path,
            timeout_seconds=float(arguments.cell_timeout_seconds),
            stall_timeout_seconds=float(arguments.stall_timeout_seconds),
        )
    except BaseException:
        write_matrix_progress(
            progress_path,
            arguments.source_commit,
            arguments.platform,
            completed_cells,
            total_cells,
            current_cell,
            "failed",
            started_unix_nanos,
        )
        raise

    for plan in plans:
        current_cell = plan.diagnostic("calibration-pilot")
        current_cell["seed_prime_receipt"] = str(prime_receipt_path)
        write_matrix_progress(
            progress_path,
            arguments.source_commit,
            arguments.platform,
            completed_cells,
            total_cells,
            current_cell,
            "running",
            started_unix_nanos,
        )
        remaining_seconds = (
            arguments.matrix_timeout_seconds - (time.monotonic() - matrix_started)
        )
        if remaining_seconds <= 0:
            write_matrix_progress(
                progress_path,
                arguments.source_commit,
                arguments.platform,
                completed_cells,
                total_cells,
                current_cell,
                "failed",
                started_unix_nanos,
            )
            raise RuntimeBudgetExceeded("G7 matrix budget was exhausted during pilots")
        pilot_environment = environment.copy()
        if plan.background_mode == "interference":
            pilot_environment["HYPHAE_G7_BACKGROUND"] = "1"
        else:
            pilot_environment.pop("HYPHAE_G7_BACKGROUND", None)
        try:
            pilot = run_calibration_pilot(
                binary,
                commit=arguments.source_commit,
                source_tree=source_tree,
                platform=arguments.platform,
                state=plan.state,
                concurrency=plan.concurrency,
                environment=pilot_environment,
                receipt_path=plan.pilot_receipt_path,
                progress_path=plan.pilot_progress_path,
                partial_path=plan.pilot_partial_path,
                timeout_seconds=min(
                    float(arguments.cell_timeout_seconds), remaining_seconds
                ),
                stall_timeout_seconds=float(arguments.stall_timeout_seconds),
            )
            if (
                plan.state == "warm"
                and plan.background_mode == "control"
                and plan.concurrency == 1
            ):
                validate_warm_control_pilot_latency(pilot)
            plan.runtime_budget = derive_cell_runtime_budget(
                pilot,
                expected_commit=arguments.source_commit,
                expected_platform=arguments.platform,
                expected_state=plan.state,
                expected_concurrency=plan.concurrency,
                observations=arguments.observations,
                warmup=arguments.warmup,
                hard_cap_seconds=float(arguments.cell_timeout_seconds),
                seed_primed=True,
            )
        except BaseException:
            write_matrix_progress(
                progress_path,
                arguments.source_commit,
                arguments.platform,
                completed_cells,
                total_cells,
                current_cell,
                "failed",
                started_unix_nanos,
            )
            raise

    calibration_seconds = time.monotonic() - matrix_started
    budgets = [
        plan.runtime_budget for plan in plans if plan.runtime_budget is not None
    ]
    try:
        matrix_runtime_plan = derive_matrix_runtime_plan(
            calibration_seconds=calibration_seconds,
            cell_budgets=budgets,
            hard_cap_seconds=float(arguments.matrix_timeout_seconds),
            expected_cell_count=total_cells,
        )
    except RuntimeBudgetExceeded:
        current_cell = {
            "phase": "matrix-budget-rejected",
            "seed_prime_receipt": str(prime_receipt_path),
            "steady_pilot_receipts": [
                str(plan.pilot_receipt_path) for plan in plans
            ],
        }
        write_matrix_progress(
            progress_path,
            arguments.source_commit,
            arguments.platform,
            completed_cells,
            total_cells,
            current_cell,
            "failed",
            started_unix_nanos,
        )
        raise
    matrix_runtime_plan.update({
        "source_commit": arguments.source_commit,
        "source_tree": source_tree,
        "platform": arguments.platform,
        "seed_prime_receipt": str(prime_receipt_path),
        "steady_pilot_receipts": [str(plan.pilot_receipt_path) for plan in plans],
        "cell_budgets": budgets,
    })
    write_json_atomic(matrix_runtime_plan_path, matrix_runtime_plan)
    write_matrix_progress(
        progress_path,
        arguments.source_commit,
        arguments.platform,
        completed_cells,
        total_cells,
        {
            "phase": "matrix-budget-accepted",
            "runtime_plan": str(matrix_runtime_plan_path),
        },
        "running",
        started_unix_nanos,
    )

    for plan in plans:
        if plan.runtime_budget is None:
            raise RuntimeError("G7 cell plan omitted its calibrated runtime budget")
        current_cell = plan.diagnostic("measurement")
        current_cell["seed_prime_receipt"] = str(prime_receipt_path)
        write_matrix_progress(
            progress_path,
            arguments.source_commit,
            arguments.platform,
            completed_cells,
            total_cells,
            current_cell,
            "running",
            started_unix_nanos,
        )
        remaining_seconds = (
            arguments.matrix_timeout_seconds - (time.monotonic() - matrix_started)
        )
        if float(plan.runtime_budget["timeout_seconds"]) > remaining_seconds:
            write_matrix_progress(
                progress_path,
                arguments.source_commit,
                arguments.platform,
                completed_cells,
                total_cells,
                current_cell,
                "failed",
                started_unix_nanos,
            )
            raise RuntimeBudgetExceeded(
                "G7 matrix has less runtime remaining than the calibrated cell budget"
            )
        cell_environment = environment.copy()
        if plan.background_mode == "interference":
            cell_environment["HYPHAE_G7_BACKGROUND"] = "1"
        else:
            cell_environment.pop("HYPHAE_G7_BACKGROUND", None)
        cell_environment["HYPHAE_G7_PROGRESS_FILE"] = str(
            runner_progress_path.resolve()
        )
        cell_environment["HYPHAE_G7_PARTIAL_RECEIPT_FILE"] = str(
            plan.partial_receipt_path.resolve()
        )
        runner_progress_path.unlink(missing_ok=True)
        plan.partial_receipt_path.unlink(missing_ok=True)
        try:
            receipt = run_cell(
                binary,
                arguments.source_commit,
                arguments.platform,
                plan.state,
                plan.concurrency,
                cell_environment,
                macos_counter_template,
                float(plan.runtime_budget["timeout_seconds"]),
                runner_progress_path,
                float(arguments.stall_timeout_seconds),
            )
            write_json_atomic(plan.runner_receipt_path, receipt)
            if not runner_progress_path.is_file():
                raise RuntimeError("G7 runner did not produce mandatory progress")
            cell_progress = validate_completed_cell_progress(
                json.loads(runner_progress_path.read_text(encoding="utf-8")),
                arguments.source_commit,
            )
            if not plan.partial_receipt_path.is_file():
                raise RuntimeError("G7 runner did not persist a partial receipt")
            partial = validate_partial_receipt(
                json.loads(plan.partial_receipt_path.read_text(encoding="utf-8")),
                expected_commit=arguments.source_commit,
                expected_tree=source_tree,
                expected_platform=arguments.platform,
                expected_state=plan.state,
                expected_concurrency=plan.concurrency,
            )
            validate_cross_artifact_dataset(
                receipt,
                cell_progress,
                partial,
                expected_observations=arguments.observations,
                expected_warmup=arguments.warmup,
            )
            persist_validated_cell_checkpoint(
                plan.validated_receipt_path,
                receipt,
                expected_commit=arguments.source_commit,
                expected_tree=source_tree,
                expected_platform=arguments.platform,
                expected_state=plan.state,
                expected_concurrency=plan.concurrency,
                background_mode=plan.background_mode,
                hardware=hardware,
                build=build,
                calibration_executable_blake3=calibration_executable,
                runtime_budget=plan.runtime_budget,
            )
        except BaseException:
            write_matrix_progress(
                progress_path,
                arguments.source_commit,
                arguments.platform,
                completed_cells,
                total_cells,
                current_cell,
                "failed",
                started_unix_nanos,
            )
            raise
        receipts.append(receipt)
        completed = plan.diagnostic("completed")
        completed["seed_prime_receipt"] = str(prime_receipt_path)
        completed_cells.append(completed)
        write_matrix_progress(
            progress_path,
            arguments.source_commit,
            arguments.platform,
            completed_cells,
            total_cells,
            None,
            "running",
            started_unix_nanos,
        )
    for state in STATES:
        for background_mode in ({value["background_mode"] for value in receipts}):
            sweep = {
                str(concurrency): next(
                    value for value in receipts
                    if value["state"] == state
                    and value["background_mode"] == background_mode
                    and value["concurrency"] == concurrency
                )
                for concurrency in CONCURRENCIES
            }
            throughput = {
                name: {
                    level: receipt["cells"][name]["throughput_per_second"]
                    for level, receipt in sweep.items()
                }
                for name in sweep["1"]["cells"]
            }
            for receipt in sweep.values():
                receipt["saturation"] = {
                    "status": "measured",
                    "levels": list(CONCURRENCIES),
                    "method": "executed-concurrency-sweep",
                    "throughput_per_second": throughput,
                }
    if arguments.background:
        for receipt in receipts:
            if receipt["background_mode"] != "interference":
                continue
            control = next(
                value for value in receipts
                if value["state"] == receipt["state"]
                and value["concurrency"] == receipt["concurrency"]
                and value["background_mode"] == "control"
            )
            receipt["background_interference"]["p99_ratio_by_cell"] = {
                name: receipt["cells"][name]["p99"] / control["cells"][name]["p99"]
                for name in receipt["cells"]
            }
    for receipt in receipts:
        receipt.pop("controller", None)
    result = {
        "schema": "hyphae-native-g7-matrix-v4",
        "gate": "G7",
        "status": "closure-candidate",
        "source_commit": arguments.source_commit,
        "platform": arguments.platform,
        "states": list(STATES),
        "concurrency": list(CONCURRENCIES),
        "background_modes": list(BACKGROUND_MODES if arguments.background else ("control",)),
        "receipts": receipts,
        "claims": [],
        "closure_declared": False,
    }
    if not runner_progress_path.is_file():
        raise RuntimeError("G7 runner progress disappeared before matrix completion")
    runner_progress = json.loads(runner_progress_path.read_text(encoding="utf-8"))
    validate_completed_cell_progress(runner_progress, arguments.source_commit)
    runner_progress_status = runner_progress["status"]
    write_json_atomic(arguments.output, result)
    write_matrix_progress(
        progress_path,
        arguments.source_commit,
        arguments.platform,
        completed_cells,
        total_cells,
        None,
        "completed",
        started_unix_nanos,
    )
    if macos_counter_workspace is not None:
        shutil.rmtree(macos_counter_workspace)
    if transient_seed_workspace is not None:
        shutil.rmtree(transient_seed_workspace)
    print(json.dumps({
        "status": "ok",
        "output": str(arguments.output),
        "cells": len(receipts),
        "progress": str(progress_path),
        "progress_status": "completed",
        "runner_progress": str(runner_progress_path),
        "runner_progress_status": runner_progress_status,
    }))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
