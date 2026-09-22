#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Strict checker for model manifests and multilingual embedding receipts."""

from __future__ import annotations

import argparse
import sys
from pathlib import Path
from typing import Any

from contract import (
    ARTIFACT_CAPTURE_SCOPE,
    ARTIFACT_CAPTURE_PHASES,
    ARTIFACT_INVENTORY_ALGORITHM,
    CONTAINMENT_PROFILE,
    ContractError,
    DIMENSIONS,
    LANES,
    PROVISIONAL_SCHEMA,
    RECEIPT_SCHEMA,
    artifact_inventory_digest,
    distribution_inventory_digest,
    documents_per_second,
    environment_artifacts_digest,
    language_counts,
    load_corpus,
    load_json,
    load_plan,
    projection_values,
    require_fields,
    require_integer,
    require_sha256,
    require_string,
    sha256_file,
    validate_manifest,
    verify_snapshot,
)
from staging import STAGED_HARNESS_FILES, validate_stage_document

HARNESS_SOURCE_FILES = STAGED_HARNESS_FILES
ARGV_CONTRACT = [
    "<staged-python-open-descriptor-with-original-argv0>",
    "-I",
    "-B",
    "-X",
    "pycache_prefix=<isolated-empty-non-authoritative-directory>",
    "<staged-reference-subject>",
    "--stage-root",
    "<absolute-read-only-stage-root>",
    "--model-dir",
    "<absolute-model-dir>",
    "--manifest",
    "<absolute-manifest>",
    "--plan",
    "<absolute-plan>",
    "--corpus",
    "<absolute-corpus>",
    "--nvidia-smi",
    "<absolute-nvidia-smi>",
    "--acquisition-record",
    "<absolute-retained-acquisition-record>",
    "--legal-evidence",
    "<absolute-retained-legal-evidence>",
    "--gpu-index",
    "<decimal-device-index>",
    "--output",
    "<private-temporary-result>",
]
OFFLINE_ENVIRONMENT = {
    "HF_DATASETS_OFFLINE": "1",
    "HF_HUB_DISABLE_TELEMETRY": "1",
    "HF_HUB_OFFLINE": "1",
    "PYTHONHASHSEED": "0",
    "PYTHONNOUSERSITE": "1",
    "PYTHONDONTWRITEBYTECODE": "1",
    "PYTHONPYCACHEPREFIX": "<isolated-empty-non-authoritative-directory>",
    "TOKENIZERS_PARALLELISM": "false",
    "TRANSFORMERS_OFFLINE": "1",
}
PERMITTED_INHERITED_ENVIRONMENT = (
    "CUDA_DEVICE_ORDER",
    "CUDA_VISIBLE_DEVICES",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "NVIDIA_DRIVER_CAPABILITIES",
    "NVIDIA_VISIBLE_DEVICES",
)
FORBIDDEN_ENVIRONMENT_HOOKS = {
    "BASH_ENV",
    "ENV",
    "GCONV_PATH",
    "GLIBC_TUNABLES",
    "LD_AUDIT",
    "LD_BIND_NOW",
    "LD_DEBUG",
    "LD_DEBUG_OUTPUT",
    "LD_DYNAMIC_WEAK",
    "LD_HWCAP_MASK",
    "LD_LIBRARY_PATH",
    "LD_ORIGIN_PATH",
    "LD_PRELOAD",
    "LD_PROFILE",
    "LD_SHOW_AUXV",
    "LD_TRACE_LOADED_OBJECTS",
    "MALLOC_CHECK_",
    "MALLOC_PERTURB_",
    "PYTHONBREAKPOINT",
    "PYTHONDONTWRITEBYTECODE",
    "PYTHONHASHSEED",
    "PYTHONHOME",
    "PYTHONINTMAXSTRDIGITS",
    "PYTHONINSPECT",
    "PYTHONIOENCODING",
    "PYTHONMALLOC",
    "PYTHONNOUSERSITE",
    "PYTHONPATH",
    "PYTHONPYCACHEPREFIX",
    "PYTHONPROFILEIMPORTTIME",
    "PYTHONSAFEPATH",
    "PYTHONSTARTUP",
    "PYTHONWARNINGS",
}
REQUIRED_DISTRIBUTIONS = {"safetensors", "tokenizers", "torch", "transformers"}
PRECISION_CONTROLS = {
    "attention_implementation": "sdpa",
    "cudnn_benchmark": False,
    "cudnn_deterministic": True,
    "float32_matmul_precision": "highest",
    "matmul_allow_tf32": False,
    "cudnn_allow_tf32": False,
    "random_seed": 20260917,
}
NON_AUTHORITATIVE_TRUST = {
    "authenticity": "operator-asserted-no-independent-anchor",
    "containment": "systemd-user-service-cgroup-v2-required",
    "claim_unlock": "forbidden",
    "operator_assertions": {
        "acquisition": "retained-response-bytes-no-authenticity-anchor",
        "legal_review": "checksummed-review-no-approved-signature-anchor",
        "gpu_identity": "nvidia-smi-observation-no-hardware-attestation-anchor",
    },
}


