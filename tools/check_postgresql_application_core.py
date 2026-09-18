#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Validate PostgreSQL Application Core v1 and its fail-closed parity claim."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import sys
import tomllib
from collections import Counter
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[1]
DEFAULT_PROFILE = ROOT / "conformance/postgresql/application-core-v1.json"
CASES_DIRECTORY = Path("conformance/postgresql/cases")
SCHEMA = "hyphae-postgresql-application-core-profile-v1"
REQUIRED_CASES = {
    "identifiers",
    "typed-parameters",
    "constraint-sqlstate",
    "redundant-limit-point-lookup",
    "like-bind",
    "returning",
    "on-conflict",
    "explicit-transaction-savepoint",
    "basic-joins",
    "group-by",
    "catalog-columns",
}
CASE_REQUIREMENTS = {
    "identifiers": "Unquoted identifiers fold to lowercase and quoted identifiers preserve exact case.",
    "typed-parameters": "Application-supplied integer, text, and boolean parameters retain declared types.",
    "constraint-sqlstate": "Unique, not-null, check, and foreign-key failures expose the PostgreSQL SQLSTATE classes used by applications.",
    "redundant-limit-point-lookup": "A primary-key point lookup accepts a redundant positive LIMIT without changing its result.",
    "like-bind": "A text LIKE pattern can be supplied as a typed application parameter.",
    "returning": "INSERT, UPDATE, and DELETE can return affected application columns in the same statement.",
    "on-conflict": "A primary-key conflict supports deterministic DO UPDATE and DO NOTHING outcomes.",
    "explicit-transaction-savepoint": "Explicit begin, commit, rollback, savepoint, and rollback-to-savepoint preserve the expected write set.",
    "basic-joins": "A parameterized indexed inner equijoin returns the matching application row.",
    "group-by": "A bounded GROUP BY with COUNT produces deterministic ordered groups.",
    "catalog-columns": "Applications can inspect table column name, ordinal, nullability, and data type through SQL catalog columns.",
}
CASE_ORACLE_SHA256 = {
    "basic-joins": "52a69cadf89f1a784395301dc5ed3ed51ce1e91158fbd5be4ba50544f0fa2544",
    "catalog-columns": "ca4a31c58c5581cbf1cee44cc637f210a8f140b8b421a57b7a66222da3b6a160",
    "constraint-sqlstate": "9a87d98a43e17aa28b7928c053ffe5b69c0ad13e0f7c650173405355ff32ef9b",
    "explicit-transaction-savepoint": "8f5a49499b3009960f630222c983f1623b76324af9ba121c53ab7092bede1192",
    "group-by": "43ede570d8ebceee1fbbe68c7f2228057069cbf71c6daff809bb23a27029356b",
    "identifiers": "5b67c852ea2d60ba17bfb817118315f1e37109d6c483158638b5f27259790500",
    "like-bind": "39ad3618f8f28a4f0626e617048de92ac1662173d07e88102a04dfbaf858d995",
    "on-conflict": "29634c77dc5a337200587972c24c0e2fc9fdc61098da67432bec0fa224be3959",
    "redundant-limit-point-lookup": "a36546365d5113d2d9fcd7d280823462850bbdde4cc3fe63adee995be719735d",
    "returning": "2c96e4453ebfc42d480eca8ca42a7898c8dcdfdcf91643b4b20d2500056f32c5",
    "typed-parameters": "52e9ea8db94960e82735a93f256eab5d8f75330a31382051cb853ac18bb3a11b",
}
CLASSIFICATIONS = {"conformant", "partial", "unsupported", "unverified"}
CLOSING_CLASSIFICATION = "conformant"
SOURCE_SHA256 = "555610c24d53e4316da5b7d3fc25c279d96856d5e0e23ee308c328c5fa881d9f"
MANIFEST_DIGEST = "sha256:1c59e2c3c818eaa0f0628f695b36e7c9e362d6b219b36a54a32df645cbd7e1af"
OCI_REGISTRY = "registry-1.docker.io"
OCI_REPOSITORY = "library/postgres"
CONTAINER_REFERENCE = f"docker.io/library/postgres:18.6-bookworm@{MANIFEST_DIGEST}"
CONFIGURATION = {
    "initdb_args": "--encoding=UTF8 --locale=C",
    "server_encoding": "UTF8",
    "client_encoding": "UTF8",
    "lc_collate": "C",
    "lc_ctype": "C",
    "timezone": "UTC",
    "settings": {
        "DateStyle": "ISO, YMD",
        "fsync": "on",
        "full_page_writes": "on",
        "jit": "off",
        "lc_messages": "C",
        "listen_addresses": "",
        "max_connections": "20",
        "max_parallel_workers_per_gather": "0",
        "shared_buffers": "128MB",
        "standard_conforming_strings": "on",
        "synchronous_commit": "on",
    },
}
FORBIDDEN_CARGO_PACKAGE = re.compile(r"(^|[-_])(postgres|postgresql|libpq)($|[-_])|^pq-sys$")
EXECUTABLE_AUTHORITIES = {
    "typed-parameters": {
        "kind": "cargo-test",
        "path": "crates/hyphae-native-runtime/tests/sql_typed_parameters.rs",
        "package": "hyphae-native-runtime",
        "test_target": "sql_typed_parameters",
        "test_name": "parameterized_insert_persists_declared_integer_text_and_boolean_types",
        "command": [
            "cargo",
            "test",
            "--locked",
            "-p",
            "hyphae-native-runtime",
            "--test",
            "sql_typed_parameters",
            "parameterized_insert_persists_declared_integer_text_and_boolean_types",
            "--",
            "--exact",
        ],
    },
    "basic-joins": {
        "kind": "cargo-test",
        "path": "crates/hyphae-native-runtime/tests/local_sql_select.rs",
        "package": "hyphae-native-runtime",
        "test_target": "local_sql_select",
        "test_name": "unix::uds_sql_matches_physical_plans_recovers_failures_and_reopens",
        "command": [
            "cargo",
            "test",
            "--locked",
            "-p",
            "hyphae-native-runtime",
            "--test",
            "local_sql_select",
            "unix::uds_sql_matches_physical_plans_recovers_failures_and_reopens",
            "--",
            "--exact",
        ],
    },
    "group-by": {
        "kind": "cargo-test",
        "path": "crates/hyphae-native-runtime/tests/sql_group_by_g2.rs",
        "package": "hyphae-native-runtime",
        "test_target": "sql_group_by_g2",
        "test_name": "projected_group_key_and_count_are_explicitly_ordered",
        "command": [
            "cargo",
            "test",
            "--locked",
            "-p",
            "hyphae-native-runtime",
            "--test",
            "sql_group_by_g2",
            "projected_group_key_and_count_are_explicitly_ordered",
            "--",
            "--exact",
        ],
    },
}


