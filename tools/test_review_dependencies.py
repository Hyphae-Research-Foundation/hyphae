from __future__ import annotations

import json
import unittest
from pathlib import Path
from unittest.mock import call, patch

from tools.review_dependencies import (
    CARGO_MANIFESTS,
    REGISTERED_DEPENDENCY_FILES,
    cargo_dependencies,
    changed_dependency_files,
    dependency_diff,
    is_dependency_manifest_or_lock,
    merge_base,
    npm_dependencies,
    python_dependencies,
    resolve_commit,
    review,
    validate_manifest_lock_pairs,
    validate_registered_dependency_files,
)

BASE = "1" * 40
HEAD = "2" * 40
MERGE_BASE = "3" * 40


class DependencyReviewTests(unittest.TestCase):
    def test_cargo_registry_dependency_requires_and_keeps_checksum(self) -> None:
        parsed = cargo_dependencies(
            'version = 4\n[[package]]\nname = "demo"\nversion = "1.2.3"\n'
            'source = "registry+https://example.invalid/index"\nchecksum = "abc"\n'
        )
        self.assertEqual(next(iter(parsed.values()))["checksum"], "abc")

    def test_npm_dependency_keeps_integrity_and_scope(self) -> None:
        lock = {
            "packages": {
                "": {"name": "root"},
                "node_modules/@scope/demo": {
                    "version": "2.0.0",
                    "resolved": "https://example.invalid/demo.tgz",
                    "integrity": "sha512-example",
                },
            }
        }
        parsed = npm_dependencies(json.dumps(lock))
        self.assertIn("@scope/demo@2.0.0|node_modules/@scope/demo", parsed)

    def test_python_dependencies_include_runtime_optional_and_build(self) -> None:
        parsed = python_dependencies(
            '[project]\ndependencies = ["one>=1"]\n'
            '[project.optional-dependencies]\ntest = ["two==2"]\n'
            '[build-system]\nrequires = ["three"]\n'
        )
        self.assertEqual(len(parsed), 3)

    def test_diff_reports_added_removed_and_metadata_changes(self) -> None:
        result = dependency_diff(
            {"same": {"checksum": "old"}, "removed": {}},
            {"same": {"checksum": "new"}, "added": {}},
        )
        self.assertEqual(result["added"], ["added"])
        self.assertEqual(result["removed"], ["removed"])
        self.assertEqual(result["metadata_changed"], ["same"])

    @patch("tools.review_dependencies.validate_cargo_lock")
    def test_metadata_only_manifest_change_accepts_current_lock(self, check_lock) -> None:
        validate_manifest_lock_pairs({"Cargo.toml"}, HEAD)
        check_lock.assert_called_once_with(HEAD)

    @patch("tools.review_dependencies.validate_cargo_lock")
    def test_fuzz_manifest_checks_its_isolated_lock(self, check_lock) -> None:
        validate_manifest_lock_pairs({"fuzz/Cargo.toml"}, HEAD)
        check_lock.assert_called_once_with(HEAD, "fuzz/Cargo.toml")

    @patch("tools.review_dependencies.validate_npm_lock")
    def test_metadata_only_javascript_manifest_validates_current_lock(
        self,
        check_lock,
    ) -> None:
        validate_manifest_lock_pairs({"sdks/typescript/package.json"}, HEAD)
        check_lock.assert_called_once_with(HEAD, "sdks/typescript/package.json")

    def test_unregistered_dependency_manifest_and_lock_are_rejected(self) -> None:
        with self.assertRaisesRegex(ValueError, "unregistered dependency"):
            validate_manifest_lock_pairs(
                {
                    "new-tool/package.json",
                    "new-tool/package-lock.json",
                },
                HEAD,
            )

    def test_registered_dependency_files_are_recognized_and_accepted(self) -> None:
        self.assertTrue(
            all(
                is_dependency_manifest_or_lock(path)
                for path in REGISTERED_DEPENDENCY_FILES
            )
        )
        validate_registered_dependency_files(set(REGISTERED_DEPENDENCY_FILES))

    def test_native_crate_manifests_are_registered(self) -> None:
        native_manifests = {
            "crates/hyphae-native-ann/Cargo.toml",
            "crates/hyphae-native-blobs/Cargo.toml",
            "crates/hyphae-native-btree/Cargo.toml",
            "crates/hyphae-native-catalog/Cargo.toml",
            "crates/hyphae-native-manifest/Cargo.toml",
            "crates/hyphae-native-mvcc/Cargo.toml",
            "crates/hyphae-native-pages/Cargo.toml",
            "crates/hyphae-native-records/Cargo.toml",
            "crates/hyphae-native-runtime/Cargo.toml",
            "crates/hyphae-native-types/Cargo.toml",
            "crates/hyphae-native-wal/Cargo.toml",
        }
        self.assertLessEqual(native_manifests, set(CARGO_MANIFESTS))

    def test_unregistered_dependency_families_are_rejected(self) -> None:
        paths = (
            "new-tool/uv.lock",
            "new-tool/requirements-dev.txt",
            "new-tool/requirements_ci.in",
            "new-tool/requirements/base.txt",
            "new-tool/constraints/windows.txt",
            "new-tool/npm-shrinkwrap.json",
            "new-tool/pnpm-lock.yaml",
            "new-tool/bun.lockb",
            "new-tool/deno.jsonc",
            "new-tool/go.mod",
            "new-tool/Gemfile.lock",
            "new-tool/composer.lock",
            "new-tool/pom.xml",
            "new-tool/Package.resolved",
            r"new-tool\Pipfile.lock",
        )
        for path in paths:
            with self.subTest(path=path):
                with self.assertRaisesRegex(ValueError, "unregistered dependency"):
                    validate_registered_dependency_files({path})

    def test_ordinary_files_do_not_trigger_dependency_registration(self) -> None:
        paths = {
            "README.md",
            "src/lock.rs",
            "docs/package-json.md",
            "data/requirements.csv",
            "notes/build.gradle.md",
            "docs/requirements/guide.md",
            "fixtures/constraints/output.json",
        }
        self.assertFalse(any(is_dependency_manifest_or_lock(path) for path in paths))
        validate_registered_dependency_files(paths)

    @patch("tools.review_dependencies.git")
    def test_changed_files_use_the_explicit_commit_range(self, run_git) -> None:
        run_git.return_value.stdout = "Cargo.toml\nCargo.lock\n"
        self.assertEqual(
            changed_dependency_files(MERGE_BASE, HEAD),
            {"Cargo.toml", "Cargo.lock"},
        )
        run_git.assert_called_once_with(
            "diff",
            "--name-only",
            f"{MERGE_BASE}..{HEAD}",
            "--",
        )

    @patch("tools.review_dependencies.resolve_commit", return_value=MERGE_BASE)
    @patch("tools.review_dependencies.git")
    def test_merge_base_is_resolved_as_a_commit(self, run_git, resolve) -> None:
        run_git.return_value.returncode = 0
        run_git.return_value.stdout = MERGE_BASE + "\n"
        self.assertEqual(merge_base(BASE, HEAD), MERGE_BASE)
        run_git.assert_called_once_with(
            "merge-base",
            "--all",
            BASE,
            HEAD,
            check=False,
        )
        resolve.assert_called_once_with(MERGE_BASE, "merge-base")

    @patch("tools.review_dependencies.git")
    def test_ambiguous_merge_bases_are_rejected(self, run_git) -> None:
        run_git.return_value.returncode = 0
        run_git.return_value.stdout = f"{MERGE_BASE}\n{'4' * 40}\n"
        with self.assertRaisesRegex(ValueError, "one canonical merge-base"):
            merge_base(BASE, HEAD)

    def test_symbolic_head_is_rejected_before_git_is_invoked(self) -> None:
        with self.assertRaisesRegex(ValueError, "lowercase 40-character"):
            resolve_commit("HEAD", "head")

    @patch("tools.review_dependencies.read_revision")
    @patch("tools.review_dependencies.validate_manifest_lock_pairs")
    @patch("tools.review_dependencies.changed_dependency_files", return_value=set())
    @patch("tools.review_dependencies.merge_base", return_value=MERGE_BASE)
    @patch("tools.review_dependencies.resolve_commit")
    def test_divergent_dag_uses_merge_base_for_files_and_dependency_deltas(
        self,
        resolve,
        find_merge_base,
        changed,
        validate_pairs,
        read_object,
    ) -> None:
        resolve.side_effect = lambda revision, _label: revision

        def object_content(_revision: str, path: str) -> str:
            if path.endswith("Cargo.lock"):
                return "version = 4\n"
            if path.endswith("package-lock.json"):
                return '{"packages": {}}\n'
            return "[project]\n"

        read_object.side_effect = object_content
        with patch.object(
            Path,
            "read_text",
            side_effect=AssertionError("review read the mutable worktree"),
        ):
            report = review(BASE, HEAD)

        self.assertEqual(report["base"], BASE)
        self.assertEqual(report["merge_base"], MERGE_BASE)
        self.assertEqual(report["head"], HEAD)
        find_merge_base.assert_called_once_with(BASE, HEAD)
        changed.assert_called_once_with(MERGE_BASE, HEAD)
        validate_pairs.assert_called_once_with(set(), HEAD)
        expected_reads = []
        for path in (
            "Cargo.lock",
            "fuzz/Cargo.lock",
            "sdks/typescript/package-lock.json",
            "integrations/javascript/package-lock.json",
            "integrations/host-smoke/package-lock.json",
            "sdks/python/pyproject.toml",
        ):
            expected_reads.extend((call(MERGE_BASE, path), call(HEAD, path)))
        self.assertEqual(read_object.call_args_list, expected_reads)


if __name__ == "__main__":
    unittest.main()
