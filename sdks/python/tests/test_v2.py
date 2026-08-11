# SPDX-License-Identifier: AGPL-3.0-only

from __future__ import annotations

import unittest
from pathlib import Path
from typing import BinaryIO, cast
from unittest.mock import patch

from hyphae_sdk.v2 import HyphaeClient, RequestOptions, Response
from hyphae_sdk.v2.http import HttpTransport, PRODUCT_MEDIA_TYPE
from hyphae_sdk.v2.local import _windows_pipe_namespace, _write_all
from hyphae_sdk.v2.protocol import (
    FRAME_KINDS,
    decode_frame,
    decode_product_request,
    decode_product_response,
    encode_frame,
    encode_product_request,
    blake3,
)


FIXTURE = Path(__file__).parents[3] / "compatibility" / "native-protocol-v1-structure-get.bin"


class FakeTransport:
    def __init__(self) -> None:
        self.calls: list[tuple[str, dict[str, object], RequestOptions]] = []

    def execute(self, operation: str, arguments: dict[str, object], options: RequestOptions) -> Response:
        self.calls.append((operation, arguments, options))
        return Response("fake", arguments, options.checked_request_id())


class FakeHttpResponse:
    status = 200

    def __init__(self, body: bytes) -> None:
        self._body = body

    def getheader(self, name: str) -> str | None:
        return {
            "Content-Length": str(len(self._body)),
            "Content-Type": PRODUCT_MEDIA_TYPE,
            "X-Hyphae-Request-Id": "17",
        }.get(name)

    def read(self, size: int = -1) -> bytes:
        return self._body[:size]


class FakeHttpConnection:
    last_path = ""

    def __init__(self, *args, **kwargs) -> None:  # type: ignore[no-untyped-def]
        del args, kwargs
        self.path = ""

    def request(self, method: str, path: str, **kwargs) -> None:  # type: ignore[no-untyped-def]
        del method, kwargs
        self.path = path
        type(self).last_path = path

    def getresponse(self) -> FakeHttpResponse:
        body = bytearray(72)
        body[:8] = b"HYPRSP01"
        import struct

        struct.pack_into("<IHHHHHH", body, 8, len(body), 1, 0, 1, 1, 2, 6)
        return FakeHttpResponse(bytes(body))

    def close(self) -> None:
        pass


class ShortWriteStream:
    def __init__(self) -> None:
        self.encoded = bytearray()
        self.flushes = 0

    def write(self, encoded: bytes) -> int:
        length = min(7, len(encoded))
        self.encoded.extend(encoded[:length])
        return length

    def flush(self) -> None:
        self.flushes += 1


