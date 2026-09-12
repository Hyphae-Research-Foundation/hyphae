# SPDX-License-Identifier: Apache-2.0
"""Memory wire vectors are generated and validated by the native Rust codec."""
from pathlib import Path
import struct
import unittest
from hyphae_sdk.v2 import ClientError, RequestOptions
from hyphae_sdk.v2.protocol import encode_product_request, decode_product_response

FIXTURES = Path(__file__).parents[3] / "compatibility"
SEARCH = {"lexical": {"query": "decisions", "candidate_limit": 4, "weight": 1}, "limit": 4}


class MemoryWireTests(unittest.TestCase):
    def test_requests_match_rust_and_refuse_older_peers(self):
        for operation, arguments in [
            ("memory_recall", {"collections": [21, 22], "search": SEARCH, "limit": 2, "provenance": b"query-manifest"}),
            ("memory_enrich", {"collection": 21, "expected_envelope_digest": bytes([9]) * 32,
                "idempotency_id": 701, "document": {"object_id": 201, "text": "remember",
                    "doc_values": {}, "vectors": {"memory": [1.0, 0.0]}}}),
        ]:
            options = RequestOptions(logical_time_micros=5)
            expected = (FIXTURES / f"native-{operation.replace('_', '-')}-v1.bin").read_bytes()
            self.assertEqual(encode_product_request(operation, arguments, options, negotiated_minor=7), expected)
            with self.assertRaises(ClientError):
                encode_product_request(operation, arguments, options, negotiated_minor=6)

    def test_cross_domain_response_and_every_truncation(self):
        encoded = (FIXTURES / "native-memory-result-v1.bin").read_bytes()
        response = decode_product_response(encoded, 17, negotiated_minor=7)
        self.assertEqual(response.kind, "memory_recall")
        self.assertEqual(response.value["expired_filtered"], 1)
        self.assertEqual([m["collection"] for m in response.value["memories"]], [21, 22])
        self.assertTrue(all(m["envelope"] == b'{"text":"remember"}' for m in response.value["memories"]))
        with self.assertRaises(ClientError):
            decode_product_response(encoded, 17, negotiated_minor=6)
        for end in range(len(encoded)):
            with self.assertRaises(ClientError):
                decode_product_response(encoded[:end], 17, negotiated_minor=7)
        forged = bytearray(encoded)
        # Common snapshot starts at 16; first nested search snapshot at 120.
        forged[120] ^= 1
        with self.assertRaises(ClientError):
            decode_product_response(bytes(forged), 17, negotiated_minor=7)
        forged = bytearray(encoded)
        struct.pack_into("<Q", forged, len(forged) - 8, 999)
        with self.assertRaises(ClientError):
            decode_product_response(bytes(forged), 17, negotiated_minor=7)

    def test_request_bounds_and_opaque_provenance(self):
        valid = {"collections": [21, 22], "search": SEARCH, "limit": 2}
        for change in ({"collections": [22, 21]}, {"collections": [21, 21]},
                       {"collections": []}, {"limit": 65}, {"provenance": b"x" * 65537}):
            with self.assertRaises(ClientError):
                encode_product_request("memory_recall", {**valid, **change}, RequestOptions())


if __name__ == "__main__":
    unittest.main()
