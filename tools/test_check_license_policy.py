#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

from tools.check_license_policy import (
    ROOT,
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


if __name__ == "__main__":
    unittest.main()
