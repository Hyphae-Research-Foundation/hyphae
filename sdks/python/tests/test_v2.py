# SPDX-License-Identifier: Apache-2.0

from __future__ import annotations

import unittest
from pathlib import Path
from typing import BinaryIO, cast
from unittest.mock import patch

from hyphae_sdk.v2 import ClientError, HyphaeClient, ProductError, RequestOptions, Response
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


def _response(kind: int, body: bytes) -> bytes:
    import struct

    return struct.pack("<8sIHH", b"HYPRSP01", 16 + len(body), kind, 0) + body


def _qualified_name() -> bytes:
    import struct

    return b"".join(
        struct.pack("<I", len(value)) + value
        for value in (b"main", b"main", b"public", b"public", b"item", b"item")
    )


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
            "X-Hyphae-Protocol-Minor": "3",
            "X-Hyphae-Request-Id": "17",
        }.get(name)

    def read(self, size: int = -1) -> bytes:
        return self._body[:size]


class FakeHttpConnection:
    last_path = ""

    def __init__(self, *args, **kwargs) -> None:  # type: ignore[no-untyped-def]
        del args, kwargs
        self.path = ""
        self.sock = None
        self.auto_open = 1

    def connect(self) -> None:
        pass

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


class FakeJsonErrorHttpResponse:
    status = 409

    def __init__(self) -> None:
        self._body = (
            b'{"code":"catalog_conflict","category":"conflict",'
            b'"retry":"after-refresh","message":"catalog changed",'
            b'"request_id":19,"trace_id":23,"object_id":29,'
            b'"transaction_state":"none","transaction_id":null,'
            b'"details":{"reason":"stale"}}'
        )

    def getheader(self, name: str) -> str | None:
        return {
            "Content-Length": str(len(self._body)),
            "Content-Type": "application/json",
            "X-Hyphae-Protocol-Minor": "3",
            "X-Hyphae-Request-Id": "19",
        }.get(name)

    def read(self, size: int = -1) -> bytes:
        return self._body[:size]


