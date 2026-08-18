#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

from __future__ import annotations

import unittest

from tools.generate_sdk_models import add_model, project_root_model


class GenerateSdkModelsTests(unittest.TestCase):
    def test_root_document_metadata_does_not_conflict_with_embedded_model(self) -> None:
        definition = {
            "type": "object",
            "properties": {
                "query": {
                    "$comment": "This model annotation remains structural.",
                    "type": "string",
                }
            },
            "required": ["query"],
            "additionalProperties": False,
        }
        document = {
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$comment": "SPDX-License-Identifier: Apache-2.0",
            "title": "ExactRetrievalRequestV1",
            "$defs": {"ExactRetrievalRequestV1": definition},
            **definition,
        }

        projected = project_root_model(document)

        self.assertNotIn("$comment", projected)
        self.assertEqual(
            projected["properties"]["query"]["$comment"],
            "This model annotation remains structural.",
        )
        self.assertEqual(
            document["$comment"],
            "SPDX-License-Identifier: Apache-2.0",
        )
        models: dict[str, dict[str, object]] = {}
        add_model(models, "ExactRetrievalRequestV1", projected, "exact.schema.json")
        add_model(
            models,
            "ExactRetrievalRequestV1",
            definition,
            "hybrid.schema.json",
        )

    def test_semantic_and_nested_comment_mismatches_remain_conflicts(self) -> None:
        original = {
            "type": "object",
            "properties": {
                "query": {"$comment": "canonical annotation", "type": "string"}
            },
        }
        mismatches = (
            ("semantic", {"type": "integer"}),
            (
                "nested comment",
                {
                    "type": "object",
                    "properties": {
                        "query": {
                            "$comment": "different annotation",
                            "type": "string",
                        }
                    },
                },
            ),
        )

        for label, mismatch in mismatches:
            with self.subTest(label=label):
                models: dict[str, dict[str, object]] = {}
                add_model(models, "Probe", original, "standalone.schema.json")
                with self.assertRaisesRegex(
                    ValueError,
                    "conflicting model Probe in embedded.schema.json",
                ):
                    add_model(models, "Probe", mismatch, "embedded.schema.json")


if __name__ == "__main__":
    unittest.main()
