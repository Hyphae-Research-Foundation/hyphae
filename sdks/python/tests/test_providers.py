# SPDX-License-Identifier: Apache-2.0
"""Provider-layer attestation records and transports."""

import json
import unittest
from unittest import mock

try:
    from blake3 import blake3
except ImportError:  # pragma: no cover
    blake3 = None

from hyphae_sdk.providers import (
    DeclaredProviderRecord,
    OllamaProvider,
    ProviderError,
)


@unittest.skipIf(blake3 is None, "providers extra is not installed")
class DeclaredProviderRecordTests(unittest.TestCase):
    def test_envelope_matches_the_cross_language_golden(self) -> None:
        record = DeclaredProviderRecord(
            provider="openai",
            model="text-embedding-3-small",
            request_digest=blake3(b"request").digest(),
            response_digest=blake3(b"response").digest(),
        )
        expected = (
            b"HYATTS01\x02"
            + (6).to_bytes(2, "little")
            + b"openai"
            + (22).to_bytes(2, "little")
            + b"text-embedding-3-small"
            + blake3(b"request").digest()
            + blake3(b"response").digest()
        )
        # The engine's pure verifier accepts exactly these bytes; the same
        # golden is asserted in the core proof suite.
        self.assertEqual(record.envelope(), expected)
        self.assertEqual(record.envelope_hex(), expected.hex())

    def test_unbounded_names_fail_closed(self) -> None:
        record = DeclaredProviderRecord(
            provider="",
            model="m",
            request_digest=bytes(32),
            response_digest=bytes(32),
        )
        with self.assertRaises(ProviderError):
            record.envelope()

    def test_ollama_embed_returns_vectors_and_record(self) -> None:
        response = json.dumps({"embeddings": [[0.5, 0.25]]}).encode("utf-8")

        class FakeResponse:
            def read(self) -> bytes:
                return response

            def __enter__(self):
                return self

            def __exit__(self, *_args) -> None:
                return None

        with mock.patch("urllib.request.urlopen", return_value=FakeResponse()):
            vectors, record = OllamaProvider().embed("all-minilm", ["hello"])
        self.assertEqual(vectors, [[0.5, 0.25]])
        self.assertEqual(record.provider, "ollama")
        self.assertEqual(record.response_digest, blake3(response).digest())
        self.assertEqual(len(record.envelope()), 8 + 1 + 2 + 6 + 2 + 10 + 64)

    def test_shape_mismatch_fails_closed(self) -> None:
        response = json.dumps({"embeddings": []}).encode("utf-8")

        class FakeResponse:
            def read(self) -> bytes:
                return response

            def __enter__(self):
                return self

            def __exit__(self, *_args) -> None:
                return None

        with mock.patch("urllib.request.urlopen", return_value=FakeResponse()):
            with self.assertRaises(ProviderError):
                OllamaProvider().embed("all-minilm", ["hello"])


if __name__ == "__main__":
    unittest.main()
