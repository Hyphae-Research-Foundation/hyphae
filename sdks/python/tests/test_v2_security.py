# SPDX-License-Identifier: Apache-2.0

from __future__ import annotations

import os
import socket
import struct
import tempfile
import threading
import unittest
from unittest.mock import patch

from hyphae_sdk.v2 import (
    ClientError,
    HyphaeClient,
    ProductError,
    RequestOptions,
    Response,
    SensitiveBytes,
)
from hyphae_sdk.v2.local import LocalTransport
from hyphae_sdk.v2.http import HttpTransport, PRODUCT_MEDIA_TYPE
from hyphae_sdk.v2.protocol import (
    API_KEY_AUTH_CAPABILITY,
    FRAME_HEADER_SIZE,
    FRAME_KINDS,
    G6_CAPABILITIES,
    PROTOCOL_MINOR,
    blake3,
    decode_frame,
    decode_product_request,
    decode_product_response,
    encode_authenticated_hello,
    encode_frame,
    encode_hello,
    encode_product_request,
    operation_required_minor,
)


API_KEY = "hyp1_" + "1" * 32 + "_" + "2" * 64


def _response(kind: int, body: bytes) -> bytes:
    return struct.pack("<8sIHH", b"HYPRSP01", 16 + len(body), kind, 0) + body


def _receipt() -> bytes:
    return (
        (29).to_bytes(16, "little")
        + struct.pack("<QQQ", 31, 37, 41)
        + bytes((43,)) * 32
        + struct.pack("<B7xQQ", 0, 1, 0)
    )


def _api_key_started_response(
    *,
    key_id: bytes = bytes((0x11,)) * 16,
    predecessor_key_id: bytes | None = None,
    authorization_epoch: int = 7,
    receipt: bytes | None = None,
    secret: bytes = API_KEY.encode(),
) -> bytes:
    predecessor = (
        b"\0" * 24
        if predecessor_key_id is None
        else struct.pack("<B7x", 1) + predecessor_key_id
    )
    body = (
        key_id
        + (1).to_bytes(16, "big")
        + predecessor
        + struct.pack("<Q", authorization_epoch)
        + (_receipt() if receipt is None else receipt)
        + struct.pack("<I", len(secret))
        + secret
    )
    return _response(43, body)


class SensitiveSecretTests(unittest.TestCase):
    def test_sensitive_bytes_are_redacted_and_contextually_zeroized(self) -> None:
        secret = SensitiveBytes(API_KEY.encode())
        self.assertNotIn(API_KEY, repr(secret))
        exposed = secret.expose()
        with secret:
            self.assertEqual(bytes(exposed), API_KEY.encode())
        self.assertEqual(exposed, bytearray(b"\0" * len(API_KEY)))
        with self.assertRaisesRegex(ClientError, "closed"):
            secret.expose()


def _welcome(minor: int, capabilities: int) -> bytes:
    return struct.pack(
        "<8sIHHQ16sIIIHHHHQQQQH",
        b"HYPWEL01",
        94,
        1,
        minor,
        capabilities,
        (7).to_bytes(16, "little"),
        16 * 1024 * 1024,
        64,
        64 * 1024,
        1,
        1,
        2,
        6,
        11,
        1024,
        4096,
        4096,
        0,
    )


def _end(payload: bytes) -> bytes:
    return struct.pack("<8sIB3xQ32s", b"HYPEND01", 56, 1, len(payload), blake3(payload))


def _authorization_denied() -> bytes:
    code = b"authorization_denied"
    message = b"native product operation is not authorized"
    return (
        struct.pack(
            "<8sIBBBBBHB",
            b"HYPERR01",
            20 + len(code) + len(message),
            6,
            0,
            0,
            0,
            len(code),
            len(message),
            0,
        )
        + code
        + message
    )


