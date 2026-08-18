# SPDX-License-Identifier: Apache-2.0

from __future__ import annotations

import asyncio
import threading
import unittest

from hyphae_sdk.v2 import (
    AsyncHyphaeClient,
    AsyncTransaction,
    ClientError,
    HttpTransport,
    HyphaeClient,
    LocalTransport,
    ProductError,
    ProductErrorFields,
    RequestOptions,
    Response,
)

API_KEY = "hyp1_" + "1" * 32 + "_" + "2" * 64


class RecordingTransport:
    def __init__(self) -> None:
        self.calls: list[tuple[str, dict[str, object], RequestOptions]] = []
        self.closed = 0
        self.aborted = 0
        self.thread_names: list[str] = []

    def execute(
        self,
        operation: str,
        arguments: dict[str, object],
        options: RequestOptions,
    ) -> Response:
        self.calls.append((operation, arguments, options))
        self.thread_names.append(threading.current_thread().name)
        kind = (
            "security_mutated"
            if operation == "security_legacy_bearer_revoke"
            else operation
        )
        return Response(kind, arguments, options.checked_request_id())

    def abort(self, cancellation: object | None = None) -> None:
        del cancellation
        self.aborted += 1

    def close(self) -> None:
        self.closed += 1


class BlockingTransport(RecordingTransport):
    def __init__(self) -> None:
        super().__init__()
        self.started = threading.Event()
        self.finished = threading.Event()
        self.release = threading.Event()
        self.options: RequestOptions | None = None

    def execute(
        self,
        operation: str,
        arguments: dict[str, object],
        options: RequestOptions,
    ) -> Response:
        del operation, arguments
        self.options = options
        self.started.set()
        self.release.wait(2)
        self.finished.set()
        raise ClientError("transport aborted")

    def abort(self, cancellation: object | None = None) -> None:
        super().abort(cancellation)
        if cancellation is None or (
            self.options is not None and cancellation is self.options.cancellation
        ):
            self.release.set()


class QueuedTransport(RecordingTransport):
    def __init__(self) -> None:
        super().__init__()
        self.first_started = threading.Event()
        self.release_first = threading.Event()
        self.active: RequestOptions | None = None

    def execute(
        self,
        operation: str,
        arguments: dict[str, object],
        options: RequestOptions,
    ) -> Response:
        if options.cancellation.cancelled:
            raise ProductError(
                ProductErrorFields(
                    code="cancelled",
                    category="cancelled",
                    retry="same-request",
                    message="cancelled",
                    request_id=options.checked_request_id(),
                )
            )
        self.calls.append((operation, arguments, options))
        self.active = options
        if operation == "first":
            self.first_started.set()
            self.release_first.wait(2)
            if options.cancellation.cancelled:
                raise ClientError("transport aborted")
        self.active = None
        return Response(operation, {}, options.checked_request_id())

    def abort(self, cancellation: object | None = None) -> None:
        self.aborted += 1
        if cancellation is None or (
            self.active is not None and cancellation is self.active.cancellation
        ):
            self.release_first.set()


class PagingTransport(RecordingTransport):
    def execute(
        self,
        operation: str,
        arguments: dict[str, object],
        options: RequestOptions,
    ) -> Response:
        self.calls.append((operation, arguments, options))
        cursor = arguments["cursor"]
        if cursor is None:
            value = {
                "items": [{"id": 1}],
                "next_cursor": {"authorization_epoch": 3, "after_id": 1},
            }
        else:
            value = {"items": [{"id": 2}], "next_cursor": None}
        return Response("security_principal_page", value, options.checked_request_id())


class RepeatingPagingTransport(RecordingTransport):
    def execute(
        self,
        operation: str,
        arguments: dict[str, object],
        options: RequestOptions,
    ) -> Response:
        self.calls.append((operation, arguments, options))
        cursor = {"authorization_epoch": 3, "after_id": 1}
        return Response(
            "security_principal_page",
            {"items": [{"id": len(self.calls)}], "next_cursor": cursor},
            options.checked_request_id(),
        )


