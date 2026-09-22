#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Internal contained measurement entry; emits only a challenged provisional result."""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
from pathlib import Path

HARNESS_DIR = Path(__file__).resolve().parent
if str(HARNESS_DIR) not in sys.path:
    sys.path.insert(0, str(HARNESS_DIR))

from check_receipt import PERMITTED_INHERITED_ENVIRONMENT, validate_provisional
from contract import ContractError, load_corpus, load_json, load_plan, validate_manifest, verify_snapshot
from run import (
    _offline_environment,
    _run_staged_subject,
    _verify_contained_privilege_contract,
    _verify_first_stage_environment,
    _verify_open_executable,
    _verify_user_manager_isolation,
)
from staging import initialize_volatile, paths_from_root, validate_stage

MAX_PROVISIONAL_BYTES = 32 * 1024 * 1024


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--stage-root", type=Path, required=True)
    parser.add_argument("--expected-stage-sha256", required=True)
    parser.add_argument("--gpu-index", type=int, required=True)
    parser.add_argument("--expected-uid", type=int, required=True)
    parser.add_argument("--expected-gid", type=int, required=True)
    parser.add_argument(
        "--expected-manager-environment-name",
        action="append",
        default=[],
    )
    return parser.parse_args()


def _read_parent_challenge() -> str:
    data = bytearray()
    while len(data) <= 65:
        chunk = os.read(0, 66 - len(data))
        if not chunk:
            break
        data.extend(chunk)
    if len(data) != 65 or data[-1:] != b"\n":
        raise ContractError("one-run parent challenge channel is missing or malformed")
    try:
        challenge = data[:-1].decode("ascii")
    except UnicodeError as error:
        raise ContractError("one-run parent challenge is not ASCII") from error
    if re.fullmatch(r"[0-9a-f]{64}", challenge) is None:
        raise ContractError("one-run parent challenge is malformed")
    return challenge


def contained_main(arguments: argparse.Namespace) -> dict:
    if arguments.gpu_index < 0 or arguments.gpu_index > 15:
        raise ContractError("GPU index must be between 0 and 15")
    challenge = _read_parent_challenge()
    expected_manager_names = set(arguments.expected_manager_environment_name)
    if len(expected_manager_names) != len(arguments.expected_manager_environment_name):
        raise ContractError("expected manager environment names are duplicated")
    _verify_first_stage_environment(expected_manager_names)
    _verify_contained_privilege_contract(arguments.expected_uid, arguments.expected_gid)
    _verify_user_manager_isolation()

    stage = paths_from_root(arguments.stage_root)
    initialize_volatile(stage)
    stage_document = validate_stage(stage)
    if stage_document["identity_sha256"] != arguments.expected_stage_sha256:
        raise ContractError("contained stage differs from the parent challenge context")
    manifest = load_json(stage.model_manifest)
    validate_manifest(manifest, require_verified=True)
    verify_snapshot(
        manifest,
        stage.model,
        stage.acquisition_record,
        stage.legal_evidence,
    )
    plan = load_plan(stage.plan)
    _, records = load_corpus(stage.corpus, plan)

    stage_fd = os.open(stage.root, os.O_RDONLY | os.O_DIRECTORY)
    python_fd = os.open(stage.python_executable, os.O_RDONLY)
    _verify_open_executable(
        python_fd, stage_document, "executables/python", "Python"
    )
    child_root = Path(f"/proc/self/fd/{stage_fd}")
    command = [
        str(child_root / "executables/python"),
        "-I",
        "-B",
        "-X",
        f"pycache_prefix={child_root / 'volatile/pycache'}",
        str(child_root / "harness/reference_subject.py"),
        "--stage-root",
        str(child_root),
        "--model-dir",
        str(child_root / "model"),
        "--manifest",
        str(child_root / "inputs/manifest.json"),
        "--plan",
        str(child_root / "inputs/plan.json"),
        "--corpus",
        str(child_root / "inputs/corpus.json"),
        "--nvidia-smi",
        str(child_root / "executables/nvidia-smi"),
        "--acquisition-record",
        str(child_root / "evidence/acquisition/acquisition.json"),
        "--legal-evidence",
        str(child_root / "evidence/legal-evidence.json"),
        "--gpu-index",
        str(arguments.gpu_index),
        "--output",
        str(child_root / "volatile/receipt.json"),
    ]
    environment = _offline_environment(child_root / "volatile/pycache")
    environment["TMPDIR"] = str(child_root / "volatile/tmp")
    try:
        _run_staged_subject(
            python_fd,
            stage_fd,
            command,
            environment,
            stage.stdout,
            stage.stderr,
            plan["measurement"]["subprocess_timeout_seconds"],
        )
    finally:
        os.close(python_fd)
        os.close(stage_fd)

    provisional = load_json(stage.receipt, maximum_bytes=MAX_PROVISIONAL_BYTES)
    provisional["parent_challenge"] = challenge
    validate_stage(stage)
    validate_provisional(
        provisional,
        manifest,
        plan,
        records,
        harness_dir=stage.harness,
        manifest_path=stage.model_manifest,
        plan_path=stage.plan,
        corpus_path=stage.corpus,
        acquisition_record_path=stage.acquisition_record,
        legal_evidence_path=stage.legal_evidence,
        expected_stage=stage_document,
        expected_inherited_environment_names=sorted(
            name for name in environment if name in PERMITTED_INHERITED_ENVIRONMENT
        ),
    )
    return provisional


def main() -> int:
    try:
        provisional = contained_main(parse_args())
        encoded = json.dumps(provisional, indent=2, ensure_ascii=False) + "\n"
        if len(encoded.encode("utf-8")) > MAX_PROVISIONAL_BYTES:
            raise ContractError("provisional result exceeds 32 MiB")
        sys.stdout.write(encoded)
        sys.stdout.flush()
    except (ContractError, OSError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
