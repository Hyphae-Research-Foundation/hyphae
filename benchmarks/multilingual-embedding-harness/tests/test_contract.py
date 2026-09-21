#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

from __future__ import annotations

import copy
import hashlib
import json
import os
import stat
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path
from unittest import mock

HARNESS = Path(__file__).resolve().parents[1]
if str(HARNESS) not in sys.path:
    sys.path.insert(0, str(HARNESS))

from check_receipt import (  # noqa: E402
    ARGV_CONTRACT,
    HARNESS_SOURCE_FILES,
    OFFLINE_ENVIRONMENT,
    PRECISION_CONTROLS,
    finalize_receipt,
    validate_receipt,
)
from acquire_evidence import acquire  # noqa: E402
from contract import (  # noqa: E402
    ACQUISITION_EVIDENCE_KIND,
    ACQUISITION_SCHEMA,
    ARTIFACT_CAPTURE_SCOPE,
    ARTIFACT_CAPTURE_PHASES,
    ARTIFACT_INVENTORY_ALGORITHM,
    ContractError,
    CONTAINMENT_PROFILE,
    CORPUS_RECORD_COUNT,
    DIMENSIONS,
    LEGAL_EVIDENCE_SCHEMA,
    MODEL_API_PURPOSE,
    MODEL_CARD_PURPOSE,
    MODEL_REPOSITORY,
    PROVISIONAL_SCHEMA,
    RECEIPT_SCHEMA,
    SPDX,
    TREE_API_PURPOSE,
    acquisition_snapshot_provenance,
    artifact_inventory_digest,
    artifact_record,
    distribution_inventory_digest,
    documents_per_second,
    environment_artifacts_digest,
    language_counts,
    load_corpus,
    load_json,
    load_json_value,
    load_plan,
    projection_values,
    sha256_file,
    validate_manifest,
    validate_plan,
    verify_snapshot,
)
from populate_manifest import populate  # noqa: E402
from reference_subject import _process_gpu_uuid  # noqa: E402
import staging as staging_module  # noqa: E402
from run import (  # noqa: E402
    MANDATORY_UNSET_ENVIRONMENT,
    SYSTEM_LIBRARY_EXEC_PATHS,
    _containment_properties,
    _containment_tools,
    _contained_runner_command,
    _manager_environment_names,
    _offline_environment,
    _parse_manager_environment_names,
    _run_staged_subject,
    _run_contained,
    _terminate_and_verify,
    _unset_manager_environment,
    _validate_effective_containment,
    _verify_cgroup_empty,
    _verify_contained_privilege_contract,
    _verify_first_stage_environment,
    _verify_user_manager_isolation,
)
from staging import (  # noqa: E402
    MAX_PYTHON_SYMLINK_BYTES,
    MAX_PYTHON_SYMLINKS,
    _copy_python_source,
    _prepare_python_source,
    _resolve_python_path,
    build_stage,
    destroy_stage,
    paths_from_root,
    validate_stage,
    validate_stage_document,
)

TEMPLATE = HARNESS / "manifests/qwen3-embedding-0.6b.template.json"
PLAN = HARNESS / "plans/qwen3-multilingual-v1.json"
CORPUS = HARNESS / "corpora/multilingual-throughput-smoke-v1.json"
RESULT_PLACEHOLDER = HARNESS / "results/qwen3-embedding-0.6b-rtx.unverified.json"
REVISION = "a" * 40


def create_snapshot(root: Path, weights: bytes = b"test-only-not-model-weights") -> None:
    (root / "README.md").write_text("Test-only model card fixture.\n", encoding="utf-8")
    (root / "config.json").write_text(
        '{"hidden_size":1024,"model_type":"qwen3"}\n', encoding="utf-8"
    )
    (root / "model.safetensors").write_bytes(weights)
    (root / "tokenizer.json").write_text("{}\n", encoding="utf-8")
    (root / "tokenizer_config.json").write_text("{}\n", encoding="utf-8")


def git_blob_oid(path: Path) -> str:
    body = path.read_bytes()
    digest = hashlib.sha1(usedforsecurity=False)
    digest.update(f"blob {len(body)}\0".encode("ascii"))
    digest.update(body)
    return digest.hexdigest()


