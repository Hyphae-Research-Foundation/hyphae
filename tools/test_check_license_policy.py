#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

from tools.check_license_policy import (
    ROOT,
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
                "# SPDX-License-Identifier: AGPL-3.0-only\n",
                encoding="utf-8",
            )
            stale = root / "stale.py"
            stale.write_text(
                "# SPDX-License-Identifier: Apache-2.0\n",
                encoding="utf-8",
            )
            missing = root / "missing.rs"
            missing.write_text("fn main() {}\n", encoding="utf-8")
            self.assertIsNone(validate_spdx_file(valid))
            self.assertIsNotNone(validate_spdx_file(stale))
            self.assertIsNotNone(validate_spdx_file(missing))

    def test_shell_source_is_discovered_and_requires_the_agpl_marker(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            tools = root / "tools"
            tools.mkdir()
            valid = tools / "valid.sh"
            valid.write_text(
                "#!/usr/bin/env sh\n"
                "# SPDX-License-Identifier: AGPL-3.0-only\n"
                "set -eu\n",
                encoding="utf-8",
            )
            missing = tools / "missing.sh"
            missing.write_text("#!/usr/bin/env sh\nset -eu\n", encoding="utf-8")

            self.assertEqual(source_files(root), [missing, valid])
            self.assertIsNone(validate_spdx_file(valid))
            self.assertIsNotNone(validate_spdx_file(missing))

    def test_schema_validation_requires_the_agpl_marker(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            valid = root / "valid.schema.json"
            valid.write_text(
                '{"$comment":"SPDX-License-Identifier: AGPL-3.0-only"}\n',
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
                'license = "AGPL-3.0-only"\n',
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
                '{"name":"local","license":"AGPL-3.0-only"}\n',
                encoding="utf-8",
            )
            (npm / "package-lock.json").write_text(
                "{\n"
                '  "packages": {\n'
                '    "": {"license": "AGPL-3.0-only"},\n'
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


if __name__ == "__main__":
    unittest.main()