class V2Tests(unittest.TestCase):
    def test_completion_blake3_matches_published_vectors(self) -> None:
        self.assertEqual(
            blake3(b"").hex(),
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262",
        )
        self.assertEqual(
            blake3(b"abc").hex(),
            "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85",
        )
    def test_shared_binary_fixture_decodes_and_reencodes_exactly(self) -> None:
        encoded = FIXTURE.read_bytes()
        frame = decode_frame(encoded)
        operation, arguments, options = decode_product_request(frame.payload)
        self.assertEqual(operation, "structure_get")
        self.assertEqual(arguments, {"key": b"shared-key"})
        self.assertEqual(options.logical_time_micros, 1_700_000_000_000_000)
        self.assertEqual(
            encode_frame(
                frame.kind,
                frame.stream_id,
                frame.request_id,
                encode_product_request(operation, arguments, options),
            ),
            encoded,
        )

    def test_independent_encoder_matches_shared_fixture(self) -> None:
        options = RequestOptions(
            logical_time_micros=1_700_000_000_000_000,
            deadline_micros=1_700_000_000_500_000,
        )
        encoded = encode_frame(
            FRAME_KINDS["execute"],
            7,
            42,
            encode_product_request("structure_get", {"key": b"shared-key"}, options),
        )
        self.assertEqual(encoded, FIXTURE.read_bytes())

    def test_transaction_and_catalog_requests_round_trip(self) -> None:
        cases = (
            ("transaction_begin", {}),
            (
                "transaction_stage_vector",
                {
                    "handle": 7,
                    "mutation": {"kind": "delete", "index": 11, "object_id": 13},
                },
            ),
            ("transaction_commit", {"handle": 7}),
            ("transaction_status_by_idempotency", {"idempotency_token": 23}),
            ("catalog_create", {"definition": b"HYCOBJ02-canonical"}),
        )
        for operation, arguments in cases:
            with self.subTest(operation=operation):
                encoded = encode_product_request(operation, arguments, RequestOptions())
                decoded_operation, decoded_arguments, _ = decode_product_request(encoded)
                self.assertEqual(decoded_operation, operation)
                self.assertEqual(decoded_arguments, arguments)

    def test_all_structure_read_requests_round_trip(self) -> None:
        key = {"keyspace": 7, "key": b"key"}
        cases = (
            {"kind": "string_get", "key": key},
            {"kind": "counter_get", "key": key},
            {"kind": "ttl", "key": key, "family": "hash"},
            {"kind": "hash_get", "key": key, "field": b"field"},
            {"kind": "hash_field_ttl", "key": key, "field": b"field"},
            {"kind": "hash_scan", "key": key, "start_after": b"field", "limit": 10},
            {"kind": "hash_length", "key": key},
            {"kind": "list_range", "key": key, "start": -2, "stop": 4},
            {"kind": "list_length", "key": key},
            {"kind": "set_contains", "key": key, "member": b"member"},
            {"kind": "set_members", "key": key, "start_after": b"member", "limit": 10},
            {"kind": "set_cardinality", "key": key},
            {"kind": "set_algebra", "keyspace": 7, "operation": "intersection", "keys": [b"a", b"b"], "output_member_limit": 10, "visit_limit": 20},
            {"kind": "sorted_set_score", "key": key, "member": b"member"},
            {"kind": "sorted_set_rank", "key": key, "member": b"member", "order": "descending"},
            {"kind": "sorted_set_range", "key": key, "start": -2, "stop": 4, "order": "descending"},
            {"kind": "sorted_set_cardinality", "key": key},
            {"kind": "stream_range", "key": key, "start": 2, "end": 4, "limit": 10},
        )
        for arguments in cases:
            with self.subTest(kind=arguments["kind"]):
                encoded = encode_product_request("structure_read", arguments, RequestOptions())
                operation, decoded, _ = decode_product_request(encoded)
                self.assertEqual(operation, "structure_read")
                self.assertEqual(decoded, arguments)

    def test_windows_pipe_endpoint_normalization_never_doubles_prefix(self) -> None:
        self.assertEqual(_windows_pipe_namespace("hyphae-test"), "hyphae-test")
        self.assertEqual(_windows_pipe_namespace("\\\\.\\pipe\\hyphae-test"), "hyphae-test")
        with self.assertRaisesRegex(Exception, "local named-pipe namespace"):
            _windows_pipe_namespace("\\\\server\\pipe\\hyphae-test")

    def test_local_pipe_write_completes_short_writes(self) -> None:
        stream = ShortWriteStream()
        encoded = encode_frame(FRAME_KINDS["hello"], 0, 17, b"")
        _write_all(cast(BinaryIO, stream), encoded)
        self.assertEqual(stream.encoded, encoded)
        self.assertEqual(stream.flushes, 1)

    def test_high_level_api_is_transport_independent(self) -> None:
        transport = FakeTransport()
        client = HyphaeClient(transport)
        response = client.structure_get(b"key", options=RequestOptions(request_id=9))
        self.assertEqual(response.request_id, 9)
        self.assertEqual(transport.calls[0][0], "structure_get")

    def test_high_level_api_exposes_explicit_transactions(self) -> None:
        transport = FakeTransport()
        client = HyphaeClient(transport)
        client.transaction_begin(options=RequestOptions(request_id=20))
        client.transaction_stage_vector(
            7,
            {"kind": "delete", "index": 11, "object_id": 13},
            options=RequestOptions(request_id=21),
        )
        client.explicit_transaction_status(7, options=RequestOptions(request_id=22))
        client.transaction_status_by_idempotency(23, options=RequestOptions(request_id=23))
        self.assertEqual(
            [call[0] for call in transport.calls],
            [
                "transaction_begin",
                "transaction_stage_vector",
                "explicit_transaction_status",
                "transaction_status_by_idempotency",
            ],
        )

    def test_transaction_stage_response_decodes_typed_result(self) -> None:
        import struct

        payload = struct.pack("<QQBBB", 7, 1, 1, 3, 1)
        encoded = b"HYPRSP01" + struct.pack("<IHH", 16 + len(payload), 28, 0) + payload
        response = decode_product_response(encoded, 24)
        self.assertEqual(response.kind, "transaction_staged")
        self.assertEqual(
            response.value,
            {
                "handle": 7,
                "operation_ordinal": 1,
                "changed": True,
                "result": {"kind": "vector", "changed": True},
            },
        )

    def test_transaction_stage_requests_match_canonical_wire_kinds(self) -> None:
        import struct

        vector = encode_product_request(
            "transaction_stage_vector",
            {
                "handle": 7,
                "mutation": {"kind": "delete", "index": 11, "object_id": 13},
            },
            RequestOptions(request_id=25),
        )
        self.assertEqual(struct.unpack_from("<H", vector, 12)[0], 36)
        self.assertEqual(
            vector[80:],
            struct.pack("<QB", 7, 1)
            + (11).to_bytes(16, "little")
            + (13).to_bytes(16, "little"),
        )

        structure = encode_product_request(
            "transaction_stage_structure",
            {
                "handle": 7,
                "mutation": {
                    "kind": "create_hash",
                    "key": {"keyspace": 17, "key": b"hash"},
                },
            },
            RequestOptions(request_id=26),
        )
        self.assertEqual(struct.unpack_from("<H", structure, 12)[0], 34)
        self.assertEqual(
            structure[80:],
            struct.pack("<QB", 7, 3)
            + (17).to_bytes(16, "little")
            + struct.pack("<I", 4)
            + b"hash"
            + b"\x03",
        )

    def test_integrated_search_uses_only_logical_collection_identity(self) -> None:
        transport = FakeTransport()
        client = HyphaeClient(transport)
        client.search_collection(13, {"limit": 1, "vectors": []}, options=RequestOptions(request_id=10))
        client.search_ingest(
            13,
            {"idempotency_id": 7, "documents": [{"object_id": 21, "text": "hello"}]},
            options=RequestOptions(request_id=11),
        )
        self.assertEqual(transport.calls[0][1]["collection"], 13)
        self.assertNotIn("binding", transport.calls[0][1])
        self.assertEqual(transport.calls[1][0], "search_ingest")

    @patch("http.client.HTTPSConnection", FakeHttpConnection)
    def test_http_client_uses_v2_and_validates_correlation(self) -> None:
        transport = HttpTransport("https://example.test")
        response = transport.execute("capabilities", {}, RequestOptions(request_id=17))
        self.assertEqual(response.kind, "capabilities")
        self.assertEqual(FakeHttpConnection.last_path, "/v2/execute")


if __name__ == "__main__":
    unittest.main()
