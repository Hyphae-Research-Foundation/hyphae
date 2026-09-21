#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Offline PyTorch/Transformers measurement subject for one NVIDIA GPU."""

from __future__ import annotations

import argparse
import csv
import gc
import hashlib
import importlib.metadata
import io
import json
import math
import os
import platform
import re
import struct
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from typing import Any

HARNESS_DIR = Path(__file__).resolve().parent
if str(HARNESS_DIR) not in sys.path:
    sys.path.insert(0, str(HARNESS_DIR))

from check_receipt import (
    ARGV_CONTRACT,
    FORBIDDEN_ENVIRONMENT_HOOKS,
    HARNESS_SOURCE_FILES,
    OFFLINE_ENVIRONMENT,
    PERMITTED_INHERITED_ENVIRONMENT,
    PRECISION_CONTROLS,
)
from contract import (
    ARTIFACT_CAPTURE_SCOPE,
    ARTIFACT_INVENTORY_ALGORITHM,
    CONTAINMENT_PROFILE,
    ContractError,
    DIMENSIONS,
    PROVISIONAL_SCHEMA,
    artifact_inventory_digest,
    artifact_record,
    distribution_inventory_digest,
    documents_per_second,
    environment_artifacts_digest,
    ensure_absolute_executable,
    expanded_records,
    language_counts,
    load_corpus,
    load_json,
    load_plan,
    projection_values,
    require_string,
    sha256_file,
    validate_manifest,
    verify_snapshot,
)
from staging import paths_from_root, validate_stage

STATIC_QUERY_FIELDS = [
    "index",
    "name",
    "uuid",
    "pci.bus_id",
    "driver_version",
    "memory.total",
    "compute_cap",
    "power.limit",
    "clocks.max.sm",
    "clocks.max.memory",
]
DYNAMIC_QUERY_FIELDS = [
    "pstate",
    "temperature.gpu",
    "power.draw",
    "clocks.current.sm",
    "clocks.current.memory",
]
PROCESS_QUERY_FIELDS = ["pid", "gpu_uuid"]


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--stage-root", type=Path, required=True)
    parser.add_argument("--model-dir", type=Path, required=True)
    parser.add_argument("--manifest", type=Path, required=True)
    parser.add_argument("--plan", type=Path, required=True)
    parser.add_argument("--corpus", type=Path, required=True)
    parser.add_argument("--nvidia-smi", type=Path, required=True)
    parser.add_argument("--acquisition-record", type=Path, required=True)
    parser.add_argument("--legal-evidence", type=Path, required=True)
    parser.add_argument("--gpu-index", type=int, required=True)
    parser.add_argument("--output", type=Path, required=True)
    return parser.parse_args()


def _nvidia_rows(
    executable: Path,
    fields: list[str],
    identifier: str | int | None = None,
    *,
    query: str = "gpu",
) -> list[list[str]]:
    command = [
        str(executable),
        f"--query-{query}={','.join(fields)}",
        "--format=csv,noheader,nounits",
    ]
    if identifier is not None:
        command.append(f"--id={identifier}")
    try:
        with tempfile.TemporaryFile() as stdout, tempfile.TemporaryFile() as stderr:
            process = subprocess.Popen(
                command,
                stdin=subprocess.DEVNULL,
                stdout=stdout,
                stderr=stderr,
                shell=False,
                env={"LC_ALL": "C"},
            )
            try:
                returncode = process.wait(timeout=15)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
                raise ContractError("bounded nvidia-smi query timed out") from None
            if returncode != 0 or stdout.tell() > 64 * 1024 or stderr.tell() > 64 * 1024:
                raise ContractError("bounded nvidia-smi query failed or exceeded 64 KiB")
            stdout.seek(0)
            output = stdout.read(64 * 1024 + 1).decode("utf-8")
    except (OSError, UnicodeError, subprocess.SubprocessError) as error:
        raise ContractError(f"bounded nvidia-smi query failed: {error}") from error
    rows = list(csv.reader(io.StringIO(output), skipinitialspace=True))
    values = [[value.strip() for value in row] for row in rows if row]
    if any(len(row) != len(fields) or any(not item for item in row) for row in values):
        raise ContractError("nvidia-smi returned an incomplete GPU row")
    return values


