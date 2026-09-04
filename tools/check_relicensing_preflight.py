#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Validate bounded 1.2.0 classification and source-bound preflight evidence."""

from __future__ import annotations

import hashlib
import json
import re
import subprocess
import sys
import tomllib
from dataclasses import dataclass
from pathlib import Path, PurePosixPath
from typing import Any


ROOT = Path(__file__).resolve().parents[1]
if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))
CONTRACT_PATH = ROOT / "config" / "relicensing-1.2.0-classification.json"
IDENTIFIER = re.compile(r"^[a-z][a-z0-9]*(?:-[a-z0-9]+)*$")
VERSION = re.compile(r"^[0-9]+\.[0-9]+\.[0-9]+$")
MAX_GIT_OUTPUT = 4 * 1024 * 1024
MAX_REPOSITORY_PATHS = 10_000
MAX_CONTRACT_BYTES = 1024 * 1024
MAX_EVIDENCE_BYTES = 4 * 1024 * 1024
SHA256 = re.compile(r"^[0-9a-f]{64}$")
GIT_OBJECT_ID = re.compile(r"^[0-9a-f]{40}$")
LEGAL_BASE_COMMIT = "fcf2f918e1539cfb7d67fd52abf0c7d57169ec18"
LEGAL_BASE_TREE = "51b283d27d0c0f5d194680de1d3e273b57f2ff95"
DEPENDENCY_EVIDENCE_EVOLUTION = (
    "Current exact dependency evidence evolved from legal base fcf2f918; the "
    "base-derived filename is retained as the classification contract path."
)
DEPENDENCY_RECEIPT_PATH = (
    "docs/gates/evidence/relicensing-1.2.0-dependencies-fcf2f918.json"
)
DEPENDENCY_AGGREGATE_PATH = (
    "docs/gates/evidence/relicensing-1.2.0-dependency-license-aggregate.json"
)
REPOSITORY_AUDIT_PATH = (
    "docs/gates/evidence/relicensing-1.2.0-repository-audit-fcf2f918.json"
)
REPRESENTATIVE_ATTESTATION_PATH = (
    "docs/gates/evidence/relicensing-1.2.0-representative-attestation.json"
)
DEPENDABOT_REVIEW_PATH = (
    "docs/gates/evidence/relicensing-1.2.0-dependabot-review.json"
)
DEPENDABOT_REVIEW_METHOD = {
    "scope": f"Every commit reachable from legal base source.commit {LEGAL_BASE_COMMIT} whose author is dependabot[bot].",
    "review": "Each parent-to-commit patch, changed path set, numstat, author, committer, subject, and tree was inspected individually.",
    "classification_rule": "A commit is mechanical only when its complete patch changes dependency versions, lock resolution, checksums, or immutable full-SHA action references and contains no authored product logic, prose, or license grant.",
}
TRANSITION_RECEIPT_PATH = "docs/gates/evidence/relicensing-1.2.0-transition.json"
HISTORICAL_APACHE_LICENSE_SHA256 = (
    "cdf5cb75ba05132c2933df3d948450e0503ede64552ee3a4d3fb9f52dab096c0"
)
HISTORICAL_APACHE_TAGS = (
    ("v0.1.0", "76b0cfdad90cf9e75d949a945c94a3badf0c6b59"),
    ("v0.2.0", "170380453a2ca6322a4c8bc50417318daee1c011"),
    ("v0.2.1", "08028e8dac077846c638f067ce74fbcf6fb75501"),
    ("v1.0.0", "839ea6e2a806ed919610952cb17fd1dd61195d76"),
    ("v1.0.1", "84161cf067141b60f4847b965ef77c5b749749c0"),
)
HISTORICAL_V1_1_0 = {
    "tag_object": "80b2f094c17ada6adc3bb879e20c3662bd93f4e4",
    "commit": "e88f2ea2c3455a393e3ac0cd69e25486cc26888e",
    "tree": "c131ab057c8ab05ed2e2389954f0e8145a71dbdb",
    "license_sha256": "0d96a4ff68ad6d4b6f1f30f713b18d5184912ba8dd389f86aa7710db079abcb0",
    "documentation_license_sha256": (
        "23ee78c8bae49cf08ea2f0c84945c66b987ebe4520881fb51b3dad4fb43d07c2"
    ),
}
SOFTWARE_CATEGORY_DEFINITION = (
    "Executable source, tests, examples, build and release tooling, configuration, "
    "machine-enforced data, schemas, generated models, and legal material distributed "
    "with software."
)

