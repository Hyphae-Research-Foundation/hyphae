from __future__ import annotations

import unittest

from tools.check_native_dependencies import (
    GateFailure,
    audit_metadata,
    audit_unsafe,
    sanitize_receipt_paths,
    validate_lint_policy,
)


REGISTRY = "registry+https://github.com/rust-lang/crates.io-index"


def package(
    name: str,
    *,
    version: str = "1.0.0",
    source: str | None = None,
    license_expression: str = "Apache-2.0",
) -> dict[str, object]:
    return {
        "id": f"{name} {version}",
        "name": name,
        "version": version,
        "source": source,
        "license": license_expression,
        "manifest_path": f"/repo/crates/{name}/Cargo.toml",
    }


def dependency(package_id: str, kind: str | None) -> dict[str, object]:
    return {
        "name": package_id.split(" ", 1)[0],
        "pkg": package_id,
        "dep_kinds": [{"kind": kind, "target": None}],
    }


def metadata(
    packages: list[dict[str, object]],
    root_dependencies: list[dict[str, object]],
) -> dict[str, object]:
    nodes = [
        {
            "id": package_record["id"],
            "deps": root_dependencies if package_record["name"] == "hyphae-native-runtime" else [],
        }
        for package_record in packages
    ]
    return {"packages": packages, "resolve": {"nodes": nodes}}


def reviewed_external(package_record: dict[str, object]) -> dict[str, str]:
    return {
        "name": str(package_record["name"]),
        "version": str(package_record["version"]),
        "source": str(package_record["source"]),
        "license": str(package_record["license"]),
        "category": "audited primitive",
        "rationale": "Fixture dependency reviewed for the native closure.",
    }


def policy(
    workspace_packages: list[str],
    external_packages: list[dict[str, object]],
    *,
    forbidden_packages: list[str] | None = None,
) -> dict[str, object]:
    return {
        "schema": "hyphae-native-dependency-policy-v1",
        "root_package": "hyphae-native-runtime",
        "workspace_packages": workspace_packages,
        "forbidden_packages": forbidden_packages or ["redb", "tantivy"],
        "external_packages": [
            reviewed_external(package_record) for package_record in external_packages
        ],
    }


def geiger_package(
    name: str,
    *,
    version: str = "1.0.0",
    unsafe_expressions: int = 0,
) -> dict[str, object]:
    return {
        "package": {
            "id": {
                "name": name,
                "version": version,
                "source": {"Path": f"file:///repo/crates/{name}"},
            }
        },
        "unsafety": {
            "used": {
                "functions": {"safe": 1, "unsafe_": 0},
                "exprs": {"safe": 1, "unsafe_": unsafe_expressions},
            },
            "unused": {
                "functions": {"safe": 0, "unsafe_": 0},
                "exprs": {"safe": 0, "unsafe_": 0},
            },
            "forbids_unsafe": False,
        },
    }


