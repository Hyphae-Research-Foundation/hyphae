#!/usr/bin/env python3
"""Fail-closed validation for the Native G6 cross-surface corpus."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import sys
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[1]
G6 = ROOT / "conformance" / "g6"
CORPUS = G6 / "fixtures" / "corpus.json"
SCHEMA_DIR = G6 / "schema"
PLATFORMS = ("linux", "macos", "windows")
REQUIRED_LANES = (
    "embedded-rust",
    "cli",
    "local-daemon",
    "http",
    "rust-sdk-local",
    "rust-sdk-http",
    "python-sdk-local",
    "python-sdk-http",
    "typescript-sdk-local",
    "typescript-sdk-http",
)
REQUIRED_FAMILIES = (
    "capabilities",
    "catalog",
    "sql",
    "structures",
    "search",
    "transactions",
    "administration",
    "proofs",
    "backup",
    "failures",
    "transport-failures",
)
HEX_40 = re.compile(r"^[0-9a-f]{40}$")
HEX_64 = re.compile(r"^[0-9a-f]{64}$")
HEX_48 = re.compile(r"^[0-9a-f]{48}$")
ERROR_FIELDS = {"code", "category", "retry", "transaction_state", "request_id"}


class ConformanceFailure(RuntimeError):
    """The corpus, transcript, receipt, or aggregate is not trustworthy."""


def canonical_bytes(value: object) -> bytes:
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=True).encode()


def digest(value: object) -> str:
    return hashlib.sha256(canonical_bytes(value)).hexdigest()


def read_json(path: Path) -> Any:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise ConformanceFailure(f"cannot read canonical JSON {path}: {error}") from error


def corpus_digest() -> str:
    return digest(read_json(CORPUS))


def schema_digest() -> str:
    names = (
        "aggregate.schema.json",
        "corpus.schema.json",
        "receipt.schema.json",
        "transcript.schema.json",
    )
    return digest({name: read_json(SCHEMA_DIR / name) for name in names})


def validate_corpus(value: object) -> dict[str, Any]:
    if not isinstance(value, dict) or set(value) != {"schema", "lanes", "families", "applicability"}:
        raise ConformanceFailure("corpus has unknown or missing fields")
    if value.get("schema") != "hyphae-native-g6-corpus-v1":
        raise ConformanceFailure("unsupported G6 corpus schema")
    if value.get("lanes") != list(REQUIRED_LANES):
        raise ConformanceFailure("corpus lane identity or order differs from the required set")
    families = value.get("families")
    if not isinstance(families, dict) or tuple(families) != REQUIRED_FAMILIES:
        raise ConformanceFailure("corpus family identity or order differs from the required set")
    for family, cases in families.items():
        if not isinstance(cases, list) or not cases or not all(isinstance(item, str) and item for item in cases):
            raise ConformanceFailure(f"corpus family {family} is empty or malformed")
        if len(cases) != len(set(cases)):
            raise ConformanceFailure(f"corpus family {family} has duplicate cases")
    case_ids = [f"{family}/{case}" for family, cases in families.items() for case in cases]
    if len(case_ids) != len(set(case_ids)):
        raise ConformanceFailure("corpus has duplicate flattened case IDs")
    applicability = value.get("applicability")
    if not isinstance(applicability, dict):
        raise ConformanceFailure("corpus applicability is malformed")
    for case_id, lanes in applicability.items():
        if case_id not in case_ids:
            raise ConformanceFailure(f"corpus applicability names unknown case {case_id}")
        if (
            not isinstance(lanes, list)
            or not lanes
            or len(lanes) != len(set(lanes))
            or any(lane not in REQUIRED_LANES for lane in lanes)
            or lanes != [lane for lane in REQUIRED_LANES if lane in lanes]
        ):
            raise ConformanceFailure(f"corpus applicability for {case_id} is malformed")
        if lanes == list(REQUIRED_LANES):
            raise ConformanceFailure(f"corpus applicability for {case_id} is redundant")
    return value


def flattened_case_ids(corpus: dict[str, Any] | None = None) -> list[str]:
    checked = corpus if corpus is not None else validate_corpus(read_json(CORPUS))
    return [f"{family}/{case}" for family, cases in checked["families"].items() for case in cases]


def applicable_lanes(case_id: str, corpus: dict[str, Any] | None = None) -> list[str]:
    checked = corpus if corpus is not None else validate_corpus(read_json(CORPUS))
    return checked["applicability"].get(case_id, list(REQUIRED_LANES))


def lane_case_ids(lane: str, corpus: dict[str, Any] | None = None) -> list[str]:
    checked = corpus if corpus is not None else validate_corpus(read_json(CORPUS))
    return [case_id for case_id in flattened_case_ids(checked) if lane in applicable_lanes(case_id, checked)]


def lane_families(lane: str, corpus: dict[str, Any] | None = None) -> list[str]:
    checked = corpus if corpus is not None else validate_corpus(read_json(CORPUS))
    cases = set(lane_case_ids(lane, checked))
    return [family for family in REQUIRED_FAMILIES if any(case.startswith(f"{family}/") for case in cases)]


def validate_case_outcome(case_id: str, outcome: object, lane: str) -> None:
    if not isinstance(outcome, dict) or not outcome:
        raise ConformanceFailure(f"transcript {lane} case {case_id} has no semantic fields")
    if "/" not in case_id:
        raise ConformanceFailure(f"transcript {lane} case IDs do not exactly match the flattened fixture order")
    family, name = case_id.split("/", 1)
    if family not in REQUIRED_FAMILIES:
        raise ConformanceFailure(f"transcript {lane} case IDs do not exactly match the flattened fixture order")
    required = {
        "capabilities": {"product_api_version", "directory_format"},
        "catalog": {"snapshot", "object_ids"} if name == "catalog-list" else {"object_id", "present"},
        "sql": {"rows_affected", "object_id", "commit_csn"} if name in {"sql-ddl", "sql-dml"} else ({"columns", "rows", "snapshot"} if name == "sql-prepared" else {"version", "text"}),
        "structures": {"family", "value", "snapshot"},
        "search": {"mode", "snapshot", "object_ids", "approximate"},
        "transactions": {"status", "transaction_id"} if name == "commit-status" else {"staged_operations", "commit_csn"},
        "administration": {"snapshot"} if name == "status" else ({"registry_version", "metric_names"} if name == "telemetry" else {"status", "snapshot_verified"}),
        "proofs": {"kind", "anchor_digest", "proof_digest", "result_digest"} if name == "generate" else {"status", "kind", "anchor_digest", "proof_digest", "semantic_reexecution_performed"},
        "backup": {"visible_csn", "checkpoint_digest", "file_count", "total_bytes"} if name in {"create", "verify"} else ({"visible_csn", "checkpoint_digest", "doctor_status", "snapshot_verified"} if name == "restore" else {"status", "snapshot_verified"}),
        "failures": ERROR_FIELDS,
        "transport-failures": ERROR_FIELDS if name == "malformed-input" else ({"stalled", "resumed", "completed"} if name == "backpressure" else (ERROR_FIELDS if name == "missing-completion" else {"status", "transaction_state", "transaction_id"})),
    }[family]
    if not required.issubset(outcome):
        missing = sorted(required - set(outcome))
        raise ConformanceFailure(f"transcript {lane} case {case_id} is missing semantic fields: {missing}")
    if family == "failures" and (outcome["request_id"] is None or not isinstance(outcome["request_id"], str)):
        raise ConformanceFailure(f"transcript {lane} case {case_id} does not preserve request identity")


def validate_transcript(value: object, expected_lane: str | None = None) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise ConformanceFailure("transcript must be an object")
    fields = {"schema", "lane", "adapter", "transport", "start", "cases", "coverage", "status"}
    if set(value) != fields or value.get("schema") != "hyphae-native-g6-transcript-v1":
        raise ConformanceFailure("transcript has unknown fields or an unsupported schema")
    lane = value.get("lane")
    if lane not in REQUIRED_LANES or (expected_lane is not None and lane != expected_lane):
        raise ConformanceFailure("transcript lane identity is invalid")
    expected_surface = {
        "embedded-rust": ("rust", "embedded"),
        "cli": ("cli", "cli"),
        "local-daemon": ("rust", "native-local"),
        "http": ("http", "http-v2"),
        "rust-sdk-local": ("rust", "native-local"),
        "rust-sdk-http": ("rust", "http-v2"),
        "python-sdk-local": ("python", "native-local"),
        "python-sdk-http": ("python", "http-v2"),
        "typescript-sdk-local": ("typescript", "native-local"),
        "typescript-sdk-http": ("typescript", "http-v2"),
    }[lane]
    if (value.get("adapter"), value.get("transport")) != expected_surface:
        raise ConformanceFailure(f"transcript {lane} adapter/transport identity is invalid")
    if value.get("status") != "passed":
        raise ConformanceFailure(f"transcript {lane} did not pass")
    start = value.get("start")
    if not isinstance(start, dict) or set(start) != {"directory_lineage", "catalog_version", "visible_csn", "root_digest"}:
        raise ConformanceFailure(f"transcript {lane} starting identity is malformed")
    if not isinstance(start["directory_lineage"], str) or HEX_48.fullmatch(start["directory_lineage"]) is None:
        raise ConformanceFailure(f"transcript {lane} lineage is malformed")
    if not isinstance(start["root_digest"], str) or HEX_64.fullmatch(start["root_digest"]) is None:
        raise ConformanceFailure(f"transcript {lane} root digest is malformed")
    if not isinstance(start["catalog_version"], int) or isinstance(start["catalog_version"], bool) or start["catalog_version"] < 1:
        raise ConformanceFailure(f"transcript {lane} catalog version is malformed")
    if start["visible_csn"] is not None and (not isinstance(start["visible_csn"], int) or isinstance(start["visible_csn"], bool) or start["visible_csn"] < 1):
        raise ConformanceFailure(f"transcript {lane} visible CSN is malformed")
    coverage = value.get("coverage")
    if coverage != lane_families(lane):
        raise ConformanceFailure(f"transcript {lane} coverage does not match its applicable corpus families")
    cases = value.get("cases")
    if not isinstance(cases, list) or not cases:
        raise ConformanceFailure(f"transcript {lane} has no executed cases")
    ids: list[str] = []
    for case in cases:
        if not isinstance(case, dict) or set(case) != {"id", "outcome"} or not isinstance(case["id"], str):
            raise ConformanceFailure(f"transcript {lane} has a malformed case")
        ids.append(case["id"])
        validate_case_outcome(case["id"], case["outcome"], lane)
    expected_ids = lane_case_ids(lane)
    if ids != expected_ids:
        raise ConformanceFailure(f"transcript {lane} case IDs do not exactly match the flattened fixture order")
    return value


def comparable_transcript(value: dict[str, Any]) -> dict[str, Any]:
    cases = []
    for case in value["cases"]:
        case = json.loads(json.dumps(case))
        outcome = case["outcome"]
        for key in ("request_id", "transport_request_id", "transport_session_id"):
            if key in outcome:
                outcome[key] = "<transport-local>"
        for key in ("snapshot",):
            if isinstance(outcome.get(key), dict):
                outcome[key].pop("root_digest", None)
                outcome[key].pop("logical_time_micros", None)
        cases.append(case)
    return {"cases": cases}


def canonical_cross_lane(transcripts: list[dict[str, Any]]) -> dict[str, Any]:
    by_lane = {
        transcript["lane"]: {case["id"]: case for case in comparable_transcript(transcript)["cases"]}
        for transcript in transcripts
    }
    canonical = []
    for case_id in flattened_case_ids():
        lanes = applicable_lanes(case_id)
        outcomes = [by_lane[lane][case_id]["outcome"] for lane in lanes]
        compare_outcomes = json.loads(json.dumps(outcomes))
        if case_id.startswith("proofs/"):
            for outcome in compare_outcomes:
                outcome.pop("anchor_digest", None)
                outcome.pop("proof_digest", None)
                outcome.pop("result_digest", None)
        if case_id.startswith("backup/"):
            for outcome in compare_outcomes:
                outcome.pop("checkpoint_digest", None)
                outcome.pop("file_count", None)
                outcome.pop("total_bytes", None)
        if any(canonical_bytes(outcome) != canonical_bytes(compare_outcomes[0]) for outcome in compare_outcomes[1:]):
            details = ", ".join(f"{lane}={outcome!r}" for lane, outcome in zip(lanes, outcomes, strict=True))
            raise ConformanceFailure(f"canonical outcome mismatch for {case_id} across applicable lanes: {details}")
        canonical.append({"id": case_id, "applicable_lanes": lanes, "outcome": compare_outcomes[0]})
    return {"cases": canonical}


def validate_starting_equivalence(transcripts: list[dict[str, Any]]) -> None:
    baseline = transcripts[0]["start"]
    for transcript in transcripts[1:]:
        start = transcript["start"]
        if start != baseline:
            raise ConformanceFailure(f"starting native identity mismatch in lane {transcript['lane']}")


def validate_receipt(value: object) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise ConformanceFailure("receipt must be an object")
    fields = {"schema", "source_commit", "platform", "status", "corpus_digest", "schema_digest", "transcript_digest", "lanes"}
    if set(value) != fields or value.get("schema") != "hyphae-native-g6-conformance-receipt-v1":
        raise ConformanceFailure("receipt has unknown fields or an unsupported schema")
    if not isinstance(value.get("source_commit"), str) or HEX_40.fullmatch(value["source_commit"]) is None:
        raise ConformanceFailure("receipt source commit is malformed")
    if value.get("platform") not in PLATFORMS or value.get("status") != "passed":
        raise ConformanceFailure("receipt platform or status is invalid")
    if value.get("corpus_digest") != corpus_digest() or value.get("schema_digest") != schema_digest():
        raise ConformanceFailure("receipt corpus or schema digest differs from the checked-in authority")
    lanes = value.get("lanes")
    if not isinstance(lanes, list) or [lane.get("lane") if isinstance(lane, dict) else None for lane in lanes] != list(REQUIRED_LANES):
        raise ConformanceFailure("receipt does not contain every required lane exactly in order")
    transcripts = [validate_transcript(lane, expected) for lane, expected in zip(lanes, REQUIRED_LANES, strict=True)]
    validate_starting_equivalence(transcripts)
    baseline = canonical_cross_lane(transcripts)
    failure_ids = [case_id for case_id in flattened_case_ids() if case_id.startswith("failures/")]
    for case_id in failure_ids:
        lanes_for_case = applicable_lanes(case_id)
        outcomes = [next(case["outcome"] for case in transcript["cases"] if case["id"] == case_id) for transcript in transcripts if transcript["lane"] in lanes_for_case]
        stable = [{field: outcome[field] for field in ERROR_FIELDS - {"request_id"}} for outcome in outcomes]
        if any(value != stable[0] for value in stable[1:]):
            raise ConformanceFailure(f"stable error parity mismatch for {case_id}")
    expected_digest = digest(baseline)
    if value.get("transcript_digest") != expected_digest:
        raise ConformanceFailure("receipt transcript digest does not match canonical lane output")
    return value


def aggregate(receipts: list[object]) -> dict[str, Any]:
    if len(receipts) != len(PLATFORMS):
        raise ConformanceFailure("aggregate requires exactly three platform receipts")
    checked = [validate_receipt(value) for value in receipts]
    if [value["platform"] for value in checked] != list(PLATFORMS):
        raise ConformanceFailure("aggregate receipts must be ordered linux, macos, windows")
    for field in ("source_commit", "corpus_digest", "schema_digest", "transcript_digest"):
        if len({value[field] for value in checked}) != 1:
            raise ConformanceFailure(f"platform receipts disagree on {field}")
    return {
        "schema": "hyphae-native-g6-conformance-aggregate-v1",
        "source_commit": checked[0]["source_commit"],
        "status": "passed",
        "platforms": list(PLATFORMS),
        "corpus_digest": checked[0]["corpus_digest"],
        "schema_digest": checked[0]["schema_digest"],
        "transcript_digest": checked[0]["transcript_digest"],
    }


def write_or_print(value: object, output: Path | None) -> None:
    encoded = json.dumps(value, indent=2, sort_keys=True) + "\n"
    if output is None:
        sys.stdout.write(encoded)
    else:
        output.parent.mkdir(parents=True, exist_ok=True)
        output.write_text(encoded, encoding="utf-8")


def main() -> int:
    parser = argparse.ArgumentParser()
    subparsers = parser.add_subparsers(dest="command", required=True)
    corpus_parser = subparsers.add_parser("corpus")
    corpus_parser.add_argument("--output", type=Path)
    receipt_parser = subparsers.add_parser("receipt")
    receipt_parser.add_argument("--receipt", type=Path, required=True)
    receipt_parser.add_argument("--output", type=Path)
    aggregate_parser = subparsers.add_parser("aggregate")
    aggregate_parser.add_argument("--receipt", type=Path, action="append", required=True)
    aggregate_parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    try:
        if args.command == "corpus":
            result: object = {
                "schema": "hyphae-native-g6-corpus-audit-v1",
                "status": "passed",
                "corpus_digest": corpus_digest(),
                "schema_digest": schema_digest(),
                "corpus": validate_corpus(read_json(CORPUS)),
            }
        elif args.command == "receipt":
            result = validate_receipt(read_json(args.receipt))
        else:
            result = aggregate([read_json(path) for path in args.receipt])
        write_or_print(result, args.output)
        return 0
    except ConformanceFailure as error:
        print(f"native G6 conformance failed: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
