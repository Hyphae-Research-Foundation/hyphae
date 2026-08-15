#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""Validate source-bound Python managed Native v2 conformance receipts."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[1]
SCHEMA_PATH = (
    ROOT / "conformance/v2/schema/python-managed-live-receipt.schema.json"
)
SCHEMA = "hyphae-python-managed-v2-conformance-receipt-v1"
PLATFORMS = {"linux", "macos", "windows"}
READS = [
    "security_assignment_list",
    "security_audit_read",
    "security_key_list",
    "security_principal_list",
    "security_role_list",
    "security_status",
]
WRITES = [
    "security_assignment_revoke",
    "security_built_in_assignment_create",
    "security_custom_assignment_create",
    "security_custom_role_create",
    "security_principal_create",
    "security_principal_set_enabled",
]
CASES = {
    "conflict",
    "next_operation_revocation",
    "pagination",
    "readback",
    "redaction",
    "replay",
    "stale_cursor",
}
FORBIDDEN_FIELDS = {
    "api_key",
    "secret",
    "serialized",
    "verifier",
    "verifier_digest",
}
RECEIPT_FIELDS = {
    "binary",
    "cases",
    "distribution",
    "fixture_binary",
    "operations",
    "platform",
    "protocol",
    "python_version",
    "schema",
    "source_commit",
    "source_tree",
    "status",
    "transcript_sha256",
    "transports",
}


class ConformanceFailure(ValueError):
    """A receipt cannot support the claimed managed-client evidence."""


def fail(message: str) -> None:
    raise ConformanceFailure(message)


def _exact_keys(value: Any, expected: set[str], context: str) -> dict[str, Any]:
    if not isinstance(value, dict) or set(value) != expected:
        fail(f"{context} fields differ")
    return value


def _digest(value: Any, length: int, context: str) -> str:
    if not isinstance(value, str) or re.fullmatch(f"[0-9a-f]{{{length}}}", value) is None:
        fail(f"{context} is invalid")
    return value


def _reject_secret_vocabulary(value: Any) -> None:
    if isinstance(value, dict):
        for key, item in value.items():
            if key.casefold() in FORBIDDEN_FIELDS:
                fail("receipt contains forbidden credential vocabulary")
            _reject_secret_vocabulary(item)
    elif isinstance(value, list):
        for item in value:
            _reject_secret_vocabulary(item)
    elif isinstance(value, str) and value.startswith("hyp1_"):
        fail("receipt contains forbidden credential material")


def validate_schema_contract(schema: dict[str, Any]) -> None:
    """Keep the checked JSON Schema aligned with the executable validator."""

    if (
        schema.get("$schema") != "https://json-schema.org/draft/2020-12/schema"
        or schema.get("$comment") != "SPDX-License-Identifier: AGPL-3.0-only"
        or schema.get("$id")
        != "https://hyphae.dev/schema/python-managed-v2-conformance-receipt-v1"
        or schema.get("type") != "object"
        or schema.get("additionalProperties") is not False
        or set(schema.get("required", [])) != RECEIPT_FIELDS
    ):
        fail("receipt JSON Schema identity or fields differ")
    properties = _exact_keys(
        schema.get("properties"), RECEIPT_FIELDS, "schema properties"
    )
    if (
        properties["schema"] != {"const": SCHEMA}
        or properties["status"] != {"const": "passed"}
        or properties["platform"].get("enum") != sorted(PLATFORMS)
        or properties["protocol"].get("properties")
        != {"major": {"const": 1}, "minor": {"const": 2}}
        or properties["operations"].get("properties")
        != {"reads": {"const": READS}, "writes": {"const": WRITES}}
        or properties["cases"].get("properties")
        != {name: {"const": True} for name in sorted(CASES)}
    ):
        fail("receipt JSON Schema policy differs")


def validate_receipt(receipt: dict[str, Any]) -> dict[str, Any]:
    """Validate one exact OS lane without accepting extra evidence fields."""

    _reject_secret_vocabulary(receipt)
    _exact_keys(receipt, RECEIPT_FIELDS, "receipt")
    if receipt.get("schema") != SCHEMA or receipt.get("status") != "passed":
        fail("receipt identity or status differs")
    source_commit = _digest(receipt.get("source_commit"), 40, "source commit")
    source_tree = _digest(receipt.get("source_tree"), 40, "source tree")
    platform = receipt.get("platform")
    if platform not in PLATFORMS:
        fail("receipt platform is invalid")
    python_version = receipt.get("python_version")
    if (
        not isinstance(python_version, str)
        or re.fullmatch(r"3\.11\.\d+", python_version) is None
    ):
        fail("Python version is invalid")

    distribution = _exact_keys(
        receipt.get("distribution"), {"filename", "sha256"}, "distribution"
    )
    filename = distribution.get("filename")
    if not isinstance(filename, str) or re.fullmatch(
        r"hyphae_sdk-\d+\.\d+\.\d+-py3-none-any\.whl", filename
    ) is None:
        fail("wheel filename is invalid")
    wheel_digest = _digest(distribution.get("sha256"), 64, "wheel digest")

    binary = _exact_keys(receipt.get("binary"), {"filename", "sha256"}, "binary")
    expected_binary = "hyphae.exe" if platform == "windows" else "hyphae"
    if binary.get("filename") != expected_binary:
        fail("binary filename differs from platform")
    binary_digest = _digest(binary.get("sha256"), 64, "binary digest")
    fixture = _exact_keys(
        receipt.get("fixture_binary"), {"filename", "sha256"}, "fixture binary"
    )
    expected_fixture = (
        "hyphae-v2-fixture.exe" if platform == "windows" else "hyphae-v2-fixture"
    )
    if fixture.get("filename") != expected_fixture:
        fail("fixture binary filename differs from platform")
    fixture_digest = _digest(fixture.get("sha256"), 64, "fixture binary digest")

    protocol = _exact_keys(receipt.get("protocol"), {"major", "minor"}, "protocol")
    if protocol != {"major": 1, "minor": 2}:
        fail("protocol version differs")
    expected_transports = (
        ["http-v2", "named-pipe"]
        if platform == "windows"
        else ["af-unix", "http-v2"]
    )
    if receipt.get("transports") != expected_transports:
        fail("transport inventory differs from platform")
    operations = _exact_keys(
        receipt.get("operations"), {"reads", "writes"}, "operations"
    )
    if operations != {"reads": READS, "writes": WRITES}:
        fail("operation inventory differs")
    cases = _exact_keys(receipt.get("cases"), CASES, "cases")
    if any(cases[name] is not True for name in CASES):
        fail("conformance cases are incomplete")
    transcript = _digest(receipt.get("transcript_sha256"), 64, "transcript digest")
    return {
        "platform": platform,
        "python_version": python_version,
        "source_commit": source_commit,
        "source_tree": source_tree,
        "binary_sha256": binary_digest,
        "fixture_binary_sha256": fixture_digest,
        "wheel_filename": filename,
        "wheel_sha256": wheel_digest,
        "transcript_sha256": transcript,
    }


