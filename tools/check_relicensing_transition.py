#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Verify the effective relicensing state and its content-bound receipt."""

from __future__ import annotations

import hashlib
import json
import subprocess
import sys
from collections import Counter
from pathlib import Path
from typing import Any

if __package__ in {None, ""}:
    sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from tools.check_relicensing_preflight import (
    CONTRACT_PATH,
    GIT_OBJECT_ID,
    LEGAL_BASE_COMMIT,
    LEGAL_BASE_TREE,
    ROOT,
    TRANSITION_RECEIPT_PATH,
    classify_path,
    load_contract,
    repository_paths,
    validate_contract,
    validate_historical_release_evidence,
    validate_preflight_evidence,
)
from tools.check_license_policy import validate_repository
from tools.check_license_policy import repository_machine_files


DIGEST_DOMAIN = b"hyphae-relicensing-transition-tree-v1\0"
EXPECTED_LICENSES = {
    "software": "Apache-2.0",
    "normative-specification": "Apache-2.0",
    "narrative-documentation": "CC-BY-SA-4.0",
}


def _git(root: Path, *arguments: str) -> str:
    return subprocess.run(
        ["git", "-C", str(root), *arguments],
        check=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=10,
    ).stdout.decode("utf-8").strip()


def transitioned_content_digest(
    root: Path, paths: list[str], excluded_paths: set[str]
) -> tuple[str, int]:
    digest = hashlib.sha256(DIGEST_DOMAIN)
    count = 0
    for relative in sorted(set(paths)):
        if relative in excluded_paths:
            continue
        path = root / relative
        if path.is_symlink() or not path.is_file():
            raise ValueError(f"{relative}: transition input must be a regular file")
        encoded_path = relative.encode("utf-8")
        encoded = path.read_bytes()
        digest.update(len(encoded_path).to_bytes(8, "big"))
        digest.update(encoded_path)
        digest.update(len(encoded).to_bytes(8, "big"))
        digest.update(encoded)
        count += 1
    return digest.hexdigest(), count


def refresh_transition_receipt(root: Path = ROOT) -> dict[str, Any]:
    contract = load_contract(root / CONTRACT_PATH.relative_to(ROOT))
    paths = repository_paths(root)
    receipt = _load_receipt(root)
    transitioned = receipt["transitioned_tree"]
    excluded = set(transitioned["excluded_paths"])
    content_sha256, path_count = transitioned_content_digest(root, paths, excluded)
    category_counts: Counter[str] = Counter()
    for relative in paths:
        category, ties = classify_path(relative, contract["classification"]["rules"])
        if ties or category is None:
            raise ValueError(f"{relative}: cannot classify transition input")
        category_counts[category] += 1
    transitioned["content_sha256"] = content_sha256
    transitioned["path_count"] = path_count
    transitioned["category_counts"] = dict(sorted(category_counts.items()))
    return receipt


def _load_receipt(root: Path) -> dict[str, Any]:
    path = root / TRANSITION_RECEIPT_PATH
    value = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(value, dict):
        raise ValueError("transition receipt must be a JSON object")
    return value


def validate_source_anchor(root: Path, source: object) -> list[str]:
    if not isinstance(source, dict):
        return ["transition receipt source must be an object"]
    failures: list[str] = []
    if source.get("worktree_state") != "content-bound-integration-tree":
        failures.append("transition receipt source mode differs")
    base_commit = source.get("base_commit")
    base_tree = source.get("base_tree")
    if (base_commit, base_tree) != (LEGAL_BASE_COMMIT, LEGAL_BASE_TREE):
        failures.append("transition receipt source differs from exact legal base")
    if (
        not isinstance(base_commit, str)
        or GIT_OBJECT_ID.fullmatch(base_commit) is None
        or not isinstance(base_tree, str)
        or GIT_OBJECT_ID.fullmatch(base_tree) is None
    ):
        failures.append("transition receipt base identity is invalid")
    else:
        try:
            actual_base_tree = _git(root, "rev-parse", f"{base_commit}^{{tree}}")
            head = _git(root, "rev-parse", "HEAD")
            subprocess.run(
                ["git", "-C", str(root), "merge-base", "--is-ancestor", base_commit, head],
                check=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                timeout=10,
            )
        except subprocess.SubprocessError as error:
            failures.append(f"transition receipt base is not an ancestor of HEAD: {error}")
        else:
            if actual_base_tree != base_tree:
                failures.append("transition receipt base tree differs")
    event = source.get("base_event")
    if not isinstance(event, dict) or event != {
        "kind": "interactive-owner-attestation",
        "evidence": (
            "docs/gates/evidence/"
            "relicensing-1.2.0-representative-attestation.json"
        ),
    }:
        failures.append("transition receipt base owner-attestation event differs")
    return failures