TOP_LEVEL_KEYS = {
    "$comment",
    "schema",
    "target_release",
    "licenses",
    "historical_releases",
    "categories",
    "mixed_file_rule",
    "classification",
    "boundaries",
    "effective_transition",
    "preflight",
    "verification",
}
EXPECTED_LICENSES = {
    "software": "Apache-2.0",
    "normative_specifications": "Apache-2.0",
    "narrative_documentation": "CC-BY-SA-4.0",
}
EXPECTED_HISTORY = [
    {
        "minimum": "0.1.0",
        "maximum": "1.0.1",
        "software": "Apache-2.0",
        "documentation": "not-separately-specified",
        "evidence": [
            "Tags v0.1.0, v0.2.0, v0.2.1, v1.0.0, and v1.0.1 contain a "
            "root Apache-2.0 LICENSE.",
            "The same tags declare Apache-2.0 in root README.md and Cargo.toml.",
            "The same tags do not contain LICENSE-DOCUMENTATION.",
        ],
        "immutable": True,
    },
    {
        "minimum": "1.1.0",
        "maximum": "1.1.0",
        "software": "AGPL-3.0-only",
        "documentation": "CC-BY-SA-4.0",
        "evidence": [
            "Tag v1.1.0 contains a root AGPL-3.0-only LICENSE and CC-BY-SA-4.0 "
            "LICENSE-DOCUMENTATION.",
            "Tag v1.1.0 declares AGPL-3.0-only software and CC-BY-SA-4.0 "
            "documentation in root README.md and declares AGPL-3.0-only in "
            "Cargo.toml.",
        ],
        "immutable": True,
    },
]
EXPECTED_CATEGORY_LICENSES = {
    "software": "Apache-2.0",
    "normative-specification": "Apache-2.0",
    "narrative-documentation": "CC-BY-SA-4.0",
    "reserved-trademark-asset": None,
    "third-party-material": None,
}
EXPECTED_CATEGORY_DEFINITIONS = {
    "software": SOFTWARE_CATEGORY_DEFINITION,
    "normative-specification": (
        "A machine-readable contract or implementable normative document that defines "
        "public behavior, wire semantics, durable formats, query or retrieval semantics, "
        "proofs, or bounded performance contracts."
    ),
    "narrative-documentation": (
        "Prose-only explanation, governance, planning, architecture, how-to guidance, "
        "gates, evidence, release history, nonnormative security threat models, and "
        "README material."
    ),
    "reserved-trademark-asset": (
        "Hyphae marks and visual identity excluded from the project software and "
        "documentation grants and governed by TRADEMARKS.md."
    ),
    "third-party-material": (
        "An explicitly identified third-party work that retains its own terms and is not "
        "relicensed by the project classification contract."
    ),
}
EXPECTED_MIXED_RULE = {
    "dominance_order": [
        "executable-or-machine-enforced",
        "normative-implementable-prose",
        "prose-only",
    ],
    "executable_or_machine_enforced": "software",
    "normative_implementable_prose": "normative-specification",
    "prose_only": "narrative-documentation",
    "readme_default": "narrative-documentation",
}
EXPECTED_AGPL_ALLOWLIST = [
    "CHANGELOG.md",
    "LICENSE-POLICY.md",
    "README.md",
    "config/relicensing-1.2.0-classification.json",
    "docs/adr/0025-agplv3-code-and-cc-by-sa-documentation.md",
    "docs/adr/0029-apache-2.0-software-and-normative-specifications.md",
    "docs/gates/evidence/relicensing-1.2.0-dependencies-fcf2f918.json",
    "docs/gates/evidence/relicensing-1.2.0-repository-audit-fcf2f918.json",
    "docs/roadmap.md",
    "docs/roadmaps/1.2.0-relicensing.md",
    "tools/check_license_policy.py",
    "tools/check_relicensing_preflight.py",
    "tools/produce_relicensing_dependency_receipt.py",
    "tools/test_check_license_policy.py",
    "tools/test_check_native_v2_authority_conformance.py",
]
EXPECTED_GENERATED_COPIES = [
    {
        "source": "contracts/json-schema/",
        "copy": "crates/hyphae-contracts/assets/json-schema/",
        "mode": "packaged-subset",
        "target_license": "Apache-2.0",
    },
    {
        "source": "contracts/openapi/",
        "copy": "crates/hyphae-contracts/assets/openapi/",
        "mode": "packaged-subset",
        "target_license": "Apache-2.0",
    },
    {
        "source": "contracts/native-mcp-v2.json",
        "copy": "crates/hyphae-contracts/assets/native-mcp-v2.json",
        "mode": "exact-copy",
        "target_license": "Apache-2.0",
    },
]
EXPECTED_TRANSITION = {
    "state": "effective-in-current-integration-tree",
    "current_software_license": "Apache-2.0",
    "current_documentation_license": "CC-BY-SA-4.0",
    "effective_at_utc": "2026-08-16T13:15:26Z",
    "transition_receipt": TRANSITION_RECEIPT_PATH,
    "requires_all_preflight_evidence": True,
}
EXPECTED_PREFLIGHT = [
    (
        "counsel-approval",
        "accepted-owner-attestation",
        [REPRESENTATIVE_ATTESTATION_PATH],
    ),
    (
        "copyright-relicensing-authority",
        "accepted-owner-attestation",
        [REPOSITORY_AUDIT_PATH, REPRESENTATIVE_ATTESTATION_PATH],
    ),
    (
        "prior-commitments",
        "accepted-owner-attestation",
        [REPOSITORY_AUDIT_PATH, REPRESENTATIVE_ATTESTATION_PATH],
    ),
    (
        "dependency-license-exact-sha",
        "accepted-evidence",
        [DEPENDENCY_AGGREGATE_PATH],
    ),
    (
        "specification-classification",
        "accepted-evidence",
        [
            "docs/adr/0029-apache-2.0-software-and-normative-specifications.md",
            "config/relicensing-1.2.0-classification.json",
        ],
    ),
    (
        "contribution-governance",
        "accepted-evidence",
        ["CONTRIBUTING.md", REPOSITORY_AUDIT_PATH],
    ),
]
BLOCKING_PREFLIGHT_STATUSES: set[str] = set()
NONBLOCKING_PREFLIGHT_STATUSES = {
    "accepted-owner-attestation",
    "accepted-evidence",
}
REPRESENTATIVE_CLASSIFICATIONS = {
    ".github/assets/hyphae-lockup.svg": "reserved-trademark-asset",
    ".github/workflows/ci.yml": "software",
    "AGENTS.md": "narrative-documentation",
    "Cargo.toml": "software",
    ".github/assets/oin-member-2-0-horiz.png": "third-party-material",
    "DCO": "third-party-material",
    "THIRD_PARTY_LICENSES.txt": "third-party-material",
    "LICENSE": "software",
    "LICENSE-DOCUMENTATION": "narrative-documentation",
    "NOTICE": "software",
    "THIRD_PARTY_NOTICES.md": "software",
    "config/example.json": "software",
    "contracts/README.md": "narrative-documentation",
    "contracts/json-schema/example.schema.json": "normative-specification",
    "contracts/openapi/example.yaml": "normative-specification",
    "crates/example/README.md": "narrative-documentation",
    "crates/example/src/lib.rs": "software",
    "crates/hyphae-native-product/assets/product-error-v1.md": (
        "normative-specification"
    ),
    "crates/hyphae-contracts/assets/openapi/example.yaml": (
        "normative-specification"
    ),
    "docs/adr/example.md": "narrative-documentation",
    "docs/api/example.md": "normative-specification",
    "docs/architecture/example.md": "narrative-documentation",
    "docs/gates/evidence/example.json": "software",
    "docs/gates/evidence/example.md": "narrative-documentation",
    "docs/gates/example.md": "narrative-documentation",
    "docs/gates/native-local-phase-1.md": "normative-specification",
    "docs/native/example.md": "normative-specification",
    "docs/operations/example.md": "narrative-documentation",
    "docs/performance/example.md": "normative-specification",
    "docs/provenance/example.md": "normative-specification",
    "docs/query/example.md": "normative-specification",
    "docs/release/receipts/example.json": "software",
    "docs/release/schema/example.schema.json": "normative-specification",
    "docs/retrieval/example.md": "normative-specification",
    "docs/roadmaps/example.md": "narrative-documentation",
    "docs/security/example.md": "narrative-documentation",
    "docs/security/native-access-control-threat-model.md": "normative-specification",
    "docs/security/server-threat-model.md": "narrative-documentation",
    "docs/security/threat-model.md": "narrative-documentation",
    "docs/storage/example.md": "normative-specification",
    "plugins/hyphae/skills/use-hyphae/SKILL.md": "narrative-documentation",
    "sdks/python/README.md": "narrative-documentation",
    "sdks/python/src/example.py": "software",
    "tools/example.py": "software",
}
EXPECTED_RULES = [
    (
        "reserved-trademark-assets",
        10,
        "reserved-trademark-asset",
        "exact",
        (".github/assets/hyphae-lockup-reversed.svg", ".github/assets/hyphae-lockup.svg"),
    ),
    (
        "software-notice-documents",
        19,
        "software",
        "basename",
        ("THIRD_PARTY_NOTICES.md",),
    ),
    (
        "third-party-material",
        11,
        "third-party-material",
        "exact",
        (".github/assets/oin-member-2-0-horiz.png", "DCO", "THIRD_PARTY_LICENSES.txt"),
    ),
    (
        "documentation-license-texts",
        20,
        "narrative-documentation",
        "basename",
        ("LICENSE-DOCUMENTATION",),
    ),
    (
        "license-policy-documents",
        21,
        "narrative-documentation",
        "basename",
        ("LICENSE-POLICY.md",),
    ),
    (
        "readme-documents",
        22,
        "narrative-documentation",
        "basename",
        ("README.md",),
    ),
    ("software-license-texts", 23, "software", "basename", ("LICENSE",)),
    (
        "embedded-normative-markdown-contracts",
        24,
        "normative-specification",
        "exact",
        ("crates/hyphae-native-product/assets/product-error-v1.md",),
    ),
    (
        "normative-authority-document-exceptions",
        25,
        "normative-specification",
        "exact",
        (
            "docs/gates/native-local-phase-1.md",
            "docs/security/native-access-control-threat-model.md",
        ),
    ),
    (
        "normative-documentation-roots",
        30,
        "normative-specification",
        "prefix",
        (
            "docs/api/",
            "docs/formal/",
            "docs/native/",
            "docs/performance/",
            "docs/provenance/",
            "docs/query/",
            "docs/retrieval/",
            "docs/storage/",
        ),
    ),
    (
        "documentation-machine-contracts",
        31,
        "normative-specification",
        "prefix",
        ("docs/release/schema/",),
    ),
    (
        "canonical-public-contracts",
        32,
        "normative-specification",
        "prefix",
        ("contracts/",),
    ),
    (
        "packaged-public-contract-copies",
        33,
        "normative-specification",
        "prefix",
        ("crates/hyphae-contracts/assets/",),
    ),
    (
        "machine-enforced-json",
        34,
        "software",
        "suffix",
        (".json",),
    ),
    (
        "documentation-narrative-default",
        40,
        "narrative-documentation",
        "prefix",
        ("docs/",),
    ),
    (
        "repository-narrative-prose",
        41,
        "narrative-documentation",
        "suffix",
        (".md",),
    ),
    (
        "software-distribution-roots",
        50,
        "software",
        "prefix",
        (
            ".agents/",
            ".claude-plugin/",
            "benchmarks/",
            "compatibility/",
            "config/",
            "conformance/",
            "crates/",
            "embed/",
            "examples/",
            "fuzz/",
            "integrations/",
            "mcp/",
            "packaging/",
            "plugins/",
            "sdks/",
            "tools/",
        ),
    ),
    ("repository-automation", 51, "software", "prefix", (".github/",)),
    (
        "root-machine-and-build-files",
        60,
        "software",
        "exact",
        (
            ".editorconfig",
            ".gitattributes",
            ".gitignore",
            "Cargo.lock",
            "Cargo.toml",
            "clippy.toml",
            "deny.toml",
            "rust-toolchain.toml",
            "rustfmt.toml",
        ),
    ),
    (
        "root-software-notice",
        61,
        "software",
        "exact",
        ("NOTICE",),
    ),
]


@dataclass(frozen=True)
class ValidationResult:
    failures: list[str]
    classifications: dict[str, str]
    blockers: list[str]


def _require_keys(
    value: Any, expected: set[str], location: str, failures: list[str]
) -> bool:
    if not isinstance(value, dict):
        failures.append(f"{location}: must be an object")
        return False
    actual = set(value)
    if actual != expected:
        missing = sorted(expected - actual)
        unknown = sorted(actual - expected)
        if missing:
            failures.append(f"{location}: missing keys {missing}")
        if unknown:
            failures.append(f"{location}: unknown keys {unknown}")
        return False
    return True


def _is_normalized_path(value: Any, *, prefix: bool = False) -> bool:
    if not isinstance(value, str) or not value or "\\" in value or value.startswith("/"):
        return False
    path_value = value[:-1] if prefix and value.endswith("/") else value
    path = PurePosixPath(path_value)
    return ".." not in path.parts and str(path) == path_value