def validate_receipt_set(
    receipts: list[dict[str, Any]],
    *,
    expected_source_commit: str,
    expected_source_tree: str,
    receipt_sha256s: list[str],
) -> dict[str, Any]:
    """Require one passed receipt per supported OS and one exact source wheel."""

    expected_source_commit = _digest(
        expected_source_commit, 40, "expected source commit"
    )
    expected_source_tree = _digest(expected_source_tree, 40, "expected source tree")
    if len(receipt_sha256s) != len(receipts):
        fail("receipt digest inventory differs")
    receipt_sha256s = [
        _digest(value, 64, "receipt digest") for value in receipt_sha256s
    ]
    rows = [validate_receipt(receipt) for receipt in receipts]
    platforms = [row["platform"] for row in rows]
    if len(platforms) != len(set(platforms)):
        fail("duplicate platform receipt")
    missing = PLATFORMS - set(platforms)
    if missing:
        fail("missing platform receipt: " + ", ".join(sorted(missing)))
    extra = set(platforms) - PLATFORMS
    if extra:
        fail("unexpected platform receipt: " + ", ".join(sorted(extra)))
    identities = {(row["source_commit"], row["source_tree"]) for row in rows}
    if len(identities) != 1:
        fail("platform receipts must bind one source commit and tree")
    if identities != {(expected_source_commit, expected_source_tree)}:
        fail("platform receipts do not bind the expected source commit and tree")
    wheels = {(row["wheel_filename"], row["wheel_sha256"]) for row in rows}
    if len(wheels) != 1:
        fail("platform receipts must bind one exact wheel")
    transcripts = {row["transcript_sha256"] for row in rows}
    if len(transcripts) != 1:
        fail("platform receipts must bind one canonical transcript")
    source_commit, source_tree = next(iter(identities))
    wheel_filename, wheel_sha256 = next(iter(wheels))
    transcript_sha256 = next(iter(transcripts))
    lanes = []
    for row, receipt_sha256 in sorted(
        zip(rows, receipt_sha256s, strict=True), key=lambda item: item[0]["platform"]
    ):
        lanes.append(
            {
                "platform": row["platform"],
                "python_version": row["python_version"],
                "receipt_sha256": receipt_sha256,
                "binary_sha256": row["binary_sha256"],
                "fixture_binary_sha256": row["fixture_binary_sha256"],
                "transcript_sha256": row["transcript_sha256"],
            }
        )
    return {
        "schema": "hyphae-python-managed-v2-conformance-aggregate-v1",
        "status": "passed",
        "source_commit": source_commit,
        "source_tree": source_tree,
        "python_series": "3.11",
        "distribution": {"filename": wheel_filename, "sha256": wheel_sha256},
        "transcript_sha256": transcript_sha256,
        "platform_count": len(PLATFORMS),
        "platforms": sorted(platforms),
        "lanes": lanes,
    }


def _write_json(path: Path, value: dict[str, Any]) -> None:
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    temporary.replace(path)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--receipt", action="append", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--expected-source-commit", required=True)
    parser.add_argument("--expected-source-tree", required=True)
    args = parser.parse_args()
    try:
        schema = json.loads(SCHEMA_PATH.read_text(encoding="utf-8"))
        validate_schema_contract(schema)
        receipt_bytes = [path.read_bytes() for path in args.receipt]
        receipts = [json.loads(value) for value in receipt_bytes]
        result = validate_receipt_set(
            receipts,
            expected_source_commit=args.expected_source_commit,
            expected_source_tree=args.expected_source_tree,
            receipt_sha256s=[hashlib.sha256(value).hexdigest() for value in receipt_bytes],
        )
        _write_json(args.output, result)
    except (OSError, UnicodeError, json.JSONDecodeError, ConformanceFailure) as error:
        print(f"Python managed Native v2 conformance failed: {error}")
        return 1
    print(json.dumps(result, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