def _validate_transition_content(
    root: Path, receipt: dict[str, Any], paths: list[str]
) -> list[str]:
    failures: list[str] = []
    if receipt.get("schema") != "hyphae-relicensing-transition-receipt-v1":
        failures.append("transition receipt schema differs")
    if receipt.get("target_release") != "1.2.0" or receipt.get("status") != (
        "effective-in-current-integration-tree"
    ):
        failures.append("transition receipt effective status differs")
    failures.extend(validate_source_anchor(root, receipt.get("source")))
    transitioned = receipt.get("transitioned_tree")
    if not isinstance(transitioned, dict):
        failures.append("transition receipt transitioned_tree must be an object")
        return failures
    excluded = transitioned.get("excluded_paths")
    if excluded != [TRANSITION_RECEIPT_PATH]:
        failures.append("transition receipt excluded path set differs")
    else:
        try:
            actual_digest, actual_count = transitioned_content_digest(
                root, paths, set(excluded)
            )
        except (OSError, ValueError) as error:
            failures.append(str(error))
        else:
            if transitioned.get("content_sha256") != actual_digest:
                failures.append("transition receipt content digest differs")
            if transitioned.get("path_count") != actual_count:
                failures.append("transition receipt path count differs")
    if transitioned.get("digest_domain") != DIGEST_DOMAIN[:-1].decode("ascii"):
        failures.append("transition receipt digest domain differs")
    return failures


def validate_current_transition_content(root: Path = ROOT) -> list[str]:
    try:
        receipt = _load_receipt(root)
        paths = repository_paths(root)
    except (OSError, UnicodeError, ValueError, json.JSONDecodeError) as error:
        return [str(error)]
    except subprocess.SubprocessError as error:
        return [f"Git transition inventory failed: {error}"]
    return _validate_transition_content(root, receipt, paths)


def validate_transition(root: Path = ROOT) -> list[str]:
    failures: list[str] = []
    try:
        contract = load_contract(root / CONTRACT_PATH.relative_to(ROOT))
        paths = repository_paths(root)
        contract_result = validate_contract(contract, paths, root)
        receipt = _load_receipt(root)
    except (OSError, UnicodeError, ValueError, json.JSONDecodeError) as error:
        return [str(error)]
    except subprocess.SubprocessError as error:
        return [f"Git transition inventory failed: {error}"]

    failures.extend(contract_result.failures)
    failures.extend(validate_historical_release_evidence(root))
    failures.extend(validate_preflight_evidence(root, require_transition_content=False))
    failures.extend(validate_repository(root))
    if contract_result.blockers:
        failures.append(f"preflight blockers remain: {contract_result.blockers}")

    category_counts: Counter[str] = Counter()
    third_party_paths = set(contract["boundaries"]["third_party"]["exact_paths"])
    marker_capable_paths = {
        path.relative_to(root).as_posix() for path in repository_machine_files(root)
    }
    for relative in paths:
        category = contract_result.classifications.get(relative)
        if category is None:
            continue
        category_counts[category] += 1
        path = root / relative
        if path.is_symlink() or not path.is_file():
            failures.append(f"{relative}: classified path must be a regular file")
            continue
        encoded = path.read_bytes()
        marker = b"SPDX-License-Identifier:"
        marker_lines = [
            line.strip() for line in encoded.splitlines()[:3] if marker in line
        ]
        if not marker_lines:
            if relative in marker_capable_paths:
                failures.append(
                    f"{relative}: classified marker-capable file lacks SPDX marker"
                )
            continue
        expected = EXPECTED_LICENSES.get(category)
        if expected is None:
            failures.append(f"{relative}: excluded category carries an SPDX grant")
            continue
        expected_marker = f"SPDX-License-Identifier: {expected}".encode("ascii")
        if relative in set(contract["verification"]["agpl_history_allowlist"]):
            continue
        if any(expected_marker not in line for line in marker_lines):
            failures.append(f"{relative}: SPDX marker differs from classified {expected}")

    for relative in third_party_paths:
        if contract_result.classifications.get(relative) != "third-party-material":
            failures.append(f"{relative}: third-party exact path classification differs")
        encoded = (root / relative).read_bytes()
        if relative == "DCO" and (
            b"Everyone is permitted to copy and distribute verbatim copies" not in encoded
        ):
            failures.append(f"{relative}: canonical third-party literal differs")
        if relative == "THIRD_PARTY_LICENSES.txt" and (
            b"Generated from config/native-dependency-policy.json and Cargo.lock." not in encoded
        ):
            failures.append(f"{relative}: generated dependency bundle authority differs")

    failures.extend(_validate_transition_content(root, receipt, paths))

    transitioned = receipt.get("transitioned_tree")
    if isinstance(transitioned, dict):
        expected_counts = dict(sorted(category_counts.items()))
        if transitioned.get("category_counts") != expected_counts:
            failures.append("transition receipt category counts differ")

    evidence = receipt.get("accepted_evidence")
    expected_evidence = sorted(
        {
            path
            for category in contract["preflight"]["evidence_categories"]
            for path in category["evidence"]
        }
        | {
            "docs/gates/evidence/relicensing-1.2.0-dependabot-review.json",
            "docs/gates/evidence/relicensing-1.2.0-representative-attestation.json",
        }
    )
    if evidence != expected_evidence:
        failures.append("transition receipt accepted evidence set differs")

    return failures