def _nvidia_query(executable: Path, identifier: str | int, fields: list[str]) -> list[str]:
    rows = _nvidia_rows(executable, fields, identifier)
    if len(rows) != 1:
        raise ContractError("nvidia-smi did not return exactly one GPU identity row")
    return rows[0]


def _gpu_static(executable: Path, gpu_uuid: str) -> dict[str, Any]:
    values = _nvidia_query(executable, gpu_uuid, STATIC_QUERY_FIELDS)
    try:
        index = int(values[0])
        memory = int(values[5])
    except ValueError as error:
        raise ContractError("nvidia-smi integer identity field is malformed") from error
    return {
        "index": index,
        "name": values[1],
        "uuid": values[2],
        "pci_bus_id": values[3],
        "driver_version": values[4],
        "memory_total_mib": memory,
        "compute_capability": values[6],
        "power_limit_watts": values[7],
        "clocks_max_sm_mhz": values[8],
        "clocks_max_memory_mhz": values[9],
    }


def _gpu_dynamic(executable: Path, gpu_uuid: str) -> dict[str, str]:
    values = _nvidia_query(executable, gpu_uuid, DYNAMIC_QUERY_FIELDS)
    return {
        "pstate": values[0],
        "temperature_celsius": values[1],
        "power_draw_watts": values[2],
        "clocks_sm_mhz": values[3],
        "clocks_memory_mhz": values[4],
    }


def _process_gpu_uuid(executable: Path) -> str:
    for _ in range(10):
        rows = _nvidia_rows(executable, PROCESS_QUERY_FIELDS, query="compute-apps")
        matches = {row[1] for row in rows if row[0] == str(os.getpid())}
        if len(matches) == 1:
            return matches.pop()
        if len(matches) > 1:
            break
        time.sleep(0.2)
    raise ContractError("PyTorch process is not bound to exactly one NVML GPU UUID")


def _package_version(name: str) -> str:
    try:
        return importlib.metadata.version(name)
    except importlib.metadata.PackageNotFoundError as error:
        raise ContractError(f"reference environment lacks required package: {name}") from error


def _canonical_distribution_name(name: str) -> str:
    return re.sub(r"[-_.]+", "-", name).lower()


def _require_staged_runtime_file(
    path: Path,
    stage_root: Path,
    stage_files: dict[str, dict[str, Any]],
    label: str,
) -> None:
    resolved_root = stage_root.resolve()
    resolved = path.resolve(strict=True)
    try:
        relative = resolved.relative_to(resolved_root).as_posix()
    except ValueError as error:
        raise ContractError(f"{label} resolves outside the execution stage") from error
    expected = stage_files.get(relative)
    if expected is None or expected["size_bytes"] != resolved.stat().st_size:
        raise ContractError(f"{label} is absent from the execution stage inventory")
    if expected["sha256"] != sha256_file(resolved):
        raise ContractError(f"{label} bytes differ from the execution stage inventory")


def _installed_distributions(
    stage_root: Path, stage_files: dict[str, dict[str, Any]]
) -> list[dict[str, Any]]:
    inventories = []
    names: set[str] = set()
    for distribution in importlib.metadata.distributions():
        raw_name = distribution.metadata.get("Name")
        if not raw_name:
            raise ContractError("installed distribution has no canonical name")
        name = _canonical_distribution_name(raw_name)
        if name in names:
            raise ContractError(f"installed distribution identity is duplicated: {name}")
        if distribution.files is None:
            raise ContractError(f"installed distribution has no file inventory: {name}")
        files = []
        for relative in distribution.files:
            if (
                "__pycache__" in Path(str(relative)).parts
                or Path(str(relative)).suffix.lower() in {".pyc", ".pyo"}
            ):
                continue
            path = Path(distribution.locate_file(relative))
            _require_staged_runtime_file(
                path, stage_root, stage_files, f"installed distribution {name} file"
            )
            files.append(
                artifact_record(
                    path,
                    f"distribution:{name}:{str(relative).replace(os.sep, '/')}",
                )
            )
        files.sort(key=lambda item: item["identity"])
        if not files:
            raise ContractError(f"installed distribution has an empty file inventory: {name}")
        inventories.append(
            {
                "name": name,
                "version": distribution.version,
                "files": files,
                "files_sha256": artifact_inventory_digest(
                    files, f"installed distribution {name}"
                ),
            }
        )
        names.add(name)
    return sorted(inventories, key=lambda item: item["name"])


