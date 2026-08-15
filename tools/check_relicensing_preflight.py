#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only

"""Validate the bounded 1.2.0 target classification without closing preflight."""

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
CONTRACT_PATH = ROOT / "config" / "relicensing-1.2.0-classification.json"
IDENTIFIER = re.compile(r"^[a-z][a-z0-9]*(?:-[a-z0-9]+)*$")
VERSION = re.compile(r"^[0-9]+\.[0-9]+\.[0-9]+$")
MAX_GIT_OUTPUT = 4 * 1024 * 1024
MAX_REPOSITORY_PATHS = 10_000
MAX_CONTRACT_BYTES = 1024 * 1024
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
    "state": "preflight-only-not-effective",
    "current_software_license": "AGPL-3.0-only",
    "current_documentation_license": "CC-BY-SA-4.0",
    "required_event": (
        "one-atomic-relicensing-commit-after-all-preflight-evidence-is-accepted"
    ),
    "requires_all_preflight_evidence": True,
}
EXPECTED_PREFLIGHT = [
    ("counsel-approval", "open-unclaimed", []),
    ("copyright-relicensing-authority", "open-unclaimed", []),
    ("prior-commitments", "open-unclaimed", []),
    ("dependency-license-exact-sha", "open-unclaimed", []),
    (
        "specification-classification",
        "accepted-decision",
        [
            "docs/adr/0029-apache-2.0-software-and-normative-specifications.md",
            "config/relicensing-1.2.0-classification.json",
        ],
    ),
    ("contribution-governance", "open-unclaimed", []),
]
REPRESENTATIVE_CLASSIFICATIONS = {
    ".github/assets/hyphae-lockup.svg": "reserved-trademark-asset",
    ".github/workflows/ci.yml": "software",
    "AGENTS.md": "narrative-documentation",
    "Cargo.toml": "software",
    "LICENSE": "software",
    "LICENSE-DOCUMENTATION": "narrative-documentation",
    "NOTICE": "software",
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
            "compatibility/",
            "config/",
            "conformance/",
            "crates/",
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

    if document["$comment"] != "SPDX-License-Identifier: AGPL-3.0-only":
        failures.append("contract.$comment: current preflight header must remain AGPL-3.0-only")
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
                [],
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
        failures.append("contract.effective_transition: preflight-only state differs")

    preflight = document["preflight"]
    if _require_keys(
        preflight,
        {"overall_status", "completion_claim", "evidence_categories"},
        "contract.preflight",
        failures,
    ):
        if preflight["overall_status"] != "blocked" or preflight[
            "completion_claim"
        ] is not False:
            failures.append("contract.preflight: must truthfully remain blocked")
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
                if evidence["status"] == "open-unclaimed":
                    blockers.append(evidence_id)
                elif evidence["status"] == "accepted-decision" and root is not None:
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
    return failures


def main() -> int:
    try:
        contract = load_contract()
        paths = repository_paths()
        historical_failures = validate_historical_release_evidence()
    except (OSError, UnicodeError, ValueError, json.JSONDecodeError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    except subprocess.SubprocessError as error:
        print(f"error: bounded git inventory failed: {error}", file=sys.stderr)
        return 1

    result = validate_contract(contract, paths, ROOT)
    failures = [*historical_failures, *result.failures]
    if failures:
        for failure in failures:
            print(f"error: {failure}", file=sys.stderr)
        return 1
    print(
        "classification contract PASS: "
        f"{len(paths)} repository paths have deterministic target categories"
    )
    print(f"open preflight blockers ({len(result.blockers)}):")
    for blocker in result.blockers:
        print(f"- {blocker}: open-unclaimed")
    print(
        "preflight remains BLOCKED; Apache-2.0 is a 1.2.0 target and is not "
        "effective in the current tree"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