class SecurityProtocolTests(unittest.TestCase):
    def test_legacy_hello_default_remains_minor_zero_and_authenticated_hello_is_bounded(self) -> None:
        legacy = encode_hello()
        current = encode_hello(maximum_minor=PROTOCOL_MINOR)
        authenticated = encode_authenticated_hello(API_KEY)

        self.assertEqual(struct.unpack_from("<H", legacy, 18)[0], 0)
        self.assertEqual(legacy[:18], current[:18])
        self.assertEqual(struct.unpack_from("<H", current, 18)[0], 4)
        self.assertEqual(authenticated[49], 1)
        self.assertEqual(struct.unpack_from("<H", authenticated, 50)[0], 102)
        self.assertEqual(struct.unpack_from("<Q", authenticated, 20)[0], 0xFF)
        self.assertEqual(authenticated[-102:], API_KEY.encode())
        self.assertEqual(
            blake3(
                encode_authenticated_hello(
                    API_KEY, "hyphae-client", maximum_minor=0
                )
            ).hex(),
            "e11f972e4e670beb1523f1e56a034c3dc85af861cd88a2761f51cb590c9ea56b",
        )
        malformed = "x" * 102
        self.assertEqual(encode_authenticated_hello(malformed)[-102:], malformed.encode())
        utf8_candidate = "x" * 100 + "é"
        self.assertEqual(
            encode_authenticated_hello(utf8_candidate)[-102:],
            utf8_candidate.encode(),
        )
        utf8_transport = LocalTransport("unused", api_key=utf8_candidate)
        self.assertEqual(utf8_transport._api_key, bytearray(utf8_candidate, "utf-8"))
        utf8_transport.close()
        with self.assertRaisesRegex(ClientError, "credential is invalid"):
            encode_authenticated_hello("x" * 101)
        with self.assertRaisesRegex(ClientError, "credential is invalid"):
            encode_authenticated_hello(b"x" * 101 + b"\xff")

    def test_security_read_and_write_requests_round_trip_with_append_only_tags(self) -> None:
        cases = (
            ("security_status", {}, False),
            ("security_principal_list", {"cursor": None, "limit": 1}, False),
            ("security_role_list", {"cursor": None, "limit": 1}, False),
            ("security_assignment_list", {"cursor": None, "limit": 1}, False),
            ("security_key_list", {"cursor": None, "limit": 1}, False),
            ("security_audit_read", {"cursor": None, "limit": 1}, False),
            ("security_principal_create", {"display_name": "analytics"}, True),
            (
                "security_principal_set_enabled",
                {"principal_id": 1, "enabled": True},
                True,
            ),
            (
                "security_custom_role_create",
                {
                    "display_name": "analytics reader",
                    "grants": [
                        {
                            "permission": "data.read",
                            "scope": {"kind": "catalog_subtree", "object_id": 9},
                        }
                    ],
                },
                True,
            ),
            (
                "security_built_in_assignment_create",
                {"principal_id": 1, "role": "owner", "scope": {"kind": "instance"}},
                True,
            ),
            (
                "security_custom_assignment_create",
                {"principal_id": 1, "role_id": 2},
                True,
            ),
            ("security_assignment_revoke", {"assignment_id": 3}, True),
            ("security_legacy_bearer_revoke", {}, True),
        )
        for offset, (operation, arguments, mutation) in enumerate(cases):
            with self.subTest(operation=operation):
                options = RequestOptions(idempotency_token=17 if mutation else None)
                wire = encode_product_request(operation, arguments, options, negotiated_minor=3)
                expected_tag = 70 if operation == "security_legacy_bearer_revoke" else 42 + offset
                self.assertEqual(struct.unpack_from("<H", wire, 12)[0], expected_tag)
                self.assertEqual(decode_product_request(wire)[:2], (operation, arguments))
                self.assertNotIn(b"hyp1_", wire)

        write_transcript = b"".join(
            encode_product_request(
                operation,
                arguments,
                RequestOptions(logical_time_micros=10, idempotency_token=17),
                negotiated_minor=3,
            )
            for operation, arguments, mutation in cases
            if mutation and operation != "security_legacy_bearer_revoke"
        )
        self.assertEqual(
            blake3(write_transcript).hex(),
            "94b3aade7ed46f3608da3b30a5516db04a7de0e9013b33ebb3752162f17f1afc",
        )

    def test_security_minor_and_idempotency_fail_closed_before_transport(self) -> None:
        with self.assertRaisesRegex(ClientError, "negotiated protocol minor"):
            encode_product_request(
                "security_status", {}, RequestOptions(), negotiated_minor=0
            )
        with self.assertRaisesRegex(ClientError, "negotiated protocol minor"):
            encode_product_request(
                "security_principal_create",
                {"display_name": "analytics"},
                RequestOptions(idempotency_token=1),
                negotiated_minor=1,
            )
        with self.assertRaisesRegex(ClientError, "idempotency_token"):
            encode_product_request(
                "security_principal_create",
                {"display_name": "analytics"},
                RequestOptions(),
            )
        with self.assertRaisesRegex(ClientError, "negotiated protocol minor"):
            encode_product_request(
                "proof_generate",
                {
                    "operation": "security_status",
                    "arguments": {},
                    "limits": {},
                },
                RequestOptions(),
                negotiated_minor=0,
            )
        lifecycle = {
            "security_api_key_issue_self_start",
            "security_api_key_issue_start",
            "security_api_key_issue_self_activate",
            "security_api_key_issue_activate",
            "security_api_key_rotate_self_start",
            "security_api_key_rotate_start",
            "security_api_key_rotate_self_activate",
            "security_api_key_rotate_activate",
            "security_api_key_issue_self_abort",
            "security_api_key_issue_abort",
            "security_api_key_rotate_self_abort",
            "security_api_key_rotate_abort",
            "security_api_key_revoke_self",
            "security_api_key_revoke",
        }
        self.assertTrue(all(operation_required_minor(operation) == 3 for operation in lifecycle))
        self.assertEqual(operation_required_minor("security_legacy_bearer_revoke"), 3)
        # Every currently expressible search body is minor-0 content; the
        # content walk exists so future operators, typed doc values, and
        # fusion methods raise the requirement without new operations.
        self.assertEqual(
            operation_required_minor(
                "search_collection",
                {
                    "filter": {
                        "kind": "not",
                        "filter": {
                            "kind": "all",
                            "filters": [
                                {"kind": "match_all"},
                                {
                                    "kind": "compare",
                                    "field": "price",
                                    "operator": "less_or_equal",
                                    "value": 40,
                                },
                            ],
                        },
                    }
                },
            ),
            0,
        )
        self.assertEqual(
            operation_required_minor(
                "search_ingest",
                {
                    "documents": [
                        {
                            "object_id": 1,
                            "text": "rust",
                            "doc_values": {
                                "flag": True,
                                "rank": 3,
                                "name": "a",
                                "blob": b"x",
                            },
                        }
                    ]
                },
            ),
            0,
        )
        self.assertEqual(
            operation_required_minor(
                "proof_generate",
                {
                    "operation": "search_collection",
                    "arguments": {"filter": {"kind": "match_all"}},
                },
            ),
            0,
        )
        memory_revoke = bytearray(
            encode_product_request(
                "security_legacy_bearer_revoke",
                {},
                RequestOptions(idempotency_token=1),
                negotiated_minor=3,
            )
        )
        memory_revoke[88] = 2
        with self.assertRaisesRegex(ClientError, "strict durability"):
            decode_product_request(bytes(memory_revoke))
        for operation in (*lifecycle, "security_legacy_bearer_revoke"):
            with self.subTest(operation=operation):
                with self.assertRaisesRegex(ClientError, "strict durability"):
                    encode_product_request(
                        operation,
                        {} if operation == "security_legacy_bearer_revoke" else (
                            {"key_id": bytes((1,)) * 16}
                            if operation in {
                                "security_api_key_issue_self_abort",
                                "security_api_key_issue_abort",
                                "security_api_key_revoke_self",
                                "security_api_key_revoke",
                            }
                            else {"successor_key_id": bytes((1,)) * 16}
                        ),
                        RequestOptions(idempotency_token=1, durability="memory"),
                        negotiated_minor=3,
                    )

    def test_security_status_page_and_mutation_responses_decode_without_secrets(self) -> None:
        status = _response(
            32,
            struct.pack("<B7xQQQQQQQQ", 1, 7, 1, 2, 3, 4, 5, 4, 7),
        )
        decoded = decode_product_response(status, 17, negotiated_minor=2)
        self.assertEqual(decoded.kind, "security_status")
        self.assertEqual(decoded.value["authorization_epoch"], 7)

        page_body = (
            struct.pack("<QI4x", 7, 1)
            + struct.pack("<B7xQB7x", 1, 7, 1)
            + (1).to_bytes(16, "big")
            + (1).to_bytes(16, "big")
            + struct.pack("<B7xI", 1, len(b"analytics"))
            + b"analytics"
        )
        page = decode_product_response(_response(33, page_body), 18, negotiated_minor=2)
        self.assertEqual(page.kind, "security_principal_page")
        self.assertEqual(page.value["items"][0]["display_name"], "analytics")

        mutation_body = (1).to_bytes(16, "big") + struct.pack("<Q", 7) + _receipt()
        mutation = decode_product_response(
            _response(38, mutation_body), 19, negotiated_minor=2
        )
        self.assertEqual(mutation.kind, "security_principal_mutated")
        self.assertNotIn("secret", repr(mutation.value).lower())
        with self.assertRaisesRegex(ClientError, "negotiated protocol minor"):
            decode_product_response(_response(38, mutation_body), 19, negotiated_minor=1)

    def test_every_security_page_and_mutation_response_kind_is_covered(self) -> None:
        empty_metadata_page = struct.pack("<QI4x", 7, 0) + b"\0" * 40
        expected_pages = {
            33: "security_principal_page",
            34: "security_role_page",
            35: "security_assignment_page",
            36: "security_key_page",
        }
        for kind, expected in expected_pages.items():
            with self.subTest(kind=kind):
                decoded = decode_product_response(
                    _response(kind, empty_metadata_page), 20, negotiated_minor=2
                )
                self.assertEqual(decoded.kind, expected)
                self.assertEqual(decoded.value["items"], [])
        audit = decode_product_response(
            _response(37, struct.pack("<I4x", 0) + b"\0" * 24),
            21,
            negotiated_minor=2,
        )
        self.assertEqual(audit.value, {"events": [], "next_cursor": None})

        identity_names = ("principal_id", "role_id", "assignment_id")
        for offset, identity_name in enumerate(identity_names):
            decoded = decode_product_response(
                _response(38 + offset, (offset + 1).to_bytes(16, "big") + struct.pack("<Q", 7) + _receipt()),
                22 + offset,
                negotiated_minor=2,
            )
            self.assertEqual(decoded.value[identity_name], offset + 1)
        decoded = decode_product_response(
            _response(41, struct.pack("<Q", 7) + _receipt()),
            25,
            negotiated_minor=2,
        )
        self.assertEqual(decoded.value["authorization_epoch"], 7)

    def test_security_response_truncation_trailing_and_noncanonical_cursor_fail_closed(self) -> None:
        page = _response(33, struct.pack("<QI4x", 7, 0) + b"\0" * 40)
        for prefix in range(16, len(page)):
            with self.subTest(prefix=prefix):
                with self.assertRaises(ClientError):
                    decode_product_response(page[:prefix], 26, negotiated_minor=2)
        trailing = bytearray(page + b"\0")
        struct.pack_into("<I", trailing, 8, len(trailing))
        with self.assertRaisesRegex(ClientError, "trailing"):
            decode_product_response(bytes(trailing), 26, negotiated_minor=2)
        bad_cursor = bytearray(page)
        bad_cursor[32] = 1
        with self.assertRaises(ClientError):
            decode_product_response(bytes(bad_cursor), 26, negotiated_minor=2)

    def test_security_audit_decodes_legacy_bearer_target(self) -> None:
        event = (
            (1).to_bytes(16, "big")
            + struct.pack("<QB7x", 2, 0)
            + b"\0" * 32
            + struct.pack("<BB6xI4x", 10, 0, 1)
            + struct.pack("<B7x", 4)
            + b"\0" * 16
            + struct.pack("<I4x", 0)
        )
        page = struct.pack("<I4x", 1) + b"\0" * 24 + event
        decoded = decode_product_response(
            _response(37, page), 27, negotiated_minor=3
        )
        self.assertEqual(decoded.value["events"][0]["targets"], [
            {"kind": "legacy_bearer"}
        ])

    def test_api_key_started_decodes_only_canonical_matching_secret(self) -> None:
        response = decode_product_response(
            _api_key_started_response(predecessor_key_id=bytes((3,)) * 16),
            28,
            negotiated_minor=3,
        )
        self.assertEqual(response.kind, "security_api_key_started")
        self.assertEqual(response.value["key_id"], bytes((0x11,)) * 16)
        self.assertEqual(response.value["predecessor_key_id"], bytes((3,)) * 16)
        secret = response.value["secret"]
        self.assertIsInstance(secret, SensitiveBytes)
        with secret:
            self.assertEqual(bytes(secret.expose()), API_KEY.encode())

        malformed = (
            b"hyp1_" + b"1" * 32 + b"_" + b"A" * 64,
            b"hyp2_" + b"1" * 32 + b"_" + b"2" * 64,
            API_KEY.encode()[:-1],
            API_KEY.encode() + b"0",
            b"\xff" + API_KEY.encode()[1:],
        )
        for candidate in malformed:
            with self.subTest(candidate=candidate[:8]):
                with self.assertRaisesRegex(ClientError, "noncanonical"):
                    decode_product_response(
                        _api_key_started_response(secret=candidate),
                        29,
                        negotiated_minor=3,
                    )

        mismatch = b"hyp1_" + b"2" * 32 + b"_" + b"3" * 64
        with self.assertRaisesRegex(ClientError, "differs from its receipt"):
            decode_product_response(
                _api_key_started_response(secret=mismatch),
                30,
                negotiated_minor=3,
            )

    def test_api_key_started_rejects_zero_ids_epoch_and_invalid_commit_receipts(self) -> None:
        cases = {
            "zero key": _api_key_started_response(key_id=b"\0" * 16),
            "zero predecessor": _api_key_started_response(
                predecessor_key_id=b"\0" * 16
            ),
            "zero epoch": _api_key_started_response(authorization_epoch=0),
        }
        receipt_offsets = {
            "transaction": (0, 16),
            "commit csn": (16, 8),
            "catalog version": (24, 8),
            "commit lsn": (32, 8),
            "WAL digest": (40, 32),
            "cohort size": (80, 8),
        }
        for name, (offset, length) in receipt_offsets.items():
            receipt = bytearray(_receipt())
            receipt[offset:offset + length] = b"\0" * length
            cases[name] = _api_key_started_response(receipt=bytes(receipt))
        invalid_durability = bytearray(_receipt())
        invalid_durability[72] = 3
        cases["durability"] = _api_key_started_response(
            receipt=bytes(invalid_durability)
        )
        invalid_position = bytearray(_receipt())
        invalid_position[88:96] = struct.pack("<Q", 1)
        cases["cohort position"] = _api_key_started_response(
            receipt=bytes(invalid_position)
        )

        for name, encoded in cases.items():
            with self.subTest(name=name):
                with self.assertRaises(ClientError):
                    decode_product_response(encoded, 31, negotiated_minor=3)

    def test_api_key_started_rejects_every_truncation_and_trailing_bytes(self) -> None:
        encoded = _api_key_started_response()
        for prefix in range(len(encoded)):
            with self.subTest(prefix=prefix):
                with self.assertRaises(ClientError):
                    decode_product_response(
                        encoded[:prefix], 32, negotiated_minor=3
                    )
        trailing = bytearray(encoded + b"\0")
        struct.pack_into("<I", trailing, 8, len(trailing))
        with self.assertRaisesRegex(ClientError, "trailing"):
            decode_product_response(bytes(trailing), 32, negotiated_minor=3)

    def test_api_key_activated_rejects_zero_key_ids(self) -> None:
        body = b"\0" * 16 + b"\0" * 24 + b"\0" * 16 + struct.pack("<Q", 7) + _receipt()
        with self.assertRaisesRegex(ClientError, "identity is zero"):
            decode_product_response(_response(44, body), 33, negotiated_minor=3)