class GateFailure(RuntimeError):
    """The profile is malformed, weakens isolation, or overstates parity."""


def mapping(value: object, label: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise GateFailure(f"{label} must be an object")
    return value


def exact_fields(value: dict[str, Any], expected: set[str], label: str) -> None:
    if set(value) != expected:
        raise GateFailure(f"{label} has unknown or missing fields")


def resolve_contained_file(
    root: Path, raw_path: object, allowed_directory: Path, label: str
) -> Path:
    """Resolve a repository-relative file without allowing lexical or symlink escape."""

    if not isinstance(raw_path, str) or not raw_path:
        raise GateFailure(f"{label} must be a nonempty repository-relative path")
    relative = Path(raw_path)
    if relative.is_absolute() or ".." in relative.parts:
        raise GateFailure(f"{label} must not be absolute or contain '..'")
    try:
        root_resolved = root.resolve(strict=True)
        allowed_resolved = (root_resolved / allowed_directory).resolve(strict=True)
        allowed_resolved.relative_to(root_resolved)
        resolved = (root_resolved / relative).resolve(strict=True)
        resolved.relative_to(allowed_resolved)
    except (OSError, ValueError) as error:
        raise GateFailure(f"{label} escapes its allowed directory or does not exist") from error
    if not resolved.is_file():
        raise GateFailure(f"{label} must resolve to a file")
    return resolved


def validate_executable_authority(root: Path, case_id: str, raw_authority: object) -> None:
    expected = EXECUTABLE_AUTHORITIES.get(case_id)
    if expected is None or raw_authority != expected:
        raise GateFailure(f"case {case_id} lacks its case-specific executable Hyphae authority")
    authority = mapping(raw_authority, f"case {case_id} executable authority")
    source_path = resolve_contained_file(
        root,
        authority["path"],
        Path("crates/hyphae-native-runtime/tests"),
        f"case {case_id} executable authority path",
    )
    test_name = authority["test_name"].rsplit("::", 1)[-1]
    source = source_path.read_text(encoding="utf-8")
    test_definition = re.compile(
        rf"#\s*\[\s*test\s*\]\s*fn\s+{re.escape(test_name)}\s*\(", re.MULTILINE
    )
    if test_definition.search(source) is None:
        raise GateFailure(f"case {case_id} executable authority does not name a Rust test")


def validate_workspace_boundary(root: Path) -> None:
    workspace = tomllib.loads((root / "Cargo.toml").read_text(encoding="utf-8"))["workspace"]
    members = workspace.get("members", [])
    if any(member == "conformance/postgresql" or member.startswith("conformance/postgresql/") for member in members):
        raise GateFailure("PostgreSQL harness cannot be a Cargo workspace member")
    if (root / "conformance/postgresql/Cargo.toml").exists():
        raise GateFailure("PostgreSQL harness must remain outside Cargo packaging")
    lock = tomllib.loads((root / "Cargo.lock").read_text(encoding="utf-8"))
    forbidden = sorted(
        package["name"]
        for package in lock.get("package", [])
        if isinstance(package, dict)
        and isinstance(package.get("name"), str)
        and FORBIDDEN_CARGO_PACKAGE.search(package["name"].lower())
    )
    if forbidden:
        raise GateFailure("PostgreSQL packages entered the product lockfile: " + ", ".join(forbidden))


def validate_profile(root: Path, profile: dict[str, Any]) -> dict[str, Any]:
    """Validate exact inventory coverage and derive the only allowed claim state."""

    exact_fields(
        profile,
        {
            "$comment",
            "schema",
            "profile",
            "boundary",
            "postgresql",
            "status_classifications",
            "cases",
        },
        "profile",
    )
    if (
        profile.get("$comment") != "SPDX-License-Identifier: Apache-2.0"
        or profile.get("schema") != SCHEMA
    ):
        raise GateFailure("unsupported PostgreSQL application profile schema")

    metadata = mapping(profile["profile"], "profile metadata")
    exact_fields(metadata, {"id", "title", "priority", "parity_claim"}, "profile metadata")
    if (
        metadata.get("id") != "postgresql-application-core-v1"
        or metadata.get("title") != "PostgreSQL Application Core v1"
        or metadata.get("priority") != "P0"
    ):
        raise GateFailure("profile identity or priority changed")
    claim = mapping(metadata["parity_claim"], "parity claim")
    exact_fields(
        claim,
        {
            "asserted",
            "scope",
            "exact_language",
            "generic_postgresql_compatible_permitted",
        },
        "parity claim",
    )
    if (
        not isinstance(claim.get("asserted"), bool)
        or claim.get("scope") != metadata["title"]
        or claim.get("exact_language") != "Hyphae satisfies PostgreSQL Application Core v1"
        or claim.get("generic_postgresql_compatible_permitted") is not False
    ):
        raise GateFailure("parity claim is malformed")

    boundary = mapping(profile["boundary"], "boundary")
    exact_fields(
        boundary,
        {"role", "harness", "workspace_excluded", "prohibited_roles"},
        "boundary",
    )
    if (
        boundary.get("role") != "external-conformance-subject"
        or boundary.get("harness") != "conformance/postgresql"
        or boundary.get("workspace_excluded") is not True
        or boundary.get("prohibited_roles") != ["runtime", "planner", "executor", "storage", "cache"]
    ):
        raise GateFailure("PostgreSQL external-only boundary changed")
    validate_workspace_boundary(root)

    postgresql = mapping(profile["postgresql"], "PostgreSQL pin")
    exact_fields(postgresql, {"version", "server_version_num", "source", "container", "configuration"}, "PostgreSQL pin")
    if postgresql.get("version") != "18.6" or postgresql.get("server_version_num") != "180006":
        raise GateFailure("PostgreSQL version must remain exactly 18.6")
    source = mapping(postgresql["source"], "PostgreSQL source")
    exact_fields(source, {"authority", "uri", "sha256"}, "PostgreSQL source")
    if source != {
        "authority": "PostgreSQL Global Development Group",
        "uri": "https://ftp.postgresql.org/pub/source/v18.6/postgresql-18.6.tar.bz2",
        "sha256": SOURCE_SHA256,
    }:
        raise GateFailure("PostgreSQL source authority or checksum changed")
    container = mapping(postgresql["container"], "PostgreSQL container")
    exact_fields(
        container,
        {"authority", "reference", "manifest_digest", "registry", "repository"},
        "PostgreSQL container",
    )
    if container != {
        "authority": "Docker Official Images library/postgres",
        "reference": CONTAINER_REFERENCE,
        "manifest_digest": MANIFEST_DIGEST,
        "registry": OCI_REGISTRY,
        "repository": OCI_REPOSITORY,
    }:
        raise GateFailure("PostgreSQL container is not pinned to the reviewed manifest")
    if postgresql.get("configuration") != CONFIGURATION:
        raise GateFailure("PostgreSQL UTF-8/C/UTC configuration changed")

    classifications = mapping(profile["status_classifications"], "status classifications")
    if set(classifications) != CLASSIFICATIONS:
        raise GateFailure("status classifications changed")
    for name, raw_definition in classifications.items():
        definition = mapping(raw_definition, f"classification {name}")
        exact_fields(definition, {"closes_mandatory_cell", "meaning"}, f"classification {name}")
        if definition.get("closes_mandatory_cell") is not (name == CLOSING_CLASSIFICATION):
            raise GateFailure(f"classification {name} has unsafe closure semantics")
        if not isinstance(definition.get("meaning"), str) or not definition["meaning"]:
            raise GateFailure(f"classification {name} requires a meaning")

    cases = profile["cases"]
    if not isinstance(cases, list):
        raise GateFailure("cases must be an array")
    seen: set[str] = set()
    oracle_paths: set[Path] = set()
    executable_authorities: set[tuple[str, str]] = set()
    counts: Counter[str] = Counter()
    open_cases: list[str] = []
    for raw_case in cases:
        case = mapping(raw_case, "case")
        exact_fields(
            case,
            {"id", "priority", "mandatory", "requirement", "oracle", "oracle_sha256", "hyphae"},
            "case",
        )
        case_id = case.get("id")
        if not isinstance(case_id, str) or case_id in seen:
            raise GateFailure("case IDs must be nonempty and unique")
        seen.add(case_id)
        if case.get("priority") != "P0" or case.get("mandatory") is not True:
            raise GateFailure(f"case {case_id} must remain mandatory P0")
        if case.get("requirement") != CASE_REQUIREMENTS.get(case_id):
            raise GateFailure(f"case {case_id} requirement changed")
        oracle = case.get("oracle")
        oracle_path = resolve_contained_file(
            root, oracle, CASES_DIRECTORY, f"case {case_id} oracle"
        )
        if oracle != (CASES_DIRECTORY / f"{case_id}.sql").as_posix():
            raise GateFailure(f"case {case_id} oracle is not case-specific")
        oracle_sha256 = hashlib.sha256(oracle_path.read_bytes()).hexdigest()
        if (
            case.get("oracle_sha256") != CASE_ORACLE_SHA256.get(case_id)
            or oracle_sha256 != case["oracle_sha256"]
        ):
            raise GateFailure(f"case {case_id} oracle bytes differ from the reviewed digest")
        if oracle_path.suffix != ".sql" or oracle_path in oracle_paths:
            raise GateFailure(f"case {case_id} has an invalid or duplicate oracle")
        oracle_paths.add(oracle_path)
        hyphae = mapping(case["hyphae"], f"case {case_id} Hyphae cell")
        exact_fields(
            hyphae,
            {"classification", "evidence", "executable_authority", "note"},
            f"case {case_id} Hyphae cell",
        )
        classification = hyphae.get("classification")
        if classification not in CLASSIFICATIONS:
            raise GateFailure(f"case {case_id} has an unknown classification")
        evidence = hyphae.get("evidence")
        if (
            not isinstance(evidence, list)
            or not evidence
            or any(not isinstance(path, str) for path in evidence)
            or len(evidence) != len(set(evidence))
        ):
            raise GateFailure(f"case {case_id} requires unique repository evidence")
        for index, evidence_path in enumerate(evidence):
            resolve_contained_file(
                root,
                evidence_path,
                Path("."),
                f"case {case_id} evidence {index}",
            )
        authority = hyphae.get("executable_authority")
        if classification == CLOSING_CLASSIFICATION:
            validate_executable_authority(root, case_id, authority)
            authority_mapping = mapping(authority, f"case {case_id} executable authority")
            authority_identity = (authority_mapping["path"], authority_mapping["test_name"])
            if authority_identity in executable_authorities:
                raise GateFailure("executable Hyphae authorities must be case-specific and unique")
            executable_authorities.add(authority_identity)
        elif authority is not None:
            raise GateFailure(f"open case {case_id} cannot carry a closing executable authority")
        if not isinstance(hyphae.get("note"), str) or not hyphae["note"]:
            raise GateFailure(f"case {case_id} requires a current-status note")
        counts[classification] += 1
        if classification != CLOSING_CLASSIFICATION:
            open_cases.append(case_id)
    if seen != REQUIRED_CASES:
        raise GateFailure("inventory must contain the exact mandatory P0 case set")
    if claim["asserted"] and open_cases:
        raise GateFailure("PostgreSQL parity claim is forbidden while mandatory cells are open")

    eligible = not open_cases
    return {
        "schema": "hyphae-postgresql-application-core-audit-v1",
        "status": "passed",
        "profile": metadata["title"],
        "postgresql_version": postgresql["version"],
        "case_count": len(cases),
        "mandatory_count": len(cases),
        "classification_counts": dict(sorted(counts.items())),
        "open_mandatory_count": len(open_cases),
        "open_mandatory_cases": sorted(open_cases),
        "parity_eligible": eligible,
        "parity_claim_asserted": claim["asserted"],
        "exact_claim_language": claim["exact_language"],
        "generic_postgresql_compatible_permitted": False,
        "oracle_receipt_claim_authority": False,
        "parity_status": "asserted" if claim["asserted"] else ("eligible" if eligible else "blocked"),
        "workspace_boundary": "external-only",
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--profile", type=Path, default=DEFAULT_PROFILE)
    parser.add_argument("--require-parity", action="store_true")
    args = parser.parse_args()
    profile_path = args.profile if args.profile.is_absolute() else ROOT / args.profile
    try:
        result = validate_profile(ROOT, json.loads(profile_path.read_text(encoding="utf-8")))
    except (OSError, tomllib.TOMLDecodeError, json.JSONDecodeError, GateFailure) as error:
        print(f"PostgreSQL Application Core v1 profile failed: {error}", file=sys.stderr)
        return 2
    print(json.dumps(result, indent=2, sort_keys=True))
    if args.require_parity and not result["parity_eligible"]:
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
