#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Validate the source- and wheel-bound Windows async named-pipe receipt."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[1]
SCHEMA_PATH = ROOT / "conformance/v2/schema/python-windows-async-receipt.schema.json"
SCHEMA = "hyphae-python-windows-async-receipt-v1"
CASES = {
    "welcome_task_cancel_reconnect": ("cancelled_task", "reconnected"),
    "welcome_deadline_reconnect": ("deadline_exceeded", "reconnected"),
    "welcome_aclose_terminal": ("cancelled", "terminal"),
    "response_task_cancel_reconnect": ("cancelled_task", "reconnected"),
    "response_deadline_reconnect": ("deadline_exceeded", "reconnected"),
    "response_aclose_terminal": ("cancelled", "terminal"),
}
RECEIPT_FIELDS = {
    "cases",
    "distribution",
    "platform",
    "python_version",
    "schema",
    "source_commit",
    "source_tree",
    "status",
    "transcript_sha256",
    "transport",
}
TRANSCRIPT_FIELDS = {
    "cases",
    "distribution",
    "platform",
    "python_version",
    "schema",
    "status",
    "transport",
}
FORBIDDEN_FIELDS = {"api_key", "endpoint", "path", "secret", "serialized", "verifier"}


class WindowsAsyncGateFailure(ValueError):
    """The hosted receipt cannot support the Windows async claim."""


def fail(message: str) -> None:
    raise WindowsAsyncGateFailure(message)


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def _exact(value: Any, fields: set[str], context: str) -> dict[str, Any]:
    if not isinstance(value, dict) or set(value) != fields:
        fail(f"{context} fields differ")
    return value


def _digest(value: Any, length: int, context: str) -> str:
    if (
        not isinstance(value, str)
        or re.fullmatch(f"[0-9a-f]{{{length}}}", value) is None
    ):
        fail(f"{context} is invalid")
    return value


def _reject_sensitive(value: Any) -> None:
    if isinstance(value, dict):
        for key, item in value.items():
            if key.casefold() in FORBIDDEN_FIELDS:
                fail("receipt contains forbidden runtime or credential vocabulary")
            _reject_sensitive(item)
    elif isinstance(value, list):
        for item in value:
            _reject_sensitive(item)
    elif isinstance(value, str) and (
        value.startswith("hyp1_") or "\\\\.\\pipe\\" in value
    ):
        fail("receipt contains forbidden runtime or credential material")


def validate_schema_contract(schema: dict[str, Any]) -> None:
    """Keep the checked JSON Schema aligned with the executable validator."""

    expected_case_schemas = {
        name: {
            "allOf": [
                {"$ref": "#/$defs/case"},
                {
                    "properties": {
                        "error": {"const": error},
                        "recovery": {"const": recovery},
                    }
                },
            ]
        }
        for name, (error, recovery) in CASES.items()
    }
    if (
        schema.get("$schema") != "https://json-schema.org/draft/2020-12/schema"
        or schema.get("$comment") != "SPDX-License-Identifier: Apache-2.0"
        or schema.get("$id")
        != "https://hyphae.dev/schema/python-windows-async-receipt-v1"
        or schema.get("type") != "object"
        or schema.get("additionalProperties") is not False
        or set(schema.get("required", [])) != RECEIPT_FIELDS
    ):
        fail("receipt JSON Schema identity or fields differ")
    properties = _exact(schema.get("properties"), RECEIPT_FIELDS, "schema properties")
    if (
        properties["schema"] != {"const": SCHEMA}
        or properties["status"] != {"const": "passed"}
        or properties["platform"] != {"const": "windows"}
        or properties["transport"] != {"const": "named-pipe"}
        or set(properties["cases"].get("required", [])) != set(CASES)
        or properties["cases"].get("properties") != expected_case_schemas
        or schema.get("$defs", {}).get("case")
        != {
            "type": "object",
            "additionalProperties": False,
            "required": ["elapsed_millis", "error", "recovery"],
            "properties": {
                "elapsed_millis": {
                    "type": "integer",
                    "minimum": 0,
                    "exclusiveMaximum": 1000,
                },
                "error": {"enum": ["cancelled_task", "cancelled", "deadline_exceeded"]},
                "recovery": {"enum": ["reconnected", "terminal"]},
            },
        }
    ):
        fail("receipt JSON Schema policy differs")


def _wheel_version(filename: str) -> str:
    match = re.fullmatch(r"hyphae_sdk-(\d+\.\d+\.\d+)-py3-none-any\.whl", filename)
    if match is None:
        fail("wheel filename is invalid")
    return match.group(1)


