#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Validate the external Valkey Application Core authority and claim boundary."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import subprocess
import sys
import tomllib
from collections import Counter
from pathlib import Path
from typing import Any

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[2]
DEFAULT_PROFILE = HERE / "application-core-v1.json"

EXPECTED_SCHEMA = "hyphae-external-valkey-application-core-v1"
EXPECTED_VERSION = "9.1.2"
EXPECTED_TAG = "9.1.2"
EXPECTED_COMMIT = "7f1dffedff6de73058b2c2a389422b6ecd56c8fb"
EXPECTED_TREE = "c8cbc1ffb2c4526c3f487aaeadcb0234636f5ca5"
EXPECTED_ARTIFACT_SHA256 = "19c23908e7d57e8d91ef85b41f5646307582f10f4f0fb999bbf89ed24ec9c983"
EXPECTED_HYPHAE_COMMIT = "7e25fc7eaa7fe91bc55e16314074d66ad943b090"
EXPECTED_HYPHAE_TREE = "aea2680c992697d68c2b52582f8399f9189108ac"
EXPECTED_CLAIM_SEMANTICS_SEAL = "22ab321f7581e6bca4399f82a17e23d2ff7cd0d7ae842b3d1a913947073e3b67"
EXPECTED_PROFILE_SHA256 = "8523a048328cbd9b3616a1dc2119ffedfbefb65f8674610683380bd1ffc742ff"
EXPECTED_SEMANTIC_BUNDLE_SHA256 = "5bc88d810acad8d5ed6deb4d2dd4c9bfb04350668a93ce045af59ec786233a93"
EXPECTED_BASELINE_RECEIPT_SCHEMA = "hyphae-baseline-harness-v2"
EXPECTED_VALKEY_RECEIPT_SCHEMA = "hyphae-external-valkey-baseline-receipt-v2"
CLASSIFICATIONS = {"exact", "equivalent", "stronger", "different", "excluded"}
LANE_SETTINGS = {
    "no": {"appendonly": "no", "appendfsync": "no"},
    "always": {"appendonly": "yes", "appendfsync": "always"},
    "everysec": {"appendonly": "yes", "appendfsync": "everysec"},
}
SEMANTIC_BUNDLE_DOMAIN = b"hyphae-valkey-semantic-source-bundle-v1\0"
SELF_SEAL_CONSTANT = re.compile(
    rb'(?m)^(EXPECTED_(?:CLAIM_SEMANTICS_SEAL|PROFILE_SHA256|SEMANTIC_BUNDLE_SHA256) = )"[0-9a-f]{64}"$'
)
PROFILE_KEYS = {
    "schema",
    "profile",
    "profile_version",
    "status",
    "foundation_only",
    "claim_semantics_seal",
    "oracle",
    "hyphae_authority",
    "executed_source_binding",
    "claim_policy",
    "classification_definitions",
    "evidence_authorities",
    "configuration_authority",
    "inventory_scope",
    "inventory",
    "summary",
}


class ProfileError(Exception):
    """One or more profile invariants failed."""