class NativeDependencyGateTests(unittest.TestCase):
    def test_metadata_closure_includes_build_and_excludes_dev_edges(self) -> None:
        root = package("hyphae-native-runtime")
        runtime = package("blake3", source=REGISTRY)
        build = package("cc", source=REGISTRY)
        dev = package("redb", source=REGISTRY)
        report = audit_metadata(
            metadata(
                [root, runtime, build, dev],
                [
                    dependency(str(runtime["id"]), None),
                    dependency(str(build["id"]), "build"),
                    dependency(str(dev["id"]), "dev"),
                ],
            ),
            policy(["hyphae-native-runtime"], [runtime, build]),
        )

        self.assertEqual(
            [entry["name"] for entry in report["packages"]],
            ["blake3", "cc", "hyphae-native-runtime"],
        )
        self.assertEqual(report["dependency_kinds"]["cc"], ["build"])
        self.assertNotIn("redb", report["dependency_kinds"])

    def test_forbidden_engine_fails_even_when_inventory_reviews_it(self) -> None:
        root = package("hyphae-native-runtime")
        forbidden = package("redb", source=REGISTRY)

        with self.assertRaisesRegex(GateFailure, "forbidden.*redb"):
            audit_metadata(
                metadata(
                    [root, forbidden],
                    [dependency(str(forbidden["id"]), None)],
                ),
                policy(
                    ["hyphae-native-runtime"],
                    [forbidden],
                    forbidden_packages=["redb"],
                ),
            )

    def test_unreviewed_and_stale_external_packages_fail_closed(self) -> None:
        root = package("hyphae-native-runtime")
        reachable = package("blake3", source=REGISTRY)
        stale = package("crc32c", source=REGISTRY)
        graph = metadata(
            [root, reachable],
            [dependency(str(reachable["id"]), None)],
        )

        with self.assertRaisesRegex(GateFailure, "unreviewed.*blake3"):
            audit_metadata(graph, policy(["hyphae-native-runtime"], []))
        with self.assertRaisesRegex(GateFailure, "stale.*crc32c"):
            audit_metadata(
                graph,
                policy(["hyphae-native-runtime"], [reachable, stale]),
            )

    def test_external_version_license_and_source_are_exact(self) -> None:
        root = package("hyphae-native-runtime")
        reachable = package("blake3", source=REGISTRY)
        graph = metadata(
            [root, reachable],
            [dependency(str(reachable["id"]), None)],
        )

        for field, replacement in (
            ("version", "9.9.9"),
            ("license", "MIT"),
            ("source", "git+https://example.invalid/dependency"),
        ):
            with self.subTest(field=field):
                configured = reviewed_external(reachable)
                configured[field] = replacement
                candidate = policy(["hyphae-native-runtime"], [])
                candidate["external_packages"] = [configured]
                with self.assertRaisesRegex(GateFailure, field):
                    audit_metadata(graph, candidate)

    def test_workspace_lints_must_forbid_unsafe_and_be_inherited(self) -> None:
        workspace = {"workspace": {"lints": {"rust": {"unsafe_code": "forbid"}}}}
        manifests = {"hyphae-native-runtime": {"lints": {"workspace": True}}}
        validate_lint_policy(workspace, manifests, ["hyphae-native-runtime"])

        with self.assertRaisesRegex(GateFailure, "unsafe_code"):
            validate_lint_policy(
                {"workspace": {"lints": {"rust": {"unsafe_code": "warn"}}}},
                manifests,
                ["hyphae-native-runtime"],
            )
        with self.assertRaisesRegex(GateFailure, "inherit"):
            validate_lint_policy(
                workspace,
                {"hyphae-native-runtime": {"lints": {"workspace": False}}},
                ["hyphae-native-runtime"],
            )

    def test_native_unsafe_or_missing_metrics_fails(self) -> None:
        closure = [
            {"name": "hyphae-native-runtime", "version": "1.0.0", "workspace": True}
        ]
        with self.assertRaisesRegex(GateFailure, "direct unsafe"):
            audit_unsafe(
                {
                    "packages": [
                        geiger_package(
                            "hyphae-native-runtime",
                            unsafe_expressions=1,
                        )
                    ],
                    "packages_without_metrics": [],
                    "used_but_not_scanned_files": [],
                },
                "",
                closure,
            )
        with self.assertRaisesRegex(GateFailure, "missing cargo-geiger metrics"):
            audit_unsafe(
                {
                    "packages": [],
                    "packages_without_metrics": [],
                    "used_but_not_scanned_files": [],
                },
                "",
                closure,
            )

    def test_external_unsafe_is_reported_but_not_rejected(self) -> None:
        closure = [
            {"name": "hyphae-native-runtime", "version": "1.0.0", "workspace": True},
            {"name": "blake3", "version": "1.0.0", "workspace": False},
        ]
        result = audit_unsafe(
            {
                "packages": [
                    geiger_package("hyphae-native-runtime"),
                    geiger_package("blake3", unsafe_expressions=7),
                ],
                "packages_without_metrics": [],
                "used_but_not_scanned_files": [],
            },
            (
                "Failed to parse file: /registry/unrelated-2.0.0/src/lib.rs, "
                "Syn(Error)"
            ),
            closure,
        )

        external = next(
            entry for entry in result["packages"] if entry["name"] == "blake3"
        )
        self.assertEqual(external["unsafe_count"], 7)
        self.assertEqual(result["external_used_unsafe_count_on_host"], 7)
        self.assertEqual(result["out_of_closure_parse_failures"], ["unrelated@2.0.0"])

    def test_parse_failure_inside_workspace_closure_fails(self) -> None:
        closure = [{"name": "blake3", "version": "1.0.0", "workspace": True}]
        report = {
            "packages": [geiger_package("blake3")],
            "packages_without_metrics": [],
            "used_but_not_scanned_files": [],
        }

        with self.assertRaisesRegex(GateFailure, "could not parse.*blake3"):
            audit_unsafe(
                report,
                "Failed to parse file: /registry/blake3-1.0.0/src/lib.rs, Syn(Error)",
                closure,
            )

    def test_external_parse_failure_is_reported_without_weakening_workspace_gate(self) -> None:
        closure = [{"name": "unicode-casefold", "version": "0.2.0", "workspace": False}]
        report = {
            "packages": [],
            "packages_without_metrics": [],
            "used_but_not_scanned_files": [],
        }
        result = audit_unsafe(
            report,
            "Failed to parse file: /registry/unicode-casefold-0.2.0/src/lib.rs, Syn(Error)",
            closure,
        )
        self.assertEqual(result["packages"][0]["status"], "not-scanned-on-host")

    def test_receipt_paths_remove_repo_cargo_and_home_identity(self) -> None:
        sanitized = sanitize_receipt_paths(
            {
                "repo": "/home/mario/work/hyphae/crates/runtime/Cargo.toml",
                "cargo": "/home/mario/.cargo/registry/src/demo",
                "home": "/home/mario/.local/bin/git",
            },
            repo_paths=["/home/mario/work/hyphae"],
            cargo_home="/home/mario/.cargo",
            home="/home/mario",
        )

        self.assertEqual(
            sanitized,
            {
                "repo": "<repo>/crates/runtime/Cargo.toml",
                "cargo": "<cargo-home>/registry/src/demo",
                "home": "<home>/.local/bin/git",
            },
        )


if __name__ == "__main__":
    unittest.main()
