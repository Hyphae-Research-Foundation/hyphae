#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Shared fail-closed contracts for the multilingual embedding harness."""

from __future__ import annotations

import hashlib
import json
import math
import os
import re
from collections import Counter
from pathlib import Path, PurePosixPath
from typing import Any
from urllib.parse import parse_qs, urlsplit

MODEL_REPOSITORY = "Qwen/Qwen3-Embedding-0.6B"
MODEL_REPOSITORY_URL = "https://huggingface.co/Qwen/Qwen3-Embedding-0.6B"
MANIFEST_SCHEMA = "hyphae-embedding-model-manifest-v1"
ACQUISITION_SCHEMA = "hyphae-embedding-acquisition-evidence-v1"
LEGAL_EVIDENCE_SCHEMA = "hyphae-embedding-legal-evidence-v1"
PLAN_SCHEMA = "hyphae-multilingual-embedding-plan-v1"
CORPUS_SCHEMA = "hyphae-multilingual-embedding-corpus-v1"
RECEIPT_SCHEMA = "hyphae-multilingual-embedding-receipt-v1"
PROVISIONAL_SCHEMA = "hyphae-multilingual-embedding-provisional-v1"
ACQUISITION_EVIDENCE_KIND = "operator-retained-hugging-face-commit-responses-v1"
MODEL_API_PURPOSE = "commit-specific-model-metadata"
TREE_API_PURPOSE = "commit-specific-tree-metadata"
MODEL_CARD_PURPOSE = "commit-specific-model-card"
ARTIFACT_INVENTORY_ALGORITHM = "sha256-portable-identity-size-content-v1"
ARTIFACT_CAPTURE_SCOPE = {
    "installed_distributions": "all importlib.metadata distributions and non-bytecode declared files before framework import and after measurement",
    "loaded_python_modules": "union of loaded module files sampled at every retained capture phase",
    "loaded_native_libraries": "union of proc-self-maps regular files sampled at every retained capture phase",
}
DIMENSIONS = [384, 768, 1024]
LANES = [("fp32", "float32"), ("bf16", "bfloat16"), ("fp16", "float16")]
ARTIFACT_CAPTURE_PHASES = ["subject-startup", "framework-imported", "tokenizer-loaded"] + [
    phase
    for lane, _ in LANES
    for phase in (
        f"lane-{lane}-model-loaded",
        *(f"lane-{lane}-dimension-{dimension}-measured" for dimension in DIMENSIONS),
        f"lane-{lane}-released",
    )
] + ["measurement-complete"]
PROJECTION_VECTOR_COUNTS = [1_000_000, 10_000_000, 50_000_000, 100_000_000, 1_000_000_000]
PLAN_SHA256 = "295e8f7da8b632dec5ec9f199820e6986a06ae6f0af7197420c6ed91037f75a6"
CORPUS_SHA256 = "ae87f7a91cf01895d55927e483b29c862cb0a03fab640a97c99cc16401233d89"
CORPUS_RECORD_COUNT = 24
CONTAINMENT_PROFILE = {
    "provider": "systemd-user-service-cgroup-v2",
    "authenticity": "operator-enforced-no-independent-attestation",
    "memory_max_bytes": 32 * 1024 * 1024 * 1024,
    "memory_swap_max_bytes": 0,
    "tasks_max": 64,
    "runtime_max_seconds": 14_460,
    "file_size_max_bytes": 32 * 1024 * 1024,
    "writable_tmpfs_max_bytes": 128 * 1024 * 1024,
    "network": "denied-by-systemd-address-family-and-cgroup-ip-policy",
    "user_manager_access": "denied-by-unset-environment-inaccessible-sockets-and-network-syscall-filter",
    "host_filesystems": "read-only-no-host-writable-path-contained-provisional-pipe",
}
SPDX = "SPDX-License-Identifier: Apache-2.0"
HEX40 = re.compile(r"[0-9a-f]{40}\Z")
HEX64 = re.compile(r"[0-9a-f]{64}\Z")
IDENTIFIER = re.compile(r"[a-z0-9][a-z0-9._-]{0,127}\Z")
LANGUAGE = re.compile(r"[A-Za-z]{2,3}(?:-[A-Za-z0-9]{2,8})*\Z")
FORBIDDEN_WEIGHT_SUFFIXES = {".bin", ".ckpt", ".gguf", ".onnx", ".pt", ".pth"}
ALLOWED_SNAPSHOT_SUFFIXES = {
    "",
    ".gitattributes",
    ".json",
    ".md",
    ".model",
    ".safetensors",
    ".tiktoken",
    ".txt",
}
MAX_JSON_BYTES = 4 * 1024 * 1024
MAX_SNAPSHOT_FILES = 128
MAX_SNAPSHOT_BYTES = 16 * 1024 * 1024 * 1024
MAX_ARTIFACT_FILES = 100_000
MAX_ARTIFACT_BYTES = 128 * 1024 * 1024 * 1024
MAX_ACQUISITION_RESPONSES = 18
MAX_ACQUISITION_BODY_BYTES = 16 * 1024 * 1024
MAX_ACQUISITION_TOTAL_BODY_BYTES = 64 * 1024 * 1024

FROZEN_INSTRUCTION = {
    "task": "Given a web search query, retrieve relevant passages that answer the query",
    "query_template": "Instruct: {task}\nQuery: {text}",
    "passage_template": "{text}",
}
FROZEN_TOKENIZER = {
    "padding_side": "left",
    "truncation_side": "right",
    "truncation": True,
    "max_length": 512,
    "add_special_tokens": True,
}
FROZEN_CHUNKING = {
    "unit": "token",
    "strategy": "truncate-right-single-chunk",
    "max_tokens": 512,
    "overlap_tokens": 0,
}
FROZEN_DATASET = {
    "repetitions": 32,
    "max_source_records": 64,
    "max_expanded_records": 4096,
}
FROZEN_MEASUREMENT = {
    "warmup_samples": 2,
    "measured_samples": 7,
    "subprocess_timeout_seconds": 14_400,
    "scope": "pretokenized-gpu-inference-pooling-normalization-device-to-host",
}
FROZEN_LANES = [
    {
        "name": "fp32",
        "inference_dtype": "float32",
        "batch_token_budget": 4096,
        "max_batch_size": 16,
    },
    {
        "name": "bf16",
        "inference_dtype": "bfloat16",
        "batch_token_budget": 8192,
        "max_batch_size": 32,
    },
    {
        "name": "fp16",
        "inference_dtype": "float16",
        "batch_token_budget": 8192,
        "max_batch_size": 32,
    },
]