def validate_transcript(transcript: object, wheel: Path) -> dict[str, Any]:
    """Validate the retained hosted observations before accepting their digest."""

    _reject_sensitive(transcript)
    row = _exact(transcript, TRANSCRIPT_FIELDS, "transcript")
    if (
        row.get("schema") != "hyphae-python-windows-async-transcript-v1"
        or row.get("status") != "passed"
        or row.get("platform") != "windows"
        or row.get("transport") != "named-pipe"
        or row.get("distribution")
        != {"filename": wheel.name, "version": _wheel_version(wheel.name)}
    ):
        fail("transcript identity differs")
    python_version = row.get("python_version")
    if (
        not isinstance(python_version, str)
        or re.fullmatch(r"3\.11\.\d+", python_version) is None
    ):
        fail("transcript Python version is invalid")
    cases = _exact(row.get("cases"), set(CASES), "transcript cases")
    _validate_cases(cases)
    return row


def _validate_cases(cases: dict[str, Any]) -> None:
    for name, (error, recovery) in CASES.items():
        row = _exact(cases[name], {"elapsed_millis", "error", "recovery"}, name)
        elapsed = row.get("elapsed_millis")
        if (
            isinstance(elapsed, bool)
            or not isinstance(elapsed, int)
            or not 0 <= elapsed < 1000
        ):
            fail(f"{name} did not terminate within one second")
        if row.get("error") != error or row.get("recovery") != recovery:
            fail(f"{name} typed outcome differs")


def validate_receipt(
    receipt: dict[str, Any],
    *,
    expected_source_commit: str,
    expected_source_tree: str,
    expected_wheel: Path,
    expected_transcript: Path,
) -> dict[str, Any]:
    """Require exact source, exact installed wheel, typed errors, and subsecond exit."""

    _reject_sensitive(receipt)
    _exact(receipt, RECEIPT_FIELDS, "receipt")
    if (
        receipt.get("schema") != SCHEMA
        or receipt.get("status") != "passed"
        or receipt.get("platform") != "windows"
        or receipt.get("transport") != "named-pipe"
    ):
        fail("receipt identity or platform differs")
    source_commit = _digest(receipt.get("source_commit"), 40, "source commit")
    source_tree = _digest(receipt.get("source_tree"), 40, "source tree")
    if source_commit != _digest(expected_source_commit, 40, "expected source commit"):
        fail("receipt does not bind the expected source commit")
    if source_tree != _digest(expected_source_tree, 40, "expected source tree"):
        fail("receipt does not bind the expected source tree")
    python_version = receipt.get("python_version")
    if (
        not isinstance(python_version, str)
        or re.fullmatch(r"3\.11\.\d+", python_version) is None
    ):
        fail("Python version is invalid")
    wheel = expected_wheel.resolve()
    if not wheel.is_file():
        fail("expected wheel is missing")
    distribution = _exact(
        receipt.get("distribution"), {"filename", "sha256"}, "distribution"
    )
    if distribution.get("filename") != wheel.name or distribution.get(
        "sha256"
    ) != sha256(wheel):
        fail("receipt does not bind the expected wheel")
    _wheel_version(wheel.name)
    cases = _exact(receipt.get("cases"), set(CASES), "cases")
    _validate_cases(cases)
    transcript_sha256 = _digest(
        receipt.get("transcript_sha256"), 64, "transcript digest"
    )
    transcript_path = expected_transcript.resolve()
    if not transcript_path.is_file():
        fail("expected transcript is missing")
    transcript_bytes = transcript_path.read_bytes()
    transcript = validate_transcript(json.loads(transcript_bytes), wheel)
    if transcript_sha256 != hashlib.sha256(transcript_bytes).hexdigest():
        fail("receipt does not bind the expected transcript")
    if transcript["python_version"] != python_version or transcript["cases"] != cases:
        fail("receipt and transcript observations differ")
    return {
        "source_commit": source_commit,
        "source_tree": source_tree,
        "wheel_sha256": distribution["sha256"],
        "transcript_sha256": transcript_sha256,
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--receipt", type=Path, required=True)
    parser.add_argument("--expected-source-commit", required=True)
    parser.add_argument("--expected-source-tree", required=True)
    parser.add_argument("--expected-wheel", type=Path, required=True)
    parser.add_argument("--expected-transcript", type=Path, required=True)
    args = parser.parse_args()
    try:
        validate_schema_contract(json.loads(SCHEMA_PATH.read_text(encoding="utf-8")))
        receipt = json.loads(args.receipt.read_text(encoding="utf-8"))
        validate_receipt(
            receipt,
            expected_source_commit=args.expected_source_commit,
            expected_source_tree=args.expected_source_tree,
            expected_wheel=args.expected_wheel,
            expected_transcript=args.expected_transcript,
        )
    except (OSError, json.JSONDecodeError, WindowsAsyncGateFailure) as error:
        print(f"python Windows async gate failed: {error}")
        return 1
    print("python Windows async gate: 6/6 hosted cases passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
