#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only

"""Fail-closed validation for Native hardware-aware performance evidence."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[1]
DEFAULT_PROFILE = ROOT / "config" / "native-performance-evidence-profile.json"
DEFAULT_SUITE_PROFILE = ROOT / "config" / "native-performance-baseline-suite-profile.json"
HEX40 = re.compile(r"[0-9a-f]{40}\Z")
HEX64 = re.compile(r"[0-9a-f]{64}\Z")


class GateFailure(ValueError):
    pass


def profile_digest(profile_path: Path = DEFAULT_PROFILE) -> str:
    return hashlib.sha256(profile_path.read_bytes()).hexdigest()


def suite_profile_digest(profile_path: Path = DEFAULT_SUITE_PROFILE) -> str:
    return hashlib.sha256(profile_path.read_bytes()).hexdigest()


def _load_profile(profile_path: Path) -> dict[str, Any]:
    try:
        profile = json.loads(profile_path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise GateFailure(f"performance evidence profile cannot be read: {error}") from error
    fields = {
        "schema",
        "receipt_schema",
        "progress_schema",
        "workload_classes",
        "engines",
        "clock_components",
        "counters",
        "evidence_classes",
        "states",
        "background_modes",
        "claims",
        "closure_declared",
    }
    if set(profile) != fields:
        raise GateFailure("performance evidence profile fields mismatch")
    if (
        profile.get("schema") != "hyphae-native-performance-evidence-profile-v1"
        or profile.get("receipt_schema") != "hyphae-native-performance-receipt-v1"
        or profile.get("progress_schema") != "hyphae-native-performance-progress-v1"
        or profile.get("claims") != []
        or profile.get("closure_declared") is not False
    ):
        raise GateFailure("performance evidence profile identity mismatch")
    for name in (
        "workload_classes",
        "engines",
        "clock_components",
        "evidence_classes",
        "states",
        "background_modes",
    ):
        values = profile.get(name)
        if (
            not isinstance(values, list)
            or not values
            or any(not isinstance(value, str) or not value for value in values)
            or len(values) != len(set(values))
        ):
            raise GateFailure(f"performance evidence profile {name} is invalid")
    counters = profile.get("counters")
    if (
        not isinstance(counters, dict)
        or not counters
        or any(
            not isinstance(name, str)
            or not name
            or not isinstance(unit, str)
            or not unit
            for name, unit in counters.items()
        )
    ):
        raise GateFailure("performance evidence profile counters are invalid")
    return profile


def _load_suite_profile(
    suite_profile_path: Path,
    evidence_profile: dict[str, Any],
) -> dict[str, Any]:
    try:
        profile = json.loads(suite_profile_path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise GateFailure(f"performance suite profile cannot be read: {error}") from error
    _require_fields(
        profile,
        {"schema", "suite_schema", "name", "required_cells", "claims", "closure_declared"},
        "performance suite profile",
    )
    if (
        profile["schema"] != "hyphae-native-performance-suite-profile-v1"
        or profile["suite_schema"] != "hyphae-native-performance-suite-v1"
        or profile["claims"] != []
        or profile["closure_declared"] is not False
    ):
        raise GateFailure("performance suite profile identity mismatch")
    _require_nonempty_string(profile["name"], "performance suite profile name")
    cells = profile["required_cells"]
    if not isinstance(cells, list) or not cells:
        raise GateFailure("performance suite profile has no required cells")
    identities = [
        _validate_suite_cell(cell, evidence_profile, "performance suite profile cell")
        for cell in cells
    ]
    if len(identities) != len(set(identities)):
        raise GateFailure("performance suite profile contains duplicate cells")
    return profile


def _validate_suite_cell(
    cell: Any,
    evidence_profile: dict[str, Any],
    label: str,
) -> tuple[Any, ...]:
    cell = _require_fields(
        cell,
        {
            "workload_class",
            "engines",
            "operation",
            "concurrency",
            "state",
            "background_mode",
        },
        label,
    )
    if cell["workload_class"] not in evidence_profile["workload_classes"]:
        raise GateFailure(f"{label} workload class is invalid")
    engine_order = {name: index for index, name in enumerate(evidence_profile["engines"])}
    engines = cell["engines"]
    if (
        not isinstance(engines, list)
        or not engines
        or any(engine not in engine_order for engine in engines)
        or len(engines) != len(set(engines))
        or engines != sorted(engines, key=engine_order.get)
    ):
        raise GateFailure(f"{label} engines are invalid or noncanonical")
    operation = _require_nonempty_string(cell["operation"], f"{label} operation")
    concurrency = _require_nonnegative_integer(cell["concurrency"], f"{label} concurrency")
    if concurrency == 0:
        raise GateFailure(f"{label} concurrency must be positive")
    if cell["state"] not in evidence_profile["states"]:
        raise GateFailure(f"{label} state is invalid")
    if cell["background_mode"] not in evidence_profile["background_modes"]:
        raise GateFailure(f"{label} background mode is invalid")
    return (
        cell["workload_class"],
        tuple(engines),
        operation,
        concurrency,
        cell["state"],
        cell["background_mode"],
    )


def _require_fields(value: Any, fields: set[str], label: str) -> dict[str, Any]:
    if not isinstance(value, dict) or set(value) != fields:
        raise GateFailure(f"{label} fields mismatch")
    return value


def _require_sha(value: Any, pattern: re.Pattern[str], label: str) -> str:
    if not isinstance(value, str) or pattern.fullmatch(value) is None:
        raise GateFailure(f"{label} is not canonical")
    return value


def _require_nonempty_string(value: Any, label: str) -> str:
    if not isinstance(value, str) or not value:
        raise GateFailure(f"{label} is empty")
    return value


def _require_nonnegative_integer(value: Any, label: str) -> int:
    if not isinstance(value, int) or isinstance(value, bool) or value < 0:
        raise GateFailure(f"{label} is not a non-negative integer")
    return value


def validate_receipt(
    payload: dict[str, Any],
    expected_commit: str,
    profile_path: Path = DEFAULT_PROFILE,
) -> dict[str, Any]:
    _require_sha(expected_commit, HEX40, "expected source commit")
    profile = _load_profile(profile_path)
    fields = {
        "schema",
        "status",
        "evidence_class",
        "source",
        "environment",
        "workload",
        "dataset",
        "measurement",
        "counters",
        "correctness",
        "claims",
        "closure_declared",
    }
    _require_fields(payload, fields, "performance receipt")
    if (
        payload.get("schema") != profile["receipt_schema"]
        or payload.get("status") != "passed"
        or payload.get("evidence_class") not in profile["evidence_classes"]
        or payload.get("claims") != []
        or payload.get("closure_declared") is not False
    ):
        raise GateFailure("performance receipt identity or open state mismatch")

    source = _require_fields(
        payload["source"],
        {"commit", "tree", "binary_sha256", "profile_sha256", "clean"},
        "performance source",
    )
    if _require_sha(source["commit"], HEX40, "source commit") != expected_commit:
        raise GateFailure("source commit differs from expected commit")
    _require_sha(source["tree"], HEX40, "source tree")
    _require_sha(source["binary_sha256"], HEX64, "binary digest")
    if (
        _require_sha(source["profile_sha256"], HEX64, "profile digest")
        != profile_digest(profile_path)
    ):
        raise GateFailure("performance profile digest differs from authority")
    if not isinstance(source["clean"], bool):
        raise GateFailure("performance source clean flag is invalid")

    environment = _require_fields(
        payload["environment"],
        {
            "platform",
            "target",
            "os",
            "compiler",
            "build_profile",
            "hardware_fingerprint",
            "dedicated",
            "virtualization",
            "topology",
            "affinity",
        },
        "performance environment",
    )
    for name in ("platform", "target", "os", "compiler", "build_profile", "virtualization", "topology", "affinity"):
        _require_nonempty_string(environment[name], f"environment {name}")
    _require_sha(environment["hardware_fingerprint"], HEX64, "hardware fingerprint")
    if not isinstance(environment["dedicated"], bool):
        raise GateFailure("environment dedicated flag is invalid")

    workload = _require_fields(
        payload["workload"],
        {"class", "engines", "operation", "parameters_sha256"},
        "performance workload",
    )
    if workload["class"] not in profile["workload_classes"]:
        raise GateFailure("performance workload class is invalid")
    engines = workload["engines"]
    expected_engine_order = {name: index for index, name in enumerate(profile["engines"])}
    if (
        not isinstance(engines, list)
        or not engines
        or any(engine not in expected_engine_order for engine in engines)
        or len(engines) != len(set(engines))
        or engines != sorted(engines, key=expected_engine_order.get)
    ):
        raise GateFailure("performance workload engines are invalid or noncanonical")
    _require_nonempty_string(workload["operation"], "performance operation")
    _require_sha(workload["parameters_sha256"], HEX64, "workload parameters digest")

    dataset = _require_fields(
        payload["dataset"],
        {"source_commit", "generator", "digest", "records", "bytes"},
        "performance dataset",
    )
    if _require_sha(dataset["source_commit"], HEX40, "dataset source commit") != source["commit"]:
        raise GateFailure("dataset source differs from receipt source")
    _require_nonempty_string(dataset["generator"], "dataset generator")
    _require_sha(dataset["digest"], HEX64, "dataset digest")
    _require_nonnegative_integer(dataset["records"], "dataset records")
    _require_nonnegative_integer(dataset["bytes"], "dataset bytes")

    measurement = _require_fields(
        payload["measurement"],
        {
            "observations",
            "warmup",
            "concurrency",
            "state",
            "background_mode",
            "elapsed_nanos",
            "clock_totals_nanos",
        },
        "performance measurement",
    )
    observations = _require_nonnegative_integer(measurement["observations"], "measurement observations")
    concurrency = _require_nonnegative_integer(measurement["concurrency"], "measurement concurrency")
    if observations == 0 or concurrency == 0:
        raise GateFailure("measurement observations and concurrency must be positive")
    _require_nonnegative_integer(measurement["warmup"], "measurement warmup")
    elapsed = _require_nonnegative_integer(measurement["elapsed_nanos"], "measurement elapsed time")
    if elapsed == 0:
        raise GateFailure("measurement elapsed time must be positive")
    if measurement["state"] not in profile["states"]:
        raise GateFailure("measurement state is invalid")
    if measurement["background_mode"] not in profile["background_modes"]:
        raise GateFailure("measurement background mode is invalid")
    clocks = measurement["clock_totals_nanos"]
    expected_clocks = set(profile["clock_components"])
    if not isinstance(clocks, dict) or set(clocks) != expected_clocks:
        raise GateFailure("performance clock components mismatch")
    for name, value in clocks.items():
        _require_nonnegative_integer(value, f"clock component {name}")
    if sum(clocks.values()) != elapsed:
        raise GateFailure("performance clock decomposition does not equal elapsed time")

    counters = payload["counters"]
    if not isinstance(counters, dict) or set(counters) != set(profile["counters"]):
        raise GateFailure("performance counters mismatch")
    for name, unit in profile["counters"].items():
        counter = _require_fields(
            counters[name],
            {"status", "value", "unit", "provider", "reason"},
            f"performance counter {name}",
        )
        if counter["unit"] != unit:
            raise GateFailure(f"performance counter unit differs: {name}")
        if counter["status"] == "measured":
            _require_nonnegative_integer(counter["value"], f"performance counter {name} value")
            if counter["provider"] in {"", "none"}:
                raise GateFailure(f"measured counter provider is invalid: {name}")
            _require_nonempty_string(counter["provider"], f"performance counter {name} provider")
            if counter["reason"] is not None and not isinstance(counter["reason"], str):
                raise GateFailure(f"measured counter reason is invalid: {name}")
        elif counter["status"] == "unsupported":
            if (
                counter["value"] is not None
                or counter["provider"] != "none"
                or not isinstance(counter["reason"], str)
                or not counter["reason"]
            ):
                raise GateFailure(f"unsupported counter must be explicit and valueless: {name}")
        else:
            raise GateFailure(f"performance counter status is invalid: {name}")

    correctness = _require_fields(
        payload["correctness"],
        {"status", "oracle", "result_digest"},
        "performance correctness",
    )
    if correctness["status"] != "passed":
        raise GateFailure("performance correctness did not pass")
    _require_nonempty_string(correctness["oracle"], "correctness oracle")
    _require_sha(correctness["result_digest"], HEX64, "correctness result digest")

    if payload["evidence_class"] == "qualification-candidate":
        if environment["dedicated"] is not True or environment["virtualization"] != "none":
            raise GateFailure("qualification environment is not dedicated physical hardware")
        if source["clean"] is not True:
            raise GateFailure("qualification source is not a clean commit")
        if any(counter["status"] != "measured" for counter in counters.values()):
            raise GateFailure("qualification counters are not completely measured")

    return {
        "schema": "hyphae-native-performance-receipt-audit-v1",
        "status": "passed",
        "evidence_class": payload["evidence_class"],
        "source_commit": source["commit"],
        "profile_sha256": source["profile_sha256"],
        "workload_class": workload["class"],
        "operation": workload["operation"],
        "counter_status": {
            name: counter["status"] for name, counter in counters.items()
        },
        "claims": [],
        "closure_declared": False,
    }


def validate_suite(
    payload: dict[str, Any],
    expected_commit: str,
    suite_profile_path: Path = DEFAULT_SUITE_PROFILE,
    evidence_profile_path: Path = DEFAULT_PROFILE,
) -> dict[str, Any]:
    _require_sha(expected_commit, HEX40, "expected source commit")
    evidence_profile = _load_profile(evidence_profile_path)
    suite_profile = _load_suite_profile(suite_profile_path, evidence_profile)
    fields = {
        "schema",
        "status",
        "suite_profile_sha256",
        "source_commit",
        "source_tree",
        "binary_sha256",
        "clean",
        "hardware_fingerprint",
        "receipts",
        "claims",
        "closure_declared",
    }
    _require_fields(payload, fields, "performance suite")
    if (
        payload["schema"] != suite_profile["suite_schema"]
        or payload["status"] != "passed"
        or payload["claims"] != []
        or payload["closure_declared"] is not False
    ):
        raise GateFailure("performance suite identity or open state mismatch")
    if (
        _require_sha(payload["suite_profile_sha256"], HEX64, "suite profile digest")
        != suite_profile_digest(suite_profile_path)
    ):
        raise GateFailure("performance suite profile digest differs from authority")
    if _require_sha(payload["source_commit"], HEX40, "suite source commit") != expected_commit:
        raise GateFailure("performance suite source differs from expected commit")
    _require_sha(payload["source_tree"], HEX40, "suite source tree")
    _require_sha(payload["binary_sha256"], HEX64, "suite binary digest")
    _require_sha(payload["hardware_fingerprint"], HEX64, "suite hardware fingerprint")
    if not isinstance(payload["clean"], bool):
        raise GateFailure("performance suite clean flag is invalid")
    receipts = payload["receipts"]
    if not isinstance(receipts, list) or not receipts:
        raise GateFailure("performance suite contains no receipts")

    observed_cells: list[tuple[Any, ...]] = []
    datasets: dict[tuple[str, str], tuple[Any, ...]] = {}
    for index, receipt in enumerate(receipts):
        if not isinstance(receipt, dict):
            raise GateFailure(f"performance suite receipt {index} is invalid")
        validate_receipt(receipt, expected_commit, evidence_profile_path)
        source = receipt["source"]
        environment = receipt["environment"]
        if (
            source["tree"] != payload["source_tree"]
            or source["binary_sha256"] != payload["binary_sha256"]
            or source["clean"] is not payload["clean"]
            or environment["hardware_fingerprint"] != payload["hardware_fingerprint"]
        ):
            raise GateFailure("performance suite source or hardware identity changed")
        cell = {
            "workload_class": receipt["workload"]["class"],
            "engines": receipt["workload"]["engines"],
            "operation": receipt["workload"]["operation"],
            "concurrency": receipt["measurement"]["concurrency"],
            "state": receipt["measurement"]["state"],
            "background_mode": receipt["measurement"]["background_mode"],
        }
        observed_cells.append(
            _validate_suite_cell(cell, evidence_profile, f"performance suite receipt {index} cell")
        )
        dataset = receipt["dataset"]
        dataset_key = (receipt["workload"]["operation"], dataset["generator"])
        dataset_identity = (
            dataset["source_commit"],
            dataset["digest"],
            dataset["records"],
            dataset["bytes"],
        )
        previous = datasets.setdefault(dataset_key, dataset_identity)
        if previous != dataset_identity:
            raise GateFailure("performance suite dataset identity changed across cells")

    if len(observed_cells) != len(set(observed_cells)):
        raise GateFailure("performance suite contains duplicate cells")
    required_cells = {
        _validate_suite_cell(cell, evidence_profile, "performance suite required cell")
        for cell in suite_profile["required_cells"]
    }
    observed = set(observed_cells)
    if observed != required_cells:
        missing = len(required_cells - observed)
        extra = len(observed - required_cells)
        raise GateFailure(f"performance suite matrix mismatch: {missing} missing, {extra} extra")
    return {
        "schema": "hyphae-native-performance-suite-audit-v1",
        "status": "passed",
        "suite": suite_profile["name"],
        "source_commit": payload["source_commit"],
        "source_tree": payload["source_tree"],
        "hardware_fingerprint": payload["hardware_fingerprint"],
        "cells": len(observed_cells),
        "claims": [],
        "closure_declared": False,
    }


def validate_progress(
    payload: dict[str, Any],
    expected_commit: str,
    previous: dict[str, Any] | None = None,
    profile_path: Path = DEFAULT_PROFILE,
) -> dict[str, Any]:
    _require_sha(expected_commit, HEX40, "expected source commit")
    profile = _load_profile(profile_path)
    fields = {
        "schema",
        "source_commit",
        "source_tree",
        "dataset_digest",
        "operation",
        "stage",
        "sequence",
        "completed_units",
        "total_units",
        "unit",
        "elapsed_nanos",
        "status",
        "checkpoint_digest",
    }
    _require_fields(payload, fields, "performance progress")
    if payload["schema"] != profile["progress_schema"]:
        raise GateFailure("performance progress schema mismatch")
    if _require_sha(payload["source_commit"], HEX40, "progress source commit") != expected_commit:
        raise GateFailure("progress source commit differs from expected commit")
    _require_sha(payload["source_tree"], HEX40, "progress source tree")
    _require_sha(payload["dataset_digest"], HEX64, "progress dataset digest")
    _require_nonempty_string(payload["operation"], "progress operation")
    _require_nonempty_string(payload["stage"], "progress stage")
    _require_nonempty_string(payload["unit"], "progress unit")
    sequence = _require_nonnegative_integer(payload["sequence"], "progress sequence")
    completed = _require_nonnegative_integer(payload["completed_units"], "progress completed units")
    total = _require_nonnegative_integer(payload["total_units"], "progress total units")
    elapsed = _require_nonnegative_integer(payload["elapsed_nanos"], "progress elapsed time")
    if sequence == 0 or total == 0 or completed > total:
        raise GateFailure("performance progress bounds are invalid")
    if payload["status"] not in {"running", "completed"}:
        raise GateFailure("performance progress status is invalid")
    checkpoint = payload["checkpoint_digest"]
    if checkpoint is not None:
        _require_sha(checkpoint, HEX64, "progress checkpoint digest")
    if payload["status"] == "completed" and (completed != total or checkpoint is None):
        raise GateFailure("completed progress requires all units and a checkpoint")
    if previous is not None:
        validate_progress(previous, expected_commit, profile_path=profile_path)
        identity = ("source_commit", "source_tree", "dataset_digest", "operation", "unit", "total_units")
        if any(previous[name] != payload[name] for name in identity):
            raise GateFailure("progress identity changed")
        if (
            sequence <= previous["sequence"]
            or completed < previous["completed_units"]
            or elapsed < previous["elapsed_nanos"]
            or previous["status"] == "completed"
        ):
            raise GateFailure("performance progress is not monotonic")
    return {
        "schema": "hyphae-native-performance-progress-audit-v1",
        "status": payload["status"],
        "source_commit": payload["source_commit"],
        "operation": payload["operation"],
        "stage": payload["stage"],
        "sequence": sequence,
        "completed_units": completed,
        "total_units": total,
        "checkpoint_digest": checkpoint,
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    evidence = parser.add_mutually_exclusive_group(required=True)
    evidence.add_argument("--receipt", type=Path)
    evidence.add_argument("--progress", type=Path)
    evidence.add_argument("--suite", type=Path)
    parser.add_argument("--previous-progress", type=Path)
    parser.add_argument("--profile", type=Path, default=DEFAULT_PROFILE)
    parser.add_argument("--suite-profile", type=Path, default=DEFAULT_SUITE_PROFILE)
    parser.add_argument("--expected-commit", required=True)
    parser.add_argument("--output", type=Path, required=True)
    arguments = parser.parse_args()
    try:
        if arguments.receipt is not None:
            if arguments.previous_progress is not None:
                raise GateFailure("previous progress is valid only with --progress")
            payload = json.loads(arguments.receipt.read_text(encoding="utf-8"))
            result = validate_receipt(payload, arguments.expected_commit, arguments.profile)
        elif arguments.progress is not None:
            payload = json.loads(arguments.progress.read_text(encoding="utf-8"))
            previous = (
                json.loads(arguments.previous_progress.read_text(encoding="utf-8"))
                if arguments.previous_progress is not None
                else None
            )
            result = validate_progress(payload, arguments.expected_commit, previous, arguments.profile)
        else:
            if arguments.previous_progress is not None:
                raise GateFailure("previous progress is valid only with --progress")
            payload = json.loads(arguments.suite.read_text(encoding="utf-8"))
            result = validate_suite(
                payload,
                arguments.expected_commit,
                arguments.suite_profile,
                arguments.profile,
            )
        arguments.output.write_text(
            json.dumps(result, indent=2, sort_keys=True) + "\n",
            encoding="utf-8",
        )
    except (GateFailure, OSError, UnicodeError, json.JSONDecodeError) as error:
        print(f"native performance evidence failed: {error}")
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