class _HttpResponse:
    status = 200

    def __init__(self, body: bytes) -> None:
        self._body = body

    def getheader(self, name: str) -> str | None:
        return {
            "Content-Length": str(len(self._body)),
            "Content-Type": PRODUCT_MEDIA_TYPE,
            "X-Hyphae-Protocol-Minor": "3",
            "X-Hyphae-Request-Id": "27",
        }.get(name)

    def read(self, size: int = -1) -> bytes:
        return self._body[:size]


class _HttpConnection:
    body = b""
    headers: dict[str, str] = {}
    path = ""

    def __init__(self, *args, **kwargs) -> None:  # type: ignore[no-untyped-def]
        del args, kwargs
        self.sock = None
        self.auto_open = 1

    def connect(self) -> None:
        pass

    def request(self, method: str, path: str, **kwargs) -> None:  # type: ignore[no-untyped-def]
        if method != "POST":
            raise AssertionError("unexpected HTTP request")
        type(self).path = path
        type(self).body = kwargs["body"]
        type(self).headers = kwargs["headers"]

    def getresponse(self) -> _HttpResponse:
        body = (1).to_bytes(16, "big") + struct.pack("<Q", 7) + _receipt()
        return _HttpResponse(_response(38, body))

    def close(self) -> None:
        pass


