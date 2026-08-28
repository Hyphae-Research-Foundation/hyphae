#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

import json
import hashlib
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from tools.check_native_g8_receipts import (
    GateFailure,
    RELEASE_REQUIREMENTS,
    aggregate,
    authority,
    validate_aggregate,
    validate_receipt,
)


ROOT = Path(__file__).resolve().parents[1]
COMMIT = "a" * 40
RELEASE_COMMIT = "b" * 40


ARTIFACT_BYTES = b"{}\n"
ARTIFACT_DIGEST = hashlib.sha256(ARTIFACT_BYTES).hexdigest()


def receipt(row: dict, platform: str, commit: str = COMMIT) -> dict:
    return {
        "schema": "hyphae-native-g8-receipt-v1",
        "gate": "G8",
        "status": "passed",
        "evidence_class": "closure-candidate",
        "source_commit": commit,
        "requirement": row["id"],
        "platform": platform,
        "acceptance": {
            item: {
                "status": "passed",
                "artifact_sha256": ARTIFACT_DIGEST,
                "observation": f"verified-{item}",
            }
            for item in row["acceptance"]
        },
        "artifacts": [{"name": "run.log", "sha256": ARTIFACT_DIGEST}],
        "claims": [],
        "closure_declared": False,
}


def write_receipt(root: Path, row: dict, platform: str) -> Path:
    directory = root / f"{row['id']}-{platform}"
    directory.mkdir()
    (directory / "run.log").write_bytes(ARTIFACT_BYTES)
    path = directory / f"{row['id']}-{platform}-receipt.json"
    path.write_text(json.dumps(receipt(row, platform)), encoding="utf-8")
    return path


def write_split_receipt(root: Path, row: dict, platform: str) -> Path:
    origin = "release" if row["id"] in RELEASE_REQUIREMENTS else "readiness"
    directory = root / origin / f"{row['id']}-{platform}"
    directory.mkdir(parents=True)
    (directory / "run.log").write_bytes(ARTIFACT_BYTES)
    commit = RELEASE_COMMIT if origin == "release" else COMMIT
    path = directory / f"{row['id']}-{platform}-receipt.json"
    path.write_text(json.dumps(receipt(row, platform, commit)), encoding="utf-8")
    return path


