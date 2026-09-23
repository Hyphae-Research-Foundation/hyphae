# SPDX-License-Identifier: Apache-2.0

from __future__ import annotations

import json
import unittest
from pathlib import Path
from typing import BinaryIO, cast
from unittest.mock import patch

from hyphae_sdk.v2 import ClientError, HyphaeClient, ProductError, RequestOptions, Response
from hyphae_sdk.v2.http import HttpTransport, PRODUCT_MEDIA_TYPE
from hyphae_sdk.v2.local import _windows_pipe_namespace, _write_all
from hyphae_sdk.v2.protocol import (
    FRAME_KINDS,
    MAX_PAYLOAD,
    decode_frame,
    decode_product_request,
    decode_product_response,
    encode_frame,
    encode_product_request,
    blake3,
    operation_required_minor,
)


FIXTURE = Path(__file__).parents[3] / "compatibility" / "native-protocol-v1-structure-get.bin"
TRANSACTION_DOCUMENT_FIXTURE = (
    Path(__file__).parents[3]
    / "compatibility"
    / "native-protocol-v1-transaction-document.bin"
)
TRANSACTION_DOCUMENT_ORDERING_FIXTURE = (
    Path(__file__).parents[3]
    / "compatibility"
    / "native-protocol-v1-transaction-document-ordering.json"
)
REQUIRED_MINOR_FIXTURE = (
    Path(__file__).parents[3]
    / "compatibility"
    / "native-protocol-v1-required-minors.json"
)


def _response(kind: int, body: bytes) -> bytes:
    import struct

    return struct.pack("<8sIHH", b"HYPRSP01", 16 + len(body), kind, 0) + body


def _qualified_name() -> bytes:
    import struct

    return b"".join(
        struct.pack("<I", len(value)) + value
        for value in (b"main", b"main", b"public", b"public", b"item", b"item")
    )


def _commit_receipt() -> bytes:
    import struct

    return b"".join(
        [
            (9).to_bytes(16, "little"),
            struct.pack("<QQQ", 7, 8, 9),
            bytes((3,)) * 32,
            b"\0" * 8,
            struct.pack("<QQ", 1, 0),
        ]
    )


def _embed_and_ingest_response(
    *, replay: bool = True, has_commit: bool = True
) -> bytes:
    import struct

    def text(value: str) -> bytes:
        encoded = value.encode()
        return struct.pack("<I", len(encoded)) + encoded

    snapshot = (
        bytes((1,)) * 24
        + struct.pack("<QQ", 7, 8)
        + bytes((2,)) * 32
        + struct.pack("<q", 10)
    )
    profile = (
        (17).to_bytes(16, "little")
        + struct.pack("<BBB5x", 1, 1, 1)
        + text("NVIDIA H100")
        + text("driver-1")
        + text("cuda-1")
        + struct.pack("<I", 2)
        + text("tokenize-v1")
        + text("qwen3-f32-v1")
    )
    commit = _commit_receipt() if has_commit else b""
    return _response(
        47,
        snapshot
        + struct.pack("<BB6xQ", has_commit, replay, 1)
        + profile
        + commit,
    )


class FakeTransport:
    def __init__(self) -> None:
        self.calls: list[tuple[str, dict[str, object], RequestOptions]] = []

    def execute(self, operation: str, arguments: dict[str, object], options: RequestOptions) -> Response:
        self.calls.append((operation, arguments, options))
        kind = "embed_and_ingested" if operation == "embed_and_ingest" else "fake"
        return Response(kind, arguments, options.checked_request_id())


