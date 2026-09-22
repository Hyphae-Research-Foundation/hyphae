#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Populate the fail-closed model template from an offline local snapshot."""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

from contract import (
    ContractError,
    HEX40,
    acquisition_snapshot_provenance,
    load_json,
    sha256_file,
    snapshot_files,
    snapshot_role,
    validate_legal_evidence,
    validate_manifest,
    verify_snapshot,
)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--template", type=Path, required=True)
    parser.add_argument("--model-dir", type=Path, required=True)
    parser.add_argument("--revision", required=True, help="full 40-character Hugging Face Git commit")
    parser.add_argument("--acquisition-record", type=Path, required=True)
    parser.add_argument("--legal-evidence", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    return parser.parse_args()


def populate(
    template_path: Path,
    model_dir: Path,
    revision: str,
    acquisition_record_path: Path,
    legal_evidence_path: Path,
) -> dict:
    template = load_json(template_path)
    validate_manifest(template, require_verified=False)
    if template["status"] != "template-unverified":
        raise ContractError("population input must be the unverified template")
    if HEX40.fullmatch(revision) is None:
        raise ContractError("revision must be a full lowercase 40-character Git commit")
    files = []
    for path in snapshot_files(model_dir):
        relative = path.relative_to(model_dir).as_posix()
        files.append(
            {
                "path": relative,
                "role": snapshot_role(relative),
                "size_bytes": path.stat().st_size,
                "sha256": sha256_file(path),
            }
        )
    output = dict(template)
    output["status"] = "verified"
    output["model"] = dict(template["model"])
    output["model"]["revision"] = revision
    output["files"] = files
    output["acquisition_provenance"] = acquisition_snapshot_provenance(
        model_dir, revision, acquisition_record_path
    )
    output["license"] = validate_legal_evidence(
        legal_evidence_path, output["acquisition_provenance"], model_dir
    )
    validate_manifest(output, require_verified=True)
    verify_snapshot(output, model_dir, acquisition_record_path, legal_evidence_path)
    return output


def main() -> int:
    arguments = parse_args()
    try:
        for path, label in (
            (arguments.template, "template"),
            (arguments.model_dir, "model directory"),
            (arguments.acquisition_record, "acquisition record"),
            (arguments.legal_evidence, "legal evidence"),
            (arguments.output, "output"),
        ):
            if not path.is_absolute():
                raise ContractError(f"{label} must be absolute")
        if not arguments.output.parent.is_dir():
            raise ContractError("output parent directory does not exist")
        if arguments.output.exists():
            raise ContractError("output already exists")
        document = populate(
            arguments.template.resolve(),
            arguments.model_dir.resolve(),
            arguments.revision,
            arguments.acquisition_record.resolve(),
            arguments.legal_evidence.resolve(),
        )
        with arguments.output.open("x", encoding="utf-8") as destination:
            json.dump(document, destination, indent=2, ensure_ascii=False)
            destination.write("\n")
    except (ContractError, OSError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    print(f"verified manifest written to {arguments.output}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