class G8ReceiptTests(unittest.TestCase):
    @staticmethod
    def semantic_validator(rows: dict):
        return patch(
            "tools.produce_native_g8_receipt.observations",
            side_effect=lambda requirement, _payload, _commit, _platform: {
                name: f"verified-{name}"
                for name in rows[requirement]["acceptance"]
            },
        )

    def test_complete_exact_sha_matrix_closes(self) -> None:
        _, rows = authority(ROOT)
        with tempfile.TemporaryDirectory() as directory:
            receipts = Path(directory)
            for requirement, row in rows.items():
                for platform in row["platforms"]:
                    write_receipt(receipts, row, platform)
            with self.semantic_validator(rows):
                result = aggregate(ROOT, receipts, COMMIT)
        self.assertEqual(result["claims"], ["G8"])
        self.assertTrue(result["closure_declared"])

    def test_missing_platform_fails_closed(self) -> None:
        _, rows = authority(ROOT)
        with tempfile.TemporaryDirectory() as directory:
            receipts = Path(directory)
            for requirement, row in rows.items():
                for platform in row["platforms"]:
                    if requirement == "multiplatform-packaging" and platform == "x86_64-pc-windows-msvc":
                        continue
                    write_receipt(receipts, row, platform)
            with self.semantic_validator(rows), self.assertRaises(GateFailure):
                aggregate(ROOT, receipts, COMMIT)

    def test_split_release_source_closes_with_distinct_audits(self) -> None:
        _, rows = authority(ROOT)
        with tempfile.TemporaryDirectory() as directory:
            receipts = Path(directory)
            for row in rows.values():
                for platform in row["platforms"]:
                    write_split_receipt(receipts, row, platform)
            with self.semantic_validator(rows):
                result = aggregate(ROOT, receipts, COMMIT, RELEASE_COMMIT)
        self.assertEqual(result["schema"], "hyphae-native-g8-aggregate-v2")
        self.assertEqual(result["release_source_commit"], RELEASE_COMMIT)
        self.assertEqual(
            result["requirements"]["multiplatform-packaging"]
            ["x86_64-unknown-linux-gnu"]["audit"]["source_commit"],
            RELEASE_COMMIT,
        )
        self.assertEqual(
            result["requirements"]["native-soak"]["linux"]
            ["audit"]["source_commit"],
            COMMIT,
        )

    def test_split_release_source_rejects_wrong_origin_or_commit(self) -> None:
        _, rows = authority(ROOT)
        with tempfile.TemporaryDirectory() as directory:
            receipts = Path(directory)
            paths = []
            for row in rows.values():
                for platform in row["platforms"]:
                    paths.append(write_split_receipt(receipts, row, platform))
            release_path = next(
                path for path in paths if "multiplatform-packaging" in path.name
            )
            payload = json.loads(release_path.read_text(encoding="utf-8"))
            payload["source_commit"] = COMMIT
            release_path.write_text(json.dumps(payload), encoding="utf-8")
            with self.semantic_validator(rows), self.assertRaises(GateFailure):
                aggregate(ROOT, receipts, COMMIT, RELEASE_COMMIT)

        with tempfile.TemporaryDirectory() as directory:
            receipts = Path(directory)
            for row in rows.values():
                for platform in row["platforms"]:
                    write_split_receipt(receipts, row, platform)
            source = next((receipts / "release").iterdir())
            destination = receipts / "readiness" / source.name
            source.rename(destination)
            with self.semantic_validator(rows), self.assertRaises(GateFailure):
                aggregate(ROOT, receipts, COMMIT, RELEASE_COMMIT)

    def test_false_acceptance_fails_closed(self) -> None:
        _, rows = authority(ROOT)
        row = rows["resource-exhaustion"]
        payload = receipt(row, "linux")
        payload["acceptance"]["disk-full"]["status"] = "failed"
        with self.assertRaises(GateFailure):
            validate_receipt(payload, COMMIT, row)

    def test_artifact_name_cannot_escape_receipt_directory(self) -> None:
        _, rows = authority(ROOT)
        row = rows["resource-exhaustion"]
        payload = receipt(row, "linux")
        payload["artifacts"][0]["name"] = "../run.log"
        with self.assertRaises(GateFailure):
            validate_receipt(payload, COMMIT, row)

    def test_missing_or_tampered_artifact_fails_closed(self) -> None:
        _, rows = authority(ROOT)
        with tempfile.TemporaryDirectory() as directory:
            receipts = Path(directory)
            paths = []
            for row in rows.values():
                for platform in row["platforms"]:
                    paths.append(write_receipt(receipts, row, platform))
            (paths[0].parent / "run.log").write_bytes(b"tampered")
            with self.semantic_validator(rows), self.assertRaises(GateFailure):
                aggregate(ROOT, receipts, COMMIT)

    def test_raw_artifact_is_semantically_revalidated(self) -> None:
        _, rows = authority(ROOT)
        with tempfile.TemporaryDirectory() as directory:
            receipts = Path(directory)
            for row in rows.values():
                for platform in row["platforms"]:
                    write_receipt(receipts, row, platform)
            with self.assertRaises(GateFailure):
                aggregate(ROOT, receipts, COMMIT)

    def test_closed_aggregate_mutations_fail_closed(self) -> None:
        _, rows = authority(ROOT)
        with tempfile.TemporaryDirectory() as directory:
            receipts = Path(directory)
            for row in rows.values():
                for platform in row["platforms"]:
                    write_receipt(receipts, row, platform)
            with self.semantic_validator(rows):
                valid = aggregate(ROOT, receipts, COMMIT)
        mutations = (
            lambda value: value.update(source_commit="b" * 40),
            lambda value: value.update(claims=[]),
            lambda value: value.update(closure_declared=False),
            lambda value: value["requirements"].pop("native-soak"),
            lambda value: value["requirements"]["native-soak"]["linux"]["audit"].update(status="failed"),
            lambda value: value["requirements"]["native-soak"]["linux"].update(receipt_sha256="bad"),
        )
        for mutation in mutations:
            with self.subTest(mutation=mutation):
                candidate = json.loads(json.dumps(valid))
                mutation(candidate)
                with self.assertRaises(GateFailure):
                    validate_aggregate(candidate, COMMIT, ROOT)


if __name__ == "__main__":
    unittest.main()