class FakeJsonErrorHttpConnection(FakeHttpConnection):
    def getresponse(self) -> FakeJsonErrorHttpResponse:
        return FakeJsonErrorHttpResponse()


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

    def test_attested_rerank_request_matches_the_cross_language_golden(self) -> None:
        envelope = (
            b"HYATTS01\x02"
            + (6).to_bytes(2, "little")
            + b"openai"
            + (22).to_bytes(2, "little")
            + b"text-embedding-3-small"
            + bytes([3]) * 32
            + bytes([4]) * 32
        )
        arguments = {
            "collection": 13,
            "request": {
                "lexical": {"query": "rust", "candidate_limit": 4, "weight": 1},
                "vectors": [],
                "limit": 4,
                "rerank": {
                    "attestation": envelope,
                    "scores": [
                        {"object_id": 201, "score": 0.75},
                        {"object_id": 202, "score": 0.25},
                    ],
                },
            },
        }
        options = RequestOptions(logical_time_micros=10, durability="memory")
        with self.assertRaises(ClientError):
            encode_product_request(
                "search_collection", arguments, options, negotiated_minor=3
            )
        encoded = encode_product_request(
            "search_collection", arguments, options, negotiated_minor=4
        )
        # The same digest is pinned by the Rust protocol goldens and the
        # TypeScript suite for this identically composed request.
        self.assertEqual(
            blake3(encoded).hex(),
            "f61fd68c170b8cf0841678aeda0819f7ff98869486b51ea10c104e8e2d4cee04",
        )

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
            (
                "catalog_visible_list",
                {
                    "parent": None,
                    "kind": None,
                    "cursor": b"opaque",
                    "item_limit": 2,
                    "visit_limit": 8,
                    "byte_limit": 4096,
                },
            ),
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

    def test_catalog_visible_page_decodes_canonical_items(self) -> None:
        import struct

        cursor = bytes((7,)) * 176
        body = (
            struct.pack("<I", len(cursor))
            + cursor
            + struct.pack("<I", 1)
            + (5).to_bytes(16, "little")
            + struct.pack("<BB6x", 3, 1)
            + (2).to_bytes(16, "little")
            + _qualified_name()
        )
        response = decode_product_response(
            _response(42, body), 27, negotiated_minor=3
        )
        self.assertEqual(response.kind, "catalog_visible_page")
        self.assertEqual(response.value["cursor"], cursor)
        self.assertEqual(response.value["items"][0]["id"], 5)
        self.assertEqual(response.value["items"][0]["object_kind"], 3)
        self.assertEqual(response.value["items"][0]["parent"], 2)

    def test_catalog_visible_page_rejects_oversized_or_impossible_counts(self) -> None:
        import struct

        for count in (4_097, 0xFFFFFFFF, 1):
            with self.subTest(count=count):
                with self.assertRaisesRegex(ClientError, "item count"):
                    decode_product_response(
                        _response(42, struct.pack("<II", 0, count)),
                        28,
                        negotiated_minor=3,
                    )
        with self.assertRaisesRegex(ClientError, "protocol maximum"):
            decode_product_response(
                _response(42, struct.pack("<I", 16 * 1024 * 1024 + 1)),
                28,
                negotiated_minor=3,
            )

    def test_catalog_visible_page_rejects_zero_ids_and_unknown_kinds(self) -> None:
        import struct

        def item(object_id: int, kind: int, parent: int | None) -> bytes:
            return (
                object_id.to_bytes(16, "little")
                + struct.pack("<BB6x", kind, parent is not None)
                + (b"" if parent is None else parent.to_bytes(16, "little"))
                + _qualified_name()
            )

        for name, encoded_item in {
            "zero object": item(0, 1, None),
            "zero kind": item(1, 0, None),
            "unknown kind": item(1, 10, None),
            "zero parent": item(1, 1, 0),
        }.items():
            with self.subTest(name=name):
                body = struct.pack("<II", 0, 1) + encoded_item
                with self.assertRaises(ClientError):
                    decode_product_response(
                        _response(42, body), 29, negotiated_minor=3
                    )

    def test_catalog_visible_page_rejects_every_truncation_and_trailing_bytes(self) -> None:
        import struct

        body = struct.pack("<II", 0, 1) + (
            (1).to_bytes(16, "little")
            + struct.pack("<BB6x", 1, 0)
            + _qualified_name()
        )
        encoded = _response(42, body)
        for prefix in range(len(encoded)):
            with self.subTest(prefix=prefix):
                with self.assertRaises(ClientError):
                    decode_product_response(
                        encoded[:prefix], 30, negotiated_minor=3
                    )
        trailing = bytearray(encoded + b"\0")
        struct.pack_into("<I", trailing, 8, len(trailing))
        with self.assertRaisesRegex(ClientError, "trailing"):
            decode_product_response(bytes(trailing), 30, negotiated_minor=3)

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

    @patch("http.client.HTTPSConnection", FakeHttpConnection)
    def test_http_client_rejects_nonexact_selected_minor_before_decoding(self) -> None:
        original = FakeHttpResponse.getheader
        for minor in (None, "2", "garbage"):
            with self.subTest(minor=minor):
                def selected_minor(response: FakeHttpResponse, name: str) -> str | None:
                    if name == "X-Hyphae-Protocol-Minor":
                        return minor
                    if name == "X-Hyphae-Session-Id":
                        return "1" * 32
                    return original(response, name)

                with patch.object(FakeHttpResponse, "getheader", selected_minor):
                    transport = HttpTransport("https://example.test")
                    with self.assertRaisesRegex(Exception, "protocol minor"):
                        transport.execute(
                            "capabilities", {}, RequestOptions(request_id=17)
                        )
                    self.assertIsNone(transport._session_id)

    @patch("http.client.HTTPSConnection", FakeHttpConnection)
    def test_http_swapped_response_cannot_poison_the_next_session(self) -> None:
        original = FakeHttpResponse.getheader
        calls = 0

        def swapped(response: FakeHttpResponse, name: str) -> str | None:
            nonlocal calls
            if name == "X-Hyphae-Request-Id":
                calls += 1
                return "99" if calls == 1 else "18"
            if name == "X-Hyphae-Session-Id":
                return "1" * 32 if calls == 1 else None
            return original(response, name)

        with patch.object(FakeHttpResponse, "getheader", swapped):
            transport = HttpTransport("https://example.test")
            with self.assertRaisesRegex(Exception, "request ID mismatch"):
                transport.execute("capabilities", {}, RequestOptions(request_id=17))
            self.assertIsNone(transport._session_id)
            response = transport.execute(
                "capabilities", {}, RequestOptions(request_id=18)
            )
            self.assertEqual(response.kind, "capabilities")
            self.assertIsNone(transport._session_id)

    @patch("http.client.HTTPSConnection", FakeJsonErrorHttpConnection)
    def test_http_client_decodes_valid_json_product_error(self) -> None:
        transport = HttpTransport("https://example.test")

        with self.assertRaises(ProductError) as caught:
            transport.execute("capabilities", {}, RequestOptions(request_id=19))

        self.assertEqual(caught.exception.status, 409)
        self.assertEqual(caught.exception.fields.code, "catalog_conflict")
        self.assertEqual(caught.exception.fields.request_id, 19)
        self.assertEqual(caught.exception.fields.trace_id, 23)
        self.assertEqual(caught.exception.fields.object_id, 29)
        self.assertEqual(caught.exception.fields.details, {"reason": "stale"})


if __name__ == "__main__":
    unittest.main()
