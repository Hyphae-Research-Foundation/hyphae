#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Validate one saved v2 keyspace receipt and all external identities."""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import re
import subprocess
import sys
from pathlib import Path
from typing import Any

import check_profile


RECEIPT_SCHEMA = "hyphae-baseline-harness-v2"
VALKEY_RECEIPT_SCHEMA = "hyphae-external-valkey-baseline-receipt-v2"
MAX_RECEIPT_BYTES = 16 * 1024 * 1024
SHA256 = re.compile(r"^[0-9a-f]{64}$")
GIT_OID = re.compile(r"^[0-9a-f]{40}$")
ALWAYS_WRITE_DOMAIN = 0x616C776179730001
NO_WRITE_DOMAIN = 0x6E6F000000000001
U64_MASK = (1 << 64) - 1
PRODUCTION_WORKLOAD = {
    "keys": 1_000_000,
    "gets": 500_000,
    "strict_sets": 10_000,
    "relaxed_sets": 200_000,
    "seed": 0x5EED202608290001,
}


class ReceiptError(Exception):
    """The saved receipt is malformed or fails an identity binding."""


def _reject_duplicate_keys(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise ReceiptError(f"duplicate JSON key {key!r}")
        result[key] = value
    return result


def _reject_nonfinite(value: str) -> None:
    raise ReceiptError(f"non-finite JSON number {value} is forbidden")


def load_receipt(path: Path) -> dict[str, Any]:
    encoded = path.read_bytes()
    if len(encoded) > MAX_RECEIPT_BYTES:
        raise ReceiptError(f"receipt exceeds {MAX_RECEIPT_BYTES} bytes")
    try:
        value = json.loads(
            encoded,
            object_pairs_hook=_reject_duplicate_keys,
            parse_constant=_reject_nonfinite,
        )
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise ReceiptError(f"cannot parse receipt: {error}") from error
    if not isinstance(value, dict):
        raise ReceiptError("receipt must be one JSON object")
    return value


def _object(value: Any, path: str, failures: list[str]) -> dict[str, Any]:
    if not isinstance(value, dict):
        failures.append(f"{path} must be an object")
        return {}
    return value


def _exact_keys(value: dict[str, Any], expected: set[str], path: str, failures: list[str]) -> None:
    if set(value) != expected:
        failures.append(f"{path} fields differ: expected {sorted(expected)}, found {sorted(value)}")


def _sha256(value: Any, path: str, failures: list[str]) -> str:
    if not isinstance(value, str) or SHA256.fullmatch(value) is None:
        failures.append(f"{path} must be one lowercase SHA-256 digest")
        return ""
    return value


def _git_oid(value: Any, path: str, failures: list[str]) -> None:
    if not isinstance(value, str) or GIT_OID.fullmatch(value) is None:
        failures.append(f"{path} must be one lowercase Git object identity")


def _positive_text(value: Any, path: str, failures: list[str]) -> str:
    if not isinstance(value, str) or not value or value == "unknown" or "\0" in value:
        failures.append(f"{path} must be a known nonempty text value")
        return ""
    return value


def _integer(
    value: Any,
    path: str,
    failures: list[str],
    *,
    minimum: int = 0,
    maximum: int = U64_MASK,
) -> int:
    if (
        isinstance(value, bool)
        or not isinstance(value, int)
        or value < minimum
        or value > maximum
    ):
        failures.append(f"{path} must be an integer in [{minimum}, {maximum}]")
        return -1
    return value


def _is_finite_number(value: Any) -> bool:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return False
    try:
        return math.isfinite(value)
    except (OverflowError, TypeError, ValueError):
        return False


def effective_config_sha256(config: dict[str, str]) -> str:
    digest = hashlib.sha256(b"hyphae-valkey-effective-config-v1\0")
    for key, value in sorted(config.items()):
        encoded_key = key.encode("utf-8")
        encoded_value = value.encode("utf-8")
        digest.update(len(encoded_key).to_bytes(8, "big"))
        digest.update(encoded_key)
        digest.update(len(encoded_value).to_bytes(8, "big"))
        digest.update(encoded_value)
    return digest.hexdigest()


def _key_sequence_sha256(seed: int, keys: int, operations: int, domain: bytes) -> str:
    state = seed & U64_MASK
    if state == 0:
        state = 1
    digest = hashlib.sha256(domain)
    for _ in range(operations):
        value = state
        value ^= value >> 12
        value ^= (value << 25) & U64_MASK
        value ^= value >> 27
        state = value & U64_MASK
        random = (state * 0x2545F4914F6CDD1D) & U64_MASK
        upper = random >> 32
        index = ((upper * upper) >> 32) % max(keys, 1)
        digest.update(index.to_bytes(8, "big"))
    return digest.hexdigest()


def write_key_sequence_sha256(seed: int, keys: int, writes: int) -> str:
    return _key_sequence_sha256(
        seed, keys, writes, b"hyphae-baseline-write-keys-v1\0"
    )


def read_key_sequence_sha256(seed: int, keys: int, reads: int) -> str:
    return _key_sequence_sha256(seed, keys, reads, b"hyphae-baseline-read-keys-v1\0")


def initial_dataset_sha256(keys: int) -> str:
    digest = hashlib.sha256(b"hyphae-baseline-keyspace-dataset-v1\0")
    for index in range(keys):
        key = f"key-{index:010}".encode("ascii")
        value = f"value-{index:010}-{index * 0x5851:040x}".encode("ascii")
        digest.update(len(key).to_bytes(8, "big"))
        digest.update(key)
        digest.update(len(value).to_bytes(8, "big"))
        digest.update(value)
    return digest.hexdigest()


def setup_marker_sha256(setup_id: str, lane: str) -> str:
    return hashlib.sha256(f"{setup_id}:{lane}\n".encode("ascii")).hexdigest()


def _summary(
    value: Any,
    path: str,
    expected_operations: int,
    expected_label: str,
    failures: list[str],
) -> None:
    summary = _object(value, path, failures)
    _exact_keys(
        summary,
        {"label", "operations", "wall_nanos", "ops_per_second", "latency_nanos"},
        path,
        failures,
    )
    if summary.get("label") != expected_label:
        failures.append(f"{path}.label differs")
    if summary.get("operations") != expected_operations:
        failures.append(f"{path}.operations differs")
    operations = _integer(
        summary.get("operations"),
        f"{path}.operations",
        failures,
        minimum=1,
        maximum=10_000_000,
    )
    wall_nanos = _integer(summary.get("wall_nanos"), f"{path}.wall_nanos", failures, minimum=1)
    throughput = summary.get("ops_per_second")
    if (
        not _is_finite_number(throughput) or throughput < 0
    ):
        failures.append(f"{path}.ops_per_second must be finite and nonnegative")
    elif operations > 0 and wall_nanos > 0:
        expected_throughput = operations * 1_000_000_000.0 / wall_nanos
        if not math.isclose(throughput, expected_throughput, rel_tol=1e-12, abs_tol=1e-9):
            failures.append(f"{path}.ops_per_second differs from operations/wall_nanos")
    latency = _object(summary.get("latency_nanos"), f"{path}.latency_nanos", failures)
    _exact_keys(
        latency,
        {"total", "mean", "p50", "p95", "p99", "p999", "max"},
        f"{path}.latency_nanos",
        failures,
    )
    total = _integer(latency.get("total"), f"{path}.latency_nanos.total", failures)
    values = {
        field: _integer(latency.get(field), f"{path}.latency_nanos.{field}", failures)
        for field in ("mean", "p50", "p95", "p99", "p999", "max")
    }
    if not (
        values["p50"]
        <= values["p95"]
        <= values["p99"]
        <= values["p999"]
        <= values["max"]
    ):
        failures.append(f"{path}.latency_nanos percentile ordering differs")
    if values["mean"] > values["max"]:
        failures.append(f"{path}.latency_nanos mean exceeds max")
    if values["max"] > wall_nanos:
        failures.append(f"{path}.latency_nanos max exceeds wall_nanos")
    if total > wall_nanos:
        failures.append(f"{path}.latency_nanos total exceeds wall_nanos")
    if operations > 0 and total >= 0 and values["mean"] >= 0:
        lower = values["mean"] * operations
        upper = (values["mean"] + 1) * operations
        if not lower <= total < upper or values["mean"] != total // operations:
            failures.append(f"{path}.latency_nanos mean/total rounding differs")
    if operations > 0 and total >= 0 and values["max"] > total:
        failures.append(f"{path}.latency_nanos max exceeds total")


def _hyphae_lane(
    value: Any,
    path: str,
    lane: str,
    workload: dict[str, Any],
    failures: list[str],
) -> tuple[str, str, str]:
    receipt = _object(value, path, failures)
    _exact_keys(
        receipt,
        {
            "lane",
            "durability",
            "persistence_acknowledgement",
            "setup_identity",
            "get_hits",
            "get",
            "read_key_sequence_sha256",
            "write_key_sequence_sha256",
            "set",
        },
        path,
        failures,
    )
    expected = {
        "always": ("strict", "fsync_per_commit", workload.get("strict_sets")),
        "no": ("memory", "none", workload.get("relaxed_sets")),
    }[lane]
    if (
        receipt.get("lane"),
        receipt.get("durability"),
        receipt.get("persistence_acknowledgement"),
    ) != (lane, expected[0], expected[1]):
        failures.append(f"{path} lane or durability identity differs")
    if receipt.get("get_hits") != workload.get("gets"):
        failures.append(f"{path}.get_hits differs from workload")
    _summary(receipt.get("get"), f"{path}.get", workload.get("gets", -1), "get_latest", failures)
    set_label = "set_strict_fsync_per_commit" if lane == "always" else "set_memory_no_fsync_ack"
    _summary(receipt.get("set"), f"{path}.set", expected[2], set_label, failures)
    read_sequence = _sha256(
        receipt.get("read_key_sequence_sha256"),
        f"{path}.read_key_sequence_sha256",
        failures,
    )
    expected_reads = read_key_sequence_sha256(
        workload["seed"], workload["keys"], workload["gets"]
    )
    if read_sequence != expected_reads:
        failures.append(f"{path}.read_key_sequence_sha256 differs from deterministic workload")
    sequence = _sha256(receipt.get("write_key_sequence_sha256"), f"{path}.write_key_sequence_sha256", failures)
    domain = ALWAYS_WRITE_DOMAIN if lane == "always" else NO_WRITE_DOMAIN
    expected_sequence = write_key_sequence_sha256(
        workload["seed"] ^ domain,
        workload["keys"],
        expected[2] if isinstance(expected[2], int) else 0,
    )
    if sequence != expected_sequence:
        failures.append(f"{path}.write_key_sequence_sha256 differs from deterministic workload")

    setup = _object(receipt.get("setup_identity"), f"{path}.setup_identity", failures)
    _exact_keys(
        setup,
        {
            "state",
            "data_directory",
            "initial_probe_absent",
            "loaded_probe_matches",
            "loaded_keys",
            "dataset_sha256",
        },
        f"{path}.setup_identity",
        failures,
    )
    if setup.get("state") != "fresh-created-database":
        failures.append(f"{path}.setup_identity.state differs")
    directory = _positive_text(setup.get("data_directory"), f"{path}.setup_identity.data_directory", failures)
    if setup.get("initial_probe_absent") is not True or setup.get("loaded_probe_matches") is not True:
        failures.append(f"{path}.setup_identity does not prove fresh load")
    if setup.get("loaded_keys") != workload.get("keys"):
        failures.append(f"{path}.setup_identity.loaded_keys differs")
    dataset = _sha256(setup.get("dataset_sha256"), f"{path}.setup_identity.dataset_sha256", failures)
    return read_sequence, sequence, f"{directory}\0{dataset}"


def _external_lane(
    value: Any,
    path: str,
    lane: dict[str, Any],
    workload: dict[str, Any],
    profile: dict[str, Any],
    failures: list[str],
    expected_valkey_binary_sha256: str | None = None,
    expected_valkey_binary_path: Path | None = None,
    executed_source_commit: Any = None,
    executed_source_tree: Any = None,
) -> tuple[str, str, str, str, str, str, str]:
    receipt = _object(value, path, failures)
    _exact_keys(
        receipt,
        {
            "schema",
            "engine",
            "lane",
            "version",
            "authority",
            "external_identity",
            "setup_identity",
            "transport",
            "persistence",
            "connection_alive",
            "get_hits",
            "get",
            "read_key_sequence_sha256",
            "write_key_sequence_sha256",
            "set",
        },
        path,
        failures,
    )
    name = lane["name"]
    if (
        receipt.get("schema") != VALKEY_RECEIPT_SCHEMA
        or receipt.get("engine") != "valkey-external"
        or receipt.get("lane") != name
        or receipt.get("version") != check_profile.EXPECTED_VERSION
        or receipt.get("transport") != "unix domain socket"
        or receipt.get("connection_alive") is not True
    ):
        failures.append(f"{path} fixed receipt identity differs")
    authority = _object(receipt.get("authority"), f"{path}.authority", failures)
    _exact_keys(
        authority,
        {
            "application_core",
            "oracle_source_commit",
            "hyphae_semantic_authority",
            "executed_source_binding",
        },
        f"{path}.authority",
        failures,
    )
    application_core = _object(
        authority.get("application_core"), f"{path}.authority.application_core", failures
    )
    _exact_keys(
        application_core,
        {
            "schema",
            "profile",
            "profile_version",
            "status",
            "foundation_only",
            "claim_semantics_sha256",
            "profile_sha256",
        },
        f"{path}.authority.application_core",
        failures,
    )
    if application_core != {
        "schema": profile["schema"],
        "profile": profile["profile"],
        "profile_version": profile["profile_version"],
        "status": profile["status"],
        "foundation_only": profile["foundation_only"],
        "claim_semantics_sha256": profile["claim_semantics_seal"]["sha256"],
        "profile_sha256": check_profile.EXPECTED_PROFILE_SHA256,
    }:
        failures.append(f"{path}.authority.application_core differs")
    if authority.get("oracle_source_commit") != profile["oracle"]["source_commit"]:
        failures.append(f"{path}.authority.oracle_source_commit differs")
    semantic_authority = _object(
        authority.get("hyphae_semantic_authority"),
        f"{path}.authority.hyphae_semantic_authority",
        failures,
    )
    if semantic_authority != profile["hyphae_authority"]:
        failures.append(f"{path}.authority.hyphae_semantic_authority differs")
    executed_binding = _object(
        authority.get("executed_source_binding"),
        f"{path}.authority.executed_source_binding",
        failures,
    )
    expected_executed_binding = {
        "policy": profile["executed_source_binding"]["policy"],
        "source_commit": executed_source_commit,
        "source_tree": executed_source_tree,
        "semantic_bundle_sha256": profile["executed_source_binding"][
            "semantic_bundle_sha256"
        ],
    }
    if executed_binding != expected_executed_binding:
        failures.append(f"{path}.authority.executed_source_binding differs")
    persistence = _object(receipt.get("persistence"), f"{path}.persistence", failures)
    _exact_keys(persistence, {"appendonly", "appendfsync"}, f"{path}.persistence", failures)
    if persistence != {"appendonly": lane["appendonly"], "appendfsync": lane["appendfsync"]}:
        failures.append(f"{path}.persistence differs")
    if receipt.get("get_hits") != workload.get("gets"):
        failures.append(f"{path}.get_hits differs from workload")
    writes = workload.get("strict_sets") if name == "always" else workload.get("relaxed_sets")
    _summary(receipt.get("get"), f"{path}.get", workload.get("gets", -1), "get_uds", failures)
    _summary(receipt.get("set"), f"{path}.set", writes, "set_uds", failures)
    read_sequence = _sha256(
        receipt.get("read_key_sequence_sha256"),
        f"{path}.read_key_sequence_sha256",
        failures,
    )
    expected_reads = read_key_sequence_sha256(
        workload["seed"], workload["keys"], workload["gets"]
    )
    if read_sequence != expected_reads:
        failures.append(f"{path}.read_key_sequence_sha256 differs from deterministic workload")
    sequence = _sha256(receipt.get("write_key_sequence_sha256"), f"{path}.write_key_sequence_sha256", failures)
    domain = ALWAYS_WRITE_DOMAIN if name == "always" else NO_WRITE_DOMAIN
    expected_sequence = write_key_sequence_sha256(
        workload["seed"] ^ domain,
        workload["keys"],
        writes if isinstance(writes, int) else 0,
    )
    if sequence != expected_sequence:
        failures.append(f"{path}.write_key_sequence_sha256 differs from deterministic workload")

    identity = _object(receipt.get("external_identity"), f"{path}.external_identity", failures)
    _exact_keys(identity, {"source_archive", "build", "configuration", "runtime"}, f"{path}.external_identity", failures)
    source = _object(identity.get("source_archive"), f"{path}.external_identity.source_archive", failures)
    _exact_keys(source, {"url", "sha256"}, f"{path}.external_identity.source_archive", failures)
    if source != {
        "url": profile["oracle"]["artifact"]["url"],
        "sha256": profile["oracle"]["artifact"]["sha256"],
    }:
        failures.append(f"{path}.external_identity.source_archive differs")

    build = _object(identity.get("build"), f"{path}.external_identity.build", failures)
    _exact_keys(
        build,
        {
            "compiler",
            "flags",
            "server_binary",
            "runner_expected_server_binary_sha256",
            "executed_server_binary_sha256",
            "retained_server_artifact",
            "retained_server_artifact_sha256",
        },
        f"{path}.external_identity.build",
        failures,
    )
    _positive_text(build.get("compiler"), f"{path}.external_identity.build.compiler", failures)
    flags = _positive_text(build.get("flags"), f"{path}.external_identity.build.flags", failures)
    for required in ("BUILD_TLS=no", "MALLOC=", "OPTIMIZATION=", "CC=", "CFLAGS=", "LDFLAGS="):
        if not any(part.startswith(required) for part in flags.split(";")):
            failures.append(f"{path}.external_identity.build.flags omits {required}")
    executable = _positive_text(build.get("server_binary"), f"{path}.external_identity.build.server_binary", failures)
    expected_binary = _sha256(
        build.get("runner_expected_server_binary_sha256"),
        f"{path}.external_identity.build.runner_expected_server_binary_sha256",
        failures,
    )
    executed_binary = _sha256(
        build.get("executed_server_binary_sha256"),
        f"{path}.external_identity.build.executed_server_binary_sha256",
        failures,
    )
    if expected_binary != executed_binary:
        failures.append(f"{path}.external_identity.build binary digests differ")
    retained_artifact = _positive_text(
        build.get("retained_server_artifact"),
        f"{path}.external_identity.build.retained_server_artifact",
        failures,
    )
    retained_digest = _sha256(
        build.get("retained_server_artifact_sha256"),
        f"{path}.external_identity.build.retained_server_artifact_sha256",
        failures,
    )
    if retained_digest != executed_binary:
        failures.append(f"{path}.external_identity.build retained artifact digest differs")
    if expected_valkey_binary_sha256 is not None and retained_digest != expected_valkey_binary_sha256:
        failures.append(f"{path}.external_identity.build differs from retained Valkey artifact")
    if expected_valkey_binary_path is not None:
        try:
            retained_path_matches = (
                Path(retained_artifact).resolve() == expected_valkey_binary_path.resolve()
            )
        except (OSError, ValueError):
            retained_path_matches = False
        if not retained_path_matches:
            failures.append(f"{path}.external_identity.build retained artifact path differs")

    configuration = _object(identity.get("configuration"), f"{path}.external_identity.configuration", failures)
    _exact_keys(
        configuration,
        {"source_file", "source_sha256", "effective_sha256", "effective"},
        f"{path}.external_identity.configuration",
        failures,
    )
    source_file = _positive_text(
        configuration.get("source_file"), f"{path}.external_identity.configuration.source_file", failures
    )
    if _sha256(configuration.get("source_sha256"), f"{path}.external_identity.configuration.source_sha256", failures) != lane["sha256"]:
        failures.append(f"{path}.external_identity.configuration source digest differs")
    effective = _object(configuration.get("effective"), f"{path}.external_identity.configuration.effective", failures)
    if not all(isinstance(key, str) and isinstance(item, str) for key, item in effective.items()):
        failures.append(f"{path}.external_identity.configuration.effective must contain text pairs")
        effective = {}
    effective_digest = _sha256(
        configuration.get("effective_sha256"),
        f"{path}.external_identity.configuration.effective_sha256",
        failures,
    )
    if effective_digest != effective_config_sha256(effective):
        failures.append(f"{path}.external_identity.configuration effective digest differs")
    expected_effective = {
        "appendonly": lane["appendonly"],
        "appendfsync": lane["appendfsync"],
        "port": "0",
        "unixsocket": lane["socket"],
        "maxmemory-policy": "noeviction",
        "maxmemory": "0",
        "databases": "1",
        "save": "",
        "protected-mode": "yes",
        "daemonize": "yes",
        "supervised": "no",
        "dir": lane["dir"],
    }
    for key, expected in expected_effective.items():
        if effective.get(key) != expected:
            failures.append(f"{path}.external_identity.configuration.effective {key} differs")

    runtime = _object(identity.get("runtime"), f"{path}.external_identity.runtime", failures)
    if not all(isinstance(key, str) and isinstance(item, str) for key, item in runtime.items()):
        failures.append(f"{path}.external_identity.runtime must contain text pairs")
    required_runtime = {
        "server_name": "valkey",
        "valkey_version": check_profile.EXPECTED_VERSION,
        "redis_git_dirty": "0",
        "server_mode": "standalone",
        "process_supervised": "no",
        "tcp_port": "0",
    }
    for key, expected in required_runtime.items():
        if runtime.get(key) != expected:
            failures.append(f"{path}.external_identity.runtime {key} differs")
    for key in (
        "redis_version",
        "valkey_release_stage",
        "redis_git_sha1",
        "redis_build_id",
        "os",
        "arch_bits",
        "gcc_version",
        "process_id",
        "run_id",
        "executable",
        "config_file",
    ):
        _positive_text(runtime.get(key), f"{path}.external_identity.runtime.{key}", failures)
    if runtime.get("executable") != executable or runtime.get("config_file") != source_file:
        failures.append(f"{path}.external_identity runtime paths differ from build/config identity")
    process_id = runtime.get("process_id") if isinstance(runtime.get("process_id"), str) else ""
    run_id = runtime.get("run_id") if isinstance(runtime.get("run_id"), str) else ""
    if GIT_OID.fullmatch(run_id) is None:
        failures.append(f"{path}.external_identity.runtime.run_id is invalid")
    if runtime.get("arch_bits") not in {"32", "64"}:
        failures.append(f"{path}.external_identity.runtime.arch_bits is invalid")
    try:
        parsed_pid = int(process_id)
        if (
            re.fullmatch(r"[1-9][0-9]*", process_id) is None
            or parsed_pid > 2**32 - 1
            or str(parsed_pid) != process_id
        ):
            raise ValueError
    except (TypeError, ValueError):
        failures.append(f"{path}.external_identity.runtime.process_id is invalid")

    setup = _object(receipt.get("setup_identity"), f"{path}.setup_identity", failures)
    _exact_keys(
        setup,
        {
            "state",
            "setup_id",
            "data_directory",
            "marker_sha256",
            "initial_dbsize",
            "loaded_dbsize",
            "dataset_sha256",
            "process_id",
            "started_pid",
            "run_id",
        },
        f"{path}.setup_identity",
        failures,
    )
    if setup.get("state") != "fresh-recreated-directory-and-server":
        failures.append(f"{path}.setup_identity.state differs")
    setup_id = _sha256(setup.get("setup_id"), f"{path}.setup_identity.setup_id", failures)
    marker_sha256 = _sha256(
        setup.get("marker_sha256"), f"{path}.setup_identity.marker_sha256", failures
    )
    if setup_id and marker_sha256 != setup_marker_sha256(setup_id, name):
        failures.append(f"{path}.setup_identity marker digest differs from setup identity")
    if setup.get("data_directory") != lane["dir"]:
        failures.append(f"{path}.setup_identity.data_directory differs")
    if setup.get("initial_dbsize") != 0 or setup.get("loaded_dbsize") != workload.get("keys"):
        failures.append(f"{path}.setup_identity cardinality proof differs")
    if (
        setup.get("process_id") != process_id
        or setup.get("started_pid") != process_id
        or setup.get("run_id") != run_id
    ):
        failures.append(f"{path}.setup_identity runtime identity differs")
    dataset = _sha256(setup.get("dataset_sha256"), f"{path}.setup_identity.dataset_sha256", failures)
    return (
        read_sequence,
        sequence,
        dataset,
        setup_id,
        executed_binary,
        process_id,
        run_id,
    )


def validate_receipt(
    receipt: dict[str, Any],
    profile: dict[str, Any],
    *,
    expected_source_commit: str | None = None,
    expected_source_tree: str | None = None,
    expected_hyphae_binary_sha256: str | None = None,
    expected_valkey_binary_sha256: str | None = None,
    expected_valkey_binary_path: Path | None = None,
    require_authoritative: bool = False,
) -> list[str]:
    failures: list[str] = []
    _exact_keys(
        receipt,
        {"schema", "evidence_authority", "hyphae_execution", "environment", "results"},
        "receipt",
        failures,
    )
    if receipt.get("schema") != RECEIPT_SCHEMA:
        failures.append("receipt.schema differs")

    evidence_authority = _object(
        receipt.get("evidence_authority"), "receipt.evidence_authority", failures
    )
    _exact_keys(
        evidence_authority,
        {"status", "hardware_qualification", "storage"},
        "receipt.evidence_authority",
        failures,
    )
    authority_status = evidence_authority.get("status")
    qualification = evidence_authority.get("hardware_qualification")
    storage = _object(evidence_authority.get("storage"), "receipt.evidence_authority.storage", failures)
    _exact_keys(
        storage,
        {"model", "device", "filesystem", "rotational", "queue_depth"},
        "receipt.evidence_authority.storage",
        failures,
    )
    if authority_status == "authoritative-dedicated-hardware":
        if (
            qualification != "aws-ec2-i7i.metal-24xl"
            or storage.get("model") != "Amazon EC2 NVMe Instance Storage"
        ):
            failures.append("authoritative receipt hardware qualification differs")
        device = storage.get("device")
        if not isinstance(device, str) or re.fullmatch(r"259:[0-9]+", device) is None:
            failures.append("authoritative receipt storage device is not NVMe")
        if storage.get("filesystem") not in {"ext4", "xfs"}:
            failures.append("authoritative receipt storage filesystem differs")
        if storage.get("rotational") is not False:
            failures.append("authoritative receipt storage must be non-rotational")
        _integer(
            storage.get("queue_depth"),
            "receipt.evidence_authority.storage.queue_depth",
            failures,
            minimum=1,
        )
    elif authority_status == "non-authoritative-diagnostic":
        _positive_text(qualification, "receipt.evidence_authority.hardware_qualification", failures)
        _positive_text(storage.get("model"), "receipt.evidence_authority.storage.model", failures)
    else:
        failures.append("receipt evidence authority status differs")
    if require_authoritative and authority_status != "authoritative-dedicated-hardware":
        failures.append("receipt is non-authoritative but authoritative evidence is required")

    execution = _object(receipt.get("hyphae_execution"), "receipt.hyphae_execution", failures)
    _exact_keys(execution, {"source", "build"}, "receipt.hyphae_execution", failures)
    source = _object(execution.get("source"), "receipt.hyphae_execution.source", failures)
    _exact_keys(
        source,
        {"pre_build", "post_build", "stable_during_build"},
        "receipt.hyphae_execution.source",
        failures,
    )
    pre_build = _object(source.get("pre_build"), "receipt.hyphae_execution.source.pre_build", failures)
    post_build = _object(source.get("post_build"), "receipt.hyphae_execution.source.post_build", failures)
    for name, identity in (("pre_build", pre_build), ("post_build", post_build)):
        identity_path = f"receipt.hyphae_execution.source.{name}"
        _exact_keys(identity, {"commit", "tree", "worktree_clean"}, identity_path, failures)
        _git_oid(identity.get("commit"), f"{identity_path}.commit", failures)
        _git_oid(identity.get("tree"), f"{identity_path}.tree", failures)
        if identity.get("worktree_clean") is not True:
            failures.append(f"{identity_path}.worktree_clean must be true")
    if pre_build != post_build or source.get("stable_during_build") is not True:
        failures.append("receipt Hyphae source identity changed during build")
    if expected_source_commit is not None and post_build.get("commit") != expected_source_commit:
        failures.append("receipt Hyphae source commit differs from expected source")
    if expected_source_tree is not None and post_build.get("tree") != expected_source_tree:
        failures.append("receipt Hyphae source tree differs from expected source")
    build = _object(execution.get("build"), "receipt.hyphae_execution.build", failures)
    _exact_keys(
        build,
        {
            "rustc",
            "rustc_verbose",
            "cargo",
            "profile",
            "rustflags",
            "cargo_encoded_rustflags",
            "rustc_wrapper",
            "rustc_workspace_wrapper",
            "cargo_profile_overrides",
            "cargo_incremental",
            "command",
            "executable",
            "runner_expected_sha256",
            "executed_harness_product_sha256",
            "embedded_product",
        },
        "receipt.hyphae_execution.build",
        failures,
    )
    _positive_text(build.get("rustc"), "receipt.hyphae_execution.build.rustc", failures)
    _positive_text(build.get("rustc_verbose"), "receipt.hyphae_execution.build.rustc_verbose", failures)
    _positive_text(build.get("cargo"), "receipt.hyphae_execution.build.cargo", failures)
    _positive_text(build.get("executable"), "receipt.hyphae_execution.build.executable", failures)
    if (
        build.get("profile") != "release"
        or build.get("rustflags") != "empty"
        or build.get("cargo_encoded_rustflags") != "empty"
        or build.get("rustc_wrapper") != "unset"
        or build.get("rustc_workspace_wrapper") != "unset"
        or build.get("cargo_profile_overrides") != "absent"
        or build.get("cargo_incremental") != "disabled"
        or build.get("command")
        != "cargo build --release --locked --manifest-path benchmarks/baseline-harness/Cargo.toml"
    ):
        failures.append("receipt Hyphae sanitized build inputs differ")
    if (
        not isinstance(build.get("rustc"), str)
        or not build["rustc"].startswith("rustc 1.96.0")
        or not isinstance(build.get("rustc_verbose"), str)
        or "release: 1.96.0" not in build["rustc_verbose"]
        or not isinstance(build.get("cargo"), str)
        or not build["cargo"].startswith("cargo 1.96.0")
    ):
        failures.append("receipt Hyphae build toolchain differs from pinned Rust 1.96.0")
    expected_hyphae = _sha256(
        build.get("runner_expected_sha256"), "receipt.hyphae_execution.build.runner_expected_sha256", failures
    )
    executed_hyphae = _sha256(
        build.get("executed_harness_product_sha256"),
        "receipt.hyphae_execution.build.executed_harness_product_sha256",
        failures,
    )
    if expected_hyphae != executed_hyphae or build.get("embedded_product") is not True:
        failures.append("receipt Hyphae harness/product binary identity differs")
    if (
        expected_hyphae_binary_sha256 is not None
        and executed_hyphae != expected_hyphae_binary_sha256
    ):
        failures.append("receipt Hyphae binary digest differs from expected executable")

    environment = _object(receipt.get("environment"), "receipt.environment", failures)
    _exact_keys(
        environment,
        {
            "os",
            "arch",
            "cpu_model",
            "logical_cpus",
            "memory_total_kib",
            "cpu_topology",
            "cpu_affinity",
            "cpu_quota",
            "kernel",
            "hardware_product_name",
            "benchmark_storage_source",
            "scaling_governor",
            "scaling_governors",
            "scaling_governor_cpu_count",
            "hypervisor_flag",
        },
        "receipt.environment",
        failures,
    )
    _positive_text(environment.get("os"), "receipt.environment.os", failures)
    _positive_text(environment.get("arch"), "receipt.environment.arch", failures)
    _integer(environment.get("logical_cpus"), "receipt.environment.logical_cpus", failures, minimum=1)
    if not isinstance(environment.get("hypervisor_flag"), bool):
        failures.append("receipt.environment.hypervisor_flag must be boolean")
    if authority_status == "authoritative-dedicated-hardware":
        if environment.get("os") != "linux":
            failures.append("authoritative receipt operating system must be normalized linux")
        if environment.get("arch") != "x86_64":
            failures.append("authoritative receipt architecture must be normalized x86_64")
        _positive_text(environment.get("cpu_model"), "receipt.environment.cpu_model", failures)
        _positive_text(environment.get("kernel"), "receipt.environment.kernel", failures)
        if environment.get("hardware_product_name") != "i7i.metal-24xl":
            failures.append("authoritative receipt DMI hardware product differs")
        if environment.get("logical_cpus") != 96:
            failures.append("authoritative receipt logical CPU count differs")
        if environment.get("scaling_governor") != "performance":
            failures.append("authoritative receipt scaling governor differs")
        if environment.get("scaling_governors") != ["performance"]:
            failures.append("authoritative receipt does not prove all performance governors")
        if environment.get("scaling_governor_cpu_count") != 96:
            failures.append("authoritative receipt performance-governor CPU count differs")
        if environment.get("hypervisor_flag") is not False:
            failures.append("authoritative receipt reports a hypervisor CPU flag")
        memory_kib = _integer(
            environment.get("memory_total_kib"),
            "receipt.environment.memory_total_kib",
            failures,
            minimum=765_041_050,
            maximum=845_571_686,
        )
        if memory_kib < 0:
            failures.append("authoritative receipt memory qualification differs")
        topology = _object(
            environment.get("cpu_topology"), "receipt.environment.cpu_topology", failures
        )
        _exact_keys(
            topology,
            {
                "logical_cpus",
                "physical_cores",
                "sockets",
                "threads_per_core",
                "logical_topology_entries",
                "complete_smt_siblings",
            },
            "receipt.environment.cpu_topology",
            failures,
        )
        for field in (
            "logical_cpus",
            "physical_cores",
            "sockets",
            "threads_per_core",
            "logical_topology_entries",
        ):
            _integer(
                topology.get(field),
                f"receipt.environment.cpu_topology.{field}",
                failures,
                minimum=1,
                maximum=1024,
            )
        if topology.get("logical_cpus") != 96:
            failures.append("authoritative receipt topology logical CPU count differs")
        if topology.get("physical_cores") != 48:
            failures.append("authoritative receipt physical core count differs")
        if topology.get("threads_per_core") != 2:
            failures.append("authoritative receipt SMT width differs")
        if topology.get("sockets") != 2:
            failures.append("authoritative receipt socket count differs")
        if topology.get("logical_topology_entries") != 96 or topology.get(
            "complete_smt_siblings"
        ) is not True:
            failures.append("authoritative receipt processor topology is incomplete")
        if environment.get("cpu_affinity") != "0-95":
            failures.append("authoritative receipt CPU affinity is incomplete")
        cpu_quota = _object(
            environment.get("cpu_quota"), "receipt.environment.cpu_quota", failures
        )
        if cpu_quota != {"state": "unlimited", "millicores": None}:
            failures.append("authoritative receipt CPU quota is not unlimited")
        storage_source = environment.get("benchmark_storage_source")
        if not isinstance(storage_source, str) or not storage_source.startswith("/dev/nvme"):
            failures.append("authoritative receipt benchmark storage is not NVMe")

    results = _object(receipt.get("results"), "receipt.results", failures)
    _exact_keys(results, {"keyspace"}, "receipt.results", failures)
    keyspace = _object(results.get("keyspace"), "receipt.results.keyspace", failures)
    _exact_keys(
        keyspace,
        {"workload", "hyphae", "valkey_no", "valkey_always", "valkey_everysec"},
        "receipt.results.keyspace",
        failures,
    )
    raw_workload = _object(
        keyspace.get("workload"), "receipt.results.keyspace.workload", failures
    )
    _exact_keys(
        raw_workload,
        {"keys", "gets", "strict_sets", "relaxed_sets", "seed"},
        "receipt.results.keyspace.workload",
        failures,
    )
    workload = {
        field: _integer(
            raw_workload.get(field),
            f"receipt.results.keyspace.workload.{field}",
            failures,
            minimum=1,
            maximum=10_000_000 if field != "seed" else U64_MASK,
        )
        for field in ("keys", "gets", "strict_sets", "relaxed_sets", "seed")
    }
    if authority_status == "authoritative-dedicated-hardware" and workload != PRODUCTION_WORKLOAD:
        failures.append("authoritative receipt requires the exact production workload and seed")
    expected_dataset = initial_dataset_sha256(max(workload["keys"], 0))

    hyphae = _object(keyspace.get("hyphae"), "receipt.results.keyspace.hyphae", failures)
    _exact_keys(hyphae, {"engine", "transport", "always_comparison", "no_comparison"}, "receipt.results.keyspace.hyphae", failures)
    if hyphae.get("engine") != "hyphae-native-embedded" or hyphae.get("transport") != "none (in-process library call)":
        failures.append("receipt.results.keyspace.hyphae identity differs")
    always_reads, always_sequence, always_setup = _hyphae_lane(
        hyphae.get("always_comparison"), "receipt.results.keyspace.hyphae.always_comparison", "always", workload, failures
    )
    no_reads, no_sequence, no_setup = _hyphae_lane(
        hyphae.get("no_comparison"), "receipt.results.keyspace.hyphae.no_comparison", "no", workload, failures
    )
    always_directory, always_dataset = always_setup.split("\0", 1)
    no_directory, no_dataset = no_setup.split("\0", 1)
    if always_directory == no_directory:
        failures.append("paired Hyphae lanes do not have isolated data directories")
    if always_dataset != no_dataset:
        failures.append("paired Hyphae lane dataset identities differ")
    if always_dataset != expected_dataset:
        failures.append("Hyphae initial dataset digest differs from deterministic workload")

    lanes = {lane["name"]: lane for lane in profile["configuration_authority"]["lanes"]}
    external_results: dict[str, tuple[str, str, str, str, str, str, str]] = {}
    for name in ("no", "always", "everysec"):
        external_results[name] = _external_lane(
            keyspace.get(f"valkey_{name}"),
            f"receipt.results.keyspace.valkey_{name}",
            lanes[name],
            workload,
            profile,
            failures,
            expected_valkey_binary_sha256,
            expected_valkey_binary_path,
            post_build.get("commit"),
            post_build.get("tree"),
        )
    if any(result[0] != always_reads for result in external_results.values()) or no_reads != always_reads:
        failures.append("Hyphae and Valkey read-key sequences differ")
    if no_sequence != external_results["no"][1]:
        failures.append("Hyphae Memory and Valkey no write-key sequences differ")
    if always_sequence != external_results["always"][1]:
        failures.append("Hyphae Strict and Valkey always write-key sequences differ")
    if any(result[2] != always_dataset for result in external_results.values()):
        failures.append("Hyphae and Valkey initial dataset identities differ")
    if len({result[3] for result in external_results.values()}) != 1:
        failures.append("Valkey lane setup identities differ")
    if len({lanes[name]["dir"] for name in external_results}) != len(external_results):
        failures.append("Valkey lanes do not have isolated data directories")
    if len({result[4] for result in external_results.values()}) != 1:
        failures.append("Valkey lanes did not execute the same bound server binary")
    if len({result[5] for result in external_results.values()}) != len(external_results):
        failures.append("Valkey lanes do not have distinct process_id identities")
    if len({result[6] for result in external_results.values()}) != len(external_results):
        failures.append("Valkey lanes do not have distinct run_id identities")
    return failures


def verify_committed_paths(
    source_root: Path,
    commit: str,
    tree: str,
    paths: list[str],
) -> list[str]:
    failures: list[str] = []
    try:
        head = subprocess.run(
            ["git", "-C", str(source_root), "rev-parse", "HEAD^{commit}"],
            check=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=10,
        ).stdout.decode("ascii").strip()
        committed_tree = subprocess.run(
            ["git", "-C", str(source_root), "rev-parse", f"{commit}^{{tree}}"],
            check=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=10,
        ).stdout.decode("ascii").strip()
        status = subprocess.run(
            [
                "git",
                "-C",
                str(source_root),
                "status",
                "--porcelain=v1",
                "--untracked-files=all",
            ],
            check=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=10,
        ).stdout
    except (OSError, UnicodeError, subprocess.SubprocessError) as error:
        return [f"cannot verify receipt source Git identity: {error}"]
    if head != commit:
        failures.append("retained source checkout HEAD differs from receipt commit")
    if committed_tree != tree:
        failures.append("receipt source tree differs from its committed Git tree")
    if status:
        failures.append("retained source checkout is not clean")
    for relative in paths:
        path = source_root / relative
        if path.is_symlink() or not path.is_file():
            failures.append(f"retained source path is not a regular file: {relative}")
            continue
        try:
            committed = subprocess.run(
                ["git", "-C", str(source_root), "show", f"{commit}:{relative}"],
                check=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                timeout=10,
            ).stdout
            current = path.read_bytes()
        except (OSError, subprocess.SubprocessError) as error:
            failures.append(f"cannot read committed source path {relative}: {error}")
            continue
        if committed != current:
            failures.append(f"retained source bytes differ from receipt commit: {relative}")
    return failures


def check_receipt(
    path: Path,
    profile_path: Path | None = None,
    *,
    expected_source_commit: str | None = None,
    expected_source_tree: str | None = None,
    harness: Path | None = None,
    valkey_binary_artifact: Path,
    source_root: Path | None = None,
    require_authoritative: bool = False,
) -> dict[str, Any]:
    receipt = load_receipt(path)
    evidence_authority = receipt.get("evidence_authority")
    authority_status = (
        evidence_authority.get("status") if isinstance(evidence_authority, dict) else None
    )
    execution = receipt.get("hyphae_execution")
    source = execution.get("source") if isinstance(execution, dict) else None
    post_build = source.get("post_build") if isinstance(source, dict) else None
    receipt_commit = post_build.get("commit") if isinstance(post_build, dict) else None
    receipt_tree = post_build.get("tree") if isinstance(post_build, dict) else None
    if source_root is not None:
        source_root = source_root.resolve()
        expected_profile_path = source_root / "benchmarks/baseline-harness/valkey/application-core-v1.json"
        if profile_path is not None and profile_path.resolve() != expected_profile_path.resolve():
            raise ReceiptError("profile path is not the authority inside the retained source checkout")
        profile_path = expected_profile_path
    elif require_authoritative or authority_status == "authoritative-dedicated-hardware":
        raise ReceiptError("authoritative validation requires --source-root")
    if profile_path is None:
        profile_path = check_profile.DEFAULT_PROFILE
    profile_path = profile_path.resolve()
    executed_commit = receipt_commit if source_root is not None and isinstance(receipt_commit, str) else None
    profile_report = check_profile.check_profile(
        profile_path,
        source_root if source_root is not None else check_profile.ROOT,
        executed_commit,
    )
    profile = json.loads(
        profile_path.read_text(encoding="utf-8"),
        object_pairs_hook=_reject_duplicate_keys,
        parse_constant=_reject_nonfinite,
    )
    harness_digest = None
    if harness is not None:
        harness_digest = hashlib.sha256(harness.read_bytes()).hexdigest()
    valkey_binary_digest = hashlib.sha256(valkey_binary_artifact.read_bytes()).hexdigest()
    failures = validate_receipt(
        receipt,
        profile,
        expected_source_commit=expected_source_commit,
        expected_source_tree=expected_source_tree,
        expected_hyphae_binary_sha256=harness_digest,
        expected_valkey_binary_sha256=valkey_binary_digest,
        expected_valkey_binary_path=valkey_binary_artifact,
        require_authoritative=require_authoritative,
    )
    if source_root is not None:
        if not isinstance(receipt_commit, str) or not isinstance(receipt_tree, str):
            failures.append("receipt source commit/tree is missing")
        else:
            binding = profile["executed_source_binding"]
            source_paths = [
                *binding["self_checked_paths"],
                *binding["authority_paths"],
                *binding["producer_validator_paths"],
                *binding["test_paths"],
            ]
            failures.extend(
                verify_committed_paths(source_root, receipt_commit, receipt_tree, source_paths)
            )
    if harness is not None:
        execution = receipt.get("hyphae_execution")
        build = execution.get("build") if isinstance(execution, dict) else None
        receipt_executable = build.get("executable") if isinstance(build, dict) else None
        if not isinstance(receipt_executable, str) or Path(receipt_executable).resolve() != harness.resolve():
            failures.append("receipt Hyphae executable path differs from expected harness")
    if failures:
        raise ReceiptError("\n".join(failures))
    return {
        "status": "ok",
        "schema": receipt["schema"],
        "profile_schema": profile_report["schema"],
        "source_commit": receipt_commit,
        "evidence_authority": receipt["evidence_authority"]["status"],
        "lanes": ["no", "always", "everysec"],
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("receipt", type=Path)
    parser.add_argument("--profile", type=Path)
    parser.add_argument("--expected-source-commit", required=True)
    parser.add_argument("--expected-source-tree", required=True)
    parser.add_argument("--harness", required=True, type=Path)
    parser.add_argument("--valkey-binary-artifact", required=True, type=Path)
    parser.add_argument("--source-root", type=Path)
    parser.add_argument("--require-authoritative", action="store_true")
    parser.add_argument("--compact", action="store_true")
    arguments = parser.parse_args()
    try:
        report = check_receipt(
            arguments.receipt.resolve(),
            arguments.profile.resolve() if arguments.profile is not None else None,
            expected_source_commit=arguments.expected_source_commit,
            expected_source_tree=arguments.expected_source_tree,
            harness=arguments.harness.resolve(),
            valkey_binary_artifact=arguments.valkey_binary_artifact.resolve(),
            source_root=arguments.source_root.resolve() if arguments.source_root is not None else None,
            require_authoritative=arguments.require_authoritative,
        )
    except (OSError, ReceiptError, check_profile.ProfileError) as error:
        print(f"Valkey baseline receipt invalid:\n{error}", file=sys.stderr)
        return 1
    print(json.dumps(report, indent=None if arguments.compact else 2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