def _rule_is_usable(rule: Any) -> bool:
    if not isinstance(rule, dict) or set(rule) != {"id", "priority", "category", "match"}:
        return False
    match = rule.get("match")
    return (
        isinstance(rule.get("id"), str)
        and isinstance(rule.get("priority"), int)
        and not isinstance(rule.get("priority"), bool)
        and isinstance(rule.get("category"), str)
        and isinstance(match, dict)
        and set(match) == {"kind", "paths"}
        and match.get("kind") in {"exact", "prefix", "suffix", "basename"}
        and isinstance(match.get("paths"), list)
        and all(isinstance(path, str) for path in match["paths"])
    )


def _match_rule(path: str, rule: dict[str, Any]) -> bool:
    match = rule["match"]
    kind = match["kind"]
    values = match["paths"]
    if kind == "exact":
        return path in values
    if kind == "prefix":
        return any(path.startswith(value) for value in values)
    if kind == "suffix":
        return any(path.endswith(value) for value in values)
    if kind == "basename":
        return PurePosixPath(path).name in values
    return False


def classify_path(
    path: str, rules: list[dict[str, Any]]
) -> tuple[str | None, list[str]]:
    matches = [rule for rule in rules if _match_rule(path, rule)]
    if not matches:
        return None, []
    best_priority = min(rule["priority"] for rule in matches)
    winners = [rule for rule in matches if rule["priority"] == best_priority]
    if len(winners) != 1:
        return None, [rule["id"] for rule in winners]
    return winners[0]["category"], []


def _validate_rules(
    classification: dict[str, Any], category_ids: set[str], failures: list[str]
) -> list[dict[str, Any]]:
    rules_value = classification.get("rules")
    if not isinstance(rules_value, list) or not rules_value:
        failures.append("classification.rules: must be a nonempty array")
        return []

    rules: list[dict[str, Any]] = []
    ids: set[str] = set()
    priorities: set[int] = set()
    matcher_values: set[tuple[str, str]] = set()
    for index, value in enumerate(rules_value):
        location = f"classification.rules[{index}]"
        if not _require_keys(
            value, {"id", "priority", "category", "match"}, location, failures
        ):
            continue
        rule_id = value["id"]
        if not isinstance(rule_id, str) or IDENTIFIER.fullmatch(rule_id) is None:
            failures.append(f"{location}.id: invalid identifier")
        elif rule_id in ids:
            failures.append(f"{location}.id: duplicate identifier {rule_id}")
        else:
            ids.add(rule_id)
        priority = value["priority"]
        if not isinstance(priority, int) or isinstance(priority, bool) or priority < 0:
            failures.append(f"{location}.priority: must be a nonnegative integer")
        elif priority in priorities:
            failures.append(f"{location}.priority: duplicate priority {priority}")
        else:
            priorities.add(priority)
        if not isinstance(value["category"], str) or value["category"] not in category_ids:
            failures.append(f"{location}.category: unknown category")
        match = value["match"]
        if not _require_keys(match, {"kind", "paths"}, f"{location}.match", failures):
            continue
        kind = match["kind"]
        paths = match["paths"]
        if not isinstance(kind, str) or kind not in {
            "exact",
            "prefix",
            "suffix",
            "basename",
        }:
            failures.append(f"{location}.match.kind: unsupported matcher")
            continue
        if not isinstance(paths, list) or not paths:
            failures.append(f"{location}.match.paths: must be a nonempty array")
            continue
        for path_index, path in enumerate(paths):
            path_location = f"{location}.match.paths[{path_index}]"
            valid = False
            if kind == "prefix":
                valid = (
                    isinstance(path, str)
                    and path.endswith("/")
                    and _is_normalized_path(path, prefix=True)
                )
            elif kind == "suffix":
                valid = (
                    isinstance(path, str)
                    and path.startswith(".")
                    and "/" not in path
                )
            elif kind == "basename":
                valid = (
                    isinstance(path, str)
                    and "/" not in path
                    and _is_normalized_path(path)
                )
            else:
                valid = _is_normalized_path(path)
            if not valid:
                failures.append(f"{path_location}: invalid normalized {kind} value")
                continue
            matcher = (kind, path)
            if matcher in matcher_values:
                failures.append(f"{path_location}: duplicate matcher {kind}:{path}")
            matcher_values.add(matcher)
        if _rule_is_usable(value):
            rules.append(value)
    actual_rules = sorted(
        (
            rule["id"],
            rule["priority"],
            rule["category"],
            rule["match"]["kind"],
            tuple(sorted(rule["match"]["paths"])),
        )
        for rule in rules
    )
    expected_rules = sorted(
        (rule_id, priority, category, kind, tuple(sorted(paths)))
        for rule_id, priority, category, kind, paths in EXPECTED_RULES
    )
    if actual_rules != expected_rules:
        failures.append("classification.rules: frozen path rules differ")
    return rules


def _validate_exact_paths(
    paths: Any,
    location: str,
    expected: list[str],
    category: str,
    rules: list[dict[str, Any]],
    failures: list[str],
) -> None:
    if not isinstance(paths, list):
        failures.append(f"{location}: must be an array")
        return
    if paths != expected:
        failures.append(f"{location}: frozen path set differs")
    for path in paths:
        if not _is_normalized_path(path):
            failures.append(f"{location}: invalid path")
            continue
        actual_category, ties = classify_path(path, rules)
        if ties or actual_category != category:
            failures.append(f"{path}: exact path lacks {category} classification")


def _path_uses_symlink(root: Path, path: Path) -> bool:
    current = root
    if current.is_symlink():
        return True
    for part in path.relative_to(root).parts:
        current /= part
        if current.is_symlink():
            return True
    return False


def _validate_generated_copies(
    document: dict[str, Any],
    rules: list[dict[str, Any]],
    root: Path | None,
    failures: list[str],
) -> None:
    classification = document.get("classification")
    if not isinstance(classification, dict):
        return
    copies = classification.get("generated_copies")
    if not isinstance(copies, list):
        failures.append("classification.generated_copies: must be an array")
        return
    for index, copy in enumerate(copies):
        _require_keys(
            copy,
            {"source", "copy", "mode", "target_license"},
            f"classification.generated_copies[{index}]",
            failures,
        )
    if copies != EXPECTED_GENERATED_COPIES:
        failures.append("classification.generated_copies: frozen copy set differs")
        return
    for index, copy in enumerate(copies):
        for key in ("source", "copy"):
            is_prefix = copy["mode"] == "packaged-subset"
            if not _is_normalized_path(copy[key], prefix=is_prefix):
                failures.append(
                    f"classification.generated_copies[{index}].{key}: invalid path"
                )
        source_category, source_ties = classify_path(copy["source"], rules)
        copied_category, copied_ties = classify_path(copy["copy"], rules)
        if source_ties or copied_ties:
            failures.append(
                f"classification.generated_copies[{index}]: ambiguous classification"
            )
        if source_category != "normative-specification" or copied_category != (
            "normative-specification"
        ):
            failures.append(
                f"classification.generated_copies[{index}]: source and copy must be "
                "normative specifications"
            )
        if copy["target_license"] != "Apache-2.0":
            failures.append(
                f"classification.generated_copies[{index}]: copies must inherit Apache-2.0"
            )
        if root is None:
            continue
        source = root / copy["source"]
        destination = root / copy["copy"]
        if copy["mode"] == "exact-copy":
            if (
                _path_uses_symlink(root, source)
                or _path_uses_symlink(root, destination)
                or not source.is_file()
                or not destination.is_file()
            ):
                failures.append(
                    f"classification.generated_copies[{index}]: exact source and copy "
                    "must be regular non-symlink files"
                )
            elif source.read_bytes() != destination.read_bytes():
                failures.append(
                    f"classification.generated_copies[{index}]: exact copy bytes differ"
                )
            continue
        if (
            _path_uses_symlink(root, source)
            or _path_uses_symlink(root, destination)
            or not source.is_dir()
            or not destination.is_dir()
        ):
            failures.append(
                f"classification.generated_copies[{index}]: packaged subset source and "
                "copy must be non-symlink directories"
            )
            continue
        source_entries = sorted(source.rglob("*"))
        copied_entries = sorted(destination.rglob("*"))
        for tree, entries in (("source", source_entries), ("copy", copied_entries)):
            for entry in entries:
                if _path_uses_symlink(root, entry):
                    failures.append(
                        f"{entry.relative_to(root)}: generated-copy {tree} contains a "
                        "symlink"
                    )
        copied_files = sorted(
            path
            for path in copied_entries
            if path.is_file() and not path.is_symlink()
        )
        if not copied_files:
            failures.append(
                f"classification.generated_copies[{index}]: packaged subset is empty"
            )
        for copied_file in copied_files:
            source_file = source / copied_file.relative_to(destination)
            if _path_uses_symlink(root, source_file) or not source_file.is_file():
                failures.append(
                    f"{copied_file.relative_to(root)}: source copy must be a regular "
                    "non-symlink file"
                )
            elif source_file.read_bytes() != copied_file.read_bytes():
                failures.append(f"{copied_file.relative_to(root)}: source copy bytes differ")