def _validate_source(
    source: Any,
    harness_dir: Path,
    manifest_path: Path,
    plan_path: Path,
    corpus_path: Path,
    acquisition_record_path: Path,
    legal_evidence_path: Path,
    expected_stage: dict[str, Any] | None,
) -> None:
    source = require_fields(
        source,
        {
            "harness_files_sha256",
            "manifest_sha256",
            "plan_sha256",
            "corpus_sha256",
            "acquisition_record_sha256",
            "legal_evidence_sha256",
            "execution_stage",
        },
        "receipt source",
    )
    expected_harness = {name: sha256_file(harness_dir / name) for name in HARNESS_SOURCE_FILES}
    if source["harness_files_sha256"] != expected_harness:
        raise ContractError("receipt harness source digests differ")
    if source["manifest_sha256"] != sha256_file(manifest_path):
        raise ContractError("receipt manifest digest differs")
    if source["plan_sha256"] != sha256_file(plan_path):
        raise ContractError("receipt plan digest differs")
    if source["corpus_sha256"] != sha256_file(corpus_path):
        raise ContractError("receipt corpus digest differs")
    if source["acquisition_record_sha256"] != sha256_file(acquisition_record_path):
        raise ContractError("receipt acquisition evidence digest differs")
    if source["legal_evidence_sha256"] != sha256_file(legal_evidence_path):
        raise ContractError("receipt legal evidence digest differs")
    stage = source["execution_stage"]
    if not isinstance(stage, dict):
        raise ContractError("receipt execution stage identity is missing")
    validate_stage_document(stage)
    if expected_stage is not None and stage != expected_stage:
        raise ContractError("receipt execution stage differs from the launched stage")
    staged_files = {item["identity"]: item for item in stage["files"]}
    for name, digest in expected_harness.items():
        staged = staged_files.get(f"harness/{name}")
        if staged is None or staged["sha256"] != digest:
            raise ContractError(f"receipt executed harness bytes differ: {name}")


def _validate_staged_inputs(source: dict[str, Any], manifest: dict[str, Any]) -> None:
    staged_files = {
        item["identity"]: item for item in source["execution_stage"]["files"]
    }
    expected = {
        "inputs/manifest.json": source["manifest_sha256"],
        "inputs/plan.json": source["plan_sha256"],
        "inputs/corpus.json": source["corpus_sha256"],
        "evidence/acquisition/acquisition.json": source["acquisition_record_sha256"],
        "evidence/legal-evidence.json": source["legal_evidence_sha256"],
    }
    for item in manifest["files"]:
        expected[f"model/{item['path']}"] = item["sha256"]
    acquisition = manifest["acquisition_provenance"]
    responses = [acquisition["model_metadata_response"], acquisition["model_card_response"]]
    responses.extend(acquisition["tree_metadata_responses"])
    for response in responses:
        expected[f"evidence/acquisition/{response['body_path']}"] = response["body_sha256"]
    capture = acquisition["capture"]
    expected[f"evidence/acquisition/{capture['tool_path']}"] = capture["tool_sha256"]
    for identity, digest in expected.items():
        staged = staged_files.get(identity)
        if staged is None or staged["sha256"] != digest:
            raise ContractError(f"receipt staged input bytes differ: {identity}")