class ManagedHttpTests(unittest.TestCase):
    def test_durable_bearer_requires_tls_outside_canonical_loopback(self) -> None:
        for origin in (
            "http://127.0.0.1:8787",
            "http://127.0.0.2:8787",
            "http://[::1]:8787",
            "http://localhost:8787",
            "http://LOCALHOST:8787",
            "https://example.test",
        ):
            with self.subTest(origin=origin):
                HttpTransport(origin, bearer_token=API_KEY)

        for origin in (
            "http://example.test",
            "http://localhost.example",
            "http://192.168.1.10",
            "http://[::ffff:127.0.0.1]",
        ):
            with self.subTest(origin=origin):
                with self.assertRaisesRegex(
                    ClientError,
                    "durable API keys require HTTPS outside loopback",
                ):
                    HttpTransport(origin, bearer_token=API_KEY)

        HttpTransport("http://example.test")

    @patch("http.client.HTTPSConnection", _HttpConnection)
    def test_http_security_mutation_uses_same_context_codec_and_redacts_repr(self) -> None:
        transport = HttpTransport("https://example.test", bearer_token=API_KEY)
        response = transport.execute(
            "security_principal_create",
            {"display_name": "analytics"},
            RequestOptions(request_id=27, idempotency_token=31),
        )
        operation, arguments, options = decode_product_request(_HttpConnection.body)
        self.assertEqual((operation, arguments), (
            "security_principal_create",
            {"display_name": "analytics"},
        ))
        self.assertEqual(options.idempotency_token, 31)
        self.assertNotIn(b"hyp1_", _HttpConnection.body)
        self.assertEqual(_HttpConnection.headers["Authorization"], f"Bearer {API_KEY}")
        self.assertNotIn(API_KEY, repr(transport))
        self.assertEqual(response.kind, "security_principal_mutated")

    @patch("http.client.HTTPSConnection", _HttpConnection)
    def test_http_routes_every_api_key_lifecycle_operation_to_dedicated_family(self) -> None:
        transport = HttpTransport("https://example.test", bearer_token=API_KEY)
        operations = (
            ("security_api_key_issue_self_start", {"principal_id": 1, "label": "issue", "roles": [], "custom_roles": [], "permission_ceiling": ["credential.self_manage"], "scope_ceiling": [{"kind": "instance"}], "expires_at_micros": None}),
            ("security_api_key_issue_start", {"principal_id": 1, "label": "issue", "roles": [], "custom_roles": [], "permission_ceiling": ["credential.self_manage"], "scope_ceiling": [{"kind": "instance"}], "expires_at_micros": None}),
            ("security_api_key_issue_self_activate", {"key_id": bytes((1,)) * 16, "confirmation_digest": bytes((2,)) * 32}),
            ("security_api_key_issue_activate", {"key_id": bytes((1,)) * 16, "confirmation_digest": bytes((2,)) * 32}),
            ("security_api_key_rotate_self_start", {"predecessor_key_id": bytes((1,)) * 16, "label": "rotate", "overlap_seconds": 0, "expires_at_micros": None}),
            ("security_api_key_rotate_start", {"predecessor_key_id": bytes((1,)) * 16, "label": "rotate", "overlap_seconds": 0, "expires_at_micros": None}),
            ("security_api_key_rotate_self_activate", {"successor_key_id": bytes((1,)) * 16, "confirmation_digest": bytes((2,)) * 32}),
            ("security_api_key_rotate_activate", {"successor_key_id": bytes((1,)) * 16, "confirmation_digest": bytes((2,)) * 32}),
            ("security_api_key_issue_self_abort", {"key_id": bytes((1,)) * 16}),
            ("security_api_key_issue_abort", {"key_id": bytes((1,)) * 16}),
            ("security_api_key_rotate_self_abort", {"successor_key_id": bytes((1,)) * 16}),
            ("security_api_key_rotate_abort", {"successor_key_id": bytes((1,)) * 16}),
            ("security_api_key_revoke_self", {"key_id": bytes((1,)) * 16}),
            ("security_api_key_revoke", {"key_id": bytes((1,)) * 16}),
            ("security_legacy_bearer_revoke", {}),
        )
        for offset, (operation, arguments) in enumerate(operations):
            with self.subTest(operation=operation):
                with self.assertRaises(ClientError):
                    transport.execute(
                        operation,
                        arguments,
                        RequestOptions(request_id=100 + offset, idempotency_token=100 + offset),
                    )
                self.assertEqual(_HttpConnection.path, "/v2/security/keys")
                self.assertEqual(_HttpConnection.headers["X-Hyphae-Protocol-Minor"], "3,4")