class TransactionTransport(RecordingTransport):
    def __init__(
        self,
        *,
        outcome_unknown: bool = False,
        response_handle: int = 7,
    ) -> None:
        super().__init__()
        self.outcome_unknown = outcome_unknown
        self.response_handle = response_handle

    def execute(
        self,
        operation: str,
        arguments: dict[str, object],
        options: RequestOptions,
    ) -> Response:
        self.calls.append((operation, arguments, options))
        if operation == "transaction_begin":
            return Response(
                "explicit_transaction_status",
                {
                    "state": "active",
                    "handle": 7,
                    "read_csn": None,
                    "staged_operations": 0,
                    "durability": "strict",
                },
                options.checked_request_id(),
            )
        if operation.startswith("transaction_stage_"):
            return Response(
                "transaction_staged",
                {
                    "handle": self.response_handle,
                    "operation_ordinal": len(self.calls) - 1,
                    "changed": True,
                    "result": {"kind": operation.removeprefix("transaction_stage_")},
                },
                options.checked_request_id(),
            )
        if operation == "transaction_commit":
            if self.outcome_unknown:
                raise ProductError(
                    ProductErrorFields(
                        code="commit_outcome_unknown",
                        category="unavailable",
                        retry="unknown-commit",
                        message="commit outcome is unknown",
                        transaction_state="outcome-unknown",
                        transaction_id=41,
                    )
                )
            return Response(
                "transaction_committed",
                {
                    "handle": self.response_handle,
                    "staged_operations": 4,
                    "commit": {},
                },
                options.checked_request_id(),
            )
        if operation == "transaction_rollback":
            return Response(
                "transaction_rolled_back",
                {"handle": self.response_handle, "discarded_operations": 1},
                options.checked_request_id(),
            )
        raise AssertionError(f"unexpected operation: {operation}")


class BlockingCommitTransport(TransactionTransport):
    def __init__(self) -> None:
        super().__init__()
        self.commit_started = threading.Event()
        self.release_commit = threading.Event()
        self.active_options: RequestOptions | None = None

    def execute(
        self,
        operation: str,
        arguments: dict[str, object],
        options: RequestOptions,
    ) -> Response:
        if operation != "transaction_commit":
            return super().execute(operation, arguments, options)
        self.calls.append((operation, arguments, options))
        self.active_options = options
        self.commit_started.set()
        self.release_commit.wait(2)
        if options.cancellation.cancelled:
            raise ProductError(
                ProductErrorFields(
                    code="cancelled",
                    category="cancelled",
                    retry="same-request",
                    message="cancelled",
                    request_id=options.checked_request_id(),
                )
            )
        return super().execute(operation, arguments, options)

    def abort(self, cancellation: object | None = None) -> None:
        self.aborted += 1
        if cancellation is None or (
            self.active_options is not None
            and cancellation is self.active_options.cancellation
        ):
            self.release_commit.set()


class SyncLifecycleTests(unittest.TestCase):
    def test_client_and_http_close_are_idempotent_terminal_and_wipe_bearer(
        self,
    ) -> None:
        recording = RecordingTransport()
        client = HyphaeClient(recording)
        client.close()
        client.close()
        self.assertEqual(recording.closed, 1)
        with self.assertRaisesRegex(ClientError, "closed"):
            client.execute("capabilities")

        http = HttpTransport("https://example.test", bearer_token=API_KEY)
        retained = http._bearer_token
        self.assertIsInstance(retained, bytearray)
        http.close()
        http.close()
        self.assertEqual(retained, bytearray(len(API_KEY.encode())))
        with self.assertRaisesRegex(ClientError, "closed"):
            http.execute("capabilities", {}, RequestOptions(request_id=1))

        local = LocalTransport("unused", api_key=API_KEY)
        local_retained = local._api_key
        self.assertIsInstance(local_retained, bytearray)
        local.close()
        local.close()
        self.assertEqual(local_retained, bytearray(len(API_KEY.encode())))
        with self.assertRaisesRegex(ClientError, "closed"):
            local.execute("capabilities", {}, RequestOptions(request_id=2))