def _validate_model(value: Any, manifest: dict[str, Any]) -> None:
    value = require_fields(
        value,
        {
            "repository",
            "revision",
            "manifest_status",
            "license_spdx",
            "license_declaration_source",
            "bundled_license_text",
            "legal_evidence_sha256",
            "product_claims_allowed",
            "config_sha256",
            "weight_files",
            "file_count",
            "acquisition_provenance",
            "snapshot_verification",
        },
        "receipt model",
    )
    expected = {
        "repository": manifest["model"]["repository"],
        "revision": manifest["model"]["revision"],
        "manifest_status": "verified",
        "license_spdx": manifest["license"]["declared_spdx"],
        "license_declaration_source": manifest["license"]["declaration_source"],
        "bundled_license_text": manifest["license"]["bundled_license_text"],
        "legal_evidence_sha256": manifest["license"]["legal_evidence_sha256"],
        "product_claims_allowed": manifest["license"]["product_claims_allowed"],
        "config_sha256": next(
            item["sha256"] for item in manifest["files"] if item["path"] == "config.json"
        ),
        "weight_files": [
            {
                "path": item["path"],
                "size_bytes": item["size_bytes"],
                "sha256": item["sha256"],
            }
            for item in manifest["files"]
            if item["role"] == "weights"
        ],
        "file_count": len(manifest["files"]),
        "acquisition_provenance": manifest["acquisition_provenance"],
        "snapshot_verification": "sha256-every-file-and-retained-commit-specific-responses-before-and-after",
    }
    if value != expected:
        raise ContractError("receipt model identity differs from verified manifest")


def _validate_workload(
    value: Any,
    manifest: dict[str, Any],
    plan: dict[str, Any],
    records: list[dict[str, str]],
) -> None:
    value = require_fields(
        value,
        {
            "source_records",
            "repetitions",
            "expanded_records",
            "language_counts",
            "tokenizer",
            "instruction",
            "chunking",
            "canonical_output",
        },
        "receipt workload",
    )
    repetitions = plan["dataset"]["repetitions"]
    tokenizer_files = [
        {"path": item["path"], "sha256": item["sha256"]}
        for item in manifest["files"]
        if item["role"] == "tokenizer"
    ]
    expected = {
        "source_records": len(records),
        "repetitions": repetitions,
        "expanded_records": len(records) * repetitions,
        "language_counts": language_counts(records, repetitions),
        "tokenizer": {**plan["tokenizer"], "files": tokenizer_files},
        "instruction": plan["instruction"],
        "chunking": plan["chunking"],
        "canonical_output": plan["canonical_output"],
    }
    if value != expected:
        raise ContractError("receipt workload identity differs from plan/corpus/manifest")


def _validate_nonempty_string_map(value: Any, fields: set[str], label: str) -> dict[str, str]:
    value = require_fields(value, fields, label)
    for key, item in value.items():
        require_string(item, f"{label} {key}")
    return value


def _validate_environment_artifacts(value: Any) -> None:
    value = require_fields(
        value,
        {
            "algorithm",
            "capture_scope",
            "capture_phases",
            "installed_distributions",
            "installed_distributions_sha256",
            "loaded_python_modules",
            "loaded_python_modules_sha256",
            "loaded_native_libraries",
            "loaded_native_libraries_sha256",
            "environment_sha256",
        },
        "environment artifacts",
    )
    if value["algorithm"] != ARTIFACT_INVENTORY_ALGORITHM:
        raise ContractError("environment artifact inventory algorithm differs")
    if value["capture_scope"] != ARTIFACT_CAPTURE_SCOPE:
        raise ContractError("environment artifact capture scope differs")
    phases = value["capture_phases"]
    if phases != ARTIFACT_CAPTURE_PHASES:
        raise ContractError("environment artifact capture phases differ from frozen v1")
    for phase in phases:
        require_string(phase, "environment artifact capture phase")

    distributions = value["installed_distributions"]
    distributions_sha256 = distribution_inventory_digest(distributions)
    if value["installed_distributions_sha256"] != distributions_sha256:
        raise ContractError("installed distribution aggregate digest differs")
    by_name = {distribution["name"]: distribution for distribution in distributions}
    if not REQUIRED_DISTRIBUTIONS.issubset(by_name):
        missing = sorted(REQUIRED_DISTRIBUTIONS - set(by_name))
        raise ContractError(f"environment lacks required distribution artifacts: {missing}")

    for field, label in (
        ("loaded_python_modules", "loaded Python modules"),
        ("loaded_native_libraries", "loaded native libraries"),
    ):
        records = value[field]
        observed_digest = artifact_inventory_digest(records, label)
        if value[f"{field}_sha256"] != observed_digest:
            raise ContractError(f"{label} aggregate digest differs")
    expected_environment_sha256 = environment_artifacts_digest(
        phases,
        value["installed_distributions_sha256"],
        value["loaded_python_modules_sha256"],
        value["loaded_native_libraries_sha256"],
    )
    if value["environment_sha256"] != expected_environment_sha256:
        raise ContractError("external Python environment aggregate digest differs")