def _loaded_python_modules(
    stage_root: Path, stage_files: dict[str, dict[str, Any]]
) -> list[dict[str, Any]]:
    records = []
    for name, module in sys.modules.items():
        value = getattr(module, "__file__", None)
        if not isinstance(value, str):
            continue
        path = Path(value)
        if path.suffix.lower() in {".pyc", ".pyo"}:
            raise ContractError(f"loaded Python module used pre-existing bytecode: {name}")
        if not path.is_absolute():
            raise ContractError(f"loaded Python module path is not absolute: {value}")
        _require_staged_runtime_file(
            path, stage_root, stage_files, f"loaded Python module {name}"
        )
        records.append(artifact_record(path, f"module:{name}"))
    return sorted(records, key=lambda item: item["identity"])


def _native_identity(path: Path) -> str:
    resolved = path.resolve()
    for root, label in (
        (Path(sys.prefix).resolve(), "python-prefix"),
        (Path(sys.base_prefix).resolve(), "python-base-prefix"),
    ):
        try:
            return f"native:{label}:{resolved.relative_to(root).as_posix()}"
        except ValueError:
            pass
    suffix = "/".join(resolved.parts[-3:])
    return f"native:external:{suffix}"


def _loaded_native_libraries() -> list[dict[str, Any]]:
    maps = Path("/proc/self/maps")
    try:
        lines = maps.read_text(encoding="utf-8").splitlines()
    except (OSError, UnicodeError) as error:
        raise ContractError(f"cannot inventory loaded native libraries: {error}") from error
    paths: set[Path] = set()
    for line in lines:
        fields = line.split(maxsplit=5)
        if len(fields) != 6 or not fields[5].startswith("/"):
            continue
        value = fields[5]
        if value.endswith(" (deleted)"):
            raise ContractError(f"loaded native artifact was deleted during execution: {value}")
        path = Path(value)
        if path.is_file():
            paths.add(path)
    records = sorted(
        (artifact_record(path, _native_identity(path)) for path in paths),
        key=lambda item: item["identity"],
    )
    if not records:
        raise ContractError("loaded native library inventory is empty")
    return records


class ExecutionArtifactCapture:
    def __init__(self, stage: Any, stage_document: dict[str, Any]) -> None:
        self.stage_root = stage.root
        self.stage_files = {
            item["identity"]: item for item in stage_document["files"]
        }
        self.distributions = _installed_distributions(
            self.stage_root, self.stage_files
        )
        self.modules: dict[str, dict[str, Any]] = {}
        self.libraries: dict[str, dict[str, Any]] = {}
        self.phases: list[str] = []

    @staticmethod
    def _merge(
        destination: dict[str, dict[str, Any]],
        records: list[dict[str, Any]],
        label: str,
    ) -> None:
        for record in records:
            identity = record["identity"]
            previous = destination.get(identity)
            if previous is not None and previous != record:
                raise ContractError(f"{label} changed during execution: {identity}")
            destination[identity] = record

    def capture(self, phase: str) -> None:
        require_string(phase, "environment capture phase")
        if phase in self.phases:
            raise ContractError(f"environment capture phase is duplicated: {phase}")
        self._merge(
            self.modules,
            _loaded_python_modules(self.stage_root, self.stage_files),
            "loaded Python module",
        )
        self._merge(self.libraries, _loaded_native_libraries(), "loaded native library")
        self.phases.append(phase)

    def finish(self) -> dict[str, Any]:
        self.capture("measurement-complete")
        if _installed_distributions(self.stage_root, self.stage_files) != self.distributions:
            raise ContractError("installed distribution artifacts changed during execution")
        return _environment_artifacts(self)


