# SPDX-License-Identifier: GPL-3.0-only

from __future__ import annotations

import io
import json
import threading
import time
import unittest
from email.message import Message
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from unittest.mock import patch

from hyphae_sdk import HyphaeApiError, HyphaeClient, HyphaeClientError


class FakeResponse:
    def __init__(self, status: int, body: bytes, headers: dict[str, str]) -> None:
        self.status = status
        self._body = io.BytesIO(body)
        self.headers = Message()
        for name, value in headers.items():
            self.headers[name] = value

    def read(self, size: int = -1) -> bytes:
        return self._body.read(size)

    def close(self) -> None:
        pass


class SlowResponseHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    drip_seconds = 0.02
    redirected_requests = 0

    def do_GET(self) -> None:  # noqa: N802
        if self.path == "/v1/health/slow-header":
            body = b'{"status":"live"}'
            response = (
                b"HTTP/1.1 200 OK\r\n"
                b"Content-Type: application/json\r\n"
                b"Content-Length: 17\r\n"
                b"X-Request-Id: request-slow-header\r\n"
                b"\r\n"
                + body
            )
            self._write_slowly(response)
            return
        if self.path == "/v1/health/slow-body":
            body = b'{"status":"live"}' + (b" " * 64)
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.send_header("X-Request-Id", "request-slow-body")
            self.end_headers()
            self.wfile.flush()
            self._write_slowly(body)
            return
        if self.path == "/v1/health/slow-error":
            body = (
                b'{"code":"limit_exceeded","message":"slow",'
                b'"request_id":"request-slow-error"}'
            )
            self.send_response(422)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.send_header("X-Request-Id", "request-slow-error")
            self.end_headers()
            self.wfile.flush()
            self._write_slowly(body)
            return
        if self.path == "/v1/witnesses/1/abc":
            body = b"W" * 64
            self.send_response(200)
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Digest", "blake3=abc")
            self.send_header("X-Request-Id", "request-slow-witness")
            self.end_headers()
            self.wfile.flush()
            self._write_slowly(body)
            return
        self.send_error(404)

    def do_POST(self) -> None:  # noqa: N802
        declared = self.headers.get("Content-Length")
        if declared is not None and declared.isascii() and declared.isdigit():
            self.rfile.read(int(declared))
        if self.path == "/v1/kv/get":
            self.send_response(307)
            self.send_header("Content-Length", "0")
            self.send_header("Location", "/redirect-target")
            self.end_headers()
            return
        if self.path == "/redirect-target":
            type(self).redirected_requests += 1
            self.send_error(500)
            return
        self.send_error(404)

    def _write_slowly(self, body: bytes) -> None:
        try:
            for value in body:
                self.wfile.write(bytes((value,)))
                self.wfile.flush()
                time.sleep(self.drip_seconds)
        except OSError:
            pass

    def log_message(self, format: str, *args: object) -> None:
        del format, args


class LocalSlowServer:
    def __enter__(self) -> str:
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), SlowResponseHandler)
        self.server.daemon_threads = True
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        host, port = self.server.server_address
        return f"http://{host}:{port}"

    def __exit__(self, *args: object) -> None:
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=1.0)