def validate_contract(
    document: Any, repository_paths: list[str], root: Path | None = None
) -> ValidationResult:
    failures: list[str] = []
    classifications: dict[str, str] = {}
    blockers: list[str] = []
    if not _require_keys(document, TOP_LEVEL_KEYS, "contract", failures):
        return ValidationResult(failures, classifications, blockers)

    if document["$comment"] != "SPDX-License-Identifier: Apache-2.0":
        failures.append("contract.$comment: effective header must be Apache-2.0")
    if document["schema"] != "hyphae-relicensing-classification-v1":
        failures.append("contract.schema: unsupported identifier")
    if document["target_release"] != "1.2.0" or VERSION.fullmatch(
        str(document["target_release"])
    ) is None:
        failures.append("contract.target_release: must be exactly 1.2.0")
    if not _require_keys(
        document["licenses"], set(EXPECTED_LICENSES), "contract.licenses", failures
    ) or document["licenses"] != EXPECTED_LICENSES:
        failures.append("contract.licenses: frozen target identifiers differ")

    history = document["historical_releases"]
    if history != EXPECTED_HISTORY:
        failures.append("contract.historical_releases: immutable release history differs")
    if isinstance(history, list):
        for index, release in enumerate(history):
            if not _require_keys(
                release,
                {
                    "minimum",
                    "maximum",
                    "software",
                    "documentation",
                    "evidence",
                    "immutable",
                },
                f"contract.historical_releases[{index}]",
                failures,
            ):
                continue
            for field in ("minimum", "maximum"):
                if not isinstance(release[field], str) or VERSION.fullmatch(
                    release[field]
                ) is None:
                    failures.append(
                        f"contract.historical_releases[{index}].{field}: invalid version"
                    )
            if not isinstance(release["evidence"], list) or not release[
                "evidence"
            ] or not all(
                isinstance(item, str) and item.strip() for item in release["evidence"]
            ):
                failures.append(
                    f"contract.historical_releases[{index}].evidence: "
                    "must be a nonempty string array"
                )

    categories = document["categories"]
    category_ids: set[str] = set()
    category_licenses: dict[str, str | None] = {}
    category_definitions: dict[str, str] = {}
    if not isinstance(categories, list):
        failures.append("contract.categories: must be an array")
    else:
        for index, category in enumerate(categories):
            location = f"contract.categories[{index}]"
            if not _require_keys(
                category, {"id", "target_license", "definition"}, location, failures
            ):
                continue
            category_id = category["id"]
            if not isinstance(category_id, str) or IDENTIFIER.fullmatch(category_id) is None:
                failures.append(f"{location}.id: invalid identifier")
                continue
            if category_id in category_ids:
                failures.append(f"{location}.id: duplicate identifier")
            category_ids.add(category_id)
            category_licenses[category_id] = category["target_license"]
            if not isinstance(category["definition"], str) or not category[
                "definition"
            ].strip():
                failures.append(f"{location}.definition: must be nonempty")
            else:
                category_definitions[category_id] = category["definition"]
    if category_licenses != EXPECTED_CATEGORY_LICENSES:
        failures.append("contract.categories: category identifiers or licenses differ")
    if category_definitions != EXPECTED_CATEGORY_DEFINITIONS:
        failures.append("contract.categories: category definitions differ")

    if not _require_keys(
        document["mixed_file_rule"],
        set(EXPECTED_MIXED_RULE),
        "contract.mixed_file_rule",
        failures,
    ) or document["mixed_file_rule"] != EXPECTED_MIXED_RULE:
        failures.append("contract.mixed_file_rule: frozen dominant-purpose rule differs")

    classification = document["classification"]
    rules: list[dict[str, Any]] = []
    if _require_keys(
        classification,
        {"precedence", "rules", "generated_copies"},
        "contract.classification",
        failures,
    ):
        if classification["precedence"] != (
            "The matching rule with the lowest unique integer priority wins; a tie is invalid."
        ):
            failures.append("contract.classification.precedence: unsupported semantics")
        rules = _validate_rules(classification, category_ids, failures)

    boundaries = document["boundaries"]
    if _require_keys(
        boundaries, {"trademarks", "third_party"}, "contract.boundaries", failures
    ):
        trademarks = boundaries["trademarks"]
        if _require_keys(
            trademarks,
            {"policy", "reserved_asset_paths", "statement"},
            "contract.boundaries.trademarks",
            failures,
        ):
            if trademarks["policy"] != "TRADEMARKS.md":
                failures.append("contract.boundaries.trademarks.policy: must be canonical")
            if not isinstance(trademarks["statement"], str) or not trademarks[
                "statement"
            ].strip():
                failures.append("contract.boundaries.trademarks.statement: must be nonempty")
            _validate_exact_paths(
                trademarks["reserved_asset_paths"],
                "contract.boundaries.trademarks.reserved_asset_paths",
                [
                    ".github/assets/hyphae-lockup-reversed.svg",
                    ".github/assets/hyphae-lockup.svg",
                ],
                "reserved-trademark-asset",
                rules,
                failures,
            )
        third_party = boundaries["third_party"]
        if _require_keys(
            third_party,
            {"notice", "exact_paths", "statement"},
            "contract.boundaries.third_party",
            failures,
        ):
            if third_party["notice"] != "THIRD_PARTY_NOTICES.md":
                failures.append("contract.boundaries.third_party.notice: must be canonical")
            _validate_exact_paths(
                third_party["exact_paths"],
                "contract.boundaries.third_party.exact_paths",
                [".github/assets/oin-member-2-0-horiz.png", "DCO", "THIRD_PARTY_LICENSES.txt"],
                "third-party-material",
                rules,
                failures,
            )
            if not isinstance(third_party["statement"], str) or not third_party[
                "statement"
            ].strip():
                failures.append("contract.boundaries.third_party.statement: must be nonempty")

    if not _require_keys(
        document["effective_transition"],
        set(EXPECTED_TRANSITION),
        "contract.effective_transition",
        failures,
    ) or document["effective_transition"] != EXPECTED_TRANSITION:
        failures.append("contract.effective_transition: effective state differs")

    preflight = document["preflight"]
    if _require_keys(
        preflight,
        {"overall_status", "completion_claim", "evidence_categories"},
        "contract.preflight",
        failures,
    ):
        if preflight["overall_status"] != "accepted" or preflight[
            "completion_claim"
        ] is not True:
            failures.append("contract.preflight: accepted completion state differs")
        evidence_categories = preflight["evidence_categories"]
        if not isinstance(evidence_categories, list):
            failures.append("contract.preflight.evidence_categories: must be an array")
        else:
            actual_preflight: list[tuple[str, str, list[str]]] = []
            seen_preflight: set[str] = set()
            for index, evidence in enumerate(evidence_categories):
                location = f"contract.preflight.evidence_categories[{index}]"
                if not _require_keys(
                    evidence, {"id", "required", "status", "evidence"}, location, failures
                ):
                    continue
                evidence_id = evidence["id"]
                if not isinstance(evidence_id, str) or IDENTIFIER.fullmatch(
                    evidence_id
                ) is None:
                    failures.append(f"{location}.id: invalid identifier")
                    continue
                if evidence_id in seen_preflight:
                    failures.append(f"{location}.id: duplicate identifier")
                seen_preflight.add(evidence_id)
                if evidence["required"] is not True:
                    failures.append(f"{location}.required: must be true")
                if not isinstance(evidence["evidence"], list) or not all(
                    _is_normalized_path(path) for path in evidence["evidence"]
                ):
                    failures.append(f"{location}.evidence: invalid path array")
                    continue
                actual_preflight.append(
                    (evidence_id, evidence["status"], evidence["evidence"])
                )
                if evidence["status"] in BLOCKING_PREFLIGHT_STATUSES:
                    blockers.append(evidence_id)
                elif evidence["status"] not in NONBLOCKING_PREFLIGHT_STATUSES:
                    failures.append(f"{location}.status: unsupported status")
                if root is not None:
                    for path in evidence["evidence"]:
                        if not (root / path).is_file():
                            failures.append(f"{location}.evidence: missing {path}")
            if actual_preflight != EXPECTED_PREFLIGHT:
                failures.append(
                    "contract.preflight.evidence_categories: truthful frozen statuses differ"
                )

    all_paths = sorted(set(repository_paths) | set(REPRESENTATIVE_CLASSIFICATIONS))
    if len(all_paths) > MAX_REPOSITORY_PATHS:
        failures.append(
            f"repository path inventory exceeds bounded limit {MAX_REPOSITORY_PATHS}"
        )
    for path in all_paths[:MAX_REPOSITORY_PATHS]:
        if not _is_normalized_path(path):
            failures.append(f"{path!r}: repository path is not normalized")
            continue
        category, ties = classify_path(path, rules)
        if ties:
            failures.append(f"{path}: ambiguous winning rules {ties}")
        elif category is None:
            failures.append(f"{path}: unclassified repository path")
        else:
            classifications[path] = category
    for path, expected in REPRESENTATIVE_CLASSIFICATIONS.items():
        if classifications.get(path) != expected:
            failures.append(
                f"{path}: expected representative category {expected}, got "
                f"{classifications.get(path)}"
            )

    _validate_generated_copies(document, rules, root, failures)
    verification = document["verification"]
    if not _require_keys(
        verification,
        {
            "agpl_history_allowlist",
            "distribution_copy_suffixes",
            "historical_agpl_literal_allowlist",
        },
        "contract.verification",
        failures,
    ):
        return ValidationResult(failures, classifications, blockers)
    if verification["agpl_history_allowlist"] != EXPECTED_AGPL_ALLOWLIST:
        failures.append("contract.verification.agpl_history_allowlist: frozen set differs")
    if verification["distribution_copy_suffixes"] != ["/LICENSE-POLICY.md"]:
        failures.append("contract.verification.distribution_copy_suffixes: frozen set differs")
    if verification["historical_agpl_literal_allowlist"] != [
        "AGPL-3.0-only",
        "GNU Affero",
    ]:
        failures.append("contract.verification.historical_agpl_literal_allowlist: frozen set differs")
    if root is not None and (root / ".git").exists():
        observed_agpl: set[str] = set()
        for path in repository_paths:
            candidate = root / path
            if not candidate.is_file() or candidate.is_symlink():
                continue
            try:
                encoded = candidate.read_bytes()
            except OSError as error:
                failures.append(f"{path}: cannot inspect effective license declarations: {error}")
                continue
            if b"AGPL-3.0-only" not in encoded and b"GNU Affero" not in encoded:
                continue
            if any(path.endswith(suffix) for suffix in verification["distribution_copy_suffixes"]):
                continue
            observed_agpl.add(path)
        if observed_agpl != set(EXPECTED_AGPL_ALLOWLIST):
            unexpected = sorted(observed_agpl - set(EXPECTED_AGPL_ALLOWLIST))
            missing = sorted(set(EXPECTED_AGPL_ALLOWLIST) - observed_agpl)
            if unexpected:
                failures.append(f"effective AGPL allowlist: unexpected paths {unexpected}")
            if missing:
                failures.append(f"effective AGPL allowlist: expected historical paths missing {missing}")
    return ValidationResult(failures, classifications, blockers)


