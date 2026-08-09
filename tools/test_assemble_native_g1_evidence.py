#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

import json
import tempfile
import unittest
from pathlib import Path

from tools.assemble_native_g1_evidence import ASSEMBLY, assemble
from tools.check_native_g1_readiness import evaluate

ROOT = Path(__file__).resolve().parents[1]


class NativeG1EvidenceAssemblyTests(unittest.TestCase):
    def profile(self) -> dict:
        return json.loads((ROOT / "config/native-g1-readiness-profile.json").read_text())

    def baseline(self) -> dict:
        return json.loads((ROOT / "config/native-g1-readiness-evidence.json").read_text())

    def write_artifacts(self, root: Path, commit: str = "a" * 40) -> None:
        for _, _, artifact in ASSEMBLY:
            (root / artifact).write_text(
                json.dumps({"status": "passed", "source_commit": commit}) + "\n"
            )

    def test_exact_seven_artifacts_close_g1(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.write_artifacts(root)
            evidence = assemble(root, self.profile(), self.baseline(), "a" * 40)
            result = evaluate(root, self.profile(), evidence)
            self.assertEqual(result["status"], "passed")
            self.assertEqual(result["passed"], 7)

    def test_missing_or_wrong_commit_artifact_fails_closed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.write_artifacts(root)
            (root / ASSEMBLY[0][2]).unlink()
            with self.assertRaisesRegex(ValueError, "missing"):
                assemble(root, self.profile(), self.baseline(), "a" * 40)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.write_artifacts(root)
            (root / ASSEMBLY[0][2]).write_text(
                json.dumps({"status": "passed", "source_commit": "b" * 40})
            )
            with self.assertRaisesRegex(ValueError, "commit"):
                assemble(root, self.profile(), self.baseline(), "a" * 40)

    def test_assembly_maps_exact_profile_once(self) -> None:
        ids = [row[0] for row in ASSEMBLY]
        self.assertEqual(ids, [row["id"] for row in self.profile()["requirements"]])
        self.assertEqual(len(ids), len(set(ids)))


if __name__ == "__main__":
    unittest.main()