class AsyncLifecycleTests(unittest.IsolatedAsyncioTestCase):
    async def test_owner_legacy_bearer_revoke_is_typed(self) -> None:
        transport = RecordingTransport()
        async with AsyncHyphaeClient(transport) as client:
            response = await client.security_legacy_bearer_revoke(
                options=RequestOptions(request_id=2, idempotency_token=3)
            )
        self.assertEqual(response.kind, "security_mutated")
        self.assertEqual(transport.calls[0][0], "security_legacy_bearer_revoke")

    async def test_execute_uses_owned_single_worker_and_close_is_terminal(self) -> None:
        transport = RecordingTransport()
        client = AsyncHyphaeClient(transport)
        response = await client.execute(
            "capabilities", options=RequestOptions(request_id=3)
        )
        self.assertEqual(response.request_id, 3)
        self.assertEqual(len(set(transport.thread_names)), 1)
        self.assertTrue(transport.thread_names[0].startswith("hyphae-v2-async"))
        await client.aclose()
        await client.aclose()
        self.assertEqual(transport.closed, 1)
        with self.assertRaisesRegex(ClientError, "closed"):
            await client.execute("capabilities")

    async def test_pending_queue_is_finite_and_rejects_without_submission(self) -> None:
        transport = QueuedTransport()
        client = AsyncHyphaeClient(transport, max_pending=1)
        first = asyncio.create_task(client.execute("first"))
        await self._wait_for(transport.first_started)
        with self.assertRaisesRegex(ClientError, "queue is full"):
            await client.execute("rejected")
        self.assertEqual([call[0] for call in transport.calls], ["first"])
        transport.release_first.set()
        await first
        await client.aclose()

    async def test_cancellation_aborts_and_waits_for_the_worker(self) -> None:
        transport = BlockingTransport()
        client = AsyncHyphaeClient(transport)
        task = asyncio.create_task(client.execute("capabilities"))
        await self._wait_for(transport.started)
        task.cancel()
        with self.assertRaises(asyncio.CancelledError):
            await task
        self.assertTrue(transport.finished.is_set())
        self.assertEqual(transport.aborted, 1)
        self.assertIsNotNone(transport.options)
        self.assertTrue(transport.options.cancellation.cancelled)
        await client.aclose()

    async def test_cancelling_queued_request_does_not_abort_active_request(
        self,
    ) -> None:
        transport = QueuedTransport()
        client = AsyncHyphaeClient(transport, max_pending=2)
        first = asyncio.create_task(client.execute("first"))
        await self._wait_for(transport.first_started)
        queued = asyncio.create_task(client.execute("queued"))
        await asyncio.sleep(0)
        queued.cancel()
        with self.assertRaises(asyncio.CancelledError):
            await queued
        self.assertFalse(first.done())
        self.assertFalse(transport.release_first.is_set())
        replacement = asyncio.create_task(client.execute("replacement"))
        transport.release_first.set()
        self.assertEqual((await first).kind, "first")
        self.assertEqual((await replacement).kind, "replacement")
        self.assertEqual(
            [call[0] for call in transport.calls], ["first", "replacement"]
        )
        await client.aclose()

    async def test_cancelling_active_request_does_not_abort_next_request(self) -> None:
        transport = QueuedTransport()
        client = AsyncHyphaeClient(transport)
        first = asyncio.create_task(client.execute("first"))
        await self._wait_for(transport.first_started)
        queued = asyncio.create_task(client.execute("queued"))
        first.cancel()
        with self.assertRaises(asyncio.CancelledError):
            await first
        self.assertEqual((await queued).kind, "queued")
        self.assertEqual(
            [call[0] for call in transport.calls],
            ["first", "queued"],
        )
        await client.aclose()

    async def _wait_for(self, event: threading.Event) -> None:
        for _ in range(100):
            if event.is_set():
                return
            await asyncio.sleep(0.01)
        self.fail("worker did not reach the expected state")

    async def test_security_pages_do_not_prefetch(self) -> None:
        transport = PagingTransport()
        async with AsyncHyphaeClient(transport) as client:
            pages = client.security_principal_pages(limit=1)
            first = await anext(pages)
            self.assertEqual(first.value["items"], [{"id": 1}])
            self.assertEqual(len(transport.calls), 1)
            second = await anext(pages)
            self.assertEqual(second.value["items"], [{"id": 2}])
            self.assertEqual(len(transport.calls), 2)
            with self.assertRaises(StopAsyncIteration):
                await anext(pages)

    async def test_security_pages_reject_repeated_cursor_before_exposing_page(
        self,
    ) -> None:
        transport = RepeatingPagingTransport()
        async with AsyncHyphaeClient(transport) as client:
            pages = client.security_principal_pages(limit=1)
            first = await anext(pages)
            self.assertEqual(first.value["items"], [{"id": 1}])
            with self.assertRaisesRegex(ClientError, "repeated"):
                await anext(pages)
        self.assertEqual(len(transport.calls), 2)