def _validate_execution_profile(
    value: Any,
    *,
    expected_inherited_environment_names: list[str] | None,
    execution_stage: dict[str, Any],
) -> None:
    value = require_fields(
        value,
        {
            "invocation",
            "software",
            "gpu",
            "precision_controls",
            "tokenizer_runtime",
            "gpu_observations",
            "environment_artifacts",
            "containment",
        },
        "execution profile",
    )
    if value["containment"] != CONTAINMENT_PROFILE:
        raise ContractError("measurement containment profile differs")
    invocation = require_fields(
        value["invocation"],
        {
            "shell",
            "automatic_network",
            "argv_contract",
            "offline_environment_names",
            "python_execution",
            "nvidia_smi_execution",
            "pycache_prefix",
            "acquisition_record",
            "legal_evidence",
            "gpu_index",
            "inherited_environment_names",
        },
        "execution invocation",
    )
    if (
        invocation["shell"] is not False
        or invocation["automatic_network"] is not False
        or invocation["argv_contract"] != ARGV_CONTRACT
        or invocation["offline_environment_names"] != sorted(OFFLINE_ENVIRONMENT)
    ):
        raise ContractError("external subject invocation is not fixed, offline, and shell-free")
    inherited = invocation["inherited_environment_names"]
    if (
        not isinstance(inherited, list)
        or inherited != sorted(set(inherited))
        or not set(inherited).issubset(PERMITTED_INHERITED_ENVIRONMENT)
    ):
        raise ContractError("external subject inherited environment crosses the launcher allowlist")
    if (
        expected_inherited_environment_names is not None
        and inherited != expected_inherited_environment_names
    ):
        raise ContractError("receipt inherited environment differs from the launcher environment")
    python_execution = require_fields(
        invocation["python_execution"], {"method", "staged_argv0"},
        "Python execution identity",
    )
    if python_execution["method"] != "staged-file-open-descriptor-execve":
        raise ContractError("Python was not executed from the staged open descriptor")
    staged_argv0 = require_string(
        python_execution["staged_argv0"], "Python staged argv0"
    )
    if staged_argv0 != "staged:executables/python":
        raise ContractError("Python staged argv0 differs")
    if invocation["nvidia_smi_execution"] != "staged-read-only-copy":
        raise ContractError("nvidia-smi was not executed from the staged copy")
    if invocation["pycache_prefix"] != "staged:volatile/pycache":
        raise ContractError("Python bytecode cache prefix differs")
    if (
        invocation["acquisition_record"]
        != "staged:evidence/acquisition/acquisition.json"
        or invocation["legal_evidence"] != "staged:evidence/legal-evidence.json"
    ):
        raise ContractError("staged evidence invocation identity differs")
    gpu_index = require_integer(invocation["gpu_index"], "GPU index", minimum=0)
    if gpu_index > 15:
        raise ContractError("GPU index exceeds harness bound")

    software = _validate_nonempty_string_map(
        value["software"],
        {
            "python_version",
            "python_implementation",
            "python_executable_sha256",
            "platform",
            "torch_version",
            "transformers_version",
            "tokenizers_version",
            "safetensors_version",
            "cuda_runtime_version",
            "cudnn_version",
            "nvidia_smi_sha256",
        },
        "software profile",
    )
    require_sha256(software["python_executable_sha256"], "Python executable digest")
    require_sha256(software["nvidia_smi_sha256"], "nvidia-smi digest")
    staged_files = {item["identity"]: item for item in execution_stage["files"]}
    if staged_files.get("executables/python", {}).get("sha256") != software[
        "python_executable_sha256"
    ]:
        raise ContractError("receipt Python digest differs from executed staged bytes")
    if staged_files.get("executables/nvidia-smi", {}).get("sha256") != software[
        "nvidia_smi_sha256"
    ]:
        raise ContractError("receipt nvidia-smi digest differs from executed staged bytes")
    _validate_environment_artifacts(value["environment_artifacts"])
    distribution_versions = {
        item["name"]: item["version"]
        for item in value["environment_artifacts"]["installed_distributions"]
    }
    for name in REQUIRED_DISTRIBUTIONS:
        if software[f"{name}_version"] != distribution_versions[name]:
            raise ContractError(f"software version differs from {name} artifact inventory")
    if value["precision_controls"] != PRECISION_CONTROLS:
        raise ContractError("precision controls differ from the measured contract")
    tokenizer = require_fields(
        value["tokenizer_runtime"],
        {"class", "is_fast", "vocab_size", "model_max_length", "truncation_side"},
        "tokenizer runtime",
    )
    require_string(tokenizer["class"], "tokenizer runtime class")
    if not isinstance(tokenizer["is_fast"], bool):
        raise ContractError("tokenizer is_fast flag is invalid")
    require_integer(tokenizer["vocab_size"], "tokenizer vocabulary size", minimum=1)
    require_integer(tokenizer["model_max_length"], "tokenizer model maximum", minimum=1)
    if tokenizer["truncation_side"] != "right":
        raise ContractError("tokenizer runtime truncation side is not frozen to right")

    gpu = require_fields(value["gpu"], {"nvidia_smi", "torch"}, "GPU identity")
    nvidia = require_fields(
        gpu["nvidia_smi"],
        {
            "index",
            "name",
            "uuid",
            "pci_bus_id",
            "driver_version",
            "memory_total_mib",
            "compute_capability",
            "power_limit_watts",
            "clocks_max_sm_mhz",
            "clocks_max_memory_mhz",
        },
        "nvidia-smi GPU identity",
    )
    require_integer(nvidia["index"], "nvidia-smi physical GPU index")
    require_integer(nvidia["memory_total_mib"], "GPU memory MiB", minimum=1)
    for name in (
        "name",
        "uuid",
        "pci_bus_id",
        "driver_version",
        "compute_capability",
        "power_limit_watts",
        "clocks_max_sm_mhz",
        "clocks_max_memory_mhz",
    ):
        require_string(nvidia[name], f"nvidia-smi {name}")
    torch_gpu = require_fields(
        gpu["torch"],
        {
            "logical_index",
            "name",
            "total_memory_bytes",
            "compute_capability",
            "multiprocessor_count",
            "bound_nvidia_uuid",
            "bound_pci_bus_id",
        },
        "PyTorch GPU identity",
    )
    if require_integer(torch_gpu["logical_index"], "PyTorch logical GPU index") != gpu_index:
        raise ContractError("PyTorch logical GPU index differs from invocation")
    require_string(torch_gpu["name"], "PyTorch GPU name")
    require_string(torch_gpu["compute_capability"], "PyTorch compute capability")
    require_integer(torch_gpu["total_memory_bytes"], "PyTorch GPU memory", minimum=1)
    require_integer(torch_gpu["multiprocessor_count"], "GPU multiprocessor count", minimum=1)
    if (
        torch_gpu["bound_nvidia_uuid"] != nvidia["uuid"]
        or torch_gpu["bound_pci_bus_id"] != nvidia["pci_bus_id"]
    ):
        raise ContractError("PyTorch logical device is not bound to the retained NVML identity")

    observations = require_fields(value["gpu_observations"], {"before", "after"}, "GPU observations")
    observation_fields = {
        "pstate",
        "temperature_celsius",
        "power_draw_watts",
        "clocks_sm_mhz",
        "clocks_memory_mhz",
    }
    for moment in ("before", "after"):
        _validate_nonempty_string_map(observations[moment], observation_fields, f"GPU {moment}")