class StructureBatchResponseTests(unittest.TestCase):
    def test_noop_response_requires_minor_seven_and_bounds_results(self) -> None:
        import struct

        body = b"".join(
            [
                struct.pack("<Q", 7),
                b"\0" * 8,
                struct.pack("<I", 1),
                b"\0" * 4,
                b"\0\2\0",
            ]
        )
        encoded = _response(46, body)
        response = decode_product_response(encoded, 9, negotiated_minor=7)
        self.assertEqual(response.kind, "structure_mutation_batch")
        self.assertEqual(response.value["read_csn"], 7)
        self.assertIsNone(response.value["commit"])
        self.assertEqual(
            response.value["results"],
            [{"changed": False, "result": {"kind": "boolean", "value": False}}],
        )
        with self.assertRaises(ClientError):
            decode_product_response(encoded, 9, negotiated_minor=6)
        excessive = bytearray(encoded)
        excessive[32:36] = struct.pack("<I", 0xFFFFFFFF)
        with self.assertRaises(ClientError):
            decode_product_response(bytes(excessive), 9, negotiated_minor=7)

    def test_committed_response_decodes_every_minor_six_result_shape(self) -> None:
        import struct

        receipt = b"".join(
            [
                (9).to_bytes(16, "little"),
                struct.pack("<QQQ", 8, 3, 11),
                bytes([4]) * 32,
                b"\0" * 8,
                struct.pack("<QQ", 1, 0),
            ]
        )
        body = b"".join(
            [
                struct.pack("<Q", 7),
                b"\1" + b"\0" * 7,
                struct.pack("<I", 2),
                b"\0" * 4,
                b"\1\6" + struct.pack("<d", 1.5),
                b"\0\7\1" + struct.pack("<I", 1) + b"x" + struct.pack("<d", 2.5),
                receipt,
            ]
        )
        response = decode_product_response(_response(46, body), 10, negotiated_minor=7)
        self.assertEqual(response.value["commit"]["transaction_id"], 9)
        self.assertEqual(
            response.value["results"],
            [
                {"changed": True, "result": {"kind": "score", "value": 1.5}},
                {
                    "changed": False,
                    "result": {
                        "kind": "popped_entry",
                        "entry": {"member": b"x", "score": 2.5},
                    },
                },
            ],
        )


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
    last_body = b""
    last_headers: dict[str, str] = {}
    requests = 0

    def __init__(self, *args, **kwargs) -> None:  # type: ignore[no-untyped-def]
        del args, kwargs
        self.path = ""
        self.sock = None
        self.auto_open = 1

    def connect(self) -> None:
        pass

    def request(self, method: str, path: str, **kwargs) -> None:  # type: ignore[no-untyped-def]
        del method
        self.path = path
        type(self).last_path = path
        type(self).last_body = kwargs["body"]
        type(self).last_headers = kwargs["headers"]
        type(self).requests += 1

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
    def test_minor_eight_embedding_profile_content_is_gated(self) -> None:
        import struct

        profile = b"HYCOBJ02" + bytes((10, 2))
        bound_search = b"HYCOBJ02" + bytes((7, 4))
        for definition in (profile, bound_search):
            arguments = {"definition": definition}
            self.assertEqual(operation_required_minor("catalog_create", arguments), 8)
            with self.assertRaisesRegex(ClientError, "protocol minor"):
                encode_product_request(
                    "catalog_create",
                    arguments,
                    RequestOptions(),
                    negotiated_minor=7,
                )
            encode_product_request(
                "catalog_create", arguments, RequestOptions(), negotiated_minor=8
            )
            response_body = struct.pack("<B3xI", 1, len(definition)) + definition
            with self.assertRaisesRegex(ClientError, "protocol minor"):
                decode_product_response(
                    _response(15, response_body), 1, negotiated_minor=7
                )
            self.assertEqual(
                decode_product_response(
                    _response(15, response_body), 1, negotiated_minor=8
                ).value,
                definition,
            )
            snapshot = bytes(24) + struct.pack("<QQ", 1, 1) + bytes(32) + struct.pack("<q", 0)
            object_body = snapshot + struct.pack("<I", len(definition)) + definition
            with self.assertRaisesRegex(ClientError, "protocol minor"):
                decode_product_response(
                    _response(12, object_body), 1, negotiated_minor=7
                )
            self.assertEqual(
                decode_product_response(
                    _response(12, object_body), 1, negotiated_minor=8
                ).value["definition"],
                definition,
            )

        listing = {
            "parent": None,
            "kind": "embedding_profile",
            "cursor": None,
            "item_limit": 1,
            "visit_limit": 1,
            "byte_limit": 4096,
        }
        self.assertEqual(operation_required_minor("catalog_list", listing), 8)
        with self.assertRaisesRegex(ClientError, "protocol minor"):
            encode_product_request(
                "catalog_list", listing, RequestOptions(), negotiated_minor=7
            )

        body = (
            struct.pack("<II", 0, 1)
            + (12).to_bytes(16, "little")
            + struct.pack("<BB6x", 10, 0)
            + _qualified_name()
        )
        with self.assertRaisesRegex(ClientError, "protocol minor"):
            decode_product_response(_response(42, body), 1, negotiated_minor=7)
        self.assertEqual(
            decode_product_response(
                _response(42, body), 1, negotiated_minor=8
            ).value["items"][0]["object_kind"],
            10,
        )

    def test_embed_and_ingest_minor_nine_codec_is_bounded_and_canonical(self) -> None:
        import struct

        private_use = "\ue000"
        supplementary = "\U0001f600"
        arguments = {
            "collection": 13,
            "batch": {
                "idempotency_id": 7,
                "documents": [
                    {
                        "object_id": 201,
                        "text": "rust database",
                        "doc_values": {
                            supplementary: "supplementary",
                            private_use: "private-use",
                        },
                    }
                ],
            },
        }
        self.assertEqual(operation_required_minor("embed_and_ingest", arguments), 9)
        with self.assertRaisesRegex(ClientError, "protocol minor"):
            encode_product_request(
                "embed_and_ingest",
                arguments,
                RequestOptions(),
                negotiated_minor=8,
            )
        encoded = encode_product_request(
            "embed_and_ingest", arguments, RequestOptions(), negotiated_minor=9
        )
        self.assertEqual(struct.unpack_from("<H", encoded, 12)[0], 73)
        operation, decoded, _ = decode_product_request(encoded, negotiated_minor=9)
        self.assertEqual(operation, "embed_and_ingest")
        self.assertEqual(
            list(decoded["batch"]["documents"][0]["doc_values"]),
            [private_use, supplementary],
        )
        self.assertEqual(
            encode_product_request(
                operation, decoded, RequestOptions(), negotiated_minor=9
            ),
            encoded,
        )

        forged = bytearray(encoded)
        struct.pack_into("<I", forged, 112, 0xFFFFFFFF)
        with self.assertRaisesRegex(ClientError, "document count"):
            decode_product_request(bytes(forged), negotiated_minor=9)

        too_many = {
            "collection": 13,
            "batch": {
                "idempotency_id": 7,
                "documents": [
                    {"object_id": index + 1, "text": "x"}
                    for index in range(257)
                ],
            },
        }
        with self.assertRaisesRegex(ClientError, "bounded list"):
            encode_product_request(
                "embed_and_ingest",
                too_many,
                RequestOptions(),
                negotiated_minor=9,
            )

        minimal_arguments = {
            "collection": 13,
            "batch": {
                "idempotency_id": 7,
                "documents": [{"object_id": 201, "text": ""}],
            },
        }
        minimal = encode_product_request(
            "embed_and_ingest",
            minimal_arguments,
            RequestOptions(),
            negotiated_minor=9,
        )
        maximum_text = "x" * (MAX_PAYLOAD - len(minimal))
        maximum_arguments = {
            "collection": 13,
            "batch": {
                "idempotency_id": 7,
                "documents": [{"object_id": 201, "text": maximum_text}],
            },
        }
        maximum = encode_product_request(
            "embed_and_ingest",
            maximum_arguments,
            RequestOptions(),
            negotiated_minor=9,
        )
        self.assertEqual(len(maximum), MAX_PAYLOAD)
        maximum_arguments["batch"]["documents"][0]["text"] += "x"
        with self.assertRaisesRegex(ClientError, "16 MiB"):
            encode_product_request(
                "embed_and_ingest",
                maximum_arguments,
                RequestOptions(),
                negotiated_minor=9,
            )

        for forbidden in (
            "model_path",
            "backend",
            "device",
            "target",
            "embedding_profile",
            "targets",
            "profiles",
        ):
            invalid = {**arguments, forbidden: "not-on-wire"}
            with self.subTest(forbidden=forbidden), self.assertRaisesRegex(
                ClientError, "request fields"
            ):
                encode_product_request(
                    "embed_and_ingest",
                    invalid,
                    RequestOptions(),
                    negotiated_minor=9,
                )

    def test_embed_and_ingest_matches_shared_rust_fixture(self) -> None:
        fixture = (
            Path(__file__).parents[3]
            / "compatibility"
            / "native-protocol-v1-embed-and-ingest.bin"
        ).read_bytes()
        frame = decode_frame(fixture)
        self.assertEqual((frame.kind, frame.stream_id, frame.request_id),
                         (FRAME_KINDS["execute"], 9, 44))
        operation, arguments, options = decode_product_request(
            frame.payload, negotiated_minor=9
        )
        self.assertEqual(operation, "embed_and_ingest")
        self.assertEqual(options.logical_time_micros, 1)
        self.assertEqual(
            encode_frame(frame.kind, frame.stream_id, frame.request_id,
                         encode_product_request(operation, arguments, options,
                                                negotiated_minor=9)),
            fixture,
        )
        independent = encode_product_request(
            "embed_and_ingest",
            {"collection": 13, "batch": {"idempotency_id": 7, "documents": [
                {"object_id": 201, "text": "rust", "doc_values": {
                    "\U0001f600": "supplementary", "\ue000": "private-use"}}
            ]}},
            RequestOptions(logical_time_micros=1), negotiated_minor=9,
        )
        self.assertEqual(encode_frame(FRAME_KINDS["execute"], 9, 44, independent),
                         fixture)

    def test_embed_and_ingest_response_reports_execution_profile_and_replay(self) -> None:
        import struct

        encoded = _embed_and_ingest_response()
        response = decode_product_response(encoded, 19, negotiated_minor=9)
        self.assertEqual(response.kind, "embed_and_ingested")
        self.assertTrue(response.value["idempotent_replay"])
        self.assertEqual(response.value["commit"]["transaction_id"], 9)
        self.assertEqual(
            response.value["execution_profile"],
            {
                "embedding_profile": 17,
                "backend": "cuda",
                "device": "NVIDIA H100",
                "driver": "driver-1",
                "runtime": "cuda-1",
                "precision": "f32",
                "kernels": ["tokenize-v1", "qwen3-f32-v1"],
                "fallback": True,
            },
        )
        for replay in (False, True):
            with self.subTest(replay=replay), self.assertRaisesRegex(
                ClientError, "commit evidence"
            ):
                decode_product_response(
                    _embed_and_ingest_response(
                        replay=replay, has_commit=False
                    ),
                    19,
                    negotiated_minor=9,
                )
        with self.assertRaisesRegex(ClientError, "protocol minor"):
            decode_product_response(encoded, 19, negotiated_minor=8)
        for prefix in range(len(encoded)):
            with self.subTest(prefix=prefix), self.assertRaises(ClientError):
                decode_product_response(encoded[:prefix], 19, negotiated_minor=9)

        excessive = bytearray(encoded)
        kernel_count_offset = (
            16
            + 80
            + 16
            + 24
            + sum(4 + len(value) for value in ("NVIDIA H100", "driver-1", "cuda-1"))
        )
        struct.pack_into("<I", excessive, kernel_count_offset, 0xFFFFFFFF)
        with self.assertRaisesRegex(ClientError, "kernel count"):
            decode_product_response(bytes(excessive), 19, negotiated_minor=9)

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

    def test_shared_required_minor_fixture_is_exhaustive(self) -> None:
        fixture = json.loads(REQUIRED_MINOR_FIXTURE.read_text())
        self.assertEqual(len(fixture["cases"]), 27)
        for case in fixture["cases"]:
            with self.subTest(case=case["name"]):
                self.assertEqual(
                    operation_required_minor(case["operation"], case["arguments"]),
                    case["required_minor"],
                )

    def test_u128_and_identity_inputs_fail_closed(self) -> None:
        for value in (True, False, -1, 1 << 128, 1.5, "1"):
            with self.subTest(value=value), self.assertRaises(ClientError):
                encode_product_request(
                    "transaction_status",
                    {"transaction_id": value},
                    RequestOptions(),
                )
        for value in (0, -1, 1 << 128):
            with self.subTest(identity=value), self.assertRaises(ClientError):
                encode_product_request(
                    "structure_read",
                    {"kind": "string_get", "key": {"keyspace": value, "key": b"k"}},
                    RequestOptions(),
                )
        encoded = encode_product_request(
            "transaction_status",
            {"transaction_id": (1 << 128) - 1},
            RequestOptions(),
        )
        self.assertEqual(decode_product_request(encoded)[1]["transaction_id"], (1 << 128) - 1)

    def test_transaction_document_matches_the_shared_fixture(self) -> None:
        encoded = TRANSACTION_DOCUMENT_FIXTURE.read_bytes()
        frame = decode_frame(encoded)
        operation, arguments, options = decode_product_request(
            frame.payload, negotiated_minor=7
        )
        self.assertEqual(operation, "transaction_stage_search")
        self.assertEqual(
            arguments,
            {
                "handle": 7,
                "mutation": {
                    "kind": "document",
                    "collection": 13,
                    "document": {
                        "object_id": 201,
                        "text": "rust database",
                        "doc_values": {
                            "blob": b"\x07",
                            "flag": True,
                            "name": "a",
                            "rank": 3,
                            "rating": 4.5,
                        },
                        "vectors": {"embedding": [1.0, 0.0]},
                    },
                },
            },
        )
        self.assertEqual(options.logical_time_micros, 10)
        self.assertEqual(options.durability, "memory")
        with self.assertRaisesRegex(ClientError, "protocol minor"):
            decode_product_request(frame.payload, negotiated_minor=6)
        self.assertEqual(
            encode_frame(
                frame.kind,
                frame.stream_id,
                frame.request_id,
                encode_product_request(
                    operation, arguments, options, negotiated_minor=7
                ),
            ),
            encoded,
        )

    def test_transaction_document_fixture_rejects_forged_counts_before_allocation(
        self,
    ) -> None:
        import struct

        payload = decode_frame(TRANSACTION_DOCUMENT_FIXTURE.read_bytes()).payload
        for offset in (138, 216, 233):
            with self.subTest(offset=offset):
                excessive = bytearray(payload)
                struct.pack_into("<I", excessive, offset, 0xFFFFFFFF)
                with self.assertRaisesRegex(ClientError, "exceeds"):
                    decode_product_request(bytes(excessive), negotiated_minor=7)

        truncated_values = bytearray(payload[:142])
        struct.pack_into("<I", truncated_values, 8, len(truncated_values))
        truncated_vectors = bytearray(payload[:220])
        struct.pack_into("<I", truncated_vectors, 8, len(truncated_vectors))
        truncated_dimension = bytearray(payload[:241])
        struct.pack_into("<I", truncated_dimension, 8, len(truncated_dimension))
        struct.pack_into("<I", truncated_dimension, 233, 2)
        for encoded in (
            truncated_values,
            truncated_vectors,
            truncated_dimension,
        ):
            with self.assertRaisesRegex(ClientError, "exceeds"):
                decode_product_request(bytes(encoded), negotiated_minor=7)

    def test_transaction_document_fixture_rejects_invalid_rust_values(self) -> None:
        import struct

        payload = decode_frame(TRANSACTION_DOCUMENT_FIXTURE.read_bytes()).payload
        for offset in (89, 105):
            with self.subTest(zero_identity_offset=offset):
                forged = bytearray(payload)
                forged[offset:offset + 16] = b"\0" * 16
                with self.assertRaisesRegex(ClientError, "zero"):
                    decode_product_request(bytes(forged), negotiated_minor=7)

        for bits in (0x8000_0000_0000_0000, 0x7FF0_0000_0000_0001):
            with self.subTest(noncanonical_float=bits):
                forged = bytearray(payload)
                struct.pack_into("<Q", forged, 208, bits)
                with self.assertRaisesRegex(ClientError, "noncanonical"):
                    decode_product_request(bytes(forged), negotiated_minor=7)

        for value in (float("inf"), float("-inf"), float("nan")):
            with self.subTest(nonfinite_vector=value):
                forged = bytearray(payload)
                struct.pack_into("<f", forged, 237, value)
                with self.assertRaisesRegex(ClientError, "nonfinite"):
                    decode_product_request(bytes(forged), negotiated_minor=7)

    def test_transaction_document_tag_requires_minor_seven(self) -> None:
        arguments = {
            "handle": 7,
            "mutation": {
                "kind": "document",
                "collection": 13,
                "document": {
                    "object_id": 201,
                    "text": "rated",
                    "doc_values": {"rating": 4.5},
                    "vectors": {},
                },
            },
        }
        with self.assertRaisesRegex(ClientError, "protocol minor"):
            encode_product_request(
                "transaction_stage_search",
                arguments,
                RequestOptions(),
                negotiated_minor=6,
            )
        encoded = encode_product_request(
            "transaction_stage_search",
            arguments,
            RequestOptions(),
            negotiated_minor=7,
        )
        with self.assertRaisesRegex(ClientError, "protocol minor"):
            decode_product_request(encoded, negotiated_minor=6)
        operation, decoded, _ = decode_product_request(encoded, negotiated_minor=7)
        self.assertEqual(operation, "transaction_stage_search")
        self.assertEqual(decoded, arguments)

    def test_transaction_document_names_use_canonical_utf8_byte_order(self) -> None:
        fixture = json.loads(TRANSACTION_DOCUMENT_ORDERING_FIXTURE.read_text())
        canonical = bytes.fromhex(fixture["canonical_request_hex"])
        operation, arguments, options = decode_product_request(
            canonical, negotiated_minor=7
        )
        document = arguments["mutation"]["document"]
        private_use = "\ue000"
        supplementary = "\U0001f600"
        self.assertEqual(
            list(document["doc_values"]), [private_use, supplementary]
        )
        self.assertEqual(list(document["vectors"]), [private_use, supplementary])

        reverse_insertion = {
            "handle": 7,
            "mutation": {
                "kind": "document",
                "collection": 13,
                "document": {
                    "object_id": 201,
                    "text": "unicode",
                    "doc_values": {
                        supplementary: "supplementary",
                        private_use: "private-use",
                    },
                    "vectors": {
                        supplementary: [0.0, 1.0],
                        private_use: [1.0, 0.0],
                    },
                },
            },
        }
        self.assertEqual(operation, "transaction_stage_search")
        self.assertEqual(
            encode_product_request(operation, reverse_insertion, options, negotiated_minor=7),
            canonical,
        )

    def test_transaction_document_rejects_nonascending_names_before_payloads(
        self,
    ) -> None:
        fixture = json.loads(TRANSACTION_DOCUMENT_ORDERING_FIXTURE.read_text())
        for name, encoded_hex in fixture["malformed_requests_hex"].items():
            with self.subTest(name=name), self.assertRaisesRegex(
                ClientError, "strictly ascending by UTF-8 bytes"
            ):
                decode_product_request(
                    bytes.fromhex(encoded_hex), negotiated_minor=7
                )

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

    def test_highlighted_request_matches_the_cross_language_golden(self) -> None:
        arguments = {
            "collection": 13,
            "request": {
                "lexical": {"query": "rust", "candidate_limit": 4, "weight": 1},
                "vectors": [],
                "limit": 4,
                "highlight": {"max_fragments": 2, "fragment_bytes": 64},
            },
        }
        options = RequestOptions(logical_time_micros=10, durability="memory")
        with self.assertRaises(ClientError):
            encode_product_request(
                "search_collection", arguments, options, negotiated_minor=4
            )
        encoded = encode_product_request(
            "search_collection", arguments, options, negotiated_minor=5
        )
        # The same digest is pinned by the Rust protocol goldens and the
        # TypeScript suite for this identically composed request.
        self.assertEqual(
            blake3(encoded).hex(),
            "1438488e4d12a342a71d1cab17bad2fecf6ddc46ecb8e73970fc6f037e5e1443",
        )

    def test_integrated_search_response_decodes_with_and_without_fragments(self) -> None:
        # Both payloads are Rust-encoded goldens for the same one-hit result;
        # the second carries the minor-5 content-derived fragments tail.
        plain = bytes.fromhex(
            "4859505253503031bc0000001600000001010101010101010101010101010101"
            "0101010101010101000000000000000003000000000000000404040404040404"
            "0404040404040404040404040404040404040404040404040500000000000000"
            "01000000c9000000000000000000000000000000000000000000f83f00000000"
            "0000000000000000000000000000000000000000010000000000000001000000"
            "00000000010000000000000001000000000000000100000000000000"
        )
        fragmented = bytes.fromhex(
            "4859505253503031d20000001600000001010101010101010101010101010101"
            "0101010101010101000000000000000003000000000000000404040404040404"
            "0404040404040404040404040404040404040404040404040500000000000000"
            "01000000c9000000000000000000000000000000000000000000f83f00000000"
            "0000000000000000000000000000000000000000010000000000000001000000"
            "0000000001000000000000000100000000000000010000000000000001010000"
            "000d00000072757374206461746162617365"
        )
        for payload, fragments in ((plain, None), (fragmented, ["rust database"])):
            response = decode_product_response(payload, None)
            self.assertEqual(response.kind, "integrated_search")
            hit = response.value["hits"][0]
            self.assertEqual(hit["object_id"], 201)
            self.assertEqual(hit.get("fragments"), fragments)

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
            {"kind": "sorted_set_score_range", "key": key, "lower": {"exclusive": 1.5}, "upper": None, "offset": 2, "limit": 16, "order": "descending"},
            {"kind": "hash_scan_reverse", "key": key, "start_before": b"field", "limit": 8},
            {"kind": "hash_scan_match", "key": key, "pattern": b"user:*", "start_after": None, "output_limit": 8, "visit_limit": 32, "match_step_limit": 256},
            {"kind": "key_scan_match", "keyspace": 7, "pattern": b"app:*", "start_after": b"app:flag", "output_limit": 8, "visit_limit": 32, "match_step_limit": 256},
            {"kind": "string_range", "key": key, "start": -5, "end": -1},
            {"kind": "set_random_members", "key": key, "seed": 42, "count": 3},
        )
        for arguments in cases:
            with self.subTest(kind=arguments["kind"]):
                required = 6 if arguments["kind"] in {"sorted_set_score_range", "hash_scan_reverse", "hash_scan_match", "key_scan_match", "string_range", "set_random_members"} else 0
                if required:
                    with self.assertRaises(ClientError):
                        encode_product_request("structure_read", arguments, RequestOptions(), negotiated_minor=5)
                encoded = encode_product_request("structure_read", arguments, RequestOptions(), negotiated_minor=max(required, 5))
                operation, decoded, _ = decode_product_request(encoded)
                self.assertEqual(operation, "structure_read")
                self.assertEqual(decoded, arguments)

    def test_minor_six_search_and_structure_mutations_round_trip(self) -> None:
        base = {
            "collection": 13,
            "request": {
                "lexical": {"query": "rust", "candidate_limit": 4, "weight": 1, "phrase": True},
                "vectors": [{"target": "image", "query": [0.0, 1.0], "candidate_limit": 4, "weight": 1, "execution": {"kind": "exact"}, "max_distance": 4.0}],
                "filter": {"kind": "match_all"},
                "sort": [],
                "facets": [],
                "range_facets": [{"field": "price", "ranges": [{"lower": None, "upper": 15.0}, {"lower": 15.0, "upper": None}]}],
                "aggregations": [{"name": "mean", "kind": "average", "field": "price"}],
                "limit": 4,
                "fusion": "relative_score",
                "autocut": 2,
                "offset": 1,
            },
        }
        with self.assertRaises(ClientError):
            encode_product_request("search_collection", base, RequestOptions(), negotiated_minor=5)
        encoded = encode_product_request("search_collection", base, RequestOptions(), negotiated_minor=6)
        operation, decoded, _ = decode_product_request(encoded, negotiated_minor=6)
        self.assertEqual(operation, "search_collection")
        self.assertEqual(decoded["request"]["fusion"], "relative_score")
        self.assertEqual(decoded["request"]["autocut"], 2)
        self.assertEqual(decoded["request"]["offset"], 1)
        self.assertEqual(decoded["request"]["lexical"]["phrase"], True)
        self.assertEqual(decoded["request"]["vectors"][0]["max_distance"], 4.0)
        self.assertEqual(
            encode_product_request(operation, decoded, RequestOptions(), negotiated_minor=6),
            encoded,
        )

        key = {"keyspace": 7, "key": b"key"}
        mutations = (
            {"kind": "sorted_set_increment", "key": key, "delta": 2.5, "member": b"a"},
            {"kind": "sorted_set_pop", "key": key, "end": "highest"},
            {"kind": "string_set_conditional", "key": key, "value": b"hello", "expires_at_micros": 99, "condition": "if_present"},
            {"kind": "string_append", "key": key, "suffix": b" world"},
            {"kind": "string_set_range", "key": key, "offset": 4, "patch": b"tail"},
            {"kind": "hash_set_if_absent", "key": key, "field": b"city", "value": b"lima"},
            {"kind": "set_pop", "key": key, "seed": 42},
        )
        for mutation in mutations:
            with self.subTest(mutation=mutation["kind"]):
                args = {"mutations": [mutation]}
                with self.assertRaises(ClientError):
                    encode_product_request("structure_mutate", args, RequestOptions(), negotiated_minor=5)
                wire = encode_product_request("structure_mutate", args, RequestOptions(), negotiated_minor=6)
                self.assertEqual(decode_product_request(wire, negotiated_minor=6)[1], args)

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
        document_mutation = {
            "kind": "document",
            "collection": 13,
            "document": {"object_id": 201, "text": "rust"},
        }
        client.transaction_stage_search(
            7,
            document_mutation,
            options=RequestOptions(request_id=24),
        )
        client.explicit_transaction_status(7, options=RequestOptions(request_id=22))
        client.transaction_status_by_idempotency(23, options=RequestOptions(request_id=23))
        self.assertEqual(
            [call[0] for call in transport.calls],
            [
                "transaction_begin",
                "transaction_stage_vector",
                "transaction_stage_search",
                "explicit_transaction_status",
                "transaction_status_by_idempotency",
            ],
        )
        self.assertEqual(transport.calls[2][1]["mutation"], document_mutation)

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
    def test_fresh_http_client_sends_minor_nine_operation_on_first_request(self) -> None:
        original = FakeHttpResponse.getheader

        def minor_nine(response: FakeHttpResponse, name: str) -> str | None:
            if name == "X-Hyphae-Protocol-Minor":
                return "9"
            return original(response, name)

        FakeHttpConnection.requests = 0
        arguments = {
            "collection": 13,
            "batch": {
                "idempotency_id": 7,
                "documents": [{"object_id": 201, "text": "rust"}],
            },
        }
        with patch.object(FakeHttpResponse, "getheader", minor_nine):
            HttpTransport("https://example.test").execute(
                "embed_and_ingest", arguments, RequestOptions(request_id=17)
            )

        self.assertEqual(FakeHttpConnection.requests, 1)
        self.assertEqual(
            FakeHttpConnection.last_headers["X-Hyphae-Protocol-Minor"],
            "3,4,5,6,7,8,9",
        )
        self.assertEqual(
            int.from_bytes(FakeHttpConnection.last_body[12:14], "little"), 73
        )

    def test_concurrent_http_responses_decode_with_response_local_minor(self) -> None:
        import struct
        import threading
        from concurrent.futures import ThreadPoolExecutor

        high_reading = threading.Event()
        release_high = threading.Event()
        capabilities = bytearray(72)
        capabilities[:8] = b"HYPRSP01"
        struct.pack_into(
            "<IHHHHHH", capabilities, 8, len(capabilities), 1, 0, 1, 1, 2, 6
        )

        class ConcurrentResponse:
            status = 200

            def __init__(
                self, body: bytes, request_id: str, minor: int, block: bool
            ) -> None:
                self._body = body
                self._request_id = request_id
                self._minor = minor
                self._block = block

            def getheader(self, name: str) -> str | None:
                return {
                    "Content-Length": str(len(self._body)),
                    "Content-Type": PRODUCT_MEDIA_TYPE,
                    "X-Hyphae-Protocol-Minor": str(self._minor),
                    "X-Hyphae-Request-Id": self._request_id,
                }.get(name)

            def read(self, size: int = -1) -> bytes:
                if self._block:
                    high_reading.set()
                    if not release_high.wait(5):
                        raise TimeoutError("concurrent response was not released")
                return self._body[:size]

            def close(self) -> None:
                pass

        class ConcurrentConnection:
            def __init__(self, *args, **kwargs) -> None:  # type: ignore[no-untyped-def]
                del args, kwargs
                self.auto_open = 1
                self.sock = None
                self.request_id = ""

            def connect(self) -> None:
                pass

            def request(
                self, method: str, path: str, **kwargs
            ) -> None:  # type: ignore[no-untyped-def]
                del method, path
                self.request_id = kwargs["headers"]["X-Hyphae-Request-Id"]

            def getresponse(self) -> ConcurrentResponse:
                if self.request_id == "31":
                    return ConcurrentResponse(
                        _embed_and_ingest_response(replay=False),
                        self.request_id,
                        9,
                        True,
                    )
                return ConcurrentResponse(
                    bytes(capabilities), self.request_id, 8, False
                )

            def close(self) -> None:
                pass

        arguments = {
            "collection": 13,
            "batch": {
                "idempotency_id": 7,
                "documents": [{"object_id": 201, "text": "rust"}],
            },
        }
        with patch("http.client.HTTPSConnection", ConcurrentConnection):
            transport = HttpTransport("https://example.test")
            with ThreadPoolExecutor(max_workers=2) as executor:
                high = executor.submit(
                    transport.execute,
                    "embed_and_ingest",
                    arguments,
                    RequestOptions(request_id=31),
                )
                try:
                    self.assertTrue(high_reading.wait(5))
                    low = transport.execute(
                        "capabilities", {}, RequestOptions(request_id=32)
                    )
                    self.assertEqual(low.kind, "capabilities")
                    self.assertEqual(transport.negotiated_minor, 8)
                finally:
                    release_high.set()
                high_response = high.result(timeout=5)
        self.assertEqual(
            high_response.value["execution_profile"]["embedding_profile"], 17
        )

    @patch("http.client.HTTPSConnection", FakeHttpConnection)
    def test_http_client_preflights_minor_nine_after_server_downgrade(self) -> None:
        original = FakeHttpResponse.getheader

        def minor_eight(response: FakeHttpResponse, name: str) -> str | None:
            if name == "X-Hyphae-Protocol-Minor":
                return "8"
            return original(response, name)

        FakeHttpConnection.requests = 0
        with patch.object(FakeHttpResponse, "getheader", minor_eight):
            transport = HttpTransport("https://example.test")
            transport.execute("capabilities", {}, RequestOptions(request_id=17))
            with self.assertRaisesRegex(ClientError, "protocol minor"):
                transport.execute(
                    "embed_and_ingest",
                    {
                        "collection": 13,
                        "batch": {
                            "idempotency_id": 7,
                            "documents": [{"object_id": 201, "text": "rust"}],
                        },
                    },
                    RequestOptions(request_id=18),
                )
        self.assertEqual(transport.negotiated_minor, 8)
        self.assertEqual(FakeHttpConnection.requests, 1)

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