class ContractError(ValueError):
    """A manifest, plan, corpus, snapshot, or receipt violated its contract."""


def _object_without_duplicates(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    value: dict[str, Any] = {}
    for key, item in pairs:
        if key in value:
            raise ContractError(f"duplicate JSON key: {key}")
        value[key] = item
    return value


def load_json(path: Path, *, maximum_bytes: int = MAX_JSON_BYTES) -> dict[str, Any]:
    value = load_json_value(path, maximum_bytes=maximum_bytes)
    if not isinstance(value, dict):
        raise ContractError(f"{path}: top-level value must be an object")
    return value


def parse_json_bytes(data: bytes, *, maximum_bytes: int, label: str) -> dict[str, Any]:
    if len(data) > maximum_bytes:
        raise ContractError(f"{label} exceeds {maximum_bytes} bytes")
    try:
        value = json.loads(
            data,
            object_pairs_hook=_object_without_duplicates,
            parse_constant=lambda item: (_ for _ in ()).throw(
                ContractError(f"non-finite JSON value: {item}")
            ),
        )
    except (UnicodeError, json.JSONDecodeError) as error:
        raise ContractError(f"{label} is not canonical JSON: {error}") from error
    if not isinstance(value, dict):
        raise ContractError(f"{label} must be a JSON object")
    return value


def load_json_value(path: Path, *, maximum_bytes: int = MAX_JSON_BYTES) -> Any:
    try:
        size = path.stat().st_size
        if size > maximum_bytes:
            raise ContractError(f"{path}: exceeds {maximum_bytes} bytes")
        value = json.loads(
            path.read_text(encoding="utf-8"),
            object_pairs_hook=_object_without_duplicates,
            parse_constant=lambda value: (_ for _ in ()).throw(
                ContractError(f"non-finite JSON value: {value}")
            ),
        )
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise ContractError(f"{path}: cannot read canonical JSON: {error}") from error
    return value


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    try:
        with path.open("rb") as source:
            while chunk := source.read(1024 * 1024):
                digest.update(chunk)
    except OSError as error:
        raise ContractError(f"{path}: cannot calculate SHA-256: {error}") from error
    return digest.hexdigest()


def artifact_record(path: Path, identity: str) -> dict[str, Any]:
    identity = require_string(identity, "environment artifact portable identity")
    try:
        resolved = path.resolve(strict=True)
        if not resolved.is_file():
            raise ContractError(f"environment artifact is not a regular file: {resolved}")
        size = resolved.stat().st_size
    except OSError as error:
        raise ContractError(f"cannot inspect environment artifact {path}: {error}") from error
    return {"identity": identity, "size_bytes": size, "sha256": sha256_file(resolved)}


def artifact_inventory_digest(records: Any, label: str) -> str:
    if not isinstance(records, list):
        raise ContractError(f"{label} must be a list")
    if len(records) > MAX_ARTIFACT_FILES:
        raise ContractError(f"{label} exceeds the artifact count bound")
    digest = hashlib.sha256(f"hyphae-{ARTIFACT_INVENTORY_ALGORITHM}\0".encode("ascii"))
    previous = ""
    total_bytes = 0
    for index, record in enumerate(records):
        record = require_fields(
            record, {"identity", "size_bytes", "sha256"}, f"{label} file {index}"
        )
        identity = require_string(record["identity"], f"{label} file {index} identity")
        if identity.startswith(("/", "\\")) or re.match(r"[A-Za-z]:[\\/]", identity):
            raise ContractError(f"{label} identity contains an absolute runtime path")
        if identity <= previous:
            raise ContractError(f"{label} portable identities must be sorted and unique")
        size = require_integer(record["size_bytes"], f"{label} file {index} size")
        sha256 = require_sha256(record["sha256"], f"{label} file {index} digest")
        total_bytes += size
        if total_bytes > MAX_ARTIFACT_BYTES:
            raise ContractError(f"{label} exceeds the artifact byte bound")
        encoded = identity.encode("utf-8")
        digest.update(len(encoded).to_bytes(8, "little"))
        digest.update(encoded)
        digest.update(size.to_bytes(16, "little"))
        digest.update(bytes.fromhex(sha256))
        previous = identity
    return digest.hexdigest()


def distribution_inventory_digest(distributions: Any) -> str:
    if not isinstance(distributions, list) or not distributions:
        raise ContractError("installed distribution inventory must be a non-empty list")
    digest = hashlib.sha256(b"hyphae-python-distribution-inventory-v1\0")
    previous = ""
    for index, distribution in enumerate(distributions):
        distribution = require_fields(
            distribution,
            {"name", "version", "files", "files_sha256"},
            f"installed distribution {index}",
        )
        name = require_string(distribution["name"], f"installed distribution {index} name")
        version = require_string(
            distribution["version"], f"installed distribution {index} version"
        )
        if name <= previous:
            raise ContractError("installed distribution names must be sorted and unique")
        files_sha256 = artifact_inventory_digest(
            distribution["files"], f"installed distribution {name}"
        )
        if distribution["files_sha256"] != files_sha256:
            raise ContractError(f"installed distribution {name} inventory digest differs")
        for value in (name, version):
            encoded = value.encode("utf-8")
            digest.update(len(encoded).to_bytes(8, "little"))
            digest.update(encoded)
        digest.update(bytes.fromhex(files_sha256))
        previous = name
    return digest.hexdigest()


def environment_artifacts_digest(
    capture_phases: list[str],
    installed_distributions_sha256: str,
    loaded_python_modules_sha256: str,
    loaded_native_libraries_sha256: str,
) -> str:
    digest = hashlib.sha256(b"hyphae-external-python-environment-v1\0")
    if not isinstance(capture_phases, list) or not capture_phases:
        raise ContractError("environment capture phases must be a non-empty list")
    for value in capture_phases:
        encoded = require_string(value, "environment capture phase").encode("utf-8")
        digest.update(len(encoded).to_bytes(8, "little"))
        digest.update(encoded)
    for value, label in (
        (installed_distributions_sha256, "installed distributions digest"),
        (loaded_python_modules_sha256, "loaded Python modules digest"),
        (loaded_native_libraries_sha256, "loaded native libraries digest"),
    ):
        digest.update(bytes.fromhex(require_sha256(value, label)))
    return digest.hexdigest()


def require_fields(value: Any, expected: set[str], label: str) -> dict[str, Any]:
    if not isinstance(value, dict) or set(value) != expected:
        observed = sorted(value) if isinstance(value, dict) else type(value).__name__
        raise ContractError(f"{label} fields mismatch: {observed}")
    return value


def require_string(value: Any, label: str) -> str:
    if not isinstance(value, str) or not value or "\x00" in value:
        raise ContractError(f"{label} must be a non-empty string without NUL")
    return value


def require_integer(value: Any, label: str, *, minimum: int = 0) -> int:
    if not isinstance(value, int) or isinstance(value, bool) or value < minimum:
        raise ContractError(f"{label} must be an integer >= {minimum}")
    return value


def require_sha256(value: Any, label: str) -> str:
    if not isinstance(value, str) or HEX64.fullmatch(value) is None:
        raise ContractError(f"{label} must be a lowercase SHA-256")
    return value


def _safe_relative_path(value: Any, label: str) -> str:
    text = require_string(value, label)
    candidate = PurePosixPath(text)
    if (
        candidate.is_absolute()
        or "\\" in text
        or not candidate.parts
        or any(part in {"", ".", ".."} for part in candidate.parts)
    ):
        raise ContractError(f"{label} is not a safe relative POSIX path")
    return text


def validate_manifest(document: dict[str, Any], *, require_verified: bool) -> dict[str, Any]:
    require_fields(
        document,
        {
            "$comment",
            "schema",
            "status",
            "model",
            "license",
            "runtime_policy",
            "acquisition_provenance",
            "files",
        },
        "model manifest",
    )
    if document["$comment"] != SPDX or document["schema"] != MANIFEST_SCHEMA:
        raise ContractError("model manifest schema or license marker differs")
    status = document["status"]
    if status not in {"template-unverified", "verified"}:
        raise ContractError("model manifest status is invalid")
    if require_verified and status != "verified":
        raise ContractError("model manifest is an unverified template")

    model = require_fields(
        document["model"],
        {
            "repository",
            "revision",
            "model_type",
            "parameter_class",
            "native_dimensions",
            "supported_output_dimensions",
        },
        "model identity",
    )
    if (
        model["repository"] != MODEL_REPOSITORY
        or model["model_type"] != "qwen3"
        or model["parameter_class"] != "0.6B"
        or model["native_dimensions"] != 1024
        or model["supported_output_dimensions"] != DIMENSIONS
    ):
        raise ContractError("model identity differs from Qwen3-Embedding-0.6B")
    revision = model["revision"]
    if status == "verified":
        if not isinstance(revision, str) or HEX40.fullmatch(revision) is None:
            raise ContractError("verified model revision must be a full lowercase Git commit")
    elif revision is not None:
        raise ContractError("template model revision must remain null")

    runtime_policy = require_fields(
        document["runtime_policy"],
        {
            "automatic_network",
            "local_files_only",
            "trust_remote_code",
            "weights_format",
            "canonical_output_dtype",
        },
        "model runtime policy",
    )
    if runtime_policy != {
        "automatic_network": False,
        "local_files_only": True,
        "trust_remote_code": False,
        "weights_format": "safetensors-only",
        "canonical_output_dtype": "float32",
    }:
        raise ContractError("model runtime policy is not the fixed offline safe policy")

    provenance = require_fields(
        document["acquisition_provenance"],
        {
            "evidence_kind",
            "repository",
            "revision",
            "capture",
            "acquisition_record_sha256",
            "model_metadata_response",
            "model_card_response",
            "tree_metadata_responses",
            "files",
        },
        "acquisition provenance",
    )
    if (
        provenance["evidence_kind"] != ACQUISITION_EVIDENCE_KIND
        or provenance["repository"] != MODEL_REPOSITORY
    ):
        raise ContractError("acquisition evidence kind or repository differs")
    if status == "verified":
        if provenance["revision"] != revision:
            raise ContractError("acquisition provenance revision differs from model revision")
        require_sha256(
            provenance["acquisition_record_sha256"], "acquisition evidence record digest"
        )
        if not isinstance(provenance["capture"], dict):
            raise ContractError("acquisition capture identity is missing")
        if not isinstance(provenance["model_metadata_response"], dict):
            raise ContractError("acquisition model metadata response is missing")
        if not isinstance(provenance["model_card_response"], dict):
            raise ContractError("acquisition model card response is missing")
        if not isinstance(provenance["tree_metadata_responses"], list) or not provenance[
            "tree_metadata_responses"
        ]:
            raise ContractError("acquisition tree metadata responses are missing")
    elif (
        provenance["revision"] is not None
        or provenance["capture"] is not None
        or provenance["acquisition_record_sha256"] is not None
        or provenance["model_metadata_response"] is not None
        or provenance["model_card_response"] is not None
        or provenance["tree_metadata_responses"] != []
        or provenance["files"] != []
    ):
        raise ContractError("template acquisition provenance must remain unverified")

    license_identity = require_fields(
        document["license"],
        {
            "expected_declared_spdx",
            "declared_spdx",
            "declaration_source",
            "bundled_license_text",
            "legal_evidence_sha256",
            "product_claims_allowed",
        },
        "model license evidence",
    )
    if license_identity["expected_declared_spdx"] != "Apache-2.0":
        raise ContractError("expected upstream SPDX declaration differs")
    bundled = require_fields(
        license_identity["bundled_license_text"], {"status", "path", "sha256"},
        "bundled license text evidence",
    )
    if status == "verified":
        if (
            license_identity["declared_spdx"] != "Apache-2.0"
            or license_identity["declaration_source"]
            != "hugging-face-model-api.cardData.license"
        ):
            raise ContractError("retained upstream SPDX declaration differs")
        require_sha256(license_identity["legal_evidence_sha256"], "legal evidence digest")
        if not isinstance(license_identity["product_claims_allowed"], bool):
            raise ContractError("legal evidence product-claim policy is invalid")
        if license_identity["product_claims_allowed"] is not False:
            raise ContractError("model manifest cannot unlock product claims")
        if bundled["status"] == "absent-in-upstream-snapshot":
            if bundled["path"] is not None or bundled["sha256"] is not None:
                raise ContractError("absent bundled license text must not assert a file")
        elif bundled["status"] == "present-in-upstream-snapshot":
            _safe_relative_path(bundled["path"], "bundled license text path")
            require_sha256(bundled["sha256"], "bundled license text digest")
        else:
            raise ContractError("bundled license text status is invalid")
    elif (
        license_identity["declared_spdx"] is not None
        or license_identity["declaration_source"] is not None
        or bundled != {"status": "unverified", "path": None, "sha256": None}
        or license_identity["legal_evidence_sha256"] is not None
        or license_identity["product_claims_allowed"] is not False
    ):
        raise ContractError("template license evidence must remain unverified and claim-closed")

    files = document["files"]
    if not isinstance(files, list) or not files:
        raise ContractError("model manifest must list files")
    paths: list[str] = []
    roles: Counter[str] = Counter()
    by_path: dict[str, dict[str, Any]] = {}
    for index, item in enumerate(files):
        item = require_fields(item, {"path", "role", "size_bytes", "sha256"}, f"file {index}")
        path = _safe_relative_path(item["path"], f"file {index} path")
        role = item["role"]
        if role not in {
            "documentation",
            "license-text",
            "metadata",
            "model-config",
            "tokenizer",
            "weight-index",
            "weights",
        }:
            raise ContractError(f"file {path} has an unknown role")
        suffix = Path(path).suffix.lower()
        if suffix in FORBIDDEN_WEIGHT_SUFFIXES or path.endswith(".py"):
            raise ContractError(f"file {path} is forbidden by the runtime policy")
        if role == "weights" and suffix != ".safetensors":
            raise ContractError(f"weight file {path} is not safetensors")
        if role == "weight-index" and not path.endswith(".safetensors.index.json"):
            raise ContractError(f"weight index {path} is not a safetensors index")
        if status == "verified":
            require_integer(item["size_bytes"], f"file {path} size", minimum=1)
            require_sha256(item["sha256"], f"file {path} digest")
        elif item["size_bytes"] is not None or item["sha256"] is not None:
            raise ContractError("template file sizes and digests must remain null")
        paths.append(path)
        roles[role] += 1
        by_path[path] = item
    if paths != sorted(paths) or len(paths) != len(set(paths)):
        raise ContractError("model manifest file paths must be sorted and unique")
    if roles["weights"] < 1 or roles["model-config"] != 1 or roles["tokenizer"] < 2:
        raise ContractError("model manifest lacks weights, config, or tokenizer identity")
    if "config.json" not in by_path or "tokenizer.json" not in by_path:
        raise ContractError("model manifest must include config.json and tokenizer.json")
    if status == "verified":
        provenance_files = provenance["files"]
        if not isinstance(provenance_files, list) or len(provenance_files) != len(files):
            raise ContractError("acquisition provenance file inventory differs from manifest")
        for index, (source, manifested) in enumerate(
            zip(provenance_files, files, strict=True)
        ):
            source = require_fields(
                source,
                {"path", "source_oid", "storage", "size_bytes", "sha256"},
                f"acquisition provenance file {index}",
            )
            if (
                source["path"] != manifested["path"]
                or source["size_bytes"] != manifested["size_bytes"]
                or source["sha256"] != manifested["sha256"]
            ):
                raise ContractError(f"acquisition provenance file {index} differs from manifest")
            if not isinstance(source["source_oid"], str) or HEX40.fullmatch(
                source["source_oid"]
            ) is None:
                raise ContractError(f"acquisition provenance file {index} source OID is invalid")
            if source["storage"] not in {"git-blob", "git-lfs-sha256"}:
                raise ContractError(f"acquisition provenance file {index} storage is unsupported")
        if bundled["status"] == "present-in-upstream-snapshot":
            path = bundled["path"]
            if path not in by_path or by_path[path]["role"] != "license-text":
                raise ContractError("bundled license text is not identified in the manifest")
            if bundled["sha256"] != by_path[path]["sha256"]:
                raise ContractError("bundled license text digest differs from the manifest")
    return document


def snapshot_files(model_dir: Path) -> list[Path]:
    if not model_dir.is_absolute() or not model_dir.is_dir():
        raise ContractError("model directory must be an existing absolute directory")
    files: list[Path] = []
    total_bytes = 0
    try:
        for path in sorted(model_dir.rglob("*")):
            if path.is_symlink():
                raise ContractError(f"model snapshot contains a mutable symlink: {path}")
            if path.is_dir():
                continue
            if not path.is_file():
                raise ContractError(f"model snapshot contains a non-regular entry: {path}")
            relative = path.relative_to(model_dir).as_posix()
            _safe_relative_path(relative, "snapshot file")
            if any(part.startswith(".") and part != ".gitattributes" for part in Path(relative).parts):
                raise ContractError(f"model snapshot contains hidden state: {relative}")
            suffix = Path(relative).suffix.lower()
            if suffix in FORBIDDEN_WEIGHT_SUFFIXES or relative.endswith(".py"):
                raise ContractError(f"model snapshot contains forbidden executable/weights: {relative}")
            if suffix not in ALLOWED_SNAPSHOT_SUFFIXES:
                raise ContractError(f"model snapshot file type is not allowlisted: {relative}")
            size = path.stat().st_size
            if size <= 0:
                raise ContractError(f"model snapshot file is empty: {relative}")
            total_bytes += size
            files.append(path)
            if len(files) > MAX_SNAPSHOT_FILES or total_bytes > MAX_SNAPSHOT_BYTES:
                raise ContractError("model snapshot exceeds bounded file or byte limits")
    except OSError as error:
        raise ContractError(f"cannot inspect model snapshot: {error}") from error
    if not files:
        raise ContractError("model snapshot is empty")
    return files


def _response_identity(response: dict[str, Any]) -> dict[str, Any]:
    return {
        "purpose": response["purpose"],
        "source_url": response["response_url"],
        "body_path": response["body_path"],
        "body_size_bytes": response["body_size_bytes"],
        "body_sha256": response["body_sha256"],
    }


def _retained_next_link(value: str | None) -> str | None:
    if value is None:
        return None
    for item in value.split(","):
        url = re.search(r"<([^>]+)>", item)
        if url is not None and re.search(r'rel="?next"?', item) is not None:
            return url.group(1)
    return None


def _validate_response(
    value: Any,
    evidence_root: Path,
    revision: str,
    index: int,
) -> tuple[dict[str, Any], Any]:
    value = require_fields(
        value,
        {
            "purpose",
            "request_url",
            "response_url",
            "http_status",
            "response_headers",
            "body_path",
            "body_size_bytes",
            "body_sha256",
        },
        f"acquisition response {index}",
    )
    purpose = value["purpose"]
    if purpose not in {MODEL_API_PURPOSE, TREE_API_PURPOSE, MODEL_CARD_PURPOSE}:
        raise ContractError(f"acquisition response {index} purpose is invalid")
    source_url = require_string(value["request_url"], f"acquisition response {index} URL")
    if value["response_url"] != source_url:
        raise ContractError("acquisition response redirects are not permitted")
    if purpose == MODEL_API_PURPOSE:
        base = f"https://huggingface.co/api/models/{MODEL_REPOSITORY}/revision/{revision}"
    elif purpose == TREE_API_PURPOSE:
        base = f"https://huggingface.co/api/models/{MODEL_REPOSITORY}/tree/{revision}"
    else:
        base = f"https://huggingface.co/{MODEL_REPOSITORY}/raw/{revision}/README.md"
    if source_url != base and not source_url.startswith(f"{base}?"):
        raise ContractError("acquisition response URL is not the commit-specific endpoint")
    if purpose == TREE_API_PURPOSE:
        query = parse_qs(urlsplit(source_url).query)
        if query.get("recursive") != ["true"] or query.get("expand") != ["true"]:
            raise ContractError("tree acquisition URL must require recursive=true and expand=true")
    if value["http_status"] != 200:
        raise ContractError("acquisition response did not retain HTTP 200")
    headers = value["response_headers"]
    if not isinstance(headers, dict) or not headers:
        raise ContractError("acquisition response headers are missing")
    for name, item in headers.items():
        if not isinstance(name, str) or name != name.lower():
            raise ContractError("acquisition response header names must be lowercase")
        require_string(item, f"acquisition response header {name}")
    body_path = _safe_relative_path(value["body_path"], "acquisition response body path")
    body = evidence_root / body_path
    size = require_integer(value["body_size_bytes"], "acquisition response body size", minimum=1)
    if size > MAX_ACQUISITION_BODY_BYTES:
        raise ContractError("retained acquisition response exceeds 16 MiB")
    digest = require_sha256(value["body_sha256"], "acquisition response body digest")
    try:
        observed_size = body.stat().st_size
    except OSError as error:
        raise ContractError(f"cannot inspect retained acquisition body {body_path}: {error}") from error
    if observed_size != size or sha256_file(body) != digest:
        raise ContractError(f"retained acquisition response bytes differ: {body_path}")
    return value, body.read_bytes() if purpose == MODEL_CARD_PURPOSE else load_json_value(body)


def _git_blob_oid(path: Path, size: int) -> str:
    digest = hashlib.sha1(usedforsecurity=False)
    digest.update(f"blob {size}\0".encode("ascii"))
    try:
        with path.open("rb") as source:
            while chunk := source.read(1024 * 1024):
                digest.update(chunk)
    except OSError as error:
        raise ContractError(f"cannot hash retained source blob {path}: {error}") from error
    return digest.hexdigest()


def acquisition_snapshot_provenance(
    model_dir: Path,
    revision: str,
    acquisition_record_path: Path,
) -> dict[str, Any]:
    if HEX40.fullmatch(revision) is None:
        raise ContractError("revision must be a full lowercase 40-character commit")
    record = load_json(acquisition_record_path)
    require_fields(
        record,
        {
            "$comment",
            "schema",
            "status",
            "evidence_kind",
            "repository",
            "revision",
            "captured_at_utc",
            "capture",
            "responses",
        },
        "acquisition evidence record",
    )
    if (
        record["$comment"] != SPDX
        or record["schema"] != ACQUISITION_SCHEMA
        or record["status"] != "captured"
        or record["evidence_kind"] != ACQUISITION_EVIDENCE_KIND
        or record["repository"] != MODEL_REPOSITORY
        or record["revision"] != revision
    ):
        raise ContractError("acquisition evidence identity or revision differs")
    captured = require_string(record["captured_at_utc"], "acquisition capture time")
    if re.fullmatch(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z", captured) is None:
        raise ContractError("acquisition capture time is not canonical UTC")
    capture = require_fields(
        record["capture"],
        {"tool", "tool_path", "tool_sha256", "https_validation", "authentication"},
        "acquisition capture identity",
    )
    if (
        capture["tool"] != "acquire_evidence.py"
        or capture["https_validation"]
        != "python-default-context-hostname-and-certificate"
        or capture["authentication"] != "public-read-no-credentials"
    ):
        raise ContractError("acquisition capture policy differs")
    tool_path = _safe_relative_path(capture["tool_path"], "retained acquisition tool path")
    require_sha256(capture["tool_sha256"], "acquisition tool digest")
    if sha256_file(acquisition_record_path.parent / tool_path) != capture["tool_sha256"]:
        raise ContractError("retained acquisition tool bytes differ")
    responses = record["responses"]
    if (
        not isinstance(responses, list)
        or len(responses) < 3
        or len(responses) > MAX_ACQUISITION_RESPONSES
    ):
        raise ContractError("acquisition evidence lacks model, model-card, and tree responses")
    model_responses = []
    model_card_responses = []
    tree_responses = []
    tree_entries: dict[str, dict[str, Any]] = {}
    total_body_bytes = 0
    for index, item in enumerate(responses):
        response, body = _validate_response(
            item, acquisition_record_path.parent, revision, index
        )
        if response["body_size_bytes"] > MAX_ACQUISITION_BODY_BYTES:
            raise ContractError("retained acquisition response exceeds 16 MiB")
        total_body_bytes += response["body_size_bytes"]
        if total_body_bytes > MAX_ACQUISITION_TOTAL_BODY_BYTES:
            raise ContractError("retained acquisition response aggregate exceeds 64 MiB")
        if response["purpose"] == MODEL_API_PURPOSE:
            model_responses.append((response, body))
            continue
        if response["purpose"] == MODEL_CARD_PURPOSE:
            model_card_responses.append((response, body))
            continue
        tree_responses.append(response)
        if not isinstance(body, list):
            raise ContractError("retained tree API body must be an array")
        for entry in body:
            if not isinstance(entry, dict) or entry.get("type") not in {"file", "directory"}:
                raise ContractError("retained tree API entry is malformed")
            if entry["type"] == "directory":
                continue
            relative = _safe_relative_path(entry.get("path"), "retained tree file path")
            oid = entry.get("oid")
            if not isinstance(oid, str) or HEX40.fullmatch(oid) is None:
                raise ContractError(f"retained tree source OID is invalid: {relative}")
            size = require_integer(entry.get("size"), f"retained tree size {relative}", minimum=1)
            lfs = entry.get("lfs")
            if lfs is not None:
                if not isinstance(lfs, dict):
                    raise ContractError(f"retained tree LFS identity is malformed: {relative}")
                lfs_sha256 = require_sha256(lfs.get("oid"), f"retained tree LFS digest {relative}")
                lfs_size = require_integer(
                    lfs.get("size"), f"retained tree LFS size {relative}", minimum=1
                )
                if size != lfs_size:
                    raise ContractError(f"retained tree LFS size differs: {relative}")
            else:
                lfs_sha256 = None
            if relative in tree_entries:
                raise ContractError(f"retained tree path is duplicated: {relative}")
            tree_entries[relative] = {
                "source_oid": oid,
                "size_bytes": size,
                "lfs_sha256": lfs_sha256,
            }
    if len(model_responses) != 1 or len(model_card_responses) != 1 or not tree_responses:
        raise ContractError("acquisition evidence response coverage differs")
    model_response, model_body = model_responses[0]
    for index, response in enumerate(tree_responses):
        next_url = _retained_next_link(response["response_headers"].get("link"))
        expected_next = (
            tree_responses[index + 1]["request_url"]
            if index + 1 < len(tree_responses)
            else None
        )
        if next_url != expected_next:
            raise ContractError("retained tree API pagination chain is incomplete")
    if not isinstance(model_body, dict):
        raise ContractError("retained model API body must be an object")
    repository = model_body.get("id", model_body.get("modelId"))
    card_data = model_body.get("cardData")
    if repository != MODEL_REPOSITORY or model_body.get("sha") != revision:
        raise ContractError("retained model API repository or commit differs")
    if not isinstance(card_data, dict) or card_data.get("license") != "apache-2.0":
        raise ContractError("retained model API SPDX declaration is not apache-2.0")
    model_card_response, model_card_body = model_card_responses[0]
    if not isinstance(model_card_body, bytes) or not model_card_body:
        raise ContractError("retained model card response is empty")

    observed = {
        path.relative_to(model_dir).as_posix(): path for path in snapshot_files(model_dir)
    }
    if set(observed) != set(tree_entries):
        missing = sorted(set(tree_entries) - set(observed))
        extra = sorted(set(observed) - set(tree_entries))
        raise ContractError(
            f"model snapshot differs from retained API tree; missing={missing}, extra={extra}"
        )
    if "README.md" not in observed or observed["README.md"].read_bytes() != model_card_body:
        raise ContractError("snapshot model card differs from retained commit-specific response")
    files = []
    for relative in sorted(tree_entries):
        expected = tree_entries[relative]
        path = observed[relative]
        size = path.stat().st_size
        sha256 = sha256_file(path)
        if expected["lfs_sha256"] is None:
            if size != expected["size_bytes"] or _git_blob_oid(path, size) != expected["source_oid"]:
                raise ContractError(f"snapshot bytes differ from retained API blob: {relative}")
            storage = "git-blob"
        else:
            if size != expected["size_bytes"] or sha256 != expected["lfs_sha256"]:
                raise ContractError(f"snapshot bytes differ from retained API LFS object: {relative}")
            storage = "git-lfs-sha256"
        files.append(
            {
                "path": relative,
                "source_oid": expected["source_oid"],
                "storage": storage,
                "size_bytes": size,
                "sha256": sha256,
            }
        )
    return {
        "evidence_kind": ACQUISITION_EVIDENCE_KIND,
        "repository": MODEL_REPOSITORY,
        "revision": revision,
        "capture": capture,
        "acquisition_record_sha256": sha256_file(acquisition_record_path),
        "model_metadata_response": _response_identity(model_response),
        "model_card_response": _response_identity(model_card_response),
        "tree_metadata_responses": [
            _response_identity(response) for response in tree_responses
        ],
        "files": files,
    }


def validate_legal_evidence(
    legal_evidence_path: Path,
    acquisition: dict[str, Any],
    model_dir: Path,
) -> dict[str, Any]:
    document = load_json(legal_evidence_path)
    require_fields(
        document,
        {
            "$comment",
            "schema",
            "status",
            "repository",
            "revision",
            "acquisition_record_sha256",
            "declared_license",
            "model_card",
            "bundled_license_text",
            "policy",
            "review",
        },
        "legal evidence record",
    )
    if (
        document["$comment"] != SPDX
        or document["schema"] != LEGAL_EVIDENCE_SCHEMA
        or document["status"] != "reviewed"
        or document["repository"] != MODEL_REPOSITORY
        or document["revision"] != acquisition["revision"]
        or document["acquisition_record_sha256"]
        != acquisition["acquisition_record_sha256"]
    ):
        raise ContractError("legal evidence identity, review status, or acquisition binding differs")
    declaration = require_fields(
        document["declared_license"],
        {"source", "upstream_value", "spdx", "model_metadata_body_sha256"},
        "declared license evidence",
    )
    if declaration != {
        "source": "hugging-face-model-api.cardData.license",
        "upstream_value": "apache-2.0",
        "spdx": "Apache-2.0",
        "model_metadata_body_sha256": acquisition["model_metadata_response"]["body_sha256"],
    }:
        raise ContractError("legal evidence SPDX declaration differs from retained metadata")
    model_card = require_fields(
        document["model_card"], {"path", "source_url", "body_sha256"},
        "legal evidence model card",
    )
    if model_card != {
        "path": "README.md",
        "source_url": acquisition["model_card_response"]["source_url"],
        "body_sha256": acquisition["model_card_response"]["body_sha256"],
    }:
        raise ContractError("legal evidence model card binding differs")
    if sha256_file(model_dir / "README.md") != model_card["body_sha256"]:
        raise ContractError("legal evidence model card bytes differ")
    bundled = require_fields(
        document["bundled_license_text"], {"status", "path", "sha256"},
        "legal evidence bundled text",
    )
    if bundled["status"] == "absent-in-upstream-snapshot":
        if bundled["path"] is not None or bundled["sha256"] is not None:
            raise ContractError("absent bundled license text asserts a file")
    elif bundled["status"] == "present-in-upstream-snapshot":
        path = _safe_relative_path(bundled["path"], "legal evidence bundled text path")
        require_sha256(bundled["sha256"], "legal evidence bundled text digest")
        candidate = model_dir / path
        if not candidate.is_file() or sha256_file(candidate) != bundled["sha256"]:
            raise ContractError("legal evidence bundled text bytes differ")
    else:
        raise ContractError("legal evidence bundled text status is invalid")
    policy = require_fields(
        document["policy"],
        {
            "license_text_required_for_product_claims",
            "product_claims_allowed",
            "determination",
        },
        "legal evidence policy",
    )
    for name in ("license_text_required_for_product_claims", "product_claims_allowed"):
        if not isinstance(policy[name], bool):
            raise ContractError(f"legal evidence policy {name} is invalid")
    require_string(policy["determination"], "legal evidence policy determination")
    if (
        policy["license_text_required_for_product_claims"]
        and bundled["status"] != "present-in-upstream-snapshot"
        and policy["product_claims_allowed"]
    ):
        raise ContractError("product claims require bundled license text under the retained policy")
    if policy["product_claims_allowed"]:
        raise ContractError("operator legal review cannot unlock product claims")
    review = require_fields(
        document["review"], {"reviewer", "reviewed_at_utc", "notes"}, "legal review"
    )
    require_string(review["reviewer"], "legal reviewer")
    require_string(review["notes"], "legal review notes")
    if re.fullmatch(
        r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z",
        require_string(review["reviewed_at_utc"], "legal review time"),
    ) is None:
        raise ContractError("legal review time is not canonical UTC")
    return {
        "expected_declared_spdx": "Apache-2.0",
        "declared_spdx": "Apache-2.0",
        "declaration_source": declaration["source"],
        "bundled_license_text": bundled,
        "legal_evidence_sha256": sha256_file(legal_evidence_path),
        "product_claims_allowed": policy["product_claims_allowed"],
    }


def verify_snapshot(
    document: dict[str, Any],
    model_dir: Path,
    acquisition_record_path: Path,
    legal_evidence_path: Path,
) -> None:
    validate_manifest(document, require_verified=True)
    observed = {path.relative_to(model_dir).as_posix(): path for path in snapshot_files(model_dir)}
    expected = {item["path"]: item for item in document["files"]}
    if set(observed) != set(expected):
        missing = sorted(set(expected) - set(observed))
        extra = sorted(set(observed) - set(expected))
        raise ContractError(f"model snapshot inventory differs; missing={missing}, extra={extra}")
    for relative, item in expected.items():
        path = observed[relative]
        if path.stat().st_size != item["size_bytes"]:
            raise ContractError(f"model snapshot size differs: {relative}")
        if sha256_file(path) != item["sha256"]:
            raise ContractError(f"model snapshot digest differs: {relative}")

    for relative in ("config.json", "tokenizer_config.json"):
        if relative not in observed:
            if relative == "tokenizer_config.json":
                raise ContractError("model snapshot lacks tokenizer_config.json")
            continue
        config = load_json(observed[relative])
        if config.get("auto_map") not in (None, {}):
            raise ContractError(f"{relative} requests remote/custom code")
        if relative == "config.json" and (
            config.get("model_type") != "qwen3" or config.get("hidden_size") != 1024
        ):
            raise ContractError("config.json model_type or native dimension differs")

    indexes = [item for item in document["files"] if item["role"] == "weight-index"]
    for index in indexes:
        value = load_json(model_dir / index["path"])
        weight_map = value.get("weight_map")
        if not isinstance(weight_map, dict) or not weight_map:
            raise ContractError(f"weight index {index['path']} has no weight_map")
        referenced = set(weight_map.values())
        if any(not isinstance(path, str) for path in referenced):
            raise ContractError(f"weight index {index['path']} has invalid paths")
        manifested_weights = {item["path"] for item in document["files"] if item["role"] == "weights"}
        if not referenced.issubset(manifested_weights):
            raise ContractError(f"weight index {index['path']} references unmanifested weights")

    acquisition = acquisition_snapshot_provenance(
        model_dir,
        document["model"]["revision"],
        acquisition_record_path,
    )
    if acquisition != document["acquisition_provenance"]:
        raise ContractError("model manifest differs from retained acquisition evidence")
    license_identity = validate_legal_evidence(
        legal_evidence_path, acquisition, model_dir
    )
    if license_identity != document["license"]:
        raise ContractError("model manifest differs from retained legal evidence")


def validate_plan(document: dict[str, Any]) -> dict[str, Any]:
    require_fields(
        document,
        {
            "$comment",
            "schema",
            "name",
            "model_repository",
            "dimensions",
            "canonical_output",
            "instruction",
            "tokenizer",
            "chunking",
            "dataset",
            "measurement",
            "lanes",
            "projection_vector_counts",
            "claims",
            "closure_declared",
        },
        "benchmark plan",
    )
    if (
        document["$comment"] != SPDX
        or document["schema"] != PLAN_SCHEMA
        or document["name"] != "qwen3-embedding-0.6b-multilingual-v1"
        or document["model_repository"] != MODEL_REPOSITORY
        or document["dimensions"] != DIMENSIONS
        or document["projection_vector_counts"] != PROJECTION_VECTOR_COUNTS
        or document["claims"] != []
        or document["closure_declared"] is not False
    ):
        raise ContractError("benchmark plan identity or non-claim state differs")
    canonical = require_fields(
        document["canonical_output"],
        {"dtype", "byte_order", "pooling", "normalization", "normalization_epsilon"},
        "canonical output",
    )
    if canonical != {
        "dtype": "float32",
        "byte_order": "little-endian",
        "pooling": "last-token",
        "normalization": "l2-after-dimension-truncation",
        "normalization_epsilon": "1e-12",
    }:
        raise ContractError("canonical FP32 output policy differs")
    instruction = require_fields(
        document["instruction"],
        {"task", "query_template", "passage_template"},
        "instruction identity",
    )
    for key in instruction:
        require_string(instruction[key], f"instruction {key}")
    if instruction != FROZEN_INSTRUCTION:
        raise ContractError("instruction task or templates differ from the frozen v1 identity")
    tokenizer = require_fields(
        document["tokenizer"],
        {
            "padding_side",
            "truncation_side",
            "truncation",
            "max_length",
            "add_special_tokens",
        },
        "tokenizer policy",
    )
    if tokenizer != FROZEN_TOKENIZER:
        raise ContractError("tokenizer policy differs from the frozen v1 identity")
    chunking = require_fields(
        document["chunking"],
        {"unit", "strategy", "max_tokens", "overlap_tokens"},
        "chunking policy",
    )
    if chunking != FROZEN_CHUNKING:
        raise ContractError("chunking token policy differs from the frozen v1 identity")
    dataset = require_fields(
        document["dataset"],
        {"repetitions", "max_source_records", "max_expanded_records"},
        "dataset policy",
    )
    for key, value in dataset.items():
        require_integer(value, f"dataset {key}", minimum=1)
    if dataset != FROZEN_DATASET:
        raise ContractError("dataset repetition or record policy differs from the frozen v1 identity")
    measurement = require_fields(
        document["measurement"],
        {"warmup_samples", "measured_samples", "subprocess_timeout_seconds", "scope"},
        "measurement policy",
    )
    require_integer(measurement["warmup_samples"], "warmup samples", minimum=1)
    require_integer(measurement["measured_samples"], "measured samples", minimum=3)
    require_integer(measurement["subprocess_timeout_seconds"], "subprocess timeout", minimum=60)
    if measurement != FROZEN_MEASUREMENT:
        raise ContractError("measurement samples, warmups, timeout, or scope differ from frozen v1")
    lanes = document["lanes"]
    if not isinstance(lanes, list) or len(lanes) != len(LANES):
        raise ContractError("benchmark plan must contain the three precision lanes")
    identities: list[tuple[str, str]] = []
    for index, lane in enumerate(lanes):
        lane = require_fields(
            lane,
            {"name", "inference_dtype", "batch_token_budget", "max_batch_size"},
            f"lane {index}",
        )
        identity = (lane["name"], lane["inference_dtype"])
        identities.append(identity)
        require_integer(lane["batch_token_budget"], f"lane {index} token budget", minimum=1)
        require_integer(lane["max_batch_size"], f"lane {index} batch size", minimum=1)
    if identities != LANES or lanes != FROZEN_LANES:
        raise ContractError("precision lane identity or token/batch limits differ from frozen v1")
    return document


def load_plan(path: Path) -> dict[str, Any]:
    if sha256_file(path) != PLAN_SHA256:
        raise ContractError("benchmark plan bytes differ from the frozen v1 digest")
    return validate_plan(load_json(path))


def load_corpus(path: Path, plan: dict[str, Any]) -> tuple[dict[str, Any], list[dict[str, str]]]:
    if sha256_file(path) != CORPUS_SHA256:
        raise ContractError("benchmark corpus bytes differ from the frozen v1 digest")
    document = load_json(path)
    require_fields(document, {"$comment", "schema", "name", "records"}, "benchmark corpus")
    if (
        document["$comment"] != SPDX
        or document["schema"] != CORPUS_SCHEMA
        or document["name"] != "multilingual-throughput-smoke-v1"
    ):
        raise ContractError("benchmark corpus identity differs")
    records = document["records"]
    if not isinstance(records, list) or len(records) != CORPUS_RECORD_COUNT:
        raise ContractError("benchmark corpus record count differs from the frozen v1 identity")
    if len(records) > plan["dataset"]["max_source_records"]:
        raise ContractError("benchmark corpus exceeds source-record bound")
    validated: list[dict[str, str]] = []
    seen: set[str] = set()
    for index, record in enumerate(records):
        record = require_fields(record, {"id", "language", "kind", "text"}, f"corpus record {index}")
        identifier = require_string(record["id"], f"corpus record {index} id")
        language = require_string(record["language"], f"corpus record {index} language")
        text = require_string(record["text"], f"corpus record {index} text")
        if IDENTIFIER.fullmatch(identifier) is None or identifier in seen:
            raise ContractError(f"corpus record {index} id is invalid or duplicated")
        if LANGUAGE.fullmatch(language) is None:
            raise ContractError(f"corpus record {index} language is invalid")
        if record["kind"] not in {"passage", "query"}:
            raise ContractError(f"corpus record {index} kind is invalid")
        if len(text.encode("utf-8")) > 4096:
            raise ContractError(f"corpus record {index} text exceeds 4096 UTF-8 bytes")
        seen.add(identifier)
        validated.append(record)
    expanded = len(validated) * plan["dataset"]["repetitions"]
    if expanded > plan["dataset"]["max_expanded_records"]:
        raise ContractError("expanded corpus exceeds plan bound")
    return document, validated


def expanded_records(records: list[dict[str, str]], plan: dict[str, Any]) -> list[dict[str, str]]:
    output: list[dict[str, str]] = []
    instruction = plan["instruction"]
    for repetition in range(plan["dataset"]["repetitions"]):
        for record in records:
            if record["kind"] == "query":
                text = instruction["query_template"].format(
                    task=instruction["task"], text=record["text"]
                )
            else:
                text = instruction["passage_template"].format(text=record["text"])
            output.append(
                {
                    "id": f"{record['id']}#r{repetition:04d}",
                    "language": record["language"],
                    "kind": record["kind"],
                    "text": text,
                }
            )
    return output


def snapshot_role(relative: str) -> str:
    name = Path(relative).name
    if relative.endswith(".safetensors.index.json"):
        return "weight-index"
    if relative.endswith(".safetensors"):
        return "weights"
    if relative == "config.json":
        return "model-config"
    if name.lower().startswith("license"):
        return "license-text"
    if (
        "tokenizer" in name.lower()
        or name in {"added_tokens.json", "merges.txt", "special_tokens_map.json", "vocab.json"}
    ):
        return "tokenizer"
    if relative.lower().endswith(".md"):
        return "documentation"
    return "metadata"


def ceil_div(numerator: int, denominator: int) -> int:
    if denominator <= 0:
        raise ContractError("projection denominator must be positive")
    return (numerator + denominator - 1) // denominator


def seconds_from_nanos(value: int) -> str:
    require_integer(value, "nanoseconds", minimum=0)
    return f"{value // 1_000_000_000}.{value % 1_000_000_000:09d}"


def documents_per_second(documents: int, elapsed_nanos: int) -> str:
    require_integer(documents, "throughput documents", minimum=1)
    require_integer(elapsed_nanos, "throughput elapsed nanoseconds", minimum=1)
    scaled = (documents * 1_000_000_000_000_000 + elapsed_nanos // 2) // elapsed_nanos
    return f"{scaled // 1_000_000}.{scaled % 1_000_000:06d}"


def projection_values(documents: int, elapsed_nanos: int) -> list[dict[str, Any]]:
    values = []
    for count in PROJECTION_VECTOR_COUNTS:
        projected = ceil_div(count * elapsed_nanos, documents)
        values.append(
            {
                "vectors": count,
                "idealized_elapsed_nanos": projected,
                "idealized_elapsed_seconds": seconds_from_nanos(projected),
            }
        )
    return values


def language_counts(records: list[dict[str, str]], repetitions: int) -> dict[str, int]:
    counts = Counter(record["language"] for record in records)
    return {language: counts[language] * repetitions for language in sorted(counts)}


def ensure_absolute_executable(path: Path, label: str) -> Path:
    if not path.is_absolute() or not path.is_file() or not path.resolve().is_file():
        raise ContractError(f"{label} must resolve from an absolute path to a regular file")
    if not os.access(path, os.X_OK):
        raise ContractError(f"{label} is not executable")
    return path


def finite_number(value: Any, label: str, *, minimum: float | None = None) -> float:
    if not isinstance(value, (int, float)) or isinstance(value, bool) or not math.isfinite(value):
        raise ContractError(f"{label} must be finite")
    converted = float(value)
    if minimum is not None and converted < minimum:
        raise ContractError(f"{label} must be >= {minimum}")
    return converted