def _validate_lane_profiles(
    profiles: Any, plan: dict[str, Any], expanded_count: int
) -> dict[str, dict[str, Any]]:
    if not isinstance(profiles, list) or len(profiles) != len(LANES):
        raise ContractError("receipt lane profiles must cover FP32, BF16, and FP16")
    by_name: dict[str, dict[str, Any]] = {}
    for index, (profile, planned) in enumerate(zip(profiles, plan["lanes"], strict=True)):
        profile = require_fields(
            profile,
            {
                "lane",
                "inference_dtype",
                "model_class",
                "parameter_dtype",
                "model_load_elapsed_nanos",
                "tokenization_elapsed_nanos",
                "documents",
                "batches",
                "non_padding_tokens",
                "padded_tokens",
                "max_sequence_tokens",
                "batch_token_budget",
                "max_batch_size",
            },
            f"lane profile {index}",
        )
        if (
            profile["lane"] != planned["name"]
            or profile["inference_dtype"] != planned["inference_dtype"]
            or profile["batch_token_budget"] != planned["batch_token_budget"]
            or profile["max_batch_size"] != planned["max_batch_size"]
            or profile["documents"] != expanded_count
        ):
            raise ContractError(f"lane profile {index} differs from plan")
        require_string(profile["model_class"], f"lane profile {index} model class")
        if profile["parameter_dtype"] != f"torch.{planned['inference_dtype']}":
            raise ContractError(f"lane profile {index} parameter dtype differs")
        require_integer(profile["model_load_elapsed_nanos"], "model load timing", minimum=1)
        require_integer(profile["tokenization_elapsed_nanos"], "tokenization timing", minimum=1)
        require_integer(profile["batches"], "batch count", minimum=1)
        non_padding = require_integer(profile["non_padding_tokens"], "non-padding tokens", minimum=1)
        padded = require_integer(profile["padded_tokens"], "padded tokens", minimum=1)
        maximum = require_integer(profile["max_sequence_tokens"], "maximum sequence", minimum=1)
        if padded < non_padding or maximum > plan["tokenizer"]["max_length"]:
            raise ContractError(f"lane profile {index} token accounting is invalid")
        by_name[profile["lane"]] = profile
    return by_name