def _environment_artifacts(capture: ExecutionArtifactCapture) -> dict[str, Any]:
    distributions = capture.distributions
    modules = [capture.modules[key] for key in sorted(capture.modules)]
    libraries = [capture.libraries[key] for key in sorted(capture.libraries)]
    value = {
        "algorithm": ARTIFACT_INVENTORY_ALGORITHM,
        "capture_scope": ARTIFACT_CAPTURE_SCOPE,
        "capture_phases": capture.phases,
        "installed_distributions": distributions,
        "installed_distributions_sha256": distribution_inventory_digest(distributions),
        "loaded_python_modules": modules,
        "loaded_python_modules_sha256": artifact_inventory_digest(
            modules, "loaded Python modules"
        ),
        "loaded_native_libraries": libraries,
        "loaded_native_libraries_sha256": artifact_inventory_digest(
            libraries, "loaded native libraries"
        ),
    }
    value["environment_sha256"] = environment_artifacts_digest(
        value["capture_phases"],
        value["installed_distributions_sha256"],
        value["loaded_python_modules_sha256"],
        value["loaded_native_libraries_sha256"],
    )
    return value


def _prepare_batches(tokenizer: Any, records: list[dict[str, str]], lane: dict[str, Any]) -> tuple[list[dict[str, Any]], dict[str, int]]:
    started = time.perf_counter_ns()
    encoded: list[dict[str, list[int]]] = []
    for record in records:
        item = tokenizer(
            record["text"],
            add_special_tokens=True,
            truncation=True,
            max_length=512,
            return_attention_mask=True,
        )
        input_ids = item.get("input_ids")
        attention_mask = item.get("attention_mask")
        if (
            not isinstance(input_ids, list)
            or not input_ids
            or not isinstance(attention_mask, list)
            or len(input_ids) != len(attention_mask)
        ):
            raise ContractError("tokenizer returned an invalid single-record encoding")
        encoded.append({"input_ids": input_ids, "attention_mask": attention_mask})

    groups: list[list[dict[str, list[int]]]] = []
    current: list[dict[str, list[int]]] = []
    current_max = 0
    budget = lane["batch_token_budget"]
    maximum_items = lane["max_batch_size"]
    for item in encoded:
        length = len(item["input_ids"])
        candidate_max = max(current_max, length)
        candidate_items = len(current) + 1
        if current and (candidate_items > maximum_items or candidate_items * candidate_max > budget):
            groups.append(current)
            current = []
            current_max = 0
            candidate_max = length
            candidate_items = 1
        if candidate_items * candidate_max > budget:
            raise ContractError(f"one encoded record exceeds the {lane['name']} token budget")
        current.append(item)
        current_max = candidate_max
    if current:
        groups.append(current)

    batches: list[dict[str, Any]] = []
    non_padding_tokens = 0
    padded_tokens = 0
    max_sequence_tokens = 0
    for group in groups:
        batch = tokenizer.pad(group, padding=True, return_tensors="pt")
        if set(batch) < {"input_ids", "attention_mask"}:
            raise ContractError("tokenizer padding omitted required tensors")
        shape = batch["input_ids"].shape
        if len(shape) != 2 or shape[0] != len(group):
            raise ContractError("tokenizer padded batch shape differs")
        batch_non_padding = int(batch["attention_mask"].sum().item())
        batch_padded = int(batch["attention_mask"].numel())
        if batch_padded > budget:
            raise ContractError(f"padded {lane['name']} batch exceeds token budget")
        non_padding_tokens += batch_non_padding
        padded_tokens += batch_padded
        max_sequence_tokens = max(max_sequence_tokens, int(shape[1]))
        batches.append(dict(batch))
    elapsed = time.perf_counter_ns() - started
    return batches, {
        "tokenization_elapsed_nanos": elapsed,
        "documents": len(records),
        "batches": len(batches),
        "non_padding_tokens": non_padding_tokens,
        "padded_tokens": padded_tokens,
        "max_sequence_tokens": max_sequence_tokens,
    }


def _output_digest(outputs: list[Any], record_ids: list[str], dimension: int) -> str:
    digest = hashlib.sha256(b"hyphae-canonical-fp32-embedding-output-v1\0")
    digest.update(struct.pack("<I", dimension))
    for identifier, row in zip(record_ids, outputs, strict=True):
        encoded_identifier = identifier.encode("utf-8")
        values = row.tolist()
        if len(values) != dimension:
            raise ContractError("canonical output dimension differs")
        if not all(math.isfinite(value) for value in values):
            raise ContractError("canonical output contains a non-finite value")
        norm = math.sqrt(math.fsum(value * value for value in values))
        if abs(norm - 1.0) > 1e-4:
            raise ContractError("canonical output is not L2-normalized")
        digest.update(struct.pack("<I", len(encoded_identifier)))
        digest.update(encoded_identifier)
        digest.update(struct.pack(f"<{dimension}f", *values))
    return digest.hexdigest()