def _reject_duplicate_json_keys(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    value: dict[str, Any] = {}
    for key, item in pairs:
        if key in value:
            raise ValueError(f"duplicate JSON key: {key}")
        value[key] = item
    return value


def _reject_nonstandard_json_constant(value: str) -> Any:
    raise ValueError(f"nonstandard JSON constant: {value}")


def load_contract(path: Path = CONTRACT_PATH) -> dict[str, Any]:
    with path.open("rb") as contract_file:
        encoded = contract_file.read(MAX_CONTRACT_BYTES + 1)
    if len(encoded) > MAX_CONTRACT_BYTES:
        raise ValueError(
            f"classification contract exceeds {MAX_CONTRACT_BYTES} bytes"
        )
    value = json.loads(
        encoded.decode("utf-8"),
        object_pairs_hook=_reject_duplicate_json_keys,
        parse_constant=_reject_nonstandard_json_constant,
    )
    if not isinstance(value, dict):
        raise ValueError("classification contract must be a JSON object")
    return value


def _load_json_evidence(root: Path, relative: str) -> dict[str, Any]:
    path = root / relative
    if _path_uses_symlink(root, path) or not path.is_file():
        raise ValueError(f"{relative}: evidence must be a regular non-symlink file")
    encoded = path.read_bytes()
    if len(encoded) > MAX_EVIDENCE_BYTES:
        raise ValueError(f"{relative}: evidence exceeds {MAX_EVIDENCE_BYTES} bytes")
    value = json.loads(
        encoded.decode("utf-8"),
        object_pairs_hook=_reject_duplicate_json_keys,
        parse_constant=_reject_nonstandard_json_constant,
    )
    if not isinstance(value, dict):
        raise ValueError(f"{relative}: evidence must be a JSON object")
    return value


def _source_binding(
    evidence: dict[str, Any], relative: str, root: Path, failures: list[str]
) -> tuple[str | None, str | None]:
    source = evidence.get("source")
    if not isinstance(source, dict):
        failures.append(f"{relative}.source: must be an object")
        return None, None
    commit = source.get("commit")
    tree = source.get("tree")
    if not isinstance(commit, str) or GIT_OBJECT_ID.fullmatch(commit) is None:
        failures.append(f"{relative}.source.commit: invalid full Git object ID")
        commit = None
    if not isinstance(tree, str) or GIT_OBJECT_ID.fullmatch(tree) is None:
        failures.append(f"{relative}.source.tree: invalid full Git object ID")
        tree = None
    if commit is not None and tree is not None:
        try:
            actual_tree = _git_read(root, "rev-parse", f"{commit}^{{tree}}").decode(
                "ascii"
            ).strip()
        except (subprocess.SubprocessError, UnicodeError, ValueError):
            failures.append(f"{relative}.source.commit: object is unavailable")
        else:
            if actual_tree != tree:
                failures.append(f"{relative}.source.tree: does not match source commit")
    return commit, tree


def _validate_digest(value: Any, location: str, failures: list[str]) -> None:
    if not isinstance(value, str) or SHA256.fullmatch(value) is None:
        failures.append(f"{location}: invalid SHA-256")


def _current_cargo_inputs(root: Path) -> list[dict[str, str]]:
    encoded_paths = _git_read(
        root,
        "ls-files",
        "--cached",
        "--others",
        "--exclude-standard",
        "-z",
    )
    paths = sorted(
        path
        for path in encoded_paths.decode("utf-8").split("\0")
        if path
        and (
            path == "deny.toml"
            or path.endswith("Cargo.toml")
            or path.endswith("Cargo.lock")
        )
    )
    inputs: list[dict[str, str]] = []
    for path in paths:
        candidate = root / path
        if _path_uses_symlink(root, candidate) or not candidate.is_file():
            raise ValueError(f"{path}: current Cargo input must be a regular file")
        inputs.append(
            {
                "path": path,
                "sha256": hashlib.sha256(candidate.read_bytes()).hexdigest(),
            }
        )
    return inputs


def validate_preflight_evidence(
    root: Path = ROOT, *, require_transition_content: bool = True
) -> list[str]:
    failures: list[str] = []
    try:
        dependency = _load_json_evidence(root, DEPENDENCY_RECEIPT_PATH)
        audit = _load_json_evidence(root, REPOSITORY_AUDIT_PATH)
        attestation = _load_json_evidence(root, REPRESENTATIVE_ATTESTATION_PATH)
        dependabot = _load_json_evidence(root, DEPENDABOT_REVIEW_PATH)
    except (OSError, UnicodeError, ValueError, json.JSONDecodeError) as error:
        return [str(error)]

    dependency_commit, dependency_tree = _source_binding(
        dependency, DEPENDENCY_RECEIPT_PATH, root, failures
    )
    try:
        from tools.check_dependency_license_aggregate import validate_aggregate

        failures.extend(validate_aggregate(root))
    except (ImportError, OSError, ValueError) as error:
        failures.append(f"{DEPENDENCY_AGGREGATE_PATH}: cannot validate aggregate: {error}")
    audit_commit, audit_tree = _source_binding(
        audit, REPOSITORY_AUDIT_PATH, root, failures
    )
    attestation_commit, attestation_tree = _source_binding(
        attestation, REPRESENTATIVE_ATTESTATION_PATH, root, failures
    )
    dependabot_commit, dependabot_tree = _source_binding(
        dependabot, DEPENDABOT_REVIEW_PATH, root, failures
    )
    if (audit_commit, audit_tree) != (LEGAL_BASE_COMMIT, LEGAL_BASE_TREE):
        failures.append("preflight evidence: repository audit differs from exact legal base")
    if (attestation_commit, attestation_tree) != (LEGAL_BASE_COMMIT, LEGAL_BASE_TREE):
        failures.append("preflight evidence: owner attestation differs from exact legal base")
    if (dependabot_commit, dependabot_tree) != (LEGAL_BASE_COMMIT, LEGAL_BASE_TREE):
        failures.append("preflight evidence: Dependabot review differs from exact legal base")

    if dependency.get("schema") != "hyphae-relicensing-dependency-receipt-v1":
        failures.append(f"{DEPENDENCY_RECEIPT_PATH}.schema: unsupported identifier")
    if dependency.get("target_release") != "1.2.0":
        failures.append(f"{DEPENDENCY_RECEIPT_PATH}.target_release: must be 1.2.0")
    scope = dependency.get("scope")
    if not isinstance(scope, dict) or scope.get("evidence_evolution") != (
        DEPENDENCY_EVIDENCE_EVOLUTION
    ):
        failures.append(
            f"{DEPENDENCY_RECEIPT_PATH}.scope: evidence evolution statement differs"
        )
    source = dependency.get("source")
    source_mode = source.get("mode") if isinstance(source, dict) else None
    if not isinstance(source, dict) or source_mode not in {
        "clean-commit",
        "integration-tree",
    }:
        failures.append(f"{DEPENDENCY_RECEIPT_PATH}.source: unsupported source mode")
    elif source_mode == "clean-commit" and source.get("worktree_clean") is not True:
        failures.append(f"{DEPENDENCY_RECEIPT_PATH}.source: clean source claim differs")
    elif source_mode == "integration-tree" and source.get("worktree_clean") is not False:
        failures.append(f"{DEPENDENCY_RECEIPT_PATH}.source: integration-tree claim differs")
    if isinstance(source, dict) and (
        source.get("legal_base_commit"), source.get("legal_base_tree")
    ) != (LEGAL_BASE_COMMIT, LEGAL_BASE_TREE):
        failures.append(
            f"{DEPENDENCY_RECEIPT_PATH}.source: legal base identity differs"
        )
    if dependency_commit is not None:
        try:
            head = _git_read(root, "rev-parse", "HEAD^{commit}").decode("ascii").strip()
            _git_read(
                root,
                "merge-base",
                "--is-ancestor",
                LEGAL_BASE_COMMIT,
                dependency_commit,
            )
            _git_read(
                root,
                "merge-base",
                "--is-ancestor",
                dependency_commit,
                head,
            )
        except (subprocess.SubprocessError, UnicodeError, ValueError):
            failures.append(
                f"{DEPENDENCY_RECEIPT_PATH}.source.commit: must descend from the legal base and anchor current HEAD"
            )
        else:
            try:
                current_tree = _git_read(root, "rev-parse", "HEAD^{tree}").decode(
                    "ascii"
                ).strip()
            except (subprocess.SubprocessError, UnicodeError, ValueError):
                current_tree = None
                failures.append(
                    f"{DEPENDENCY_RECEIPT_PATH}.source.tree: current HEAD tree unavailable"
                )
            if source_mode == "clean-commit" and dependency_commit != head:
                failures.append(
                    f"{DEPENDENCY_RECEIPT_PATH}.source.commit: clean receipt must bind current HEAD"
                )
            if source_mode == "clean-commit" and dependency_tree != current_tree:
                failures.append(
                    f"{DEPENDENCY_RECEIPT_PATH}.source.tree: clean receipt must bind current HEAD tree"
                )
            if source_mode == "clean-commit":
                try:
                    status = _git_read(
                        root,
                        "status",
                        "--porcelain=v1",
                        "--untracked-files=all",
                    )
                except (subprocess.SubprocessError, ValueError):
                    failures.append(
                        f"{DEPENDENCY_RECEIPT_PATH}.source: cannot verify clean source claim"
                    )
                else:
                    if status:
                        failures.append(
                            f"{DEPENDENCY_RECEIPT_PATH}.source: clean source claim is false for current content"
                        )
    inventory = dependency.get("inventory")
    if not isinstance(inventory, dict):
        failures.append(f"{DEPENDENCY_RECEIPT_PATH}.inventory: must be an object")
    else:
        packages = inventory.get("packages")
        if not isinstance(packages, list) or not packages:
            failures.append(f"{DEPENDENCY_RECEIPT_PATH}.inventory.packages: must be nonempty")
        else:
            expected_digest = hashlib.sha256(
                json.dumps(
                    packages,
                    ensure_ascii=True,
                    separators=(",", ":"),
                    sort_keys=True,
                ).encode("utf-8")
            ).hexdigest()
            if inventory.get("canonical_sha256") != expected_digest:
                failures.append(
                    f"{DEPENDENCY_RECEIPT_PATH}.inventory.canonical_sha256: digest differs"
                )
            if inventory.get("package_count") != len(packages):
                failures.append(
                    f"{DEPENDENCY_RECEIPT_PATH}.inventory.package_count: count differs"
                )
            seen: set[tuple[str, str, str]] = set()
            workspace_count = 0
            observed_copyleft_candidates: set[tuple[str, str, str]] = set()
            for index, package in enumerate(packages):
                location = f"{DEPENDENCY_RECEIPT_PATH}.inventory.packages[{index}]"
                if not isinstance(package, dict) or set(package) != {
                    "license",
                    "license_file",
                    "name",
                    "source",
                    "version",
                }:
                    failures.append(f"{location}: malformed package identity")
                    continue
                identity = (
                    package.get("name"),
                    package.get("version"),
                    package.get("source"),
                )
                if not all(isinstance(value, str) and value for value in identity):
                    failures.append(f"{location}: incomplete package identity")
                    continue
                if identity in seen:
                    failures.append(f"{location}: duplicate package identity")
                seen.add(identity)
                if package["source"].startswith("workspace:"):
                    workspace_count += 1
                elif isinstance(package.get("license"), str) and any(
                    identifier in package["license"]
                    for identifier in ("AGPL-", "GPL-", "LGPL-")
                ):
                    observed_copyleft_candidates.add(identity)
                if package.get("license") is None and package.get("license_file") is None:
                    failures.append(f"{location}: package has no license evidence")
            if inventory.get("workspace_package_count") != workspace_count:
                failures.append(
                    f"{DEPENDENCY_RECEIPT_PATH}.inventory.workspace_package_count: count differs"
                )
            if inventory.get("external_package_count") != len(packages) - workspace_count:
                failures.append(
                    f"{DEPENDENCY_RECEIPT_PATH}.inventory.external_package_count: count differs"
                )
            if len(packages) != 299 or workspace_count != 27:
                failures.append(
                    f"{DEPENDENCY_RECEIPT_PATH}.inventory: frozen exact-source counts differ"
                )
    review = dependency.get("compatibility_review")
    if not isinstance(review, dict):
        failures.append(
            f"{DEPENDENCY_RECEIPT_PATH}.compatibility_review: must be an object"
        )
    else:
        deny = review.get("cargo_deny")
        if not isinstance(deny, dict) or deny.get("exit_status") != 0:
            failures.append(
                f"{DEPENDENCY_RECEIPT_PATH}.compatibility_review.cargo_deny: must pass"
            )
        for key in (
            "external_gpl_2_0_only_candidates",
            "external_strong_copyleft_without_permissive_alternative",
            "packages_without_license_or_license_file",
        ):
            if review.get(key) != []:
                failures.append(
                    f"{DEPENDENCY_RECEIPT_PATH}.compatibility_review.{key}: must be empty"
                )
        reported_candidates = review.get("external_copyleft_candidates")
        if not isinstance(reported_candidates, list):
            failures.append(
                f"{DEPENDENCY_RECEIPT_PATH}.compatibility_review.external_copyleft_candidates: must be an array"
            )
        else:
            reported_identities = {
                (candidate.get("name"), candidate.get("version"), candidate.get("source"))
                for candidate in reported_candidates
                if isinstance(candidate, dict)
            }
            if reported_identities != observed_copyleft_candidates:
                failures.append(
                    f"{DEPENDENCY_RECEIPT_PATH}.compatibility_review.external_copyleft_candidates: inventory differs"
                )
        if review.get("result") != "pass" or dependency.get("result") != "pass":
            failures.append(f"{DEPENDENCY_RECEIPT_PATH}.result: must be pass")
    inputs = dependency.get("source_inputs")
    if not isinstance(inputs, list) or not inputs:
        failures.append(f"{DEPENDENCY_RECEIPT_PATH}.source_inputs: must be nonempty")
    elif dependency_commit is not None:
        expected_inputs_digest = hashlib.sha256(
            json.dumps(
                inputs,
                ensure_ascii=True,
                separators=(",", ":"),
                sort_keys=True,
            ).encode("utf-8")
        ).hexdigest()
        if not isinstance(source, dict) or source.get("source_inputs_sha256") != (
            expected_inputs_digest
        ):
            failures.append(
                f"{DEPENDENCY_RECEIPT_PATH}.source.source_inputs_sha256: digest differs"
            )
        seen_paths: set[str] = set()
        for index, source_input in enumerate(inputs):
            location = f"{DEPENDENCY_RECEIPT_PATH}.source_inputs[{index}]"
            if not isinstance(source_input, dict) or set(source_input) != {"path", "sha256"}:
                failures.append(f"{location}: malformed source input")
                continue
            path = source_input.get("path")
            digest = source_input.get("sha256")
            if not _is_normalized_path(path):
                failures.append(f"{location}.path: invalid path")
                continue
            if path in seen_paths:
                failures.append(f"{location}.path: duplicate path")
            seen_paths.add(path)
            _validate_digest(digest, f"{location}.sha256", failures)
            try:
                candidate = root / path
                if _path_uses_symlink(root, candidate) or not candidate.is_file():
                    raise ValueError("current input is not a regular file")
                encoded = candidate.read_bytes()
            except (OSError, subprocess.SubprocessError, ValueError):
                failures.append(f"{location}.path: unavailable from current content")
                continue
            if isinstance(digest, str) and hashlib.sha256(encoded).hexdigest() != digest:
                failures.append(f"{location}.sha256: source bytes differ")
        try:
            current_inputs = _current_cargo_inputs(root)
        except (OSError, UnicodeError, subprocess.SubprocessError, ValueError) as error:
            failures.append(
                f"{DEPENDENCY_RECEIPT_PATH}.source_inputs: cannot inventory current Cargo inputs: {error}"
            )
        else:
            if inputs != current_inputs:
                failures.append(
                    f"{DEPENDENCY_RECEIPT_PATH}.source_inputs: current Cargo input set or digest differs"
                )

    if audit.get("schema") != "hyphae-relicensing-repository-audit-v1":
        failures.append(f"{REPOSITORY_AUDIT_PATH}.schema: unsupported identifier")
    if audit.get("result") != "accepted-source-bound-preflight-evidence":
        failures.append(f"{REPOSITORY_AUDIT_PATH}.result: must be accepted")
    audit_source = audit.get("source")
    if isinstance(audit_source, dict) and audit_commit is not None:
        try:
            commit_count = int(
                _git_read(root, "rev-list", "--count", audit_commit).decode("ascii").strip()
            )
            tracked_path_bytes = _git_read(
                root, "ls-tree", "-r", "--name-only", audit_commit
            )
            tracked_paths = tracked_path_bytes.splitlines()
            tree_entry_bytes = _git_read(root, "ls-tree", "-r", "--full-tree", audit_commit)
        except (subprocess.SubprocessError, UnicodeError, ValueError):
            failures.append(f"{REPOSITORY_AUDIT_PATH}.source: cannot recompute inventory")
        else:
            if audit_source.get("reachable_commit_count") != commit_count:
                failures.append(
                    f"{REPOSITORY_AUDIT_PATH}.source.reachable_commit_count: count differs"
                )
            if audit_source.get("tracked_path_count") != len(tracked_paths):
                failures.append(
                    f"{REPOSITORY_AUDIT_PATH}.source.tracked_path_count: count differs"
                )
            if audit_source.get("tracked_path_list_sha256") != hashlib.sha256(
                tracked_path_bytes
            ).hexdigest():
                failures.append(
                    f"{REPOSITORY_AUDIT_PATH}.source.tracked_path_list_sha256: digest differs"
                )
            if audit_source.get("tree_entries_sha256") != hashlib.sha256(
                tree_entry_bytes
            ).hexdigest():
                failures.append(
                    f"{REPOSITORY_AUDIT_PATH}.source.tree_entries_sha256: digest differs"
                )
    author_inventory = audit.get("author_inventory")
    if not isinstance(author_inventory, dict):
        failures.append(f"{REPOSITORY_AUDIT_PATH}.author_inventory: must be an object")
    else:
        actors = author_inventory.get("actors")
        if not isinstance(actors, list) or not actors:
            failures.append(
                f"{REPOSITORY_AUDIT_PATH}.author_inventory.actors: must be nonempty"
            )
        elif any(
            not isinstance(actor, dict)
            or actor.get("authority_state")
            not in {
                "accepted-interactive-owner-attestation",
                "covered-by-owner-first-party-authority-attestation",
                "accepted-mechanical-first-party-review",
            }
            for actor in actors
        ):
            failures.append(f"{REPOSITORY_AUDIT_PATH}.author_inventory: authority state differs")
        if isinstance(actors, list) and sum(
            actor.get("commit_count", 0) for actor in actors if isinstance(actor, dict)
        ) != 847:
            failures.append(
                f"{REPOSITORY_AUDIT_PATH}.author_inventory: commit counts differ"
            )
    governance = audit.get("contribution_governance")
    if not isinstance(governance, dict) or governance.get("result") != (
        "accepted-effective-inbound-equals-outbound-with-dco"
    ):
        failures.append(
            f"{REPOSITORY_AUDIT_PATH}.contribution_governance: accepted governance differs"
        )

    if attestation.get("schema") != (
        "hyphae-relicensing-representative-attestation-v1"
    ):
        failures.append(
            f"{REPRESENTATIVE_ATTESTATION_PATH}.schema: unsupported identifier"
        )
    if attestation.get("status") != "accepted-interactive-owner-attestation":
        failures.append(
            f"{REPRESENTATIVE_ATTESTATION_PATH}.status: accepted status differs"
        )
    representative = attestation.get("representative")
    if not isinstance(representative, dict) or representative.get("authentication") != (
        "interactive-owner-attestation"
    ):
        failures.append(
            f"{REPRESENTATIVE_ATTESTATION_PATH}.representative: unsupported authentication claim"
        )
    elif representative.get("name") != "Mario Gutiérrez" or representative.get(
        "capacity"
    ) != "owner and representative of Celiums Solutions LLC":
        failures.append(f"{REPRESENTATIVE_ATTESTATION_PATH}.representative: identity differs")
    missing = attestation.get("missing_authentication")
    if missing != []:
        failures.append(
            f"{REPRESENTATIVE_ATTESTATION_PATH}.missing_authentication: must be empty"
        )
    statements = attestation.get("statements_prepared_for_attestation")
    transition_authorization = (
        statements.get("transition_authorization")
        if isinstance(statements, dict)
        else None
    )
    copyright_authority = (
        statements.get("copyright_authority") if isinstance(statements, dict) else None
    )
    counsel = statements.get("counsel_approval") if isinstance(statements, dict) else None
    prior_commitments = (
        statements.get("prior_commitments") if isinstance(statements, dict) else None
    )
    dispositions = attestation.get("identity_dispositions")
    ec2_user = dispositions.get("ec2_user_commits") if isinstance(dispositions, dict) else None
    dependabot_disposition = (
        dispositions.get("dependabot_commits") if isinstance(dispositions, dict) else None
    )
    decisive_booleans = {
        "owner transition authorization": (
            transition_authorization.get("confirmed")
            if isinstance(transition_authorization, dict)
            else None
        ),
        "copyright authority": (
            copyright_authority.get("confirmed")
            if isinstance(copyright_authority, dict)
            else None
        ),
        "qualified counsel": (
            counsel.get("qualified_open_source_counsel")
            if isinstance(counsel, dict)
            else None
        ),
        "counsel written approval": (
            counsel.get("written_approval_received")
            if isinstance(counsel, dict)
            else None
        ),
        "confidential counsel record": (
            counsel.get("confidential_record_not_embedded")
            if isinstance(counsel, dict)
            else None
        ),
        "prior commitment absence": (
            prior_commitments.get("absence_confirmed")
            if isinstance(prior_commitments, dict)
            else None
        ),
        "ec2-user authority": (
            ec2_user.get("authority_confirmed")
            if isinstance(ec2_user, dict)
            else None
        ),
        "Dependabot disposition": (
            dependabot_disposition.get("authority_confirmed")
            if isinstance(dependabot_disposition, dict)
            else None
        ),
    }
    for label, value in decisive_booleans.items():
        if value is not True:
            failures.append(
                f"{REPRESENTATIVE_ATTESTATION_PATH}: {label} must be explicitly true"
            )
    if not isinstance(counsel, dict) or counsel.get(
        "apache_section_6_and_trademarks_scope_confirmed"
    ) is not False or counsel.get("nonconfidential_record_locator_or_sha256") is not None:
        failures.append(
            f"{REPRESENTATIVE_ATTESTATION_PATH}.counsel_approval: confidential scope must not be overstated"
        )

    if dependabot.get("schema") != "hyphae-relicensing-dependabot-review-v1":
        failures.append(f"{DEPENDABOT_REVIEW_PATH}.schema: unsupported identifier")
    reviews = dependabot.get("reviews")
    if not isinstance(reviews, list) or not reviews:
        failures.append(f"{DEPENDABOT_REVIEW_PATH}.reviews: must be a nonempty array")
    else:
        if dependabot.get("reviewed_commit_count") != len(reviews) or dependabot.get(
            "result"
        ) != "accepted-mechanical-first-party-review":
            failures.append(f"{DEPENDABOT_REVIEW_PATH}: accepted review result differs")
        try:
            if dependabot_commit is None:
                raise ValueError("Dependabot source commit is invalid")
            observed = _git_read(
                root,
                "log",
                dependabot_commit,
                "--author=dependabot\\|dependabot\\[bot\\]",
                "--format=%H",
            ).decode("ascii").splitlines()
        except (subprocess.SubprocessError, UnicodeError, ValueError):
            observed = []
            failures.append(f"{DEPENDABOT_REVIEW_PATH}.reviews: Git inventory unavailable")
        reviewed = [review.get("commit") for review in reviews if isinstance(review, dict)]
        if sorted(reviewed) != sorted(observed):
            failures.append(f"{DEPENDABOT_REVIEW_PATH}.reviews: commit inventory differs")
        method = dependabot.get("method")
        ordered = "".join(f"{commit}\n" for commit in sorted(observed)).encode("ascii")
        expected_method_keys = {*DEPENDABOT_REVIEW_METHOD, "ordered_commit_ids_sha256"}
        if not isinstance(method, dict) or set(method) != expected_method_keys:
            failures.append(f"{DEPENDABOT_REVIEW_PATH}.method: missing or unknown fields")
            method = {}
        elif any(method.get(key) != value for key, value in DEPENDABOT_REVIEW_METHOD.items()):
            failures.append(f"{DEPENDABOT_REVIEW_PATH}.method: review method differs")
        method_digest = method.get("ordered_commit_ids_sha256")
        _validate_digest(
            method_digest,
            f"{DEPENDABOT_REVIEW_PATH}.method.ordered_commit_ids_sha256",
            failures,
        )
        if method_digest != hashlib.sha256(ordered).hexdigest():
            failures.append(f"{DEPENDABOT_REVIEW_PATH}.method: commit digest differs")
        for index, review in enumerate(reviews):
            location = f"{DEPENDABOT_REVIEW_PATH}.reviews[{index}]"
            if not isinstance(review, dict):
                failures.append(f"{location}: must be an object")
                continue
            commit = review.get("commit")
            parent = review.get("parent")
            if not isinstance(commit, str) or not isinstance(parent, str):
                failures.append(f"{location}: commit or parent is invalid")
                continue
            try:
                patch = _git_read_unbounded(root, "diff-tree", "--no-ext-diff", "--binary", "--full-index", parent, commit)
                tree = _git_read(root, "rev-parse", f"{commit}^{{tree}}").decode("ascii").strip()
                actual_parent = _git_read(root, "rev-parse", f"{commit}^").decode("ascii").strip()
                subject = _git_read(root, "show", "-s", "--format=%s", commit).decode("utf-8").strip()
                changed_paths = _git_read(
                    root, "diff-tree", "--no-commit-id", "--name-only", "-r", commit
                ).splitlines()
                numstat = _git_read(
                    root, "diff-tree", "--no-commit-id", "--numstat", "-r", commit
                ).decode("utf-8").splitlines()
            except (subprocess.SubprocessError, UnicodeError, ValueError):
                failures.append(f"{location}: source commit cannot be recomputed")
                continue
            if review.get("patch_sha256") != hashlib.sha256(patch).hexdigest():
                failures.append(f"{location}.patch_sha256: patch differs")
            if review.get("tree") != tree:
                failures.append(f"{location}.tree: tree differs")
            if actual_parent != parent:
                failures.append(f"{location}.parent: parent differs")
            if review.get("subject") != subject:
                failures.append(f"{location}.subject: subject differs")
            if review.get("changed_path_count") != len(changed_paths):
                failures.append(f"{location}.changed_path_count: count differs")
            insertions = 0
            deletions = 0
            for row in numstat:
                fields = row.split("\t", 2)
                if len(fields) != 3 or not fields[0].isdigit() or not fields[1].isdigit():
                    failures.append(f"{location}: non-text numstat is unsupported")
                    break
                insertions += int(fields[0])
                deletions += int(fields[1])
            if review.get("insertions") != insertions:
                failures.append(f"{location}.insertions: count differs")
            if review.get("deletions") != deletions:
                failures.append(f"{location}.deletions: count differs")
    if require_transition_content:
        try:
            from tools.check_relicensing_transition import (
                validate_current_transition_content,
            )

            failures.extend(validate_current_transition_content(root))
        except (ImportError, OSError, ValueError) as error:
            failures.append(
                f"{TRANSITION_RECEIPT_PATH}: cannot validate current transition content: {error}"
            )
    return failures


def repository_paths(root: Path = ROOT) -> list[str]:
    completed = subprocess.run(
        [
            "git",
            "-C",
            str(root),
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
        ],
        check=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=10,
    )
    if len(completed.stdout) > MAX_GIT_OUTPUT:
        raise ValueError(f"git path inventory exceeds {MAX_GIT_OUTPUT} bytes")
    paths = [part.decode("utf-8") for part in completed.stdout.split(b"\0") if part]
    if len(paths) > MAX_REPOSITORY_PATHS:
        raise ValueError(f"git path inventory exceeds {MAX_REPOSITORY_PATHS} paths")
    return paths


def _git_read(root: Path, *arguments: str) -> bytes:
    completed = subprocess.run(
        ["git", "-C", str(root), *arguments],
        check=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=10,
    )
    if len(completed.stdout) > MAX_CONTRACT_BYTES:
        raise ValueError(
            f"historical git evidence exceeds {MAX_CONTRACT_BYTES} bytes"
        )
    return completed.stdout


def _git_read_unbounded(root: Path, *arguments: str) -> bytes:
    completed = subprocess.run(
        ["git", "-C", str(root), *arguments],
        check=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=10,
    )
    if len(completed.stdout) > MAX_EVIDENCE_BYTES:
        raise ValueError(f"git evidence exceeds {MAX_EVIDENCE_BYTES} bytes")
    return completed.stdout


def validate_historical_release_evidence(root: Path = ROOT) -> list[str]:
    failures: list[str] = []
    for tag, expected_commit in HISTORICAL_APACHE_TAGS:
        commit = _git_read(root, "rev-parse", "--verify", f"{tag}^{{commit}}")
        if commit.decode("ascii").strip() != expected_commit:
            failures.append(f"{tag}: historical tag commit differs")

        license_bytes = _git_read(root, "show", f"{tag}:LICENSE")
        if hashlib.sha256(license_bytes).hexdigest() != (
            HISTORICAL_APACHE_LICENSE_SHA256
        ):
            failures.append(f"{tag}: root LICENSE is not the retained Apache-2.0 text")

        cargo_bytes = _git_read(root, "show", f"{tag}:Cargo.toml")
        try:
            cargo = tomllib.loads(cargo_bytes.decode("utf-8"))
            cargo_license = cargo["workspace"]["package"]["license"]
        except (KeyError, TypeError, UnicodeError, tomllib.TOMLDecodeError):
            cargo_license = None
        if cargo_license != "Apache-2.0":
            failures.append(f"{tag}: root Cargo.toml does not declare Apache-2.0")

        readme = _git_read(root, "show", f"{tag}:README.md").decode("utf-8")
        if "Apache License 2.0" not in readme:
            failures.append(f"{tag}: root README.md does not declare Apache-2.0")

        documentation_license = subprocess.run(
            [
                "git",
                "-C",
                str(root),
                "cat-file",
                "-e",
                f"{tag}:LICENSE-DOCUMENTATION",
            ],
            check=False,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=10,
        )
        if documentation_license.returncode == 0:
            failures.append(
                f"{tag}: unexpected LICENSE-DOCUMENTATION contradicts historical evidence"
            )
    tag = "v1.1.0"
    if _git_read(root, "rev-parse", "--verify", tag).decode("ascii").strip() != (
        HISTORICAL_V1_1_0["tag_object"]
    ):
        failures.append(f"{tag}: immutable annotated tag object differs")
    if _git_read(root, "cat-file", "-t", tag).decode("ascii").strip() != "tag":
        failures.append(f"{tag}: immutable release tag is not annotated")
    if _git_read(root, "rev-parse", f"{tag}^{{commit}}").decode("ascii").strip() != (
        HISTORICAL_V1_1_0["commit"]
    ):
        failures.append(f"{tag}: immutable release commit differs")
    if _git_read(root, "rev-parse", f"{tag}^{{tree}}").decode("ascii").strip() != (
        HISTORICAL_V1_1_0["tree"]
    ):
        failures.append(f"{tag}: immutable release tree differs")
    for path, key in (
        ("LICENSE", "license_sha256"),
        ("LICENSE-DOCUMENTATION", "documentation_license_sha256"),
    ):
        if hashlib.sha256(_git_read(root, "show", f"{tag}:{path}")).hexdigest() != (
            HISTORICAL_V1_1_0[key]
        ):
            failures.append(f"{tag}: immutable {path} evidence differs")
    cargo = tomllib.loads(
        _git_read(root, "show", f"{tag}:Cargo.toml").decode("utf-8")
    )
    if cargo.get("workspace", {}).get("package", {}).get("license") != (
        "AGPL-3.0-only"
    ):
        failures.append(f"{tag}: immutable Cargo.toml license evidence differs")
    return failures


def main() -> int:
    try:
        contract = load_contract()
        paths = repository_paths()
        historical_failures = validate_historical_release_evidence()
        evidence_failures = validate_preflight_evidence()
    except (OSError, UnicodeError, ValueError, json.JSONDecodeError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    except subprocess.SubprocessError as error:
        print(f"error: bounded git inventory failed: {error}", file=sys.stderr)
        return 1

    result = validate_contract(contract, paths, ROOT)
    failures = [*historical_failures, *evidence_failures, *result.failures]
    if failures:
        for failure in failures:
            print(f"error: {failure}", file=sys.stderr)
        return 1
    print(
        "classification contract PASS: "
        f"{len(paths)} repository paths have deterministic target categories"
    )
    print(
        "preflight ACCEPTED: Apache-2.0 is effective for current first-party "
        "software and normative specifications"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