@unittest.skipIf(os.name == "nt", "AF_UNIX live parity runs on POSIX")
class LocalLiveTests(unittest.TestCase):
    def test_unmanaged_local_offers_current_lifecycle_minor_and_accepts_downgrade(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            endpoint = os.path.join(directory, "hyphae.sock")
            ready = threading.Event()
            observed: dict[str, bytes] = {}

            def serve() -> None:
                listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                listener.bind(endpoint)
                listener.listen(1)
                ready.set()
                connection, _ = listener.accept()
                try:
                    hello_frame = _read_frame(connection)
                    observed["hello"] = hello_frame.payload
                    connection.sendall(
                        encode_frame(
                            FRAME_KINDS["welcome"],
                            0,
                            hello_frame.request_id,
                            _welcome(0, G6_CAPABILITIES),
                        )
                    )
                finally:
                    connection.close()
                    listener.close()

            thread = threading.Thread(target=serve)
            thread.start()
            self.assertTrue(ready.wait(2))
            transport = LocalTransport(endpoint)
            transport._connect(17)
            self.assertEqual(transport.negotiated_minor, 0)
            transport.close()
            thread.join(2)
            self.assertFalse(thread.is_alive())
            self.assertEqual(observed["hello"], encode_hello(maximum_minor=PROTOCOL_MINOR))

    def test_malformed_and_wrong_credentials_reach_the_owner_and_deny_uniformly(self) -> None:
        candidates = ("x" * 102, API_KEY)
        with tempfile.TemporaryDirectory() as directory:
            endpoint = os.path.join(directory, "hyphae.sock")
            ready = threading.Event()
            observed: list[bytes] = []

            def serve() -> None:
                listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                listener.bind(endpoint)
                listener.listen(len(candidates))
                ready.set()
                try:
                    for _ in candidates:
                        connection, _ = listener.accept()
                        try:
                            hello_frame = _read_frame(connection)
                            observed.append(hello_frame.payload[-102:])
                            connection.sendall(
                                encode_frame(
                                    FRAME_KINDS["failure"],
                                    0,
                                    hello_frame.request_id,
                                    _authorization_denied(),
                                )
                            )
                        finally:
                            connection.close()
                finally:
                    listener.close()

            thread = threading.Thread(target=serve)
            thread.start()
            self.assertTrue(ready.wait(2))
            denials = []
            for offset, candidate in enumerate(candidates):
                with LocalTransport(endpoint, api_key=candidate) as transport:
                    with self.assertRaises(ProductError) as caught:
                        transport.execute(
                            "security_status",
                            {},
                            RequestOptions(request_id=31 + offset),
                        )
                    denials.append(caught.exception.fields)
            thread.join(2)
            self.assertFalse(thread.is_alive())
            self.assertEqual(observed, [candidate.encode() for candidate in candidates])
            self.assertEqual(denials[0], denials[1])
            self.assertEqual(denials[0].code, "authorization_denied")

    def test_managed_local_negotiates_minor_three_and_executes_security_status(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            endpoint = os.path.join(directory, "hyphae.sock")
            ready = threading.Event()
            observed: dict[str, object] = {}

            def serve() -> None:
                listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                listener.bind(endpoint)
                listener.listen(1)
                ready.set()
                connection, _ = listener.accept()
                try:
                    hello_frame = _read_frame(connection)
                    observed["hello"] = hello_frame.payload
                    connection.sendall(
                        encode_frame(
                            FRAME_KINDS["welcome"],
                            0,
                            hello_frame.request_id,
                            _welcome(3, G6_CAPABILITIES | API_KEY_AUTH_CAPABILITY),
                        )
                    )
                    request_frame = _read_frame(connection)
                    observed["request"] = decode_product_request(request_frame.payload)
                    payload = _response(
                        32,
                        struct.pack("<B7xQQQQQQQQ", 1, 7, 1, 1, 0, 0, 1, 0, 1),
                    )
                    connection.sendall(
                        encode_frame(
                            FRAME_KINDS["data"],
                            request_frame.stream_id,
                            request_frame.request_id,
                            payload,
                        )
                    )
                    connection.sendall(
                        encode_frame(
                            FRAME_KINDS["end"],
                            request_frame.stream_id,
                            request_frame.request_id,
                            _end(payload),
                        )
                    )
                finally:
                    connection.close()
                    listener.close()

            thread = threading.Thread(target=serve)
            thread.start()
            self.assertTrue(ready.wait(2))
            transport = LocalTransport(endpoint, api_key=API_KEY)
            retained_credential = transport._api_key
            with transport:
                response = transport.execute(
                    "security_status", {}, RequestOptions(request_id=17)
                )
                self.assertEqual(response.kind, "security_status")
                self.assertEqual(transport.negotiated_minor, 3)
                self.assertNotIn(API_KEY, repr(transport))
            thread.join(2)
            self.assertFalse(thread.is_alive())
            self.assertEqual(retained_credential, bytearray(102))
            hello = observed["hello"]
            self.assertIsInstance(hello, bytes)
            self.assertEqual(hello[-102:], API_KEY.encode())  # type: ignore[index]
            self.assertEqual(observed["request"][0], "security_status")  # type: ignore[index]


def _read_frame(connection: socket.socket):  # type: ignore[no-untyped-def]
    header = _read_exact(connection, FRAME_HEADER_SIZE)
    length = struct.unpack_from("<I", header, 24)[0]
    return decode_frame(header + _read_exact(connection, length))


def _read_exact(connection: socket.socket, length: int) -> bytes:
    output = bytearray()
    while len(output) < length:
        chunk = connection.recv(length - len(output))
        if not chunk:
            raise AssertionError("socket closed before frame completed")
        output.extend(chunk)
    return bytes(output)


class SecurityClientTests(unittest.TestCase):
    def test_typed_security_methods_preserve_context_and_require_tokens(self) -> None:
        class Capture:
            def __init__(self) -> None:
                self.calls: list[tuple[str, dict[str, object], RequestOptions]] = []

            def execute(
                self,
                operation: str,
                arguments: dict[str, object],
                options: RequestOptions,
            ):
                self.calls.append((operation, arguments, options))
                expected = {
                    "security_principal_list": "security_principal_page",
                    "security_role_list": "security_role_page",
                    "security_assignment_list": "security_assignment_page",
                    "security_key_list": "security_key_page",
                    "security_audit_read": "security_audit_page",
                    "security_principal_create": "security_principal_mutated",
                    "security_principal_set_enabled": "security_mutated",
                    "security_custom_role_create": "security_custom_role_mutated",
                    "security_built_in_assignment_create": "security_assignment_mutated",
                    "security_custom_assignment_create": "security_assignment_mutated",
                    "security_assignment_revoke": "security_mutated",
                    "security_legacy_bearer_revoke": "security_mutated",
                }.get(operation, operation)
                return Response(expected, {}, options.checked_request_id())

        transport = Capture()
        client = HyphaeClient(transport)
        read_options = RequestOptions(request_id=20)
        write_options = RequestOptions(request_id=21, idempotency_token=31)
        client.security_status(options=read_options)
        client.security_principal_list(limit=7, options=read_options)
        client.security_role_list(limit=7, options=read_options)
        client.security_assignment_list(limit=7, options=read_options)
        client.security_key_list(limit=7, options=read_options)
        client.security_audit_read(limit=7, options=read_options)
        client.security_principal_create("analytics", options=write_options)
        client.security_principal_set_enabled(1, True, options=write_options)
        client.security_custom_role_create(
            "analytics reader",
            [{"permission": "data.read", "scope": {"kind": "instance"}}],
            options=write_options,
        )
        client.security_built_in_assignment_create(
            1, "reader", {"kind": "instance"}, options=write_options
        )
        client.security_custom_assignment_create(1, 2, options=write_options)
        client.security_assignment_revoke(3, options=write_options)
        client.security_legacy_bearer_revoke(options=write_options)
        self.assertEqual([call[0] for call in transport.calls], [
            "security_status",
            "security_principal_list",
            "security_role_list",
            "security_assignment_list",
            "security_key_list",
            "security_audit_read",
            "security_principal_create",
            "security_principal_set_enabled",
            "security_custom_role_create",
            "security_built_in_assignment_create",
            "security_custom_assignment_create",
            "security_assignment_revoke",
            "security_legacy_bearer_revoke",
        ])
        self.assertTrue(all(call[2].idempotency_token == 31 for call in transport.calls[6:]))
        with self.assertRaisesRegex(ClientError, "idempotency_token"):
            client.security_principal_create("analytics", options=RequestOptions())


if __name__ == "__main__":
    unittest.main()