def _reject_duplicate_keys(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise ProfileError(f"duplicate JSON key {key!r}")
        result[key] = value
    return result


def _reject_nonfinite(value: str) -> None:
    raise ProfileError(f"non-finite JSON number {value} is forbidden")


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(128 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _config_values(path: Path) -> dict[str, str]:
    values: dict[str, str] = {}
    for raw_line in path.read_text(encoding="utf-8").splitlines():
        line = raw_line.strip()
        if not line or line.startswith("#"):
            continue
        key, separator, value = line.partition(" ")
        if not separator:
            raise ProfileError(f"{path}: malformed config line: {raw_line}")
        values[key] = value.strip().strip('"')
    return values


def claim_semantics(profile: dict[str, Any]) -> dict[str, Any]:
    return {
        "profile": profile.get("profile"),
        "profile_version": profile.get("profile_version"),
        "status": profile.get("status"),
        "foundation_only": profile.get("foundation_only"),
        "claim_policy": profile.get("claim_policy"),
        "classification_definitions": profile.get("classification_definitions"),
        "configuration_authority": profile.get("configuration_authority"),
        "evidence_authorities": profile.get("evidence_authorities"),
        "hyphae_authority": profile.get("hyphae_authority"),
        "executed_source_binding": profile.get("executed_source_binding"),
        "inventory": profile.get("inventory"),
        "inventory_scope": profile.get("inventory_scope"),
        "oracle": profile.get("oracle"),
    }


def claim_semantics_sha256(profile: dict[str, Any]) -> str:
    encoded = json.dumps(
        claim_semantics(profile),
        ensure_ascii=True,
        separators=(",", ":"),
        sort_keys=True,
    ).encode("utf-8")
    return hashlib.sha256(encoded).hexdigest()


def _binding_paths(binding: dict[str, Any]) -> tuple[list[str], list[str]]:
    self_checked = binding.get("self_checked_paths")
    grouped = [
        binding.get("authority_paths"),
        binding.get("producer_validator_paths"),
        binding.get("test_paths"),
    ]
    if not isinstance(self_checked, list) or any(not isinstance(group, list) for group in grouped):
        raise ProfileError("executed source binding paths must be arrays")
    semantic = [path for group in grouped for path in group]
    all_paths = [*self_checked, *semantic]
    if not all_paths or not all(isinstance(path, str) and path for path in all_paths):
        raise ProfileError("executed source binding paths must be nonempty strings")
    if len(set(all_paths)) != len(all_paths):
        raise ProfileError("executed source binding paths must be unique")
    return self_checked, semantic


def semantic_bundle_sha256(root: Path, paths: list[str], source_commit: str | None = None) -> str:
    digest = hashlib.sha256(SEMANTIC_BUNDLE_DOMAIN)
    for relative in sorted(paths):
        encoded_path = relative.encode("utf-8")
        if source_commit is None:
            encoded = (root / relative).read_bytes()
        else:
            encoded = subprocess.run(
                ["git", "-C", str(root), "show", f"{source_commit}:{relative}"],
                check=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                timeout=10,
            ).stdout
        if relative == "benchmarks/baseline-harness/valkey/check_profile.py":
            encoded = SELF_SEAL_CONSTANT.sub(rb'\1"<sealed-value>"', encoded)
        digest.update(len(encoded_path).to_bytes(8, "big"))
        digest.update(encoded_path)
        digest.update(len(encoded).to_bytes(8, "big"))
        digest.update(encoded)
    return digest.hexdigest()


def runner_source_errors(
    source: str, artifact_url: str, lanes: list[dict[str, Any]]
) -> list[str]:
    required_source = [
        f"VALKEY_ARTIFACT_SHA256={EXPECTED_ARTIFACT_SHA256}",
        f"VALKEY_ARTIFACT_URL={artifact_url}",
        'HYPHAE_VALKEY_PROFILE_SHA256=$(sha256sum "$VALKEY_PROFILE_PATH"',
        "HYPHAE_VALKEY_SEMANTIC_BUNDLE_SHA256=$(python3",
        "--semantic-bundle-only",
        "HYPHAE_VALKEY_CLAIM_SEMANTICS_SHA256=$(python3",
        "--claim-seal-only",
        'test "$(sha256sum "$VALKEY_ARCHIVE"',
        'VALKEY_SERVER="$VALKEY_BUILD/src/valkey-server"',
        'HYPHAE_VALKEY_SERVER_SHA256=$(sha256sum "$VALKEY_SERVER"',
        'install -m 0555 "$VALKEY_SERVER" "$RETAINED_VALKEY_SERVER"',
        'export HYPHAE_VALKEY_RETAINED_SERVER_ARTIFACT="$RETAINED_VALKEY_SERVER"',
        'export HYPHAE_VALKEY_SOURCE_ARCHIVE_URL="$VALKEY_ARTIFACT_URL"',
        'export HYPHAE_VALKEY_SOURCE_ARCHIVE_SHA256="$VALKEY_ARTIFACT_SHA256"',
        "export HYPHAE_VALKEY_COMPILER",
        "export HYPHAE_VALKEY_BUILD_FLAGS=",
        "export HYPHAE_VALKEY_SETUP_ID",
        "VALKEY_SETUP_NONCE=$(</proc/sys/kernel/random/uuid)",
        'test -z "$(git status --porcelain=v1 --untracked-files=all)"',
        "HYPHAE_SOURCE_PRE_COMMIT=$(git rev-parse 'HEAD^{commit}')",
        "HYPHAE_SOURCE_PRE_TREE=$(git rev-parse 'HEAD^{tree}')",
        "export HYPHAE_SOURCE_PRE_CLEAN=true",
        "HYPHAE_SOURCE_POST_COMMIT=$(git rev-parse 'HEAD^{commit}')",
        "HYPHAE_SOURCE_POST_TREE=$(git rev-parse 'HEAD^{tree}')",
        "export HYPHAE_SOURCE_POST_CLEAN=true",
        'test "$HYPHAE_SOURCE_POST_COMMIT" = "$HYPHAE_SOURCE_PRE_COMMIT"',
        'test "$HYPHAE_SOURCE_POST_TREE" = "$HYPHAE_SOURCE_PRE_TREE"',
        "export RUSTFLAGS=",
        "export CARGO_ENCODED_RUSTFLAGS=",
        "export CARGO_INCREMENTAL=0",
        "unset RUSTC RUSTC_WRAPPER RUSTC_WORKSPACE_WRAPPER",
        "export HYPHAE_RUSTFLAGS_STATE=empty",
        "export HYPHAE_CARGO_ENCODED_RUSTFLAGS_STATE=empty",
        "export HYPHAE_RUSTC_WRAPPER_STATE=unset",
        "export HYPHAE_RUSTC_WORKSPACE_WRAPPER_STATE=unset",
        "export HYPHAE_CARGO_PROFILE_OVERRIDES_STATE=absent",
        "export HYPHAE_CARGO_INCREMENTAL_STATE=disabled",
        "export HYPHAE_BUILD_PROFILE=release",
        'test -z "$(compgen -A variable CARGO_PROFILE_)"',
        "HYPHAE_RUSTC_VERBOSE=$(rustc -vV)",
        "HYPHAE_CARGO_VERSION=$(cargo --version)",
        "export HYPHAE_BUILD_COMMAND=",
        'test "$HARDWARE_PRODUCT_NAME" = "i7i.metal-24xl"',
        'test "$(nproc)" -eq 96',
        'test "$PHYSICAL_CORES" -eq 48',
        'test "$SMT_THREADS" -eq 2',
        'test "$SOCKETS" -eq 2',
        'test "$CPU_AFFINITY" = "0-95"',
        'test "$CPU_QUOTA" = max',
        "grep -qw hypervisor /proc/cpuinfo",
        "MemTotal:",
        "lscpu -p=CPU,CORE,SOCKET",
        'NVME_SOURCE=$(findmnt -n -o SOURCE --target /mnt/nvme)',
        'test "$EC2_NVME_MODEL" = "Amazon EC2 NVMe Instance Storage"',
        "export HYPHAE_RECEIPT_AUTHORITY=authoritative-dedicated-hardware",
        "export HYPHAE_HARDWARE_QUALIFICATION=aws-ec2-i7i.metal-24xl",
        'export HYPHAE_EC2_NVME_MODEL="$EC2_NVME_MODEL"',
        'export HYPHAE_NVME_DEVICE_ID="$NVME_DEVICE_ID"',
        'export HYPHAE_NVME_FILESYSTEM="$NVME_FILESYSTEM"',
        "export HYPHAE_NVME_ROTATIONAL=false",
        'export HYPHAE_NVME_QUEUE_DEPTH="$NVME_QUEUE_DEPTH"',
        'HYPHAE_HARNESS_PRODUCT_BINARY_SHA256=$(sha256sum "$HARNESS"',
        "rm -rf /mnt/nvme/valkey-no /mnt/nvme/valkey-always /mnt/nvme/valkey-everysec",
        "pgrep -x valkey-server",
        "/run/hyphae-valkey-no.pid /run/hyphae-valkey-always.pid /run/hyphae-valkey-everysec.pid",
        'export HYPHAE_VALKEY_NO_PID="$NO_PID"',
        'export HYPHAE_VALKEY_ALWAYS_PID="$ALWAYS_PID"',
        'export HYPHAE_VALKEY_EVERYSEC_PID="$EVERYSEC_PID"',
        ".hyphae-fresh-setup",
        'check_receipt.py "$OUT/keyspace.json"',
        '--expected-source-commit "$HYPHAE_SOURCE_POST_COMMIT"',
        '--expected-source-tree "$HYPHAE_SOURCE_POST_TREE"',
        '--harness "$HARNESS"',
        '--valkey-binary-artifact "$RETAINED_VALKEY_SERVER"',
        '--source-root "$REPO"',
        "--require-authoritative",
        "BUILD_TLS=no MALLOC=",
        "CC=",
        "CFLAGS=",
        "LDFLAGS=",
    ]
    for lane in lanes:
        name = lane.get("name")
        digest = lane.get("sha256")
        if isinstance(name, str) and isinstance(digest, str):
            variable = f"VALKEY_{name.upper()}_CONFIG_SHA256={digest}"
            required_source.extend(
                [
                    variable,
                    f'sha256sum "$VALKEY_CONFIG/valkey-{name}.conf"',
                ]
            )
    return [
        f"metal runner is missing exact Valkey identity binding {required!r}"
        for required in required_source
        if required not in source
    ]


def check_profile(
    profile_path: Path = DEFAULT_PROFILE,
    repo_root: Path = ROOT,
    executed_source_commit: str | None = None,
) -> dict[str, Any]:
    errors: list[str] = []
    try:
        profile = json.loads(
            profile_path.read_text(encoding="utf-8"),
            object_pairs_hook=_reject_duplicate_keys,
            parse_constant=_reject_nonfinite,
        )
    except (OSError, json.JSONDecodeError) as error:
        raise ProfileError(f"cannot load {profile_path}: {error}") from error

    oracle = profile.get("oracle", {})
    if _sha256(profile_path) != EXPECTED_PROFILE_SHA256:
        errors.append("full profile byte digest differs from the reviewed authority")
    if set(profile) != PROFILE_KEYS:
        errors.append(f"profile field set must be exactly {sorted(PROFILE_KEYS)}")
    if (
        profile.get("profile") != "Valkey Application Core"
        or profile.get("profile_version") != 1
        or profile.get("status") != "foundation_only"
        or profile.get("foundation_only") is not True
    ):
        errors.append("profile identity must remain exact foundation-only version 1")
    expected_pins = {
        "schema": (profile.get("schema"), EXPECTED_SCHEMA),
        "oracle.version": (oracle.get("version"), EXPECTED_VERSION),
        "oracle.tag": (oracle.get("tag"), EXPECTED_TAG),
        "oracle.source_commit": (oracle.get("source_commit"), EXPECTED_COMMIT),
        "oracle.source_tree": (oracle.get("source_tree"), EXPECTED_TREE),
        "oracle.artifact.sha256": (
            oracle.get("artifact", {}).get("sha256"),
            EXPECTED_ARTIFACT_SHA256,
        ),
        "hyphae_authority.source_commit": (
            profile.get("hyphae_authority", {}).get("source_commit"),
            EXPECTED_HYPHAE_COMMIT,
        ),
        "hyphae_authority.source_tree": (
            profile.get("hyphae_authority", {}).get("source_tree"),
            EXPECTED_HYPHAE_TREE,
        ),
    }
    for field, (actual, expected) in expected_pins.items():
        if actual != expected:
            errors.append(f"{field} must be {expected!r}, found {actual!r}")

    artifact_url = oracle.get("artifact", {}).get("url", "")
    if artifact_url != "https://codeload.github.com/valkey-io/valkey/tar.gz/refs/tags/9.1.2":
        errors.append("oracle artifact URL is not the pinned 9.1.2 source archive")
    if oracle.get("runtime_dependency") is not False:
        errors.append("Valkey must remain an external oracle, not a runtime dependency")

    claims = profile.get("claim_policy", {})
    for field in ("valkey_compatible", "redis_compatible", "resp_surface"):
        if claims.get(field) is not False:
            errors.append(f"claim_policy.{field} must be false")
    if claims.get("allowed_product_description") != "native keyspace/data-structure engine":
        errors.append("the allowed product description must remain native keyspace/data-structure engine")
    prohibited = set(claims.get("prohibited_claims", []))
    required_prohibited = {
        "Valkey-compatible",
        "Redis-compatible",
        "drop-in replacement",
        "RESP-compatible",
    }
    if prohibited != required_prohibited:
        errors.append("claim_policy.prohibited_claims must retain the complete compatibility deny-list")
    definitions = profile.get("classification_definitions", {})
    if not isinstance(definitions, dict) or set(definitions) != CLASSIFICATIONS:
        errors.append("classification definitions must cover exactly all five classifications")

    binding = profile.get("executed_source_binding", {})
    expected_binding_keys = {
        "policy",
        "profile_path",
        "self_checked_paths",
        "authority_paths",
        "producer_validator_paths",
        "test_paths",
        "semantic_bundle_sha256",
        "semantic_change_requires_profile_revision",
    }
    if not isinstance(binding, dict) or set(binding) != expected_binding_keys:
        errors.append("executed source binding schema differs")
        self_checked_paths: list[str] = []
        semantic_paths: list[str] = []
    else:
        try:
            self_checked_paths, semantic_paths = _binding_paths(binding)
        except ProfileError as error:
            errors.append(str(error))
            self_checked_paths, semantic_paths = [], []
        required_self_checked = {
            "benchmarks/baseline-harness/valkey/application-core-v1.json",
        }
        if set(self_checked_paths) != required_self_checked:
            errors.append("executed source self-checked profile path set differs")
        if "benchmarks/baseline-harness/valkey/check_profile.py" not in semantic_paths:
            errors.append("sealed semantic bundle omits its normalized profile checker")
        required_producer_paths = {
            ".github/workflows/ci.yml",
            "Cargo.toml",
            "Cargo.lock",
            "rust-toolchain.toml",
            "benchmarks/baseline-harness/Cargo.toml",
            "benchmarks/baseline-harness/Cargo.lock",
            "benchmarks/baseline-harness/scripts/run-metal.sh",
            "benchmarks/baseline-harness/src/main.rs",
            "benchmarks/baseline-harness/src/ablation_suite.rs",
            "benchmarks/baseline-harness/src/keyspace_suite.rs",
            "benchmarks/baseline-harness/src/lexical_suite.rs",
            "benchmarks/baseline-harness/src/sql_suite.rs",
            "benchmarks/baseline-harness/src/util.rs",
            "benchmarks/baseline-harness/valkey/check_profile.py",
            "benchmarks/baseline-harness/valkey/check_receipt.py",
        }
        if not required_producer_paths.issubset(set(semantic_paths)):
            errors.append("sealed semantic bundle omits a producer/build/workload source")
        evidence_paths = set(profile.get("evidence_authorities", {}).values())
        if not evidence_paths.issubset(set(semantic_paths)):
            errors.append("sealed semantic bundle omits an evidence authority")
        required_semantic_authority_paths = {
            "crates/hyphae-native-runtime/src/lib.rs",
            "crates/hyphae-native-runtime/src/model.rs",
            "crates/hyphae-native-runtime/src/set_algebra.rs",
            "crates/hyphae-native-runtime/src/structure_v3.rs",
            "crates/hyphae-native-runtime/src/wal_codec.rs",
            "crates/hyphae-native-product/src/lib.rs",
            "crates/hyphae-native-product/src/structures.rs",
            "crates/hyphae-native-product/src/operation.rs",
            "crates/hyphae-native-product/src/limits.rs",
            "crates/hyphae-native-product/src/error.rs",
            "crates/hyphae-native-product/src/error_codec.rs",
            "crates/hyphae-native-protocol/src/product.rs",
        }
        if not required_semantic_authority_paths.issubset(set(semantic_paths)):
            errors.append("sealed semantic bundle omits an implementation/limit/codec authority")
        if (
            binding.get("policy") != "exact-profile-and-sealed-semantic-bundle-v1"
            or binding.get("profile_path")
            != "benchmarks/baseline-harness/valkey/application-core-v1.json"
            or binding.get("semantic_change_requires_profile_revision") is not True
        ):
            errors.append("executed source binding policy differs")
    try:
        actual_semantic_bundle = semantic_bundle_sha256(repo_root, semantic_paths)
    except (OSError, subprocess.SubprocessError) as error:
        errors.append(f"cannot read semantic source bundle: {error}")
        actual_semantic_bundle = ""
    if (
        binding.get("semantic_bundle_sha256") != EXPECTED_SEMANTIC_BUNDLE_SHA256
        or actual_semantic_bundle != EXPECTED_SEMANTIC_BUNDLE_SHA256
    ):
        errors.append(
            "semantic authority/test bytes changed; a profile revision is required; "
            f"actual {actual_semantic_bundle}"
        )
    if executed_source_commit is not None:
        if re.fullmatch(r"[0-9a-f]{40}", executed_source_commit) is None:
            errors.append("executed source commit must be one lowercase Git object identity")
        else:
            try:
                for relative in [*self_checked_paths, *semantic_paths]:
                    committed = subprocess.run(
                        ["git", "-C", str(repo_root), "show", f"{executed_source_commit}:{relative}"],
                        check=True,
                        stdout=subprocess.PIPE,
                        stderr=subprocess.PIPE,
                        timeout=10,
                    ).stdout
                    if committed != (repo_root / relative).read_bytes():
                        errors.append(f"executed source path differs from clean tree: {relative}")
                committed_profile = subprocess.run(
                    [
                        "git",
                        "-C",
                        str(repo_root),
                        "show",
                        f"{executed_source_commit}:{binding.get('profile_path', '')}",
                    ],
                    check=True,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    timeout=10,
                ).stdout
                if hashlib.sha256(committed_profile).hexdigest() != EXPECTED_PROFILE_SHA256:
                    errors.append("executed clean tree contains a different profile authority")
                committed_bundle = semantic_bundle_sha256(
                    repo_root, semantic_paths, executed_source_commit
                )
                if committed_bundle != EXPECTED_SEMANTIC_BUNDLE_SHA256:
                    errors.append("executed clean tree semantic bundle requires a profile revision")
            except (OSError, subprocess.SubprocessError) as error:
                errors.append(f"executed clean tree lacks a required profile/authority/test path: {error}")

    evidence = profile.get("evidence_authorities", {})
    for evidence_id, relative_path in evidence.items():
        path = repo_root / relative_path
        if not evidence_id or not path.is_file():
            errors.append(f"evidence authority {evidence_id!r} does not resolve to a file")

    product_operation_source = (
        repo_root / "crates/hyphae-native-product/src/operation.rs"
    ).read_text(encoding="utf-8")
    if 'Self::StructureRead(read) => (1, format!("{:?}", read.value).len())' not in product_operation_source:
        errors.append("ProductLimits StructureRead Debug-byte admission authority differs")
    product_limits_source = (
        repo_root / "crates/hyphae-native-product/src/limits.rs"
    ).read_text(encoding="utf-8")
    for required_source in (
        "pub const MAX_PRODUCT_CONTEXT_COUNT: usize = 4_096;",
        "pub const MAX_PRODUCT_CONTEXT_BYTES: usize = 16 * 1024 * 1024;",
        "bytes > self.max_response_bytes",
    ):
        if required_source not in product_limits_source:
            errors.append(f"ProductLimits response authority is missing {required_source!r}")

    inventory = profile.get("inventory")
    if not isinstance(inventory, list):
        inventory = []
        errors.append("inventory must be an array")
    seal = profile.get("claim_semantics_seal")
    actual_claim_seal = claim_semantics_sha256(profile)
    expected_seal = {
        "algorithm": "sha256",
        "canonicalization": "sorted-key-compact-json-utf8-v1",
        "sha256": EXPECTED_CLAIM_SEMANTICS_SEAL,
    }
    if seal != expected_seal or actual_claim_seal != EXPECTED_CLAIM_SEMANTICS_SEAL:
        errors.append(
            "claim semantics changed without a reviewed full-seal checker update; "
            f"actual {actual_claim_seal}"
        )

    seen: set[tuple[str, str | None]] = set()
    zrange_cases: set[str] = set()
    counts: Counter[str] = Counter()
    for index, entry in enumerate(inventory):
        operation = entry.get("operation")
        case = entry.get("case")
        classification = entry.get("classification")
        prefix = f"inventory[{index}]"
        if not isinstance(operation, str) or not re.fullmatch(r"[A-Z][A-Z0-9_]*", operation):
            errors.append(f"{prefix}.operation must be one uppercase Valkey command")
        if operation == "ZRANGE":
            if case not in {"rank", "score"}:
                errors.append("ZRANGE inventory cases must be rank or score")
            else:
                zrange_cases.add(case)
        elif case is not None:
            errors.append(f"{operation}: only split ZRANGE inventory entries may carry case")
        identity = (operation, case)
        if identity in seen:
            errors.append(f"duplicate inventory operation case {operation}:{case}")
        else:
            seen.add(identity)
        if classification not in CLASSIFICATIONS:
            errors.append(f"{prefix}.classification is invalid")
            continue
        counts[classification] += 1
        native = entry.get("native")
        if not isinstance(native, list):
            errors.append(f"{prefix}.native must be an array")
            native = []
        if classification == "excluded":
            expected_keys = {"operation", "family", "classification", "native", "gap", "evidence"}
            if native:
                errors.append(f"{operation}: excluded operations cannot name a native mapping")
            if not entry.get("gap"):
                errors.append(f"{operation}: excluded operations require an explicit gap")
        else:
            expected_keys = {
                "operation",
                "family",
                "classification",
                "native",
                "scope",
                "reason",
                "evidence",
            }
            if operation == "ZRANGE":
                expected_keys.add("case")
            if not native or not all(isinstance(value, str) and value for value in native):
                errors.append(f"{operation}: claimed operations require native mappings")
            if not entry.get("scope") or not entry.get("reason"):
                errors.append(f"{operation}: claimed operations require scope and reason")
        if set(entry) != expected_keys:
            errors.append(f"{operation}: claim field set must be exactly {sorted(expected_keys)}")
        if operation in {"ZPOPMIN", "ZPOPMAX"}:
            reason = entry.get("reason", "")
            if classification != "different":
                errors.append(f"{operation}: typed lifecycle mismatch requires different classification")
            if "typed explicit lifecycle" not in reason:
                errors.append(f"{operation}: reason must retain the lifecycle mismatch")
        if operation == "GETRANGE":
            if classification != "different":
                errors.append("GETRANGE: missing-key mismatch requires different classification")
            if "Valkey returns empty bytes" not in entry.get("reason", ""):
                errors.append("GETRANGE: reason must retain the missing-key mismatch")
        if operation == "SET":
            scope = entry.get("scope", "")
            if "missing from every live structure family or is already a live scalar" not in scope:
                errors.append("SET: exact scope must remain missing-or-live-scalar only")
            if "Live typed hash, list, set, sorted-set, and stream collections are excluded" not in scope:
                errors.append("SET: exact scope must exclude live typed collections")
        if operation == "TTL":
            scope = entry.get("scope", "")
            reason = entry.get("reason", "")
            if "expires_at_micros - snapshot_time_micros >= 1000000" not in scope:
                errors.append("TTL: equivalent scope must require a positive future seconds value")
            if "now == expiry boundaries" not in reason or "missing, expired" not in reason:
                errors.append("TTL: boundary exclusions differ")
        if operation == "PTTL":
            scope = entry.get("scope", "")
            reason = entry.get("reason", "")
            if "expires_at_micros - snapshot_time_micros >= 1000" not in scope:
                errors.append("PTTL: equivalent scope must require a positive future milliseconds value")
            if "now == expiry boundaries" not in reason or "missing, expired" not in reason:
                errors.append("PTTL: boundary exclusions differ")
        if operation == "SETRANGE":
            if classification != "different":
                errors.append("SETRANGE: missing-key lifecycle mismatch requires different classification")
            reason = entry.get("reason", "")
            if "empty patch" not in reason or "leaves the key absent" not in reason:
                errors.append("SETRANGE: reason must retain the empty-patch missing-key mismatch")
        if operation == "EXPIREAT":
            scope = entry.get("scope", "")
            reason = entry.get("reason", "")
            if (
                "signed Unix-seconds" not in scope
                or "[-9223372036854, 9223372036854]" not in scope
            ):
                errors.append("EXPIREAT: shared signed-seconds bounds differ")
            if (
                "Convert with expires_at_micros = unix_seconds * 1000000; exactly" not in reason
                or "signed i64 microseconds without overflow" not in reason
            ):
                errors.append("EXPIREAT: seconds-to-microseconds formula differs")
        if operation == "PEXPIREAT":
            scope = entry.get("scope", "")
            reason = entry.get("reason", "")
            if (
                "signed Unix-milliseconds" not in scope
                or "[-9223372036854775, 9223372036854775]" not in scope
            ):
                errors.append("PEXPIREAT: shared signed-milliseconds bounds differ")
            if (
                "Convert with expires_at_micros = unix_milliseconds * 1000; exactly" not in reason
                or "signed i64 microseconds without overflow" not in reason
            ):
                errors.append("PEXPIREAT: milliseconds-to-microseconds formula differs")
        if operation == "ZRANGE":
            if classification != "equivalent":
                errors.append(f"ZRANGE {case}: scoped case must remain equivalent")
            if "live pre-existing explicitly created sorted set" not in entry.get("scope", ""):
                errors.append("ZRANGE: equivalent scope must require a live pre-existing sorted set")
            reason = entry.get("reason", "")
            if "missing or expired collections are outside scope" not in reason:
                errors.append("ZRANGE: equivalent scope must exclude missing and expired collections")
            if case == "rank":
                if entry.get("native") != ["StructureRead.SortedSetRange"]:
                    errors.append("ZRANGE rank: native mapping differs")
                if "no BYSCORE, BYLEX, LIMIT, or OFFSET shape" not in entry.get("scope", ""):
                    errors.append("ZRANGE rank: LIMIT/OFFSET must remain outside scope")
                scope = entry.get("scope", "")
                reason = entry.get("reason", "")
                if "normalized inclusive span returns at most 4096 items" not in scope:
                    errors.append("ZRANGE rank: normalized rank span bound differs")
                if (
                    'response_debug_bytes = format!("{:?}", ProductStructureReadResult::SortedSetEntries(entries)).len() <= 16777216'
                    not in scope
                ):
                    errors.append("ZRANGE rank: ProductLimits Debug-byte authority differs")
                if (
                    "response_count = 1" not in reason
                    or "response_debug_bytes <= max_response_bytes = 16777216" not in reason
                    or "oversized item/Debug-byte results" not in reason
                ):
                    errors.append("ZRANGE rank: ProductLimits envelope differs")
            if case == "score":
                if entry.get("native") != ["StructureRead.SortedSetScoreRange"]:
                    errors.append("ZRANGE score: native mapping differs")
                scope = entry.get("scope", "")
                if "explicit LIMIT offset/count" not in scope or "1 <= count <= 4096" not in scope:
                    errors.append("ZRANGE score: bounded LIMIT/OFFSET mapping differs")
                if "1 <= count <= 4096" not in scope or "Returned items must be <= count and <= 4096" not in scope:
                    errors.append("ZRANGE score: item/count envelope differs")
                if (
                    'response_debug_bytes = format!("{:?}", ProductStructureReadResult::SortedSetEntries(entries)).len() <= 16777216'
                    not in scope
                ):
                    errors.append("ZRANGE score: ProductLimits Debug-byte authority differs")
                reason = entry.get("reason", "")
                if (
                    "response_count = 1" not in reason
                    or "response_debug_bytes <= max_response_bytes = 16777216" not in reason
                    or "oversized item/Debug-byte results" not in reason
                ):
                    errors.append("ZRANGE score: ProductLimits envelope differs")
        if operation in {"SINTER", "SUNION", "SDIFF"}:
            scope = entry.get("scope", "")
            reason = entry.get("reason", "")
            if "1..=64 input key positions" not in scope:
                errors.append(f"{operation}: set algebra input-key-position bound differs")
            if "output_member_limit in 1..=4096" not in scope or "visit_limit in 1..=1000000" not in scope:
                errors.append(f"{operation}: set algebra ProductLimits request bounds differ")
            if (
                'response_debug_bytes = format!("{:?}", ProductStructureReadResult::SetAlgebra { members, visited }).len() is <= 16777216'
                not in scope
            ):
                errors.append(f"{operation}: set algebra Debug-byte authority differs")
            if "response_count = 1" not in reason or "exact Rust Debug byte length" not in reason:
                errors.append(f"{operation}: set algebra response admission authority differs")
        references = entry.get("evidence")
        if not isinstance(references, list) or not references:
            errors.append(f"{operation}: at least one evidence authority is required")
        else:
            unknown = sorted(set(references) - set(evidence))
            if unknown:
                errors.append(f"{operation}: unknown evidence authorities {unknown}")
            if operation in {"ZPOPMIN", "ZPOPMAX"} and "operation-tests" not in references:
                errors.append(f"{operation}: lifecycle classification requires executable evidence")

    if zrange_cases != {"rank", "score"}:
        errors.append("inventory must contain exactly the rank and score ZRANGE cases")

    summary = profile.get("summary", {})
    expected_summary = {"operations": len(inventory), **{key: counts[key] for key in sorted(CLASSIFICATIONS)}}
    if summary != expected_summary:
        errors.append(f"summary must equal computed inventory counts {expected_summary}")
    if set(counts) != CLASSIFICATIONS:
        errors.append("the first profile must explicitly use all five classifications")

    config_authority = profile.get("configuration_authority", {})
    if config_authority.get("identity_info_field") != "valkey_version":
        errors.append("server identity must come from INFO valkey_version")
    if config_authority.get("effective_config_capture") != "CONFIG GET *":
        errors.append("receipts must bind the complete effective CONFIG GET * result")
    if config_authority.get("transport") != "unix_domain_socket" or config_authority.get("tcp_enabled") is not False:
        errors.append("Valkey oracle transport must remain UDS-only with TCP disabled")

    lanes = config_authority.get("lanes", [])
    lane_names = {lane.get("name") for lane in lanes if isinstance(lane, dict)}
    if lane_names != set(LANE_SETTINGS) or len(lanes) != len(LANE_SETTINGS):
        errors.append("configuration authority must contain exactly no, always, and everysec lanes")
    for lane in lanes:
        if not isinstance(lane, dict) or lane.get("name") not in LANE_SETTINGS:
            continue
        name = lane["name"]
        relative = lane.get("config", "")
        config_path = repo_root / relative
        try:
            config_path.resolve().relative_to((HERE / "config").resolve())
        except (OSError, ValueError):
            errors.append(f"lane {name}: config must remain under the standalone Valkey config directory")
            continue
        if not config_path.is_file():
            errors.append(f"lane {name}: config file is missing")
            continue
        actual_sha = _sha256(config_path)
        if lane.get("sha256") != actual_sha:
            errors.append(f"lane {name}: config sha256 mismatch; actual {actual_sha}")
        try:
            values = _config_values(config_path)
        except (OSError, ProfileError) as error:
            errors.append(str(error))
            continue
        required = {
            "port": "0",
            "unixsocket": lane.get("socket"),
            "save": "",
            "maxmemory-policy": "noeviction",
            "dir": lane.get("dir"),
            **LANE_SETTINGS[name],
        }
        for key, expected in required.items():
            if values.get(key) != expected or lane.get(key, expected) != expected:
                errors.append(f"lane {name}: {key} must be {expected!r}")
        if name == "everysec" and "No Hyphae equivalence claim" not in lane.get("comparison", ""):
            errors.append("everysec lane must explicitly deny a Hyphae durability equivalence")

    root_manifest_path = repo_root / "Cargo.toml"
    baseline_manifest_path = repo_root / "benchmarks/baseline-harness/Cargo.toml"
    try:
        root_manifest = tomllib.loads(root_manifest_path.read_text(encoding="utf-8"))
        baseline_manifest = tomllib.loads(baseline_manifest_path.read_text(encoding="utf-8"))
    except (OSError, tomllib.TOMLDecodeError) as error:
        errors.append(f"cannot inspect Cargo graph boundary: {error}")
    else:
        members = root_manifest.get("workspace", {}).get("members", [])
        if any(member.rstrip("/") == "benchmarks/baseline-harness" for member in members):
            errors.append("standalone baseline harness entered the root Cargo workspace")
        root_dependencies = root_manifest.get("workspace", {}).get("dependencies", {})
        forbidden = sorted({"redis", "valkey"} & set(root_dependencies))
        if forbidden:
            errors.append(f"external oracle dependencies entered the root Cargo graph: {forbidden}")
        if "workspace" not in baseline_manifest:
            errors.append("baseline harness must remain its own standalone Cargo workspace")

    harness_source = (repo_root / "benchmarks/baseline-harness/src/keyspace_suite.rs").read_text(
        encoding="utf-8"
    )
    for required_source in (
        "valkey_version:",
        'arg("GET")',
        'arg("*")',
        f'const VALKEY_VERSION: &str = "{EXPECTED_VERSION}";',
        f'const VALKEY_SOURCE_COMMIT: &str = "{EXPECTED_COMMIT}";',
        f'"{EXPECTED_ARTIFACT_SHA256}";',
        f'const VALKEY_PROFILE_SCHEMA: &str = "{EXPECTED_SCHEMA}";',
        "HYPHAE_VALKEY_CLAIM_SEMANTICS_SHA256",
        "HYPHAE_VALKEY_SEMANTIC_BUNDLE_SHA256",
        f'const VALKEY_RECEIPT_SCHEMA: &str = "{EXPECTED_VALKEY_RECEIPT_SCHEMA}";',
        "HYPHAE_VALKEY_SERVER_SHA256",
        "sha256_file(&proc_executable)",
        "validate_effective_config(lane, socket, &effective_config)",
        "write_key_sequence_sha256",
        "read_key_sequence_sha256",
        "validate_distinct_valkey_runtime_identities",
        'redis::cmd("DBSIZE")',
        ".hyphae-fresh-setup",
        "fresh-created-database",
    ):
        if required_source not in harness_source:
            errors.append(f"keyspace harness is missing required receipt binding {required_source!r}")

    util_source = (repo_root / "benchmarks/baseline-harness/src/util.rs").read_text(
        encoding="utf-8"
    )
    if f'"{EXPECTED_BASELINE_RECEIPT_SCHEMA}"' not in util_source:
        errors.append("baseline harness must emit the version-2 receipt schema")
    for required_source in (
        "HYPHAE_SOURCE_PRE_COMMIT",
        "HYPHAE_SOURCE_PRE_TREE",
        "HYPHAE_SOURCE_POST_COMMIT",
        "HYPHAE_SOURCE_POST_TREE",
        "HYPHAE_HARNESS_PRODUCT_BINARY_SHA256",
        "HYPHAE_RUSTFLAGS_STATE",
        "HYPHAE_CARGO_ENCODED_RUSTFLAGS_STATE",
        "HYPHAE_RUSTC_WRAPPER_STATE",
        "HYPHAE_RUSTC_WORKSPACE_WRAPPER_STATE",
        "HYPHAE_CARGO_PROFILE_OVERRIDES_STATE",
        "HYPHAE_CARGO_INCREMENTAL_STATE",
        "HYPHAE_RUSTC_VERBOSE",
        "HYPHAE_CARGO_VERSION",
        "HYPHAE_BUILD_PROFILE",
        "HYPHAE_BUILD_COMMAND",
        "HYPHAE_RECEIPT_AUTHORITY",
        "std::env::current_exe()",
        "executed_harness_product_sha256",
    ):
        if required_source not in util_source:
            errors.append(f"receipt writer is missing Hyphae identity binding {required_source!r}")

    metal_script = (repo_root / "benchmarks/baseline-harness/scripts/run-metal.sh").read_text(
        encoding="utf-8"
    )
    errors.extend(runner_source_errors(metal_script, artifact_url, lanes))

    ci_source = (repo_root / ".github/workflows/ci.yml").read_text(encoding="utf-8")
    for required_source in (
        "--executed-source-commit \"$(git rev-parse HEAD)\" --compact",
        "-s benchmarks/baseline-harness/valkey -p 'test_*.py' -v",
        "cargo test --locked --manifest-path benchmarks/baseline-harness/Cargo.toml",
        "rust_producer_interoperates_with_independent_saved_receipt_validator",
    ):
        if required_source not in ci_source:
            errors.append(f"CI is missing standalone Valkey contract gate {required_source!r}")

    if errors:
        raise ProfileError("\n".join(errors))

    return {
        "status": "ok",
        "schema": profile["schema"],
        "profile": profile["profile"],
        "oracle": {
            "version": oracle["version"],
            "tag": oracle["tag"],
            "source_commit": oracle["source_commit"],
            "artifact_sha256": oracle["artifact"]["sha256"],
        },
        "hyphae_source_commit": profile["hyphae_authority"]["source_commit"],
        "claim_semantics_sha256": EXPECTED_CLAIM_SEMANTICS_SEAL,
        "profile_sha256": EXPECTED_PROFILE_SHA256,
        "semantic_bundle_sha256": EXPECTED_SEMANTIC_BUNDLE_SHA256,
        "classifications": {key: counts[key] for key in sorted(CLASSIFICATIONS)},
        "operations": len(inventory),
        "lanes": [lane["name"] for lane in lanes],
        "compatibility_claim": False,
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("profile", nargs="?", type=Path, default=DEFAULT_PROFILE)
    parser.add_argument("--compact", action="store_true")
    parser.add_argument("--executed-source-commit")
    parser.add_argument("--semantic-bundle-only", action="store_true")
    parser.add_argument("--claim-seal-only", action="store_true")
    arguments = parser.parse_args()
    try:
        report = check_profile(
            arguments.profile.resolve(), ROOT, arguments.executed_source_commit
        )
    except ProfileError as error:
        print(f"Valkey Application Core profile invalid:\n{error}", file=sys.stderr)
        return 1
    if arguments.semantic_bundle_only:
        print(report["semantic_bundle_sha256"])
    elif arguments.claim_seal_only:
        print(report["claim_semantics_sha256"])
    else:
        indent = None if arguments.compact else 2
        print(json.dumps(report, indent=indent, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