def transition_for_committed_tree(root: Path = ROOT) -> dict[str, Any]:
    """Return the accepted transition identity for one exact clean Git tree."""
    status = _git(root, "status", "--porcelain=v1", "--untracked-files=all")
    if status:
        raise ValueError("relicensing transition source must be an exact clean tree")
    receipt = _load_receipt(root)
    commit = _git(root, "rev-parse", "HEAD^{commit}")
    tree = _git(root, "rev-parse", f"{commit}^{{tree}}")
    transitioned = receipt["transitioned_tree"]
    excluded = transitioned.get("excluded_paths")
    if excluded != [TRANSITION_RECEIPT_PATH]:
        raise ValueError("transition receipt excluded path set differs")
    paths = repository_paths(root)
    actual_digest, actual_count = transitioned_content_digest(
        root, paths, set(excluded)
    )
    if (
        receipt.get("schema") != "hyphae-relicensing-transition-receipt-v1"
        or receipt.get("target_release") != "1.2.0"
        or receipt.get("status") != "effective-in-current-integration-tree"
        or transitioned.get("content_sha256") != actual_digest
        or transitioned.get("path_count") != actual_count
    ):
        raise ValueError("relicensing transition receipt differs from exact tree")
    return {
        "schema": receipt["schema"],
        "target_release": receipt["target_release"],
        "status": receipt["status"],
        "commit": commit,
        "tree": tree,
        "content_sha256": transitioned["content_sha256"],
        "path_count": transitioned["path_count"],
    }


def main() -> int:
    if "--refresh" in sys.argv[1:]:
        if sys.argv[1:] != ["--refresh"]:
            print("error: --refresh cannot be combined with other arguments", file=sys.stderr)
            return 1
        try:
            receipt = refresh_transition_receipt()
            (ROOT / TRANSITION_RECEIPT_PATH).write_text(
                json.dumps(receipt, indent=2, ensure_ascii=False) + "\n",
                encoding="utf-8",
            )
        except (OSError, ValueError, subprocess.SubprocessError) as error:
            print(f"error: {error}", file=sys.stderr)
            return 1
        print("transition receipt refreshed; run the checker again after concurrent writes stop")
        return 0
    failures = validate_transition()
    if failures:
        for failure in failures:
            print(f"error: {failure}", file=sys.stderr)
        return 1
    paths = repository_paths(ROOT)
    print(
        "relicensing transition PASS: effective Apache-2.0/CC-BY-SA-4.0 "
        f"classification and receipt cover {len(paths)} paths"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