def _run_sample(
    torch: Any,
    functional: Any,
    model: Any,
    batches: list[dict[str, Any]],
    record_ids: list[str],
    device: Any,
    dimension: int,
    *,
    digest_output: bool,
) -> tuple[int, int, int, str | None]:
    torch.cuda.reset_peak_memory_stats(device)
    torch.cuda.synchronize(device)
    start_event = torch.cuda.Event(enable_timing=True)
    end_event = torch.cuda.Event(enable_timing=True)
    outputs: list[Any] = []
    host_started = time.perf_counter_ns()
    start_event.record()
    with torch.inference_mode():
        for batch in batches:
            device_batch = {name: tensor.to(device, non_blocking=False) for name, tensor in batch.items()}
            hidden = model(**device_batch).last_hidden_state
            pooled = hidden[:, -1, :dimension].to(dtype=torch.float32)
            normalized = functional.normalize(pooled, p=2, dim=1, eps=1e-12)
            outputs.extend(normalized.cpu().unbind(dim=0))
    end_event.record()
    torch.cuda.synchronize(device)
    host_elapsed = time.perf_counter_ns() - host_started
    gpu_elapsed = max(1, round(start_event.elapsed_time(end_event) * 1_000_000))
    peak_allocated = int(torch.cuda.max_memory_allocated(device))
    digest = _output_digest(outputs, record_ids, dimension) if digest_output else None
    return host_elapsed, gpu_elapsed, peak_allocated, digest


def _measure_cell(
    torch: Any,
    functional: Any,
    model: Any,
    batches: list[dict[str, Any]],
    records: list[dict[str, str]],
    device: Any,
    lane_profile: dict[str, Any],
    plan: dict[str, Any],
    dimension: int,
) -> dict[str, Any]:
    record_ids = [record["id"] for record in records]
    for _ in range(plan["measurement"]["warmup_samples"]):
        _run_sample(
            torch,
            functional,
            model,
            batches,
            record_ids,
            device,
            dimension,
            digest_output=False,
        )
    raw_samples = []
    for sample_index in range(plan["measurement"]["measured_samples"]):
        host, gpu, peak, output_sha256 = _run_sample(
            torch,
            functional,
            model,
            batches,
            record_ids,
            device,
            dimension,
            digest_output=True,
        )
        raw_samples.append(
            {
                "sample": sample_index,
                "host_elapsed_nanos": host,
                "gpu_elapsed_nanos": gpu,
                "documents": lane_profile["documents"],
                "batches": lane_profile["batches"],
                "non_padding_tokens": lane_profile["non_padding_tokens"],
                "padded_tokens": lane_profile["padded_tokens"],
                "peak_allocated_bytes": peak,
                "output_sha256": output_sha256,
            }
        )
    total_documents = lane_profile["documents"] * len(raw_samples)
    total_host = sum(sample["host_elapsed_nanos"] for sample in raw_samples)
    throughput = {
        "basis": "sum-of-raw-host-timing-samples",
        "total_documents": total_documents,
        "total_host_elapsed_nanos": total_host,
        "documents_per_second": documents_per_second(total_documents, total_host),
    }
    return {
        "lane": lane_profile["lane"],
        "inference_dtype": lane_profile["inference_dtype"],
        "dimension": dimension,
        "canonical_output_dtype": "float32-le",
        "measurement_scope": plan["measurement"]["scope"],
        "warmup_samples": plan["measurement"]["warmup_samples"],
        "raw_timing_samples": raw_samples,
        "throughput": throughput,
        "projection": {
            "status": "non-claim-derived-projection",
            "basis": throughput,
            "assumptions": [
                "ideal linear scaling from this measured cell",
                "no allowance for ingestion, storage, indexing, queueing, or contention",
                "not a capacity, latency, cost, or completion-time claim",
            ],
            "values": projection_values(total_documents, total_host),
        },
    }


def _dtype(torch: Any, name: str) -> Any:
    return {"float32": torch.float32, "bfloat16": torch.bfloat16, "float16": torch.float16}[name]