class AsyncTransactionTests(unittest.IsolatedAsyncioTestCase):
    async def test_transaction_stages_every_engine_and_commits(self) -> None:
        transport = TransactionTransport()
        async with AsyncHyphaeClient(transport) as client:
            transaction = await AsyncTransaction.begin(client)
            await transaction.stage_sql("insert into t values (1)")
            await transaction.stage_structure({"kind": "delete", "key": b"k"})
            await transaction.stage_search({"kind": "delete", "object_id": 3})
            await transaction.stage_vector({"kind": "delete", "object_id": 5})
            committed = await transaction.commit()
        self.assertEqual(committed.kind, "transaction_committed")
        self.assertEqual(transaction.state, "committed")
        self.assertEqual(
            [call[0] for call in transport.calls],
            [
                "transaction_begin",
                "transaction_stage_sql",
                "transaction_stage_structure",
                "transaction_stage_search",
                "transaction_stage_vector",
                "transaction_commit",
            ],
        )

    async def test_context_rolls_back_active_transaction(self) -> None:
        transport = TransactionTransport()
        async with AsyncHyphaeClient(transport) as client:
            transaction = await client.begin_transaction()
            async with transaction:
                await transaction.stage_sql("delete from t")
        self.assertEqual(transaction.state, "rolled_back")
        self.assertEqual(transport.calls[-1][0], "transaction_rollback")

    async def test_outcome_unknown_is_terminal_and_never_rolled_back(self) -> None:
        transport = TransactionTransport(outcome_unknown=True)
        async with AsyncHyphaeClient(transport) as client:
            transaction = await client.begin_transaction()
            await transaction.stage_sql("delete from t")
            with self.assertRaises(ProductError):
                await transaction.commit()
            self.assertTrue(transaction.outcome_unknown)
            self.assertEqual(transaction.transaction_id, 41)
        self.assertNotIn("transaction_rollback", [call[0] for call in transport.calls])

    async def test_transaction_rejects_mismatched_response_handle(self) -> None:
        transport = TransactionTransport(response_handle=9)
        async with AsyncHyphaeClient(transport) as client:
            transaction = await client.begin_transaction()
            with self.assertRaisesRegex(ClientError, "mismatched"):
                await transaction.stage_sql("delete from t")
            self.assertEqual(transaction.state, "invalidated")

    async def test_cancelled_commit_is_unknown_without_rollback(self) -> None:
        transport = BlockingCommitTransport()
        async with AsyncHyphaeClient(transport) as client:
            transaction = await client.begin_transaction()
            commit = asyncio.create_task(transaction.commit())
            await self._wait_for(transport.commit_started)
            commit.cancel()
            with self.assertRaises(asyncio.CancelledError):
                await commit
            self.assertTrue(transaction.outcome_unknown)
        self.assertNotIn(
            "transaction_rollback",
            [call[0] for call in transport.calls],
        )

    async def test_aclose_during_commit_is_unknown_without_rollback(self) -> None:
        transport = BlockingCommitTransport()
        client = AsyncHyphaeClient(transport)
        transaction = await client.begin_transaction()
        commit = asyncio.create_task(transaction.commit())
        await self._wait_for(transport.commit_started)
        await client.aclose()
        with self.assertRaises(ProductError) as caught:
            await commit
        self.assertEqual(caught.exception.code, "cancelled")
        self.assertTrue(transaction.outcome_unknown)
        self.assertNotIn(
            "transaction_rollback",
            [call[0] for call in transport.calls],
        )

    @staticmethod
    async def _wait_for(event: threading.Event) -> None:
        for _ in range(100):
            if event.is_set():
                return
            await asyncio.sleep(0.01)
        raise AssertionError("commit did not reach the expected state")


if __name__ == "__main__":
    unittest.main()