def _validate_cell(
    cell: Any,
    lane: dict[str, Any],
    dimension: int,
    plan: dict[str, Any],
) -> None:
    cell = require_fields(
        cell,
        {
            "lane",
            "inference_dtype",
            "dimension",
            "canonical_output_dtype",
            "measurement_scope",
            "warmup_samples",
            "raw_timing_samples",
            "throughput",
            "projection",
        },
        f"receipt cell {lane['lane']}/{dimension}",
    )
    if (
        cell["lane"] != lane["lane"]
        or cell["inference_dtype"] != lane["inference_dtype"]
        or cell["dimension"] != dimension
        or cell["canonical_output_dtype"] != "float32-le"
        or cell["measurement_scope"] != plan["measurement"]["scope"]
        or cell["warmup_samples"] != plan["measurement"]["warmup_samples"]
    ):
        raise ContractError(f"receipt cell {lane['lane']}/{dimension} identity differs")
    samples = cell["raw_timing_samples"]
    expected_samples = plan["measurement"]["measured_samples"]
    if not isinstance(samples, list) or len(samples) != expected_samples:
        raise ContractError(f"receipt cell {lane['lane']}/{dimension} raw sample count differs")
    output_digests: set[str] = set()
    total_host = 0
    for index, sample in enumerate(samples):
        sample = require_fields(
            sample,
            {
                "sample",
                "host_elapsed_nanos",
                "gpu_elapsed_nanos",
                "documents",
                "batches",
                "non_padding_tokens",
                "padded_tokens",
                "peak_allocated_bytes",
                "output_sha256",
            },
            f"raw timing sample {lane['lane']}/{dimension}/{index}",
        )
        if (
            sample["sample"] != index
            or sample["documents"] != lane["documents"]
            or sample["batches"] != lane["batches"]
            or sample["non_padding_tokens"] != lane["non_padding_tokens"]
            or sample["padded_tokens"] != lane["padded_tokens"]
        ):
            raise ContractError(f"raw timing sample {lane['lane']}/{dimension}/{index} identity differs")
        host = require_integer(sample["host_elapsed_nanos"], "host elapsed time", minimum=1)
        gpu = require_integer(sample["gpu_elapsed_nanos"], "GPU elapsed time", minimum=1)
        require_integer(sample["peak_allocated_bytes"], "peak allocated bytes", minimum=1)
        if gpu > host:
            raise ContractError("GPU event time exceeds enclosing host time")
        output_digests.add(require_sha256(sample["output_sha256"], "canonical output digest"))
        total_host += host
    if len(output_digests) != 1:
        raise ContractError(f"canonical output changed across {lane['lane']}/{dimension} samples")

    total_documents = lane["documents"] * expected_samples
    throughput = require_fields(
        cell["throughput"],
        {"basis", "total_documents", "total_host_elapsed_nanos", "documents_per_second"},
        "measured throughput",
    )
    expected_throughput = {
        "basis": "sum-of-raw-host-timing-samples",
        "total_documents": total_documents,
        "total_host_elapsed_nanos": total_host,
        "documents_per_second": documents_per_second(total_documents, total_host),
    }
    if throughput != expected_throughput:
        raise ContractError(f"throughput math differs for {lane['lane']}/{dimension}")

    projection = require_fields(
        cell["projection"],
        {"status", "basis", "assumptions", "values"},
        "non-claim projection",
    )
    if projection != {
        "status": "non-claim-derived-projection",
        "basis": expected_throughput,
        "assumptions": [
            "ideal linear scaling from this measured cell",
            "no allowance for ingestion, storage, indexing, queueing, or contention",
            "not a capacity, latency, cost, or completion-time claim",
        ],
        "values": projection_values(total_documents, total_host),
    }:
        raise ContractError(f"projection math or non-claim label differs for {lane['lane']}/{dimension}")