def write_json(path: Path, value: object) -> None:
    path.write_text(json.dumps(value, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")


def create_acquisition_evidence(
    root: Path,
    model_dir: Path,
    revision: str = REVISION,
    *,
    lfs_weights: bool = False,
) -> tuple[Path, Path]:
    root.mkdir()
    (root / "acquisition-tool.py").write_bytes(
        (HARNESS / "acquire_evidence.py").read_bytes()
    )
    model_body = {
        "id": MODEL_REPOSITORY,
        "sha": revision,
        "cardData": {"license": "apache-2.0"},
    }
    tree_body = []
    for path in sorted(model_dir.rglob("*")):
        if not path.is_file():
            continue
        relative = path.relative_to(model_dir).as_posix()
        entry = {
            "type": "file",
            "path": relative,
            "oid": git_blob_oid(path),
            "size": path.stat().st_size,
        }
        if lfs_weights and relative == "model.safetensors":
            entry["oid"] = "b" * 40
            entry["lfs"] = {
                "oid": sha256_file(path),
                "size": path.stat().st_size,
                "pointerSize": 128,
            }
        tree_body.append(entry)
    model_path = root / "model-api.json"
    model_card_path = root / "model-card.md"
    tree_path = root / "tree-api-0001.json"
    write_json(model_path, model_body)
    model_card_path.write_bytes((model_dir / "README.md").read_bytes())
    write_json(tree_path, tree_body)
    model_url = (
        f"https://huggingface.co/api/models/{MODEL_REPOSITORY}/revision/{revision}"
        "?expand=cardData&expand=sha"
    )
    tree_url = (
        f"https://huggingface.co/api/models/{MODEL_REPOSITORY}/tree/{revision}"
        "?recursive=true&expand=true"
    )
    model_card_url = (
        f"https://huggingface.co/{MODEL_REPOSITORY}/raw/{revision}/README.md"
    )

    def response(purpose: str, url: str, path: Path) -> dict:
        return {
            "purpose": purpose,
            "request_url": url,
            "response_url": url,
            "http_status": 200,
            "response_headers": {
                "content-type": "application/json; charset=utf-8",
                "etag": f'"{sha256_file(path)}"',
            },
            "body_path": path.name,
            "body_size_bytes": path.stat().st_size,
            "body_sha256": sha256_file(path),
        }

    acquisition = {
        "$comment": SPDX,
        "schema": ACQUISITION_SCHEMA,
        "status": "captured",
        "evidence_kind": ACQUISITION_EVIDENCE_KIND,
        "repository": MODEL_REPOSITORY,
        "revision": revision,
        "captured_at_utc": "2026-09-18T00:00:00Z",
        "capture": {
            "tool": "acquire_evidence.py",
            "tool_path": "acquisition-tool.py",
            "tool_sha256": sha256_file(HARNESS / "acquire_evidence.py"),
            "https_validation": "python-default-context-hostname-and-certificate",
            "authentication": "public-read-no-credentials",
        },
        "responses": [
            response(MODEL_API_PURPOSE, model_url, model_path),
            response(MODEL_CARD_PURPOSE, model_card_url, model_card_path),
            response(TREE_API_PURPOSE, tree_url, tree_path),
        ],
    }
    acquisition_path = root / "acquisition.json"
    write_json(acquisition_path, acquisition)
    legal = {
        "$comment": SPDX,
        "schema": LEGAL_EVIDENCE_SCHEMA,
        "status": "reviewed",
        "repository": MODEL_REPOSITORY,
        "revision": revision,
        "acquisition_record_sha256": sha256_file(acquisition_path),
        "declared_license": {
            "source": "hugging-face-model-api.cardData.license",
            "upstream_value": "apache-2.0",
            "spdx": "Apache-2.0",
            "model_metadata_body_sha256": sha256_file(model_path),
        },
        "model_card": {
            "path": "README.md",
            "source_url": model_card_url,
            "body_sha256": sha256_file(model_card_path),
        },
        "bundled_license_text": {
            "status": "absent-in-upstream-snapshot",
            "path": None,
            "sha256": None,
        },
        "policy": {
            "license_text_required_for_product_claims": True,
            "product_claims_allowed": False,
            "determination": "Measurement is allowed; product claims remain blocked without text.",
        },
        "review": {
            "reviewer": "test-only legal fixture",
            "reviewed_at_utc": "2026-09-18T00:01:00Z",
            "notes": "Tests declaration evidence separately from bundled text.",
        },
    }
    legal_path = root / "legal-evidence.json"
    write_json(legal_path, legal)
    return acquisition_path, legal_path


def create_environment_artifacts(root: Path) -> tuple[dict, list[Path]]:
    paths = []
    distributions = []
    for name in ("safetensors", "tokenizers", "torch", "transformers"):
        path = root / f"{name}.artifact"
        path.write_text(f"test-only {name}\n", encoding="utf-8")
        paths.append(path)
        files = [artifact_record(path, f"distribution:{name}:{name}.artifact")]
        distributions.append(
            {
                "name": name,
                "version": "test",
                "files": files,
                "files_sha256": artifact_inventory_digest(
                    files, f"installed distribution {name}"
                ),
            }
        )
    module = root / "loaded-module.py"
    native = root / "loaded-native.so"
    module.write_text("# test-only module\n", encoding="utf-8")
    native.write_bytes(b"test-only native library")
    paths.extend((module, native))
    modules = [artifact_record(module, "module:test_fixture")]
    libraries = [artifact_record(native, "native:python-prefix:loaded-native.so")]
    phases = ARTIFACT_CAPTURE_PHASES
    value = {
        "algorithm": ARTIFACT_INVENTORY_ALGORITHM,
        "capture_scope": ARTIFACT_CAPTURE_SCOPE,
        "capture_phases": phases,
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
        phases,
        value["installed_distributions_sha256"],
        value["loaded_python_modules_sha256"],
        value["loaded_native_libraries_sha256"],
    )
    return value, paths


def create_receipt(
    manifest: dict,
    manifest_path: Path,
    plan: dict,
    records: list[dict[str, str]],
    acquisition_path: Path,
    legal_path: Path,
    environment_artifacts: dict,
    execution_stage: dict,
) -> dict:
    repetitions = plan["dataset"]["repetitions"]
    documents = len(records) * repetitions
    batches = 24
    non_padding_tokens = 12_000
    padded_tokens = 14_000
    tokenizer_files = [
        {"path": item["path"], "sha256": item["sha256"]}
        for item in manifest["files"]
        if item["role"] == "tokenizer"
    ]
    lane_profiles = []
    cells = []
    for lane in plan["lanes"]:
        profile = {
            "lane": lane["name"],
            "inference_dtype": lane["inference_dtype"],
            "model_class": "transformers.Qwen3Model",
            "parameter_dtype": f"torch.{lane['inference_dtype']}",
            "model_load_elapsed_nanos": 1_000_000,
            "tokenization_elapsed_nanos": 100_000,
            "documents": documents,
            "batches": batches,
            "non_padding_tokens": non_padding_tokens,
            "padded_tokens": padded_tokens,
            "max_sequence_tokens": 96,
            "batch_token_budget": lane["batch_token_budget"],
            "max_batch_size": lane["max_batch_size"],
        }
        lane_profiles.append(profile)
        for dimension in DIMENSIONS:
            samples = [
                {
                    "sample": index,
                    "host_elapsed_nanos": 2_000_000 + index,
                    "gpu_elapsed_nanos": 1_500_000 + index,
                    "documents": documents,
                    "batches": batches,
                    "non_padding_tokens": non_padding_tokens,
                    "padded_tokens": padded_tokens,
                    "peak_allocated_bytes": 3_000_000,
                    "output_sha256": f"{dimension:064x}",
                }
                for index in range(plan["measurement"]["measured_samples"])
            ]
            total_documents = documents * len(samples)
            total_host = sum(sample["host_elapsed_nanos"] for sample in samples)
            throughput = {
                "basis": "sum-of-raw-host-timing-samples",
                "total_documents": total_documents,
                "total_host_elapsed_nanos": total_host,
                "documents_per_second": documents_per_second(total_documents, total_host),
            }
            cells.append(
                {
                    "lane": lane["name"],
                    "inference_dtype": lane["inference_dtype"],
                    "dimension": dimension,
                    "canonical_output_dtype": "float32-le",
                    "measurement_scope": plan["measurement"]["scope"],
                    "warmup_samples": plan["measurement"]["warmup_samples"],
                    "raw_timing_samples": samples,
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
            )
    return {
        "schema": RECEIPT_SCHEMA,
        "status": "operator-attested-non-authoritative-measurement",
        "source": {
            "harness_files_sha256": {
                name: sha256_file(HARNESS / name) for name in HARNESS_SOURCE_FILES
            },
            "manifest_sha256": sha256_file(manifest_path),
            "plan_sha256": sha256_file(PLAN),
            "corpus_sha256": sha256_file(CORPUS),
            "acquisition_record_sha256": sha256_file(acquisition_path),
            "legal_evidence_sha256": sha256_file(legal_path),
            "execution_stage": execution_stage,
        },
        "model": {
            "repository": manifest["model"]["repository"],
            "revision": manifest["model"]["revision"],
            "manifest_status": "verified",
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
            "source_records": len(records),
            "repetitions": repetitions,
            "expanded_records": documents,
            "language_counts": language_counts(records, repetitions),
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
                "gpu_index": 0,
                "inherited_environment_names": [],
            },
            "software": {
                "python_version": "3.12.1",
                "python_implementation": "CPython",
                "python_executable_sha256": next(
                    item["sha256"]
                    for item in execution_stage["files"]
                    if item["identity"] == "executables/python"
                ),
                "platform": "Linux-test",
                "torch_version": "test",
                "transformers_version": "test",
                "tokenizers_version": "test",
                "safetensors_version": "test",
                "cuda_runtime_version": "test",
                "cudnn_version": "test",
                "nvidia_smi_sha256": next(
                    item["sha256"]
                    for item in execution_stage["files"]
                    if item["identity"] == "executables/nvidia-smi"
                ),
            },
            "environment_artifacts": environment_artifacts,
            "containment": CONTAINMENT_PROFILE,
            "gpu": {
                "nvidia_smi": {
                    "index": 3,
                    "name": "NVIDIA RTX test fixture",
                    "uuid": "GPU-test-bound-uuid",
                    "pci_bus_id": "00000000:41:00.0",
                    "driver_version": "test",
                    "memory_total_mib": 24_576,
                    "compute_capability": "8.9",
                    "power_limit_watts": "300.00",
                    "clocks_max_sm_mhz": "2500",
                    "clocks_max_memory_mhz": "10501",
                },
                "torch": {
                    "logical_index": 0,
                    "name": "NVIDIA RTX test fixture",
                    "total_memory_bytes": 24_000_000_000,
                    "compute_capability": "8.9",
                    "multiprocessor_count": 76,
                    "bound_nvidia_uuid": "GPU-test-bound-uuid",
                    "bound_pci_bus_id": "00000000:41:00.0",
                },
            },
            "precision_controls": PRECISION_CONTROLS,
            "tokenizer_runtime": {
                "class": "transformers.Qwen2TokenizerFast",
                "is_fast": True,
                "vocab_size": 151_643,
                "model_max_length": 32_768,
                "truncation_side": "right",
            },
            "gpu_observations": {
                "before": {
                    "pstate": "P8",
                    "temperature_celsius": "31",
                    "power_draw_watts": "20.0",
                    "clocks_sm_mhz": "210",
                    "clocks_memory_mhz": "405",
                },
                "after": {
                    "pstate": "P2",
                    "temperature_celsius": "60",
                    "power_draw_watts": "180.0",
                    "clocks_sm_mhz": "2400",
                    "clocks_memory_mhz": "10501",
                },
            },
        },
        "lane_profiles": lane_profiles,
        "cells": cells,
        "trust": {
            "authenticity": "operator-asserted-no-independent-anchor",
            "containment": "systemd-user-service-cgroup-v2-required",
            "claim_unlock": "forbidden",
            "operator_assertions": {
                "acquisition": "retained-response-bytes-no-authenticity-anchor",
                "legal_review": "checksummed-review-no-approved-signature-anchor",
                "gpu_identity": "nvidia-smi-observation-no-hardware-attestation-anchor",
            },
        },
        "claims": [],
        "closure_declared": False,
    }


class EmbeddingHarnessContractTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.model_dir = self.root / "model"
        self.model_dir.mkdir()
        create_snapshot(self.model_dir)
        self.acquisition_path, self.legal_path = create_acquisition_evidence(
            self.root / "evidence", self.model_dir
        )
        self.manifest = populate(
            TEMPLATE,
            self.model_dir,
            REVISION,
            self.acquisition_path,
            self.legal_path,
        )
        self.manifest_path = self.root / "manifest.json"
        write_json(self.manifest_path, self.manifest)
        self.plan = load_plan(PLAN)
        _, self.records = load_corpus(CORPUS, self.plan)
        environment_dir = self.root / "environment"
        environment_dir.mkdir()
        self.environment_artifacts, self.environment_paths = create_environment_artifacts(
            environment_dir
        )
        self.python_executable = Path(sys.executable).resolve()
        self.stage = build_stage(
            self.root / "stage",
            harness_dir=HARNESS,
            model_dir=self.model_dir,
            manifest_path=self.manifest_path,
            plan_path=PLAN,
            corpus_path=CORPUS,
            acquisition_record_path=self.acquisition_path,
            legal_evidence_path=self.legal_path,
            python_executable=self.python_executable,
            nvidia_smi=Path("/bin/true").resolve(),
        )
        self.execution_stage = validate_stage(self.stage)
        self.receipt = create_receipt(
            self.manifest,
            self.manifest_path,
            self.plan,
            self.records,
            self.acquisition_path,
            self.legal_path,
            self.environment_artifacts,
            self.execution_stage,
        )

    def tearDown(self) -> None:
        destroy_stage(self.stage)
        self.temporary.cleanup()

    def validate(self, receipt: dict | None = None) -> dict:
        return validate_receipt(
            self.receipt if receipt is None else receipt,
            self.manifest,
            self.plan,
            self.records,
            harness_dir=HARNESS,
            manifest_path=self.manifest_path,
            plan_path=PLAN,
            corpus_path=CORPUS,
            acquisition_record_path=self.acquisition_path,
            legal_evidence_path=self.legal_path,
            expected_stage=self.execution_stage,
        )

    def test_unpopulated_template_fails_closed(self) -> None:
        with self.assertRaisesRegex(ContractError, "unverified template"):
            validate_manifest(load_json(TEMPLATE), require_verified=True)

    def test_controlled_acquisition_retains_responses_and_review_gate(self) -> None:
        model_body = json.dumps(
            {"id": MODEL_REPOSITORY, "sha": REVISION, "cardData": {"license": "apache-2.0"}}
        ).encode("utf-8")
        tree_body = b"[]"
        model_card_body = b"# test-only model card\n"

        def fetch(url: str, purpose: str, body_path: str) -> tuple[dict, bytes, None]:
            body = {
                MODEL_API_PURPOSE: model_body,
                MODEL_CARD_PURPOSE: model_card_body,
                TREE_API_PURPOSE: tree_body,
            }[purpose]
            return (
                {
                    "purpose": purpose,
                    "request_url": url,
                    "response_url": url,
                    "http_status": 200,
                    "response_headers": {"content-type": "application/json"},
                    "body_path": body_path,
                    "body_size_bytes": len(body),
                    "body_sha256": hashlib.sha256(body).hexdigest(),
                },
                body,
                None,
            )

        output = self.root / "captured"
        with mock.patch("acquire_evidence._fetch", side_effect=fetch):
            acquire(REVISION, output)
        acquisition = load_json(output / "acquisition.json")
        legal = load_json(output / "legal-evidence.review-required.json")
        self.assertEqual(acquisition["revision"], REVISION)
        self.assertEqual(len(acquisition["responses"]), 3)
        self.assertEqual(legal["status"], "review-required")
        self.assertFalse(legal["policy"]["product_claims_allowed"])

    def test_populated_manifest_accepts_declared_license_without_license_file(self) -> None:
        self.assertFalse((self.model_dir / "LICENSE").exists())
        self.assertEqual(self.manifest["license"]["declared_spdx"], "Apache-2.0")
        self.assertEqual(
            self.manifest["license"]["bundled_license_text"]["status"],
            "absent-in-upstream-snapshot",
        )
        self.assertFalse(self.manifest["license"]["product_claims_allowed"])
        verify_snapshot(
            self.manifest, self.model_dir, self.acquisition_path, self.legal_path
        )

    def test_legal_policy_blocks_claims_when_text_is_required(self) -> None:
        legal = load_json(self.legal_path)
        legal["policy"]["product_claims_allowed"] = True
        write_json(self.legal_path, legal)
        with self.assertRaisesRegex(ContractError, "require bundled license text"):
            populate(
                TEMPLATE,
                self.model_dir,
                REVISION,
                self.acquisition_path,
                self.legal_path,
            )

    def test_operator_legal_assertion_cannot_unlock_claims(self) -> None:
        legal = load_json(self.legal_path)
        legal["policy"]["license_text_required_for_product_claims"] = False
        legal["policy"]["product_claims_allowed"] = True
        write_json(self.legal_path, legal)
        with self.assertRaisesRegex(ContractError, "cannot unlock product claims"):
            populate(
                TEMPLATE,
                self.model_dir,
                REVISION,
                self.acquisition_path,
                self.legal_path,
            )

    def test_unreviewed_legal_evidence_fails_closed(self) -> None:
        legal = load_json(self.legal_path)
        legal["status"] = "review-required"
        write_json(self.legal_path, legal)
        with self.assertRaisesRegex(ContractError, "review status"):
            populate(
                TEMPLATE,
                self.model_dir,
                REVISION,
                self.acquisition_path,
                self.legal_path,
            )

    def test_false_revision_acquisition_fails_closed(self) -> None:
        with self.assertRaisesRegex(ContractError, "identity or revision"):
            populate(
                TEMPLATE,
                self.model_dir,
                "b" * 40,
                self.acquisition_path,
                self.legal_path,
            )

    def test_mutable_or_false_source_url_fails_closed(self) -> None:
        acquisition = load_json(self.acquisition_path)
        acquisition["responses"][0]["request_url"] = (
            f"https://huggingface.co/api/models/{MODEL_REPOSITORY}"
        )
        acquisition["responses"][0]["response_url"] = acquisition["responses"][0][
            "request_url"
        ]
        write_json(self.acquisition_path, acquisition)
        with self.assertRaisesRegex(ContractError, "commit-specific endpoint"):
            acquisition_snapshot_provenance(
                self.model_dir, REVISION, self.acquisition_path
            )

    def test_tree_acquisition_requires_recursive_true(self) -> None:
        acquisition = load_json(self.acquisition_path)
        tree = next(
            item
            for item in acquisition["responses"]
            if item["purpose"] == TREE_API_PURPOSE
        )
        tree["request_url"] = tree["request_url"].split("?", 1)[0]
        tree["response_url"] = tree["request_url"]
        write_json(self.acquisition_path, acquisition)
        with self.assertRaisesRegex(ContractError, "recursive=true"):
            acquisition_snapshot_provenance(
                self.model_dir, REVISION, self.acquisition_path
            )

    def test_nested_tree_evidence_is_exhaustive(self) -> None:
        model_dir = self.root / "nested-model"
        model_dir.mkdir()
        create_snapshot(model_dir)
        nested = model_dir / "1_Pooling"
        nested.mkdir()
        (nested / "config.json").write_text('{"pooling_mode_lasttoken":true}\n')
        acquisition_path, legal_path = create_acquisition_evidence(
            self.root / "nested-evidence", model_dir
        )
        manifest = populate(
            TEMPLATE, model_dir, REVISION, acquisition_path, legal_path
        )
        self.assertIn("1_Pooling/config.json", {item["path"] for item in manifest["files"]})

        acquisition = load_json(acquisition_path)
        tree_response = next(
            item
            for item in acquisition["responses"]
            if item["purpose"] == TREE_API_PURPOSE
        )
        tree_path = acquisition_path.parent / tree_response["body_path"]
        tree = load_json_value(tree_path)
        tree = [item for item in tree if item["path"] != "1_Pooling/config.json"]
        write_json(tree_path, tree)
        tree_response["body_size_bytes"] = tree_path.stat().st_size
        tree_response["body_sha256"] = sha256_file(tree_path)
        write_json(acquisition_path, acquisition)
        with self.assertRaisesRegex(ContractError, "snapshot differs from retained API tree"):
            acquisition_snapshot_provenance(model_dir, REVISION, acquisition_path)

    def test_retained_http_body_mutation_fails_closed(self) -> None:
        (self.acquisition_path.parent / "model-api.json").write_text(
            "{}\n", encoding="utf-8"
        )
        with self.assertRaisesRegex(ContractError, "response bytes differ"):
            acquisition_snapshot_provenance(
                self.model_dir, REVISION, self.acquisition_path
            )

    def test_acquisition_response_count_is_bounded_on_acceptance(self) -> None:
        acquisition = load_json(self.acquisition_path)
        acquisition["responses"].extend(
            copy.deepcopy(acquisition["responses"][-1]) for _ in range(16)
        )
        write_json(self.acquisition_path, acquisition)
        with self.assertRaisesRegex(ContractError, "lacks model, model-card, and tree"):
            acquisition_snapshot_provenance(
                self.model_dir, REVISION, self.acquisition_path
            )

    def test_snapshot_rejects_forged_local_digests_not_in_api_tree(self) -> None:
        weights_path = self.model_dir / "model.safetensors"
        weights_path.write_bytes(b"x" * weights_path.stat().st_size)
        changed = copy.deepcopy(self.manifest)
        weights = next(item for item in changed["files"] if item["path"] == weights_path.name)
        weights["sha256"] = sha256_file(weights_path)
        acquisition_file = next(
            item
            for item in changed["acquisition_provenance"]["files"]
            if item["path"] == weights_path.name
        )
        acquisition_file["sha256"] = weights["sha256"]
        with self.assertRaisesRegex(ContractError, "retained API blob"):
            verify_snapshot(changed, self.model_dir, self.acquisition_path, self.legal_path)

    def test_snapshot_binds_api_lfs_sha256_and_size(self) -> None:
        model_dir = self.root / "lfs-model"
        model_dir.mkdir()
        create_snapshot(model_dir, b"test-only-resolved-lfs-object")
        acquisition_path, legal_path = create_acquisition_evidence(
            self.root / "lfs-evidence", model_dir, lfs_weights=True
        )
        manifest = populate(
            TEMPLATE, model_dir, REVISION, acquisition_path, legal_path
        )
        weights = next(
            item
            for item in manifest["acquisition_provenance"]["files"]
            if item["path"] == "model.safetensors"
        )
        self.assertEqual(weights["storage"], "git-lfs-sha256")
        path = model_dir / "model.safetensors"
        path.write_bytes(b"x" * path.stat().st_size)
        changed = copy.deepcopy(manifest)
        local = next(item for item in changed["files"] if item["path"] == path.name)
        local["sha256"] = sha256_file(path)
        acquisition_file = next(
            item
            for item in changed["acquisition_provenance"]["files"]
            if item["path"] == path.name
        )
        acquisition_file["sha256"] = local["sha256"]
        with self.assertRaisesRegex(ContractError, "retained API LFS object"):
            verify_snapshot(changed, model_dir, acquisition_path, legal_path)

    def test_plan_rejects_material_profile_mutations(self) -> None:
        mutations = (
            (("instruction", "task"), "different task"),
            (("dataset", "repetitions"), 31),
            (("measurement", "measured_samples"), 8),
            (("measurement", "warmup_samples"), 3),
            (("measurement", "subprocess_timeout_seconds"), 14_399),
            (("tokenizer", "truncation_side"), "left"),
            (("tokenizer", "max_length"), 511),
            (("chunking", "max_tokens"), 511),
            (("lanes", 0, "batch_token_budget"), 4095),
            (("lanes", 1, "max_batch_size"), 31),
            (("lanes", 2, "inference_dtype"), "bfloat16"),
        )
        for path, value in mutations:
            with self.subTest(path=path):
                changed = copy.deepcopy(self.plan)
                target = changed
                for part in path[:-1]:
                    target = target[part]
                target[path[-1]] = value
                with self.assertRaises(ContractError):
                    validate_plan(changed)

    def test_plan_and_corpus_raw_identity_are_frozen(self) -> None:
        changed_plan = self.root / "changed-plan.json"
        changed_plan.write_bytes(PLAN.read_bytes() + b"\n")
        with self.assertRaisesRegex(ContractError, "frozen v1 digest"):
            load_plan(changed_plan)
        self.assertEqual(len(self.records), CORPUS_RECORD_COUNT)
        changed_corpus = self.root / "changed-corpus.json"
        changed_corpus.write_bytes(CORPUS.read_bytes() + b"\n")
        with self.assertRaisesRegex(ContractError, "frozen v1 digest"):
            load_corpus(changed_corpus, self.plan)

    def test_accepts_strict_receipt(self) -> None:
        self.assertEqual(self.validate()["cells"], 9)

    def test_only_parent_challenge_can_finalize_provisional_result(self) -> None:
        challenge = "e" * 64
        provisional = {
            "schema": PROVISIONAL_SCHEMA,
            "parent_challenge": challenge,
            "source": self.receipt["source"],
            "model": self.receipt["model"],
            "workload": self.receipt["workload"],
            "execution_profile": self.receipt["execution_profile"],
            "lane_profiles": self.receipt["lane_profiles"],
            "cells": self.receipt["cells"],
        }
        arguments = {
            "harness_dir": HARNESS,
            "manifest_path": self.manifest_path,
            "plan_path": PLAN,
            "corpus_path": CORPUS,
            "acquisition_record_path": self.acquisition_path,
            "legal_evidence_path": self.legal_path,
            "expected_stage": self.execution_stage,
        }
        finalized = finalize_receipt(
            provisional,
            challenge,
            self.manifest,
            self.plan,
            self.records,
            **arguments,
        )
        self.assertEqual(
            finalized["status"], "operator-attested-non-authoritative-measurement"
        )
        self.assertNotIn("parent_challenge", finalized)
        with self.assertRaisesRegex(ContractError, "one-run parent challenge"):
            finalize_receipt(
                provisional,
                "f" * 64,
                self.manifest,
                self.plan,
                self.records,
                **arguments,
            )

    def test_rejects_truncation_side_drift(self) -> None:
        changed = copy.deepcopy(self.receipt)
        changed["execution_profile"]["tokenizer_runtime"]["truncation_side"] = "left"
        with self.assertRaisesRegex(ContractError, "truncation side"):
            self.validate(changed)

    def test_rejects_logical_gpu_uuid_or_pci_mismatch(self) -> None:
        for field, value in (
            ("bound_nvidia_uuid", "GPU-other"),
            ("bound_pci_bus_id", "00000000:42:00.0"),
        ):
            with self.subTest(field=field):
                changed = copy.deepcopy(self.receipt)
                changed["execution_profile"]["gpu"]["torch"][field] = value
                with self.assertRaisesRegex(ContractError, "NVML identity"):
                    self.validate(changed)

    def test_runtime_maps_process_to_exactly_one_gpu_uuid(self) -> None:
        pid = str(os.getpid())
        with mock.patch(
            "reference_subject._nvidia_rows",
            return_value=[["999", "GPU-other"], [pid, "GPU-selected"]],
        ):
            self.assertEqual(_process_gpu_uuid(Path("/unused")), "GPU-selected")
        with mock.patch(
            "reference_subject._nvidia_rows",
            return_value=[[pid, "GPU-one"], [pid, "GPU-two"]],
        ):
            with self.assertRaisesRegex(ContractError, "exactly one NVML GPU UUID"):
                _process_gpu_uuid(Path("/unused"))

    def test_rejects_projection_or_sample_mutation(self) -> None:
        changed = copy.deepcopy(self.receipt)
        changed["cells"][0]["projection"]["values"][0]["idealized_elapsed_nanos"] += 1
        with self.assertRaisesRegex(ContractError, "projection math"):
            self.validate(changed)
        changed = copy.deepcopy(self.receipt)
        changed["cells"][0]["raw_timing_samples"].pop()
        with self.assertRaisesRegex(ContractError, "raw sample count"):
            self.validate(changed)

    def test_rejects_claims(self) -> None:
        changed = copy.deepcopy(self.receipt)
        changed["claims"] = ["one billion vectors in one hour"]
        with self.assertRaisesRegex(ContractError, "non-claim state"):
            self.validate(changed)

    def test_rejects_environment_inventory_mutation(self) -> None:
        changed = copy.deepcopy(self.receipt)
        environment = changed["execution_profile"]["environment_artifacts"]
        environment["installed_distributions"][0]["files"][0]["sha256"] = "0" * 64
        with self.assertRaisesRegex(ContractError, "inventory digest differs"):
            self.validate(changed)

    def test_rejects_environment_capture_phase_omission(self) -> None:
        changed = copy.deepcopy(self.receipt)
        environment = changed["execution_profile"]["environment_artifacts"]
        environment["capture_phases"].pop()
        with self.assertRaisesRegex(ContractError, "capture phases differ"):
            self.validate(changed)

    def test_portable_inventory_does_not_require_original_paths(self) -> None:
        for path in self.environment_paths:
            path.unlink()
        environment = self.receipt["execution_profile"]["environment_artifacts"]
        identities = [
            file["identity"]
            for distribution in environment["installed_distributions"]
            for file in distribution["files"]
        ]
        self.assertTrue(all(not identity.startswith("/") for identity in identities))
        self.assertEqual(self.validate()["status"], "passed")

    def test_staged_closure_rejects_pyc_substitution(self) -> None:
        self.stage.harness.chmod(0o700)
        pyc = self.stage.harness / "reference_subject.pyc"
        pyc.write_bytes(b"test-only forged bytecode")
        pyc.chmod(0o400)
        self.stage.harness.chmod(0o500)
        with self.assertRaisesRegex(ContractError, "bytecode cache state"):
            validate_stage(self.stage)

    def test_stage_rejects_manifest_count_before_model_copy_and_cleans_up(self) -> None:
        changed = copy.deepcopy(self.manifest)
        for index in range(124):
            path = f"zz-test-{index:03d}.json"
            changed["files"].append(
                {"path": path, "role": "metadata", "size_bytes": 1, "sha256": "d" * 64}
            )
            changed["acquisition_provenance"]["files"].append(
                {
                    "path": path,
                    "source_oid": "c" * 40,
                    "storage": "git-blob",
                    "size_bytes": 1,
                    "sha256": "d" * 64,
                }
            )
        manifest_path = self.root / "too-many-files-manifest.json"
        write_json(manifest_path, changed)
        stage_root = self.root / "rejected-count-stage"
        with self.assertRaisesRegex(ContractError, "file count exceeds"):
            build_stage(
                stage_root,
                harness_dir=HARNESS,
                model_dir=self.model_dir,
                manifest_path=manifest_path,
                plan_path=PLAN,
                corpus_path=CORPUS,
                acquisition_record_path=self.acquisition_path,
                legal_evidence_path=self.legal_path,
                python_executable=self.python_executable,
                nvidia_smi=Path("/bin/true").resolve(),
            )
        self.assertFalse(stage_root.exists())

    def test_stage_rejects_manifest_aggregate_size_before_model_copy(self) -> None:
        changed = copy.deepcopy(self.manifest)
        changed["files"][0]["size_bytes"] = 16 * 1024 * 1024 * 1024 + 1
        changed["acquisition_provenance"]["files"][0]["size_bytes"] = changed[
            "files"
        ][0]["size_bytes"]
        manifest_path = self.root / "oversize-manifest.json"
        write_json(manifest_path, changed)
        stage_root = self.root / "rejected-size-stage"
        with self.assertRaisesRegex(ContractError, "aggregate size exceeds"):
            build_stage(
                stage_root,
                harness_dir=HARNESS,
                model_dir=self.model_dir,
                manifest_path=manifest_path,
                plan_path=PLAN,
                corpus_path=CORPUS,
                acquisition_record_path=self.acquisition_path,
                legal_evidence_path=self.legal_path,
                python_executable=self.python_executable,
                nvidia_smi=Path("/bin/true").resolve(),
            )
        self.assertFalse(stage_root.exists())

    def test_stage_rejects_manifest_size_that_lies_about_actual_source(self) -> None:
        changed = copy.deepcopy(self.manifest)
        changed["files"][0]["size_bytes"] += 1
        changed["acquisition_provenance"]["files"][0]["size_bytes"] = changed[
            "files"
        ][0]["size_bytes"]
        manifest_path = self.root / "lying-size-manifest.json"
        write_json(manifest_path, changed)
        stage_root = self.root / "lying-size-stage"
        with self.assertRaisesRegex(ContractError, "size differs from manifest"):
            build_stage(
                stage_root,
                harness_dir=HARNESS,
                model_dir=self.model_dir,
                manifest_path=manifest_path,
                plan_path=PLAN,
                corpus_path=CORPUS,
                acquisition_record_path=self.acquisition_path,
                legal_evidence_path=self.legal_path,
                python_executable=self.python_executable,
                nvidia_smi=Path("/bin/true").resolve(),
            )
        self.assertFalse(stage_root.exists())

    def test_stage_rejects_acquisition_body_lying_size(self) -> None:
        acquisition = load_json(self.acquisition_path)
        acquisition["responses"][0]["body_size_bytes"] += 1
        write_json(self.acquisition_path, acquisition)
        stage_root = self.root / "lying-acquisition-stage"
        with self.assertRaisesRegex(ContractError, "size differs from manifest"):
            build_stage(
                stage_root,
                harness_dir=HARNESS,
                model_dir=self.model_dir,
                manifest_path=self.manifest_path,
                plan_path=PLAN,
                corpus_path=CORPUS,
                acquisition_record_path=self.acquisition_path,
                legal_evidence_path=self.legal_path,
                python_executable=self.python_executable,
                nvidia_smi=Path("/bin/true").resolve(),
            )
        self.assertFalse(stage_root.exists())

    def test_original_path_swap_cannot_change_staged_inputs(self) -> None:
        original = self.model_dir / "model.safetensors"
        moved = self.model_dir / "model.safetensors.original"
        original.rename(moved)
        original.write_bytes(b"path-swapped-untrusted-bytes")
        model_api = self.acquisition_path.parent / "model-api.json"
        model_api.rename(self.acquisition_path.parent / "model-api.original.json")
        model_api.write_text("{}\n", encoding="utf-8")
        verify_snapshot(
            load_json(self.stage.model_manifest),
            self.stage.model,
            self.stage.acquisition_record,
            self.stage.legal_evidence,
        )
        validate_stage(self.stage)

    def test_staged_path_swap_is_detected_before_publication(self) -> None:
        self.stage.model.chmod(0o700)
        weights = self.stage.model / "model.safetensors"
        weights.rename(self.stage.model / "model.safetensors.original")
        weights.write_bytes(b"staged-path-substitution")
        weights.chmod(0o400)
        self.stage.model.chmod(0o500)
        with self.assertRaisesRegex(ContractError, "execution stage file inventory differs"):
            validate_stage(self.stage)

    def test_containment_profile_bounds_resources_and_network(self) -> None:
        properties = _containment_properties(
            Path("/stage"), Path("/receipt"), Path("/stage/volatile")
        )
        self.assertIn("MemoryMax=34359738368", properties)
        self.assertIn("MemorySwapMax=0", properties)
        self.assertIn("TasksMax=64", properties)
        self.assertIn("IPAddressDeny=any", properties)
        self.assertIn("RestrictAddressFamilies=AF_UNIX", properties)
        self.assertIn("SystemCallFilter=~@network-io", properties)
        self.assertIn("NoNewPrivileges=yes", properties)
        self.assertIn("RestrictSUIDSGID=yes", properties)
        self.assertFalse(any(item.startswith("CapabilityBoundingSet=") for item in properties))
        self.assertIn("NoExecPaths=/", properties)
        expected_exec_paths = {
            "/stage",
            "/stage/executables/python",
            "/stage/executables/nvidia-smi",
            *SYSTEM_LIBRARY_EXEC_PATHS,
        }
        self.assertIn(f"ExecPaths={' '.join(sorted(expected_exec_paths))}", properties)
        self.assertIn("ProtectSystem=strict", properties)
        self.assertIn("ProtectHome=read-only", properties)
        self.assertIn("ProtectControlGroups=yes", properties)
        unset_property = next(
            item for item in properties if item.startswith("UnsetEnvironment=")
        )
        self.assertEqual(
            set(unset_property.removeprefix("UnsetEnvironment=").split()),
            MANDATORY_UNSET_ENVIRONMENT,
        )
        self.assertTrue(any(item.startswith("InaccessiblePaths=") for item in properties))
        self.assertTrue(
            any(item.startswith("TemporaryFileSystem=/stage/volatile:rw,size=134217728") for item in properties)
        )

    def test_first_contained_executable_is_staged_python(self) -> None:
        command = _contained_runner_command(
            stage=self.stage,
            stage_identity_sha256=self.execution_stage["identity_sha256"],
            gpu_index=0,
        )
        self.assertEqual(command[0], str(self.stage.python_executable))
        self.assertNotEqual(command[0], str(self.python_executable))
        self.assertEqual(command[1:5], ["-I", "-B", "-X", f"pycache_prefix={self.stage.pycache}"])
        self.assertEqual(command[5], str(self.stage.harness / "contained_run.py"))

    def test_staged_venv_preserves_source_packages_without_pyc(self) -> None:
        venv = self.root / "test-venv"
        subprocess.run(
            [str(self.python_executable), "-m", "venv", "--without-pip", str(venv)],
            check=True,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        site_packages = list(venv.glob("lib/python*/site-packages"))
        self.assertEqual(len(site_packages), 1)
        (site_packages[0] / "staged_test_package.py").write_text("VALUE = 17\n")
        metadata = site_packages[0] / "staged_test_package-1.0.dist-info"
        metadata.mkdir()
        (metadata / "METADATA").write_text("Name: staged-test-package\nVersion: 1.0\n")
        console = venv / "bin/staged-test-console"
        console.write_text("#!/bin/sh\nexit 0\n")
        (metadata / "RECORD").write_text(
            "staged_test_package.py,,\n"
            "staged_test_package-1.0.dist-info/METADATA,,\n"
            "staged_test_package-1.0.dist-info/RECORD,,\n"
            "../../../bin/staged-test-console,,\n"
        )
        extras = site_packages[0] / "extras"
        extras.mkdir()
        (extras / "staged_extra_package.py").write_text("VALUE = 23\n")
        (site_packages[0] / "staged-extra.pth").write_text("extras\n")
        cache = site_packages[0] / "__pycache__"
        cache.mkdir()
        (cache / "staged_test_package.pyc").write_bytes(b"forged-bytecode")
        stage = build_stage(
            self.root / "venv-stage",
            harness_dir=HARNESS,
            model_dir=self.model_dir,
            manifest_path=self.manifest_path,
            plan_path=PLAN,
            corpus_path=CORPUS,
            acquisition_record_path=self.acquisition_path,
            legal_evidence_path=self.legal_path,
            python_executable=venv / "bin/python",
            nvidia_smi=Path("/bin/true").resolve(),
        )
        try:
            self.assertTrue((stage.root / "pyvenv.cfg").is_file())
            self.assertTrue((stage.root / "bin/staged-test-console").is_file())
            self.assertTrue(
                (stage.root / "lib" / site_packages[0].parent.name / "site-packages"
                 / "staged_test_package-1.0.dist-info/METADATA").is_file()
            )
            self.assertEqual(list(stage.root.rglob("*.pyc")), [])
            python_fd = os.open(stage.python_executable, os.O_RDONLY)
            stage_fd = os.open(stage.root, os.O_RDONLY | os.O_DIRECTORY)
            descriptor_root = Path(f"/proc/self/fd/{stage_fd}")
            try:
                _run_staged_subject(
                    python_fd,
                    stage_fd,
                    [
                        str(descriptor_root / "executables/python"),
                        "-I",
                        "-B",
                        "-X",
                        f"pycache_prefix={descriptor_root / 'volatile/pycache'}",
                        "-c",
                        (
                            "import staged_test_package,staged_extra_package; "
                            "print(staged_test_package.VALUE + staged_extra_package.VALUE)"
                        ),
                    ],
                    _offline_environment(descriptor_root / "volatile/pycache"),
                    stage.stdout,
                    stage.stderr,
                    10,
                )
            finally:
                os.close(python_fd)
                os.close(stage_fd)
            self.assertEqual(stage.stdout.read_text().strip(), "40")
            validate_stage(stage)
        finally:
            destroy_stage(stage)

    def test_staged_venv_materializes_normal_sysconfigdata_symlink(self) -> None:
        venv = self.root / "sysconfig-venv"
        subprocess.run(
            [str(self.python_executable), "-m", "venv", "--without-pip", str(venv)],
            check=True,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        site_packages = next(venv.glob("lib/python*/site-packages"))
        version_directory = site_packages.parent.name
        base_root = self.root / "sysconfig-base"
        (base_root / "bin").mkdir(parents=True)
        standard_library = base_root / "lib" / version_directory
        standard_library.mkdir(parents=True)
        target = standard_library / "_sysconfigdata__x86_64-linux-gnu.py"
        target.write_text("build_time_vars = {'fixture': True}\n", encoding="utf-8")
        linked = standard_library / "_sysconfigdata__linux_x86_64-linux-gnu.py"
        link_text = os.path.relpath(target, linked.parent)
        linked.symlink_to(link_text)
        (standard_library / "sitecustomize.py").symlink_to(
            "/etc/python3.12/sitecustomize.py"
        )
        (venv / "pyvenv.cfg").write_text(
            f"home = {base_root / 'bin'}\n"
            "include-system-site-packages = false\n"
            f"version = {version_directory.removeprefix('python')}\n",
            encoding="utf-8",
        )
        stage = build_stage(
            self.root / "sysconfig-stage",
            harness_dir=HARNESS,
            model_dir=self.model_dir,
            manifest_path=self.manifest_path,
            plan_path=PLAN,
            corpus_path=CORPUS,
            acquisition_record_path=self.acquisition_path,
            legal_evidence_path=self.legal_path,
            python_executable=venv / "bin/python",
            nvidia_smi=Path("/bin/true").resolve(),
            require_venv=True,
        )
        try:
            staged = stage.root / "base/lib" / version_directory / linked.name
            self.assertFalse(staged.is_symlink())
            self.assertTrue(stat.S_ISREG(staged.lstat().st_mode))
            self.assertEqual(staged.read_bytes(), target.read_bytes())
            self.assertEqual(staged.stat().st_mode & 0o222, 0)
            self.assertFalse((stage.root / "base/lib" / version_directory / "sitecustomize.py").exists())
            self.assertFalse(any(path.is_symlink() for path in stage.root.rglob("*")))
            document = validate_stage(stage)
            self.assertEqual(
                document["python_source_links"],
                [
                    {
                        "identity": f"base/lib/{version_directory}/{linked.name}",
                        "source_identity": (
                            f"base-python/lib/{version_directory}/{linked.name}"
                        ),
                        "link_text": link_text,
                        "resolved_target_identity": (
                            f"base-python/lib/{version_directory}/{target.name}"
                        ),
                        "resolved_target_size_bytes": target.stat().st_size,
                        "resolved_target_sha256": sha256_file(target),
                    }
                ],
            )
            tampered = copy.deepcopy(document)
            tampered["python_source_links"][0]["link_text"] = "different.py"
            with self.assertRaisesRegex(ContractError, "identity digest differs"):
                validate_stage_document(tampered)
            for field, value in (
                ("resolved_target_size_bytes", target.stat().st_size + 1),
                ("resolved_target_sha256", "0" * 64),
            ):
                with self.subTest(field=field):
                    tampered = copy.deepcopy(document)
                    tampered["python_source_links"][0][field] = value
                    with self.assertRaisesRegex(
                        ContractError, "differs from the staged materialization"
                    ):
                        validate_stage_document(tampered)
        finally:
            destroy_stage(stage)

    def test_python_source_links_reject_absolute_escape_loop_and_depth(self) -> None:
        absolute_root = self.root / "absolute-link-root"
        absolute_root.mkdir()
        absolute_link = absolute_root / "absolute"
        absolute_link.symlink_to("/etc/passwd")
        with self.assertRaisesRegex(ContractError, "bounded relative link text"):
            _resolve_python_path(absolute_link, absolute_root, "venv")

        escape_root = self.root / "escape-link-root"
        escape_root.mkdir()
        outside = self.root / "outside-link-target"
        outside.write_text("outside\n", encoding="utf-8")
        escape_link = escape_root / "escape"
        escape_link.symlink_to("../outside-link-target")
        with self.assertRaisesRegex(ContractError, "escapes its approved root"):
            _resolve_python_path(escape_link, escape_root, "venv")

        loop_root = self.root / "loop-link-root"
        loop_root.mkdir()
        (loop_root / "first").symlink_to("second")
        (loop_root / "second").symlink_to("first")
        with self.assertRaisesRegex(ContractError, "symlink loop"):
            _resolve_python_path(loop_root / "first", loop_root, "venv")

        depth_root = self.root / "depth-link-root"
        depth_root.mkdir()
        (depth_root / "target").write_text("target\n", encoding="utf-8")
        for index in range(MAX_PYTHON_SYMLINKS + 1):
            target_name = (
                f"link-{index + 1}"
                if index < MAX_PYTHON_SYMLINKS
                else "target"
            )
            (depth_root / f"link-{index}").symlink_to(target_name)
        with self.assertRaisesRegex(ContractError, "symlink chain exceeds its bound"):
            _resolve_python_path(depth_root / "link-0", depth_root, "venv")

        long_root = self.root / "long-link-root"
        long_root.mkdir()
        long_link = long_root / "long"
        long_link.symlink_to("a/" * (MAX_PYTHON_SYMLINK_BYTES // 2 + 1))
        with self.assertRaisesRegex(ContractError, "bounded relative link text"):
            _resolve_python_path(long_link, long_root, "venv")

    def test_python_source_link_rejects_device_target(self) -> None:
        device_link = self.root / "device-link"
        device_link.symlink_to(os.path.relpath("/dev/null", device_link.parent))
        with self.assertRaisesRegex(ContractError, "not a regular file or directory"):
            _resolve_python_path(device_link, Path("/"), "base-python")

    def test_python_source_links_reject_directory_and_fifo_targets(self) -> None:
        root = self.root / "irregular-link-root"
        root.mkdir()
        directory = root / "directory"
        directory.mkdir()
        (root / "directory-link").symlink_to(directory.name)
        with self.assertRaisesRegex(ContractError, "not a regular file"):
            _resolve_python_path(root / "directory-link", root, "venv")
        (directory / "nested.py").write_text("nested = True\n", encoding="utf-8")
        with self.assertRaisesRegex(ContractError, "directory symlink"):
            _resolve_python_path(root / "directory-link/nested.py", root, "venv")

        fifo = root / "fifo"
        os.mkfifo(fifo)
        (root / "fifo-link").symlink_to(fifo.name)
        with self.assertRaisesRegex(ContractError, "not a regular file or directory"):
            _resolve_python_path(root / "fifo-link", root, "venv")

    def test_python_source_copy_revalidates_link_and_target_after_copy(self) -> None:
        link_root = self.root / "link-race-root"
        link_root.mkdir()
        target = link_root / "target.py"
        target.write_text("trusted = True\n", encoding="utf-8")
        replacement = link_root / "replacement.py"
        replacement.write_text("trusted = True\n", encoding="utf-8")
        linked = link_root / "linked.py"
        linked.symlink_to(target.name)
        source = _prepare_python_source(
            linked, link_root, "venv", Path("lib/linked.py")
        )
        original_open = staging_module._open_python_path
        calls = 0

        def replace_link_after_open(*args: object, **kwargs: object) -> object:
            nonlocal calls
            result = original_open(*args, **kwargs)
            calls += 1
            if calls == 1:
                linked.unlink()
                linked.symlink_to(replacement.name)
            return result

        with mock.patch(
            "staging._open_python_path", side_effect=replace_link_after_open
        ):
            with self.assertRaisesRegex(ContractError, "link or target changed"):
                _copy_python_source(source, self.root / "link-race-stage")

        target_root = self.root / "target-race-root"
        target_root.mkdir()
        target = target_root / "target.py"
        target.write_text("trusted = True\n", encoding="utf-8")
        linked = target_root / "linked.py"
        linked.symlink_to(target.name)
        source = _prepare_python_source(
            linked, target_root, "venv", Path("lib/linked.py")
        )
        calls = 0

        def mutate_target_after_open(*args: object, **kwargs: object) -> object:
            nonlocal calls
            result = original_open(*args, **kwargs)
            calls += 1
            if calls == 1:
                target.write_text("hostile = True\n", encoding="utf-8")
            return result

        with mock.patch(
            "staging._open_python_path", side_effect=mutate_target_after_open
        ):
            with self.assertRaisesRegex(ContractError, "changed while being copied"):
                _copy_python_source(source, self.root / "target-race-stage")

    def test_hostile_pth_fixtures_fail_closed(self) -> None:
        venv = self.root / "hostile-pth-venv"
        subprocess.run(
            [str(self.python_executable), "-m", "venv", "--without-pip", str(venv)],
            check=True,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        site_packages = next(venv.glob("lib/python*/site-packages"))
        linked = site_packages / "linked"
        fixtures = (
            ("import os\n", "executable/import line"),
            ("/tmp\n", "canonical and relative"),
            ("../outside\n", "canonical and relative"),
            ("linked\n", "symlink"),
        )
        pth = site_packages / "hostile.pth"
        for index, (content, message) in enumerate(fixtures):
            with self.subTest(content=content.strip()):
                if content == "linked\n":
                    linked.symlink_to(site_packages)
                pth.write_text(content)
                stage_root = self.root / f"hostile-pth-stage-{index}"
                with self.assertRaisesRegex(ContractError, message):
                    build_stage(
                        stage_root,
                        harness_dir=HARNESS,
                        model_dir=self.model_dir,
                        manifest_path=self.manifest_path,
                        plan_path=PLAN,
                        corpus_path=CORPUS,
                        acquisition_record_path=self.acquisition_path,
                        legal_evidence_path=self.legal_path,
                        python_executable=venv / "bin/python",
                        nvidia_smi=Path("/bin/true").resolve(),
                    )
                self.assertFalse(stage_root.exists())
                linked.unlink(missing_ok=True)

    def test_venv_rejects_system_site_packages(self) -> None:
        venv = self.root / "system-site-venv"
        subprocess.run(
            [str(self.python_executable), "-m", "venv", "--without-pip", str(venv)],
            check=True,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        configuration = venv / "pyvenv.cfg"
        configuration.write_text(
            configuration.read_text().replace(
                "include-system-site-packages = false",
                "include-system-site-packages = true",
            )
        )
        stage_root = self.root / "system-site-stage"
        with self.assertRaisesRegex(ContractError, "include-system-site-packages = false"):
            build_stage(
                stage_root,
                harness_dir=HARNESS,
                model_dir=self.model_dir,
                manifest_path=self.manifest_path,
                plan_path=PLAN,
                corpus_path=CORPUS,
                acquisition_record_path=self.acquisition_path,
                legal_evidence_path=self.legal_path,
                python_executable=venv / "bin/python",
                nvidia_smi=Path("/bin/true").resolve(),
            )
        self.assertFalse(stage_root.exists())

    def test_measurement_rejects_non_venv_python_before_containment(self) -> None:
        stage_root = self.root / "non-venv-stage"
        with self.assertRaisesRegex(ContractError, "must be a real isolated venv"):
            build_stage(
                stage_root,
                harness_dir=HARNESS,
                model_dir=self.model_dir,
                manifest_path=self.manifest_path,
                plan_path=PLAN,
                corpus_path=CORPUS,
                acquisition_record_path=self.acquisition_path,
                legal_evidence_path=self.legal_path,
                python_executable=self.python_executable,
                nvidia_smi=Path("/bin/true").resolve(),
                require_venv=True,
            )
        self.assertFalse(stage_root.exists())

    def test_containment_unavailable_fails_closed(self) -> None:
        with mock.patch("run.shutil.which", return_value=None):
            with self.assertRaisesRegex(ContractError, "required for measurement containment"):
                _containment_tools()

    def test_positive_non_gpu_systemd_lifecycle_when_available(self) -> None:
        try:
            _, systemctl, _ = _containment_tools()
            _manager_environment_names(systemctl)
        except ContractError as error:
            self.skipTest(f"user systemd containment unavailable: {error}")
        venv = self.root / "lifecycle-venv"
        subprocess.run(
            [str(self.python_executable), "-m", "venv", "--without-pip", str(venv)],
            check=True,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        stage = build_stage(
            self.root / "lifecycle-stage",
            harness_dir=HARNESS,
            model_dir=self.model_dir,
            manifest_path=self.manifest_path,
            plan_path=PLAN,
            corpus_path=CORPUS,
            acquisition_record_path=self.acquisition_path,
            legal_evidence_path=self.legal_path,
            python_executable=venv / "bin/python",
            nvidia_smi=Path("/bin/true").resolve(),
            require_venv=True,
        )
        try:
            stage_document = validate_stage(stage)
            uid = os.getuid()
            gid = os.getgid()
            command = [
                str(stage.python_executable),
                "-I",
                "-B",
                "-X",
                f"pycache_prefix={stage.pycache}",
                "-c",
                (
                    "import os,subprocess,sys,time; "
                    "blocked=False; "
                    f"expected_uid={uid}; expected_gid={gid}; "
                    "status=dict(line.split(':',1) for line in open('/proc/self/status') if ':' in line); "
                    "caps=('CapEff','CapPrm','CapInh','CapAmb'); "
                    "privileged=(expected_uid == 0 or expected_gid == 0 or "
                    "os.getresuid() != (expected_uid,)*3 or os.getresgid() != (expected_gid,)*3 or "
                    "status.get('Uid','').split() != [str(expected_uid)]*4 or "
                    "status.get('Gid','').split() != [str(expected_gid)]*4 or "
                    "status.get('NoNewPrivs','').strip() != '1' or "
                    "any(int(status.get(name,'1').strip(),16) != 0 for name in caps)); "
                    "\nif privileged:\n sys.exit(8)\n"
                    "\ntry:\n subprocess.run(['/bin/true'],check=True)\n"
                    "except (PermissionError,FileNotFoundError):\n blocked=True\n"
                    "\nif not blocked:\n sys.exit(9)\n"
                    "sys.stdin.readline(); time.sleep(1); sys.stdout.write('{}')"
                ),
            ]
            try:
                provisional = _run_contained(
                    command,
                    parent_challenge="a" * 64,
                    stage_root=stage.root,
                    publish_path=self.root / "unused-probe-output",
                    volatile_path=stage.root / "volatile",
                    timeout_seconds=10,
                    os_runtime_loaders=stage_document["os_runtime_loaders"],
                )
            except ContractError as error:
                self.skipTest(f"host systemd sandbox unavailable: {error}")
            self.assertEqual(provisional, b"{}")
        finally:
            destroy_stage(stage)

    def test_manager_loader_and_audit_hooks_are_unset_by_name_only(self) -> None:
        names = _parse_manager_environment_names(
            b"LANG=C\nLD_PRELOAD=test-only\nLD_AUDIT=test-only\n"
            b"PYTHONPATH=test-only\nXAI_API_KEY=test-only\n"
        )
        unset = _unset_manager_environment(names)
        self.assertNotIn("LANG", unset)
        self.assertIn("LD_PRELOAD", unset)
        self.assertIn("LD_AUDIT", unset)
        self.assertIn("PYTHONPATH", unset)
        self.assertIn("XAI_API_KEY", unset)

    def test_first_staged_python_environment_has_no_extra_names(self) -> None:
        _verify_first_stage_environment(
            {"LANG"},
            environment={"LANG": "C", "USER": "test", "INVOCATION_ID": "test"},
        )
        with self.assertRaisesRegex(ContractError, "frozen policy"):
            _verify_first_stage_environment(
                {"LANG"},
                environment={"LANG": "C", "PATH": "/untrusted"},
            )
        with self.assertRaisesRegex(ContractError, "forbidden environment hook"):
            _verify_first_stage_environment(
                {"LANG", "LD_PRELOAD"},
                environment={"LANG": "C", "LD_PRELOAD": "test-only"},
            )

    def test_ineffective_systemd_property_or_controller_fails_closed(self) -> None:
        cgroup_root = self.root / "cgroup"
        cgroup = cgroup_root / "test-unit"
        cgroup.mkdir(parents=True)
        (cgroup / "memory.max").write_text("34359738368\n")
        (cgroup / "memory.swap.max").write_text("0\n")
        (cgroup / "pids.max").write_text("64\n")
        properties = {
            "ActiveState": "active",
            "ControlGroup": "/test-unit",
            "MemoryMax": "34359738368",
            "MemorySwapMax": "0",
            "TasksMax": "64",
            "RuntimeMaxUSec": "4h 1min",
            "LimitFSIZE": "33554432",
            "LimitNOFILE": "128",
            "IPAddressDeny": "0.0.0.0/0 ::/0",
            "RestrictAddressFamilies": "AF_UNIX",
            "SystemCallFilter": "~@network-io",
            "SystemCallErrorNumber": "1",
            "NoNewPrivileges": "yes",
            "RestrictSUIDSGID": "yes",
            "ProtectSystem": "strict",
            "ProtectHome": "read-only",
            "ProtectControlGroups": "yes",
            "KillMode": "control-group",
            "MemoryAccounting": "yes",
            "TasksAccounting": "yes",
            "IPAccounting": "yes",
            "ReadOnlyPaths": f"/stage /tmp /var/tmp /dev/shm /run/user/{os.getuid()}",
            "ReadWritePaths": "",
            "InaccessiblePaths": (
                f"/run/user/{os.getuid()}/bus /run/user/{os.getuid()}/systemd/private "
                "/run/systemd/private /run/dbus/system_bus_socket /var/run/dbus/system_bus_socket "
                "/bin /sbin /usr/bin /usr/sbin /usr/local/bin /usr/local/sbin"
            ),
            "TemporaryFileSystem": "/stage/volatile:rw,size=134217728,mode=0700",
            "UnsetEnvironment": " ".join(sorted(MANDATORY_UNSET_ENVIRONMENT)),
            "UMask": "0077",
            "NoExecPaths": "/",
            "ExecPaths": " ".join(
                sorted(
                    {
                        "/stage",
                        "/stage/executables/python",
                        "/stage/executables/nvidia-smi",
                        *SYSTEM_LIBRARY_EXEC_PATHS,
                    }
                )
            ),
        }
        _validate_effective_containment(
            properties,
            stage_root=Path("/stage"),
            publish_path=Path("/receipt"),
            volatile_path=Path("/stage/volatile"),
            cgroup_root=cgroup_root,
        )
        changed = dict(properties)
        changed["ProtectControlGroups"] = "no"
        with self.assertRaisesRegex(ContractError, "ProtectControlGroups"):
            _validate_effective_containment(
                changed,
                stage_root=Path("/stage"),
                publish_path=Path("/receipt"),
                volatile_path=Path("/stage/volatile"),
                cgroup_root=cgroup_root,
            )
        (cgroup / "memory.max").write_text("max\n")
        with self.assertRaisesRegex(ContractError, "memory.max"):
            _validate_effective_containment(
                properties,
                stage_root=Path("/stage"),
                publish_path=Path("/receipt"),
                volatile_path=Path("/stage/volatile"),
                cgroup_root=cgroup_root,
            )

    def test_effective_property_sets_reject_extras_and_runtime_expansion(self) -> None:
        cgroup_root = self.root / "property-cgroup"
        cgroup = cgroup_root / "test-unit"
        cgroup.mkdir(parents=True)
        (cgroup / "memory.max").write_text("34359738368\n")
        (cgroup / "memory.swap.max").write_text("0\n")
        (cgroup / "pids.max").write_text("64\n")
        uid = os.getuid()
        base = {
            "ActiveState": "active",
            "ControlGroup": "/test-unit",
            "MemoryMax": "34359738368",
            "MemorySwapMax": "0",
            "TasksMax": "64",
            "RuntimeMaxUSec": "4h 1min",
            "LimitFSIZE": "33554432",
            "LimitNOFILE": "128",
            "IPAddressDeny": "any",
            "RestrictAddressFamilies": "AF_UNIX",
            "SystemCallFilter": "~@network-io",
            "SystemCallErrorNumber": "EPERM",
            "NoNewPrivileges": "yes",
            "RestrictSUIDSGID": "yes",
            "ProtectSystem": "strict",
            "ProtectHome": "read-only",
            "ProtectControlGroups": "yes",
            "KillMode": "control-group",
            "MemoryAccounting": "yes",
            "TasksAccounting": "yes",
            "IPAccounting": "yes",
            "ReadOnlyPaths": f"/stage /tmp /var/tmp /dev/shm /run/user/{uid}",
            "ReadWritePaths": "",
            "InaccessiblePaths": (
                f"/run/user/{uid}/bus /run/user/{uid}/systemd/private "
                "/run/systemd/private /run/dbus/system_bus_socket /var/run/dbus/system_bus_socket "
                "/bin /sbin /usr/bin /usr/sbin /usr/local/bin /usr/local/sbin"
            ),
            "TemporaryFileSystem": "/stage/volatile:rw,size=134217728,mode=0700",
            "UnsetEnvironment": " ".join(sorted(MANDATORY_UNSET_ENVIRONMENT)),
            "UMask": "0077",
            "NoExecPaths": "/",
            "ExecPaths": " ".join(
                sorted(
                    {
                        "/stage",
                        "/stage/executables/python",
                        "/stage/executables/nvidia-smi",
                        *SYSTEM_LIBRARY_EXEC_PATHS,
                    }
                )
            ),
        }
        mutations = (
            ("RuntimeMaxUSec", "4h 2min", "runtime limit"),
            (
                "ReadOnlyPaths",
                f"/stage /tmp /var/tmp /dev/shm /run/user/{uid} /extra",
                "read-only path set",
            ),
            ("ReadWritePaths", "/extra", "writable path set"),
            ("InaccessiblePaths", "/run/systemd/private", "inaccessible socket set"),
            (
                "TemporaryFileSystem",
                "/stage/volatile:rw,size=134217728,mode=0777",
                "writable tmpfs",
            ),
            ("UnsetEnvironment", "XDG_RUNTIME_DIR", "environment removal"),
            ("SystemCallFilter", "", "deny polarity"),
            ("SystemCallFilter", "@network-io", "deny polarity"),
            (
                "SystemCallFilter",
                "~@network-io @network-io",
                "deny polarity",
            ),
            ("SystemCallErrorNumber", "13", "syscall error policy"),
            ("NoExecPaths", "", "NoExecPaths"),
            (
                "ExecPaths",
                " ".join(
                    sorted(
                        {
                            "/stage",
                            "/stage/executables/python",
                            "/stage/executables/nvidia-smi",
                            "/bin/true",
                            *SYSTEM_LIBRARY_EXEC_PATHS,
                        }
                    )
                ),
                "ExecPaths set",
            ),
            ("IPAddressDeny", "0.0.0.0/0", "IPAddressDeny"),
            ("UMask", "0022", "UMask"),
            ("NoNewPrivileges", "no", "NoNewPrivileges"),
            ("RestrictSUIDSGID", "no", "RestrictSUIDSGID"),
        )
        for field, value, message in mutations:
            with self.subTest(field=field):
                changed = dict(base)
                changed[field] = value
                with self.assertRaisesRegex(ContractError, message):
                    _validate_effective_containment(
                        changed,
                        stage_root=Path("/stage"),
                        publish_path=Path("/receipt"),
                        volatile_path=Path("/stage/volatile"),
                        cgroup_root=cgroup_root,
                    )

    def test_contained_privilege_contract_rejects_usable_capabilities(self) -> None:
        status = self.root / "process-status"
        baseline = (
            "Uid:\t1000\t1000\t1000\t1000\n"
            "Gid:\t1000\t1000\t1000\t1000\n"
            "CapInh:\t0000000000000000\n"
            "CapPrm:\t0000000000000000\n"
            "CapEff:\t0000000000000000\n"
            "CapBnd:\t000001ffffffffff\n"
            "CapAmb:\t0000000000000000\n"
            "NoNewPrivs:\t1\n"
        )
        status.write_text(baseline, encoding="ascii")
        with mock.patch("run.os.getuid", return_value=1000), mock.patch(
            "run.os.getgid", return_value=1000
        ), mock.patch("run.os.getresuid", return_value=(1000, 1000, 1000)), mock.patch(
            "run.os.getresgid", return_value=(1000, 1000, 1000)
        ):
            _verify_contained_privilege_contract(1000, 1000, status_path=status)
            for name in ("CapEff", "CapPrm", "CapInh", "CapAmb"):
                with self.subTest(name=name):
                    status.write_text(
                        baseline.replace(
                            f"{name}:\t0000000000000000",
                            f"{name}:\t0000000000000001",
                        ),
                        encoding="ascii",
                    )
                    with self.assertRaisesRegex(
                        ContractError, f"capability set is nonzero: {name}"
                    ):
                        _verify_contained_privilege_contract(1000, 1000, status_path=status)
            status.write_text(
                baseline.replace("NoNewPrivs:\t1", "NoNewPrivs:\t0"), encoding="ascii"
            )
            with self.assertRaisesRegex(ContractError, "NoNewPrivileges is not active"):
                _verify_contained_privilege_contract(1000, 1000, status_path=status)

    def test_staged_executables_reject_privilege_bits_and_file_capabilities(self) -> None:
        self.stage.python_executable.chmod(0o4500)
        with self.assertRaisesRegex(ContractError, "setuid or setgid"):
            validate_stage(self.stage)
        self.stage.python_executable.chmod(0o500)
        with mock.patch("staging.os.getxattr", return_value=b"test-capability"):
            with self.assertRaisesRegex(ContractError, "retains file capabilities"):
                validate_stage(self.stage)

    def test_sibling_unit_socket_escape_is_rejected(self) -> None:
        class ReachableSocket:
            def connect(self, _path: str) -> None:
                return None

            def close(self) -> None:
                return None

        with self.assertRaisesRegex(ContractError, "user-manager socket is reachable"):
            _verify_user_manager_isolation(
                environment={},
                socket_factory=lambda *_: ReachableSocket(),
            )
        with self.assertRaisesRegex(ContractError, "DBUS_SESSION_BUS_ADDRESS"):
            _verify_user_manager_isolation(
                environment={"DBUS_SESSION_BUS_ADDRESS": "unix:path=/run/user/bus"},
                socket_factory=lambda *_: ReachableSocket(),
            )

    def test_teardown_command_failure_is_not_suppressed(self) -> None:
        with mock.patch("run._systemctl", side_effect=ContractError("stop denied")):
            with self.assertRaisesRegex(ContractError, "stop denied"):
                _terminate_and_verify(
                    Path("/systemctl"),
                    "test.service",
                    self.root / "cgroup",
                    temporary_directory=self.root,
                )

    def test_already_unloaded_unit_teardown_succeeds_only_with_empty_postconditions(self) -> None:
        absent_cgroup = self.root / "already-removed-cgroup"
        with mock.patch(
            "run._systemctl",
            return_value=(5, "", "unit not loaded"),
        ), mock.patch("run._show_unit", return_value=None):
            _terminate_and_verify(
                Path("/systemctl"),
                "removed.service",
                absent_cgroup,
                temporary_directory=self.root,
            )

        live_cgroup = self.root / "still-live-cgroup"
        live_cgroup.mkdir()
        (live_cgroup / "cgroup.procs").write_text("123\n")
        with mock.patch(
            "run._systemctl",
            return_value=(5, "", "unit not loaded"),
        ), mock.patch("run._show_unit", return_value=None):
            with self.assertRaisesRegex(ContractError, "still has processes"):
                _terminate_and_verify(
                    Path("/systemctl"),
                    "removed.service",
                    live_cgroup,
                    temporary_directory=self.root,
                )

    def test_escaped_session_child_blocks_empty_cgroup_proof(self) -> None:
        child = subprocess.Popen(
            [
                str(self.python_executable),
                "-c",
                "import os,time; os.setsid(); time.sleep(30)",
            ]
        )
        cgroup = self.root / "escaped-cgroup"
        cgroup.mkdir()
        (cgroup / "cgroup.procs").write_text(f"{child.pid}\n")
        try:
            with self.assertRaisesRegex(ContractError, "still has processes"):
                _verify_cgroup_empty(cgroup)
        finally:
            child.kill()
            child.wait()

    def test_operator_gpu_assertion_cannot_change_non_claim_trust(self) -> None:
        changed = copy.deepcopy(self.receipt)
        changed["trust"]["authenticity"] = "hardware-verified"
        with self.assertRaisesRegex(ContractError, "trust, or non-claim state differs"):
            self.validate(changed)

    def test_staged_python_executes_by_open_descriptor_without_bytecode(self) -> None:
        python_fd = os.open(self.stage.python_executable, os.O_RDONLY)
        stage_fd = os.open(self.stage.root, os.O_RDONLY | os.O_DIRECTORY)
        descriptor_root = Path(f"/proc/self/fd/{stage_fd}")
        environment = _offline_environment(descriptor_root / "volatile/pycache")
        try:
            validate_stage(paths_from_root(descriptor_root))
            _run_staged_subject(
                python_fd,
                stage_fd,
                [
                    str(descriptor_root / "executables/python"),
                    "-I",
                    "-B",
                    "-X",
                    f"pycache_prefix={descriptor_root / 'volatile/pycache'}",
                    "-c",
                    "import sys; print(sys.executable); print(sys.dont_write_bytecode, sys.pycache_prefix)",
                ],
                environment,
                self.stage.stdout,
                self.stage.stderr,
                10,
            )
        finally:
            os.close(python_fd)
            os.close(stage_fd)
        output = self.stage.stdout.read_text(encoding="utf-8")
        self.assertIn("/proc/self/fd/", output)
        self.assertIn("True /proc/self/fd/", output)
        validate_stage(self.stage)

    def test_staged_child_output_is_file_bounded(self) -> None:
        python_fd = os.open(self.stage.python_executable, os.O_RDONLY)
        stage_fd = os.open(self.stage.root, os.O_RDONLY | os.O_DIRECTORY)
        try:
            with self.assertRaisesRegex(ContractError, "reference subject failed"):
                _run_staged_subject(
                    python_fd,
                    stage_fd,
                    [
                        str(self.python_executable),
                        "-I",
                        "-B",
                        "-c",
                        "import sys; sys.stdout.write('x' * 1000000)",
                    ],
                    _offline_environment(),
                    self.stage.stdout,
                    self.stage.stderr,
                    10,
                    maximum_file_bytes=1024,
                )
        finally:
            os.close(python_fd)
            os.close(stage_fd)
        self.assertLessEqual(self.stage.stdout.stat().st_size, 1024)

    def test_staged_timeout_kills_complete_process_group(self) -> None:
        marker = self.root / "escaped-grandchild"
        python_fd = os.open(self.stage.python_executable, os.O_RDONLY)
        stage_fd = os.open(self.stage.root, os.O_RDONLY | os.O_DIRECTORY)
        script = (
            "import os,time; "
            f"marker={str(marker)!r}; "
            "child=os.fork(); "
            "time.sleep(0.5) if child == 0 else time.sleep(10); "
            "open(marker,'w').write('escaped') if child == 0 else None"
        )
        try:
            with self.assertRaisesRegex(ContractError, "fixed timeout"):
                _run_staged_subject(
                    python_fd,
                    stage_fd,
                    [str(self.python_executable), "-I", "-B", "-c", script],
                    _offline_environment(),
                    self.stage.stdout,
                    self.stage.stderr,
                    0.1,
                )
        finally:
            os.close(python_fd)
            os.close(stage_fd)
        time.sleep(0.6)
        self.assertFalse(marker.exists())

    def test_end_to_end_launcher_uses_and_cleans_staged_runner(self) -> None:
        output = self.root / "no-gpu-receipt.json"
        completed = subprocess.run(
            [
                str(self.python_executable),
                str(HARNESS / "run.py"),
                "--python",
                str(self.python_executable),
                "--model-dir",
                str(self.model_dir),
                "--manifest",
                str(self.manifest_path),
                "--plan",
                str(PLAN),
                "--corpus",
                str(CORPUS),
                "--nvidia-smi",
                str(Path("/bin/true").resolve()),
                "--acquisition-record",
                str(self.acquisition_path),
                "--legal-evidence",
                str(self.legal_path),
                "--output",
                str(output),
            ],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            timeout=30,
            check=False,
        )
        self.assertNotEqual(completed.returncode, 0)
        self.assertFalse(output.exists())
        self.assertEqual(list(self.root.glob(".embedding-stage-*")), [])

    def test_public_launcher_rejects_old_internal_flags_before_publication(self) -> None:
        output = self.root / "forbidden-internal-output.json"
        completed = subprocess.run(
            [
                str(self.python_executable),
                str(HARNESS / "run.py"),
                "--python",
                str(self.python_executable),
                "--model-dir",
                str(self.model_dir),
                "--manifest",
                str(self.manifest_path),
                "--plan",
                str(PLAN),
                "--corpus",
                str(CORPUS),
                "--nvidia-smi",
                str(Path("/bin/true").resolve()),
                "--acquisition-record",
                str(self.acquisition_path),
                "--legal-evidence",
                str(self.legal_path),
                "--output",
                str(output),
                "--execute-stage",
                str(self.stage.root),
            ],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=30,
            check=False,
        )
        self.assertNotEqual(completed.returncode, 0)
        self.assertFalse(output.exists())

    def test_direct_contained_entry_without_parent_channel_cannot_publish(self) -> None:
        completed = subprocess.run(
            [
                str(self.python_executable),
                "-B",
                str(HARNESS / "contained_run.py"),
                "--stage-root",
                str(self.stage.root),
                "--expected-stage-sha256",
                self.execution_stage["identity_sha256"],
                "--gpu-index",
                "0",
                "--expected-uid",
                str(os.getuid()),
                "--expected-gid",
                str(os.getgid()),
            ],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=30,
            check=False,
        )
        self.assertNotEqual(completed.returncode, 0)
        self.assertEqual(completed.stdout, b"")

    def test_launcher_drops_unapproved_environment(self) -> None:
        hostile = {
            "PATH": "/tmp/attacker-bin",
            "PYTHONPATH": "/tmp/attacker-python",
            "HOME": "/tmp/attacker-home",
            "LD_PRELOAD": "/tmp/attacker.so",
            "LD_LIBRARY_PATH": "/opt/pinned-cuda/lib",
            "NVIDIA_VISIBLE_DEVICES": "GPU-test",
        }
        with mock.patch.dict(os.environ, hostile, clear=True):
            observed = _offline_environment()
        self.assertNotIn("PATH", observed)
        self.assertNotIn("PYTHONPATH", observed)
        self.assertNotIn("HOME", observed)
        self.assertNotIn("LD_PRELOAD", observed)
        self.assertNotIn("LD_LIBRARY_PATH", observed)
        self.assertEqual(observed["NVIDIA_VISIBLE_DEVICES"], "GPU-test")

    def test_receipt_rejects_launcher_boundary_expansion(self) -> None:
        changed = copy.deepcopy(self.receipt)
        changed["execution_profile"]["invocation"]["inherited_environment_names"] = [
            "PATH"
        ]
        with self.assertRaisesRegex(ContractError, "launcher allowlist"):
            self.validate(changed)

    def test_rtx_result_placeholder_remains_unverified(self) -> None:
        placeholder = load_json(RESULT_PLACEHOLDER)
        self.assertEqual(placeholder["status"], "unverified-no-measurement")
        self.assertIsNone(placeholder["model"]["revision"])
        self.assertIsNone(placeholder["model"]["acquisition_record_sha256"])
        self.assertIsNone(placeholder["model"]["legal_evidence_sha256"])
        self.assertIsNone(placeholder["receipt_sha256"])
        self.assertEqual(placeholder["claims"], [])


if __name__ == "__main__":
    unittest.main()