class ClientTests(unittest.TestCase):
    deadline_seconds = 0.1
    deadline_ceiling_seconds = 0.6

    def assert_deadline(self, operation) -> None:  # type: ignore[no-untyped-def]
        started = time.monotonic()
        with self.assertRaisesRegex(
            HyphaeClientError, "request/response deadline elapsed"
        ):
            operation()
        self.assertLess(
            time.monotonic() - started,
            self.deadline_ceiling_seconds,
            "the absolute deadline did not interrupt progress-based I/O",
        )

    def test_rejects_non_origins_and_unsafe_secrets(self) -> None:
        with self.assertRaises(HyphaeClientError):
            HyphaeClient("file:///tmp/hyphae")
        with self.assertRaises(HyphaeClientError):
            HyphaeClient("https://example.test/prefix")
        with self.assertRaises(HyphaeClientError):
            HyphaeClient("https://example.test", bearer_token="bad\nsecret")

    def test_rejects_nonfinite_timeouts(self) -> None:
        for timeout in (float("nan"), float("inf"), float("-inf")):
            with self.subTest(timeout=timeout):
                with self.assertRaises(HyphaeClientError):
                    HyphaeClient("https://example.test", timeout_seconds=timeout)

    def test_rejects_redirects_before_replaying_bearer_requests(self) -> None:
        SlowResponseHandler.redirected_requests = 0
        with LocalSlowServer() as base_url:
            client = HyphaeClient(base_url, bearer_token="secret")
            with self.assertRaisesRegex(HyphaeClientError, "redirects are not allowed"):
                client.get({"key_hex": "61"})
        self.assertEqual(SlowResponseHandler.redirected_requests, 0)

    @patch("hyphae_sdk.client._DeadlineGuard")
    @patch("urllib.request.build_opener", side_effect=RuntimeError("opener failed"))
    def test_opener_construction_failure_closes_deadline_guard(
        self, _build_opener, guard_type
    ) -> None:  # type: ignore[no-untyped-def]
        with self.assertRaisesRegex(RuntimeError, "opener failed"):
            HyphaeClient("https://example.test").liveness()
        guard_type.return_value.close.assert_called_once_with()

    @patch("urllib.request.build_opener")
    def test_decodes_correlated_json(self, build_opener) -> None:  # type: ignore[no-untyped-def]
        build_opener.return_value.open.return_value = FakeResponse(
            200,
            json.dumps({"status": "live"}).encode(),
            {"Content-Type": "application/json", "X-Request-Id": "request-1"},
        )
        response = HyphaeClient("https://example.test").liveness()
        self.assertEqual(response.value, {"status": "live"})
        self.assertEqual(response.request_id, "request-1")

    @patch("urllib.request.build_opener")
    def test_exposes_stable_api_errors(self, build_opener) -> None:  # type: ignore[no-untyped-def]
        build_opener.return_value.open.return_value = FakeResponse(
            409,
            json.dumps(
                {
                    "code": "idempotency_conflict",
                    "message": "conflict",
                    "request_id": "request-2",
                }
            ).encode(),
            {"Content-Type": "application/json", "X-Request-Id": "request-2"},
        )
        with self.assertRaises(HyphaeApiError) as caught:
            HyphaeClient("https://example.test").put({"records": []})
        self.assertEqual(caught.exception.code, "idempotency_conflict")

    @patch("urllib.request.build_opener")
    def test_enforces_streaming_byte_bound(self, build_opener) -> None:  # type: ignore[no-untyped-def]
        build_opener.return_value.open.return_value = FakeResponse(
            200,
            b'{"status":"live"}',
            {
                "Content-Length": "1",
                "Content-Type": "application/json",
                "X-Request-Id": "request-3",
            },
        )
        with self.assertRaises(HyphaeClientError):
            HyphaeClient("https://example.test", response_bytes=4).liveness()

    @patch("urllib.request.build_opener")
    def test_preserves_large_integers_and_rejects_floats(self, build_opener) -> None:  # type: ignore[no-untyped-def]
        build_opener.return_value.open.return_value = FakeResponse(
            200,
            b'{"status":"live","sequence":9223372036854775807}',
            {"Content-Type": "application/json", "X-Request-Id": "request-4"},
        )
        response = HyphaeClient("https://example.test").liveness()
        self.assertEqual(response.value["sequence"], 9223372036854775807)  # type: ignore[typeddict-item]

        build_opener.return_value.open.return_value = FakeResponse(
            200,
            b'{"status":"live","invalid":1.5}',
            {"Content-Type": "application/json", "X-Request-Id": "request-5"},
        )
        with self.assertRaises(HyphaeClientError):
            HyphaeClient("https://example.test").liveness()

    def test_complete_deadline_rejects_slow_headers(self) -> None:
        with LocalSlowServer() as base_url:
            client = HyphaeClient(base_url, timeout_seconds=self.deadline_seconds)
            self.assert_deadline(
                lambda: client._json("v1/health/slow-header", False)
            )

    def test_complete_deadline_carries_remaining_time_into_body(self) -> None:
        with LocalSlowServer() as base_url:
            client = HyphaeClient(base_url, timeout_seconds=self.deadline_seconds)
            self.assert_deadline(lambda: client._json("v1/health/slow-body", False))

    def test_complete_deadline_covers_http_error_body(self) -> None:
        with LocalSlowServer() as base_url:
            client = HyphaeClient(base_url, timeout_seconds=self.deadline_seconds)
            self.assert_deadline(
                lambda: client._json("v1/health/slow-error", False)
            )

    def test_complete_deadline_covers_witness_body(self) -> None:
        proof = {
            "checkpoint_sequence": 1,
            "snapshot_digest": "abc",
            "witness": {
                "path": "/v1/witnesses/1/abc",
                "file_bytes": 64,
            },
        }
        with LocalSlowServer() as base_url:
            client = HyphaeClient(base_url, timeout_seconds=self.deadline_seconds)
            self.assert_deadline(lambda: client.download_witness(proof))  # type: ignore[arg-type]


if __name__ == "__main__":
    unittest.main()