def _validate_measurement_payload(
    payload: dict[str, Any],
    manifest: dict[str, Any],
    plan: dict[str, Any],
    records: list[dict[str, str]],
    *,
    harness_dir: Path,
    manifest_path: Path,
    plan_path: Path,
    corpus_path: Path,
    acquisition_record_path: Path,
    legal_evidence_path: Path,
    expected_inherited_environment_names: list[str] | None = None,
    expected_stage: dict[str, Any] | None = None,
) -> dict[str, Any]:
    _validate_source(
        payload["source"],
        harness_dir,
        manifest_path,
        plan_path,
        corpus_path,
        acquisition_record_path,
        legal_evidence_path,
        expected_stage,
    )
    _validate_staged_inputs(payload["source"], manifest)
    _validate_model(payload["model"], manifest)
    _validate_workload(payload["workload"], manifest, plan, records)
    _validate_execution_profile(
        payload["execution_profile"],
        expected_inherited_environment_names=expected_inherited_environment_names,
        execution_stage=payload["source"]["execution_stage"],
    )
    expanded_count = len(records) * plan["dataset"]["repetitions"]
    lanes = _validate_lane_profiles(payload["lane_profiles"], plan, expanded_count)
    cells = payload["cells"]
    if not isinstance(cells, list) or len(cells) != len(LANES) * len(DIMENSIONS):
        raise ContractError("receipt does not contain all nine lane/dimension cells")
    offset = 0
    for lane_name, _ in LANES:
        for dimension in DIMENSIONS:
            _validate_cell(cells[offset], lanes[lane_name], dimension, plan)
            offset += 1
    return {
        "status": "passed",
        "cells": len(cells),
        "raw_samples": len(cells) * plan["measurement"]["measured_samples"],
        "claims": [],
    }


def validate_provisional(
    provisional: dict[str, Any],
    manifest: dict[str, Any],
    plan: dict[str, Any],
    records: list[dict[str, str]],
    **arguments: Any,
) -> dict[str, Any]:
    require_fields(
        provisional,
        {
            "schema",
            "parent_challenge",
            "source",
            "model",
            "workload",
            "execution_profile",
            "lane_profiles",
            "cells",
        },
        "provisional embedding result",
    )
    if provisional["schema"] != PROVISIONAL_SCHEMA:
        raise ContractError("provisional embedding schema differs")
    require_sha256(provisional["parent_challenge"], "parent challenge")
    return _validate_measurement_payload(
        provisional, manifest, plan, records, **arguments
    )


