#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

from tools.check_license_policy import (
    ROOT,
    machine_files,
    normative_markdown_files,
    source_files,
    validate_package_manifests,
    validate_repository,
    validate_schema_file,
    validate_spdx_file,
)


class LicensePolicyTests(unittest.TestCase):
    def test_current_repository_is_consistent(self) -> None:
        self.assertEqual(validate_repository(ROOT), [])

    def test_spdx_validation_accepts_shebang_and_rejects_stale_or_missing(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            valid = root / "valid.py"
            valid.write_text(
                "#!/usr/bin/env python3\n"
                "# SPDX-License-Identifier: Apache-2.0\n",
                encoding="utf-8",
            )
            stale = root / "stale.py"
            stale.write_text(
                "# SPDX-License-Identifier: AGPL-3.0-only\n",
                encoding="utf-8",
            )
            missing = root / "missing.rs"
            missing.write_text("fn main() {}\n", encoding="utf-8")
            self.assertIsNone(validate_spdx_file(valid))
            self.assertIsNotNone(validate_spdx_file(stale))
            self.assertIsNotNone(validate_spdx_file(missing))

    def test_shell_source_is_discovered_and_requires_the_apache_marker(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            tools = root / "tools"
            tools.mkdir()
            valid = tools / "valid.sh"
            valid.write_text(
                "#!/usr/bin/env sh\n"
                "# SPDX-License-Identifier: Apache-2.0\n"
                "set -eu\n",
                encoding="utf-8",
            )
            missing = tools / "missing.sh"
            missing.write_text("#!/usr/bin/env sh\nset -eu\n", encoding="utf-8")

            self.assertEqual(source_files(root), [missing, valid])
            self.assertIsNone(validate_spdx_file(valid))
            self.assertIsNotNone(validate_spdx_file(missing))

    def test_yaml_astro_slt_and_extensionless_machine_files_are_discovered(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            workflow = root / "workflow.yml"
            astro = root / "page.astro"
            editorconfig = root / ".editorconfig"
            slt = root / "query.slt"
            workflow.write_text("# SPDX-License-Identifier: Apache-2.0\nname: test\n")
            astro.write_text("---\n// SPDX-License-Identifier: Apache-2.0\n---\n")
            editorconfig.write_text("# SPDX-License-Identifier: Apache-2.0\nroot = true\n")
            slt.write_text("# SPDX-License-Identifier: Apache-2.0\nquery I\n")
            self.assertEqual(machine_files(root), [editorconfig, astro, slt, workflow])
            self.assertIsNone(validate_spdx_file(workflow))
            self.assertIsNone(validate_spdx_file(astro))
            self.assertIsNone(validate_spdx_file(editorconfig))
            self.assertIsNone(validate_spdx_file(slt))

    def test_exact_repository_extensionless_and_slt_inventory_has_markers(self) -> None:
        expected = {
            ".editorconfig",
            ".gitattributes",
            ".github/CODEOWNERS",
            ".gitignore",
            "fuzz/.gitignore",
            "crates/hyphae-native-runtime/tests/corpus/g2-smoke.slt",
        }
        observed = {
            path.relative_to(ROOT).as_posix()
            for path in machine_files(ROOT)
            if path.suffix == ".slt" or path.name.startswith(".") or path.name == "CODEOWNERS"
        }
        self.assertEqual(observed, expected)
        for relative in expected:
            self.assertIsNone(validate_spdx_file(ROOT / relative))

    def test_normative_markdown_coverage_is_policy_derived(self) -> None:
        paths = {path.relative_to(ROOT).as_posix() for path in normative_markdown_files(ROOT)}
        self.assertIn("docs/native/access-control-v1.md", paths)
        self.assertIn("crates/hyphae-native-product/assets/product-error-v1.md", paths)
        self.assertNotIn("README.md", paths)

    def test_strict_json_exceptions_are_frozen_by_exact_inventory(self) -> None:
        from tools.check_license_policy import (
            JSON_EXCEPTION_PATH_COUNT,
            JSON_EXCEPTION_PATHS_SHA256,
        )

        self.assertEqual(JSON_EXCEPTION_PATH_COUNT, 91)
        self.assertRegex(JSON_EXCEPTION_PATHS_SHA256, r"^[0-9a-f]{64}$")

    def test_schema_validation_requires_the_apache_marker(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            valid = root / "valid.schema.json"
            valid.write_text(
                '{"$comment":"SPDX-License-Identifier: Apache-2.0"}\n',
                encoding="utf-8",
            )
            stale = root / "stale.schema.json"
            stale.write_text(
                '{"$comment":"SPDX-License-Identifier: GPL-3.0-only"}\n',
                encoding="utf-8",
            )
            malformed = root / "malformed.schema.json"
            malformed.write_text("{", encoding="utf-8")
            self.assertIsNone(validate_schema_file(valid))
            self.assertIsNotNone(validate_schema_file(stale))
            self.assertIsNotNone(validate_schema_file(malformed))

    def test_package_manifests_resolve_workspace_licenses_fail_closed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "Cargo.toml").write_text(
                "[workspace]\n"
                'members = ["member"]\n'
                "[workspace.package]\n"
                'license = "Apache-2.0"\n',
                encoding="utf-8",
            )
            member = root / "member"
            member.mkdir()
            (member / "Cargo.toml").write_text(
                "[package]\n"
                'name = "member"\n'
                'version = "1.0.0"\n'
                "license.workspace = true\n",
                encoding="utf-8",
            )
            self.assertEqual(validate_package_manifests(root), [])

            nested = root / "nested"
            nested.mkdir()
            (nested / "Cargo.toml").write_text(
                "[workspace]\n"
                "[package]\n"
                'name = "nested"\n'
                'version = "1.0.0"\n'
                "license.workspace = true\n",
                encoding="utf-8",
            )
            failures = validate_package_manifests(root)
            self.assertTrue(
                any(
                    "nested/Cargo.toml: package license does not resolve"
                    in failure
                    for failure in failures
                )
            )

    def test_package_manifest_discovery_covers_npm_links_and_python(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            npm = root / "npm"
            npm.mkdir()
            (npm / "package.json").write_text(
                '{"name":"local","license":"Apache-2.0"}\n',
                encoding="utf-8",
            )
            (npm / "package-lock.json").write_text(
                "{\n"
                '  "packages": {\n'
                '    "": {"license": "Apache-2.0"},\n'
                '    "../linked": {"license": "GPL-3.0-only"},\n'
                '    "node_modules/linked": {"resolved": "../linked", "link": true}\n'
                "  }\n"
                "}\n",
                encoding="utf-8",
            )
            python = root / "python"
            python.mkdir()
            (python / "pyproject.toml").write_text(
                "[project]\n"
                'name = "local-python"\n'
                'version = "1.0.0"\n'
                'license = "GPL-3.0-only"\n'
                'license-files = ["LICENSE"]\n',
                encoding="utf-8",
            )

            failures = validate_package_manifests(root)
            self.assertTrue(
                any(
                    "package-lock.json:packages.../linked.license" in failure
                    for failure in failures
                )
            )
            self.assertTrue(
                any(
                    "python/pyproject.toml: project license differs" in failure
                    for failure in failures
                )
            )
            self.assertTrue(
                any(
                    "python/pyproject.toml: license-files are incomplete" in failure
                    for failure in failures
                )
            )

    def test_gitignored_generated_website_is_outside_manifest_discovery(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            website = root / "website"
            website.mkdir()
            (website / "package.json").write_text(
                '{"name":"generated-site","license":"UNLICENSED"}\n',
                encoding="utf-8",
            )
            self.assertEqual(validate_package_manifests(root), [])

    def test_publishable_npm_packages_rebuild_before_pack_and_include_notices(self) -> None:
        for relative in (
            "sdks/typescript/package.json",
            "integrations/javascript/package.json",
        ):
            package = __import__("json").loads((ROOT / relative).read_text(encoding="utf-8"))
            self.assertEqual(package["scripts"]["prepack"], "rm -rf dist && npm run build")
            self.assertIn("THIRD_PARTY_NOTICES.md", package["files"])


if __name__ == "__main__":
    unittest.main()