def measure(arguments: argparse.Namespace) -> dict[str, Any]:
    for name, value in OFFLINE_ENVIRONMENT.items():
        expected = str(arguments.stage_root / "volatile/pycache") if name == "PYTHONPYCACHEPREFIX" else value
        if os.environ.get(name) != expected:
            raise ContractError(f"offline environment differs for {name}")
    if not sys.flags.isolated or not sys.dont_write_bytecode:
        raise ContractError("Python isolation and bytecode writes were not disabled with -I -B")
    if sys.pycache_prefix != str(arguments.stage_root / "volatile/pycache"):
        raise ContractError("Python did not use the isolated bytecode cache prefix")
    observed_environment_names = set(os.environ)
    intended_environment_names = (
        set(OFFLINE_ENVIRONMENT)
        | (observed_environment_names & set(PERMITTED_INHERITED_ENVIRONMENT))
        | {"TMPDIR"}
    )
    if observed_environment_names != intended_environment_names:
        raise ContractError("reference subject environment names differ from the frozen policy")
    forbidden_subject_hooks = FORBIDDEN_ENVIRONMENT_HOOKS - set(OFFLINE_ENVIRONMENT)
    if observed_environment_names & forbidden_subject_hooks:
        raise ContractError("reference subject retained a forbidden environment hook")
    if os.environ.get("TMPDIR") != str(arguments.stage_root / "volatile/tmp"):
        raise ContractError("reference subject temporary directory differs from the staged policy")
    if arguments.gpu_index < 0 or arguments.gpu_index > 15:
        raise ContractError("GPU index must be between 0 and 15")
    stage = paths_from_root(arguments.stage_root)
    execution_stage = validate_stage(stage)
    model_dir = arguments.model_dir
    manifest_path = arguments.manifest
    plan_path = arguments.plan
    corpus_path = arguments.corpus
    nvidia_smi = ensure_absolute_executable(arguments.nvidia_smi, "nvidia-smi")
    acquisition_record_path = arguments.acquisition_record
    legal_evidence_path = arguments.legal_evidence
    for path, label in (
        (arguments.stage_root, "stage root"),
        (arguments.model_dir, "model directory"),
        (arguments.manifest, "manifest"),
        (arguments.plan, "plan"),
        (arguments.corpus, "corpus"),
        (arguments.acquisition_record, "acquisition record"),
        (arguments.legal_evidence, "legal evidence"),
        (arguments.output, "output"),
    ):
        if not path.is_absolute():
            raise ContractError(f"{label} must be absolute")
    if (
        model_dir != stage.model
        or manifest_path != stage.model_manifest
        or plan_path != stage.plan
        or corpus_path != stage.corpus
        or nvidia_smi != stage.nvidia_smi
        or acquisition_record_path != stage.acquisition_record
        or legal_evidence_path != stage.legal_evidence
        or arguments.output != stage.receipt
    ):
        raise ContractError("reference subject inputs are outside the validated execution stage")
    manifest = load_json(manifest_path)
    validate_manifest(manifest, require_verified=True)
    verify_snapshot(manifest, model_dir, acquisition_record_path, legal_evidence_path)
    plan = load_plan(plan_path)
    _, source_records = load_corpus(corpus_path, plan)
    records = expanded_records(source_records, plan)
    artifact_capture = ExecutionArtifactCapture(stage, execution_stage)
    artifact_capture.capture("subject-startup")

    try:
        import torch
        import torch.nn.functional as functional
        from transformers import AutoModel, AutoTokenizer
    except ImportError as error:
        raise ContractError(f"reference environment import failed: {error}") from error
    artifact_capture.capture("framework-imported")
    if not torch.cuda.is_available():
        raise ContractError("PyTorch CUDA is unavailable")
    if arguments.gpu_index >= torch.cuda.device_count():
        raise ContractError("requested GPU index is outside PyTorch device count")
    device = torch.device(f"cuda:{arguments.gpu_index}")
    torch.cuda.set_device(device)
    torch.random.default_generator.manual_seed(PRECISION_CONTROLS["random_seed"])
    torch.cuda.manual_seed(PRECISION_CONTROLS["random_seed"])
    torch.backends.cuda.matmul.allow_tf32 = False
    torch.backends.cudnn.allow_tf32 = False
    torch.backends.cudnn.benchmark = False
    torch.backends.cudnn.deterministic = True
    torch.set_float32_matmul_precision("highest")
    torch.cuda.synchronize(device)

    gpu_uuid = _process_gpu_uuid(nvidia_smi)
    static_gpu = _gpu_static(nvidia_smi, gpu_uuid)
    if static_gpu["uuid"] != gpu_uuid:
        raise ContractError("NVML UUID selection returned a different physical GPU")
    gpu_before = _gpu_dynamic(nvidia_smi, gpu_uuid)
    properties = torch.cuda.get_device_properties(arguments.gpu_index)
    tokenizer = AutoTokenizer.from_pretrained(
        str(model_dir),
        local_files_only=True,
        trust_remote_code=False,
        use_fast=True,
    )
    tokenizer.padding_side = "left"
    tokenizer.truncation_side = "right"
    if tokenizer.padding_side != plan["tokenizer"]["padding_side"]:
        raise ContractError("tokenizer padding side differs from plan")
    if tokenizer.truncation_side != plan["tokenizer"]["truncation_side"]:
        raise ContractError("tokenizer truncation side differs from plan")
    artifact_capture.capture("tokenizer-loaded")

    lane_profiles = []
    cells = []
    for lane in plan["lanes"]:
        if lane["inference_dtype"] == "bfloat16" and not torch.cuda.is_bf16_supported():
            raise ContractError("requested BF16 lane is unsupported on this GPU")
        batches, preparation = _prepare_batches(tokenizer, records, lane)
        load_started = time.perf_counter_ns()
        model = AutoModel.from_pretrained(
            str(model_dir),
            local_files_only=True,
            trust_remote_code=False,
            use_safetensors=True,
            torch_dtype=_dtype(torch, lane["inference_dtype"]),
            attn_implementation="sdpa",
        )
        model.eval()
        model.to(device)
        torch.cuda.synchronize(device)
        model_load_elapsed = time.perf_counter_ns() - load_started
        artifact_capture.capture(f"lane-{lane['name']}-model-loaded")
        parameter_dtype = str(next(model.parameters()).dtype)
        if parameter_dtype != f"torch.{lane['inference_dtype']}":
            raise ContractError(f"{lane['name']} model parameter dtype differs after loading")
        lane_profile = {
            "lane": lane["name"],
            "inference_dtype": lane["inference_dtype"],
            "model_class": f"{type(model).__module__}.{type(model).__qualname__}",
            "parameter_dtype": parameter_dtype,
            "model_load_elapsed_nanos": model_load_elapsed,
            **preparation,
            "batch_token_budget": lane["batch_token_budget"],
            "max_batch_size": lane["max_batch_size"],
        }
        lane_profiles.append(lane_profile)
        for dimension in DIMENSIONS:
            cells.append(
                _measure_cell(
                    torch,
                    functional,
                    model,
                    batches,
                    records,
                    device,
                    lane_profile,
                    plan,
                    dimension,
                )
            )
            artifact_capture.capture(f"lane-{lane['name']}-dimension-{dimension}-measured")
        del model
        del batches
        gc.collect()
        torch.cuda.empty_cache()
        artifact_capture.capture(f"lane-{lane['name']}-released")
    gpu_after = _gpu_dynamic(nvidia_smi, gpu_uuid)
    verify_snapshot(manifest, model_dir, acquisition_record_path, legal_evidence_path)

    harness_dir = Path(__file__).resolve().parent
    tokenizer_files = [
        {"path": item["path"], "sha256": item["sha256"]}
        for item in manifest["files"]
        if item["role"] == "tokenizer"
    ]
    software = {
        "python_version": platform.python_version(),
        "python_implementation": platform.python_implementation(),
        "python_executable_sha256": next(
            item["sha256"]
            for item in execution_stage["files"]
            if item["identity"] == "executables/python"
        ),
        "platform": platform.platform(),
        "torch_version": _package_version("torch"),
        "transformers_version": _package_version("transformers"),
        "tokenizers_version": _package_version("tokenizers"),
        "safetensors_version": _package_version("safetensors"),
        "cuda_runtime_version": str(torch.version.cuda),
        "cudnn_version": str(torch.backends.cudnn.version()),
        "nvidia_smi_sha256": sha256_file(nvidia_smi),
    }
    environment_artifacts = artifact_capture.finish()
    execution_stage = validate_stage(stage)
    return {
        "schema": PROVISIONAL_SCHEMA,
        "source": {
            "harness_files_sha256": {
                name: sha256_file(harness_dir / name) for name in HARNESS_SOURCE_FILES
            },
            "manifest_sha256": sha256_file(manifest_path),
            "plan_sha256": sha256_file(plan_path),
            "corpus_sha256": sha256_file(corpus_path),
            "acquisition_record_sha256": sha256_file(acquisition_record_path),
            "legal_evidence_sha256": sha256_file(legal_evidence_path),
            "execution_stage": execution_stage,
        },
        "model": {
            "repository": manifest["model"]["repository"],
            "revision": manifest["model"]["revision"],
            "manifest_status": manifest["status"],
            "license_spdx": manifest["license"]["declared_spdx"],
            "license_declaration_source": manifest["license"]["declaration_source"],
            "bundled_license_text": manifest["license"]["bundled_license_text"],
            "legal_evidence_sha256": manifest["license"]["legal_evidence_sha256"],
            "product_claims_allowed": manifest["license"]["product_claims_allowed"],
            "config_sha256": next(
                item["sha256"]
                for item in manifest["files"]
                if item["path"] == "config.json"
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
        },
        "workload": {
            "source_records": len(source_records),
            "repetitions": plan["dataset"]["repetitions"],
            "expanded_records": len(records),
            "language_counts": language_counts(source_records, plan["dataset"]["repetitions"]),
            "tokenizer": {**plan["tokenizer"], "files": tokenizer_files},
            "instruction": plan["instruction"],
            "chunking": plan["chunking"],
            "canonical_output": plan["canonical_output"],
        },
        "execution_profile": {
            "invocation": {
                "shell": False,
                "automatic_network": False,
                "argv_contract": ARGV_CONTRACT,
                "offline_environment_names": sorted(OFFLINE_ENVIRONMENT),
                "python_execution": {
                    "method": "staged-file-open-descriptor-execve",
                    "staged_argv0": "staged:executables/python",
                },
                "nvidia_smi_execution": "staged-read-only-copy",
                "pycache_prefix": "staged:volatile/pycache",
                "acquisition_record": "staged:evidence/acquisition/acquisition.json",
                "legal_evidence": "staged:evidence/legal-evidence.json",
                "gpu_index": arguments.gpu_index,
                "inherited_environment_names": sorted(
                    name
                    for name in PERMITTED_INHERITED_ENVIRONMENT
                    if name in os.environ
                ),
            },
            "software": software,
            "environment_artifacts": environment_artifacts,
            "containment": CONTAINMENT_PROFILE,
            "gpu": {
                "nvidia_smi": static_gpu,
                "torch": {
                    "logical_index": arguments.gpu_index,
                    "name": properties.name,
                    "total_memory_bytes": int(properties.total_memory),
                    "compute_capability": f"{properties.major}.{properties.minor}",
                    "multiprocessor_count": int(properties.multi_processor_count),
                    "bound_nvidia_uuid": static_gpu["uuid"],
                    "bound_pci_bus_id": static_gpu["pci_bus_id"],
                },
            },
            "precision_controls": PRECISION_CONTROLS,
            "tokenizer_runtime": {
                "class": f"{type(tokenizer).__module__}.{type(tokenizer).__qualname__}",
                "is_fast": bool(tokenizer.is_fast),
                "vocab_size": int(tokenizer.vocab_size),
                "model_max_length": int(tokenizer.model_max_length),
                "truncation_side": tokenizer.truncation_side,
            },
            "gpu_observations": {"before": gpu_before, "after": gpu_after},
        },
        "lane_profiles": lane_profiles,
        "cells": cells,
    }


def main() -> int:
    arguments = parse_args()
    try:
        if not arguments.output.is_absolute() or not arguments.output.parent.is_dir():
            raise ContractError("output and its existing parent must be absolute")
        receipt = measure(arguments)
        arguments.output.write_text(
            json.dumps(receipt, indent=2, ensure_ascii=False) + "\n", encoding="utf-8"
        )
        validate_stage(paths_from_root(arguments.stage_root))
    except (ContractError, OSError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