def finalize_receipt(
    provisional: dict[str, Any],
    parent_challenge: str,
    manifest: dict[str, Any],
    plan: dict[str, Any],
    records: list[dict[str, str]],
    **arguments: Any,
) -> dict[str, Any]:
    validate_provisional(provisional, manifest, plan, records, **arguments)
    if provisional["parent_challenge"] != parent_challenge:
        raise ContractError("provisional result does not match the one-run parent challenge")
    return {
        "schema": RECEIPT_SCHEMA,
        "status": "operator-attested-non-authoritative-measurement",
        "source": provisional["source"],
        "model": provisional["model"],
        "workload": provisional["workload"],
        "execution_profile": provisional["execution_profile"],
        "lane_profiles": provisional["lane_profiles"],
        "cells": provisional["cells"],
        "trust": NON_AUTHORITATIVE_TRUST,
        "claims": [],
        "closure_declared": False,
    }


def validate_receipt(
    receipt: dict[str, Any],
    manifest: dict[str, Any],
    plan: dict[str, Any],
    records: list[dict[str, str]],
    **arguments: Any,
) -> dict[str, Any]:
    require_fields(
        receipt,
        {
            "schema",
            "status",
            "source",
            "model",
            "workload",
            "execution_profile",
            "lane_profiles",
            "cells",
            "trust",
            "claims",
            "closure_declared",
        },
        "embedding receipt",
    )
    if (
        receipt["schema"] != RECEIPT_SCHEMA
        or receipt["status"] != "operator-attested-non-authoritative-measurement"
        or receipt["trust"] != NON_AUTHORITATIVE_TRUST
        or receipt["claims"] != []
        or receipt["closure_declared"] is not False
    ):
        raise ContractError("embedding receipt identity, trust, or non-claim state differs")
    return _validate_measurement_payload(receipt, manifest, plan, records, **arguments)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--receipt", type=Path, required=True)
    parser.add_argument("--manifest", type=Path, required=True)
    parser.add_argument("--model-dir", type=Path, required=True)
    parser.add_argument("--plan", type=Path, required=True)
    parser.add_argument("--corpus", type=Path, required=True)
    parser.add_argument("--acquisition-record", type=Path, required=True)
    parser.add_argument("--legal-evidence", type=Path, required=True)
    return parser.parse_args()


def main() -> int:
    arguments = parse_args()
    try:
        for path, label in (
            (arguments.receipt, "receipt"),
            (arguments.manifest, "manifest"),
            (arguments.model_dir, "model directory"),
            (arguments.plan, "plan"),
            (arguments.corpus, "corpus"),
            (arguments.acquisition_record, "acquisition record"),
            (arguments.legal_evidence, "legal evidence"),
        ):
            if not path.is_absolute():
                raise ContractError(f"{label} must be absolute")
        manifest_path = arguments.manifest.resolve()
        model_dir = arguments.model_dir.resolve()
        plan_path = arguments.plan.resolve()
        corpus_path = arguments.corpus.resolve()
        manifest = load_json(manifest_path)
        validate_manifest(manifest, require_verified=True)
        acquisition_record_path = arguments.acquisition_record.resolve()
        legal_evidence_path = arguments.legal_evidence.resolve()
        verify_snapshot(
            manifest, model_dir, acquisition_record_path, legal_evidence_path
        )
        plan = load_plan(plan_path)
        _, records = load_corpus(corpus_path, plan)
        receipt = load_json(arguments.receipt.resolve(), maximum_bytes=32 * 1024 * 1024)
        audit = validate_receipt(
            receipt,
            manifest,
            plan,
            records,
            harness_dir=Path(__file__).resolve().parent,
            manifest_path=manifest_path,
            plan_path=plan_path,
            corpus_path=corpus_path,
            acquisition_record_path=acquisition_record_path,
            legal_evidence_path=legal_evidence_path,
        )
    except ContractError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    print(
        f"non-authoritative embedding receipt checks passed: {audit['cells']} cells, "
        f"{audit['raw_samples']} raw samples, no claims"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
