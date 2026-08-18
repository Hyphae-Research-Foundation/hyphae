# SPDX-License-Identifier: Apache-2.0
"""Owned-executor asynchronous lifecycle for the synchronous Native v2 core."""

from __future__ import annotations

import asyncio
import json
from collections.abc import AsyncIterator
from concurrent.futures import Future, ThreadPoolExecutor
from functools import partial
from typing import Any, TypeAlias, TypeVar

from .client import AbortableTransport, HyphaeClient
from .http import HttpTransport
from .local import LocalTransport
from .models import (
    CancellationToken,
    ClientError,
    ProductError,
    RequestOptions,
    Response,
)

_WorkerOutcome: TypeAlias = tuple[Response | None, BaseException | None]
_T = TypeVar("_T")


class AsyncHyphaeClient:
    """Async Native v2 client with one owned, serial worker and explicit aborts."""

    def __init__(self, transport: AbortableTransport, *, max_pending: int = 64) -> None:
        required_methods = ("execute", "abort", "close")
        if not all(
            callable(getattr(transport, method, None)) for method in required_methods
        ):
            raise ClientError("async transport must support execute, abort, and close")
        if isinstance(max_pending, bool) or not isinstance(max_pending, int) or not 0 < max_pending <= 4096:
            raise ClientError("max_pending must be between 1 and 4096")
        self._transport = transport
        self._abort_invalidates_session = bool(
            getattr(transport, "abort_invalidates_session", True)
        )
        self._client = HyphaeClient(transport)
        self._executor = ThreadPoolExecutor(
            max_workers=1,
            thread_name_prefix="hyphae-v2-async",
        )
        self._pending: dict[
            asyncio.Task[_WorkerOutcome],
            tuple[CancellationToken, Future[Response]],
        ] = {}
        self._max_pending = max_pending
        self._closed = False
        self._close_task: asyncio.Task[None] | None = None

    @classmethod
    def local(
        cls,
        endpoint: str,
        *,
        client_identity: str = "hyphae-python-sdk-v2",
        api_key: str | None = None,
        max_pending: int = 64,
    ) -> AsyncHyphaeClient:
        return cls(
            LocalTransport(
                endpoint,
                client_identity=client_identity,
                api_key=api_key,
            ),
            max_pending=max_pending,
        )

    @classmethod
    def local_authenticated(
        cls,
        endpoint: str,
        api_key: str,
        *,
        client_identity: str = "hyphae-python-sdk-v2",
        max_pending: int = 64,
    ) -> AsyncHyphaeClient:
        return cls.local(
            endpoint,
            client_identity=client_identity,
            api_key=api_key,
            max_pending=max_pending,
        )

    @classmethod
    def http(cls, base_url: str, **kwargs: Any) -> AsyncHyphaeClient:
        max_pending = kwargs.pop("max_pending", 64)
        return cls(HttpTransport(base_url, **kwargs), max_pending=max_pending)

    async def execute(
        self,
        operation: str,
        arguments: dict[str, object] | None = None,
        *,
        options: RequestOptions | None = None,
    ) -> Response:
        if self._closed:
            raise ClientError("async Hyphae client is closed")
        if len(self._pending) >= self._max_pending:
            raise ClientError("async Hyphae pending request queue is full")
        request_options = options or RequestOptions()
        loop = asyncio.get_running_loop()
        execute = partial(
            self._client.execute,
            operation,
            arguments,
            options=request_options,
        )
        source_future = self._executor.submit(execute)
        executor_future = asyncio.wrap_future(source_future, loop=loop)
        worker = asyncio.create_task(self._settle_worker(executor_future))
        self._pending[worker] = (request_options.cancellation, source_future)
        try:
            response, error = await asyncio.shield(worker)
            if error is not None:
                raise error
            assert response is not None
            return response
        except asyncio.CancelledError as cancellation:
            request_options.cancellation.cancel()
            abort_error: BaseException | None = None
            if not source_future.cancel():
                try:
                    self._transport.abort(request_options.cancellation)
                except BaseException as error:
                    abort_error = error
            _, worker_error = await self._wait_after_cancellation(worker)
            if worker_error is not None:
                cancellation.add_note(
                    f"Native v2 worker stopped with {type(worker_error).__name__}"
                )
            if abort_error is not None:
                cancellation.add_note(
                    f"Native v2 abort failed with {type(abort_error).__name__}"
                )
            raise
        finally:
            self._pending.pop(worker, None)

    @staticmethod
    async def _settle_worker(
        worker: asyncio.Future[Response],
    ) -> _WorkerOutcome:
        try:
            return await worker, None
        except BaseException as error:
            return None, error

    @staticmethod
    async def _wait_after_cancellation(
        task: asyncio.Future[_T],
    ) -> _T:
        while True:
            try:
                return await asyncio.shield(task)
            except asyncio.CancelledError:
                continue

    async def aclose(self) -> None:
        task = self._close_task
        if task is None:
            self._closed = True
            task = asyncio.create_task(self._close_owned_resources())
            self._close_task = task
        try:
            await asyncio.shield(task)
        except asyncio.CancelledError as cancellation:
            try:
                await self._wait_after_cancellation(task)
            except BaseException as cleanup_error:
                cancellation.add_note(
                    f"Native v2 close failed with {type(cleanup_error).__name__}"
                )
            raise

    async def _close_owned_resources(self) -> None:
        cleanup_error: BaseException | None = None
        for cancellation, source in tuple(self._pending.values()):
            cancellation.cancel()
            source.cancel()
        try:
            self._transport.abort(None)
        except BaseException as error:
            cleanup_error = error
        if self._pending:
            await asyncio.gather(
                *(asyncio.shield(worker) for worker in tuple(self._pending)),
                return_exceptions=True,
            )
        try:
            self._client.close()
        except BaseException as error:
            cleanup_error = cleanup_error or error
        self._executor.shutdown(wait=True, cancel_futures=True)
        if cleanup_error is not None:
            raise cleanup_error

    async def __aenter__(self) -> AsyncHyphaeClient:
        if self._closed:
            raise ClientError("async Hyphae client is closed")
        return self

    async def __aexit__(self, *exc_info: object) -> None:
        del exc_info
        await self.aclose()

    async def _execute_expected(
        self,
        operation: str,
        expected_kind: str,
        arguments: dict[str, object] | None = None,
        *,
        options: RequestOptions | None = None,
    ) -> Response:
        response = await self.execute(operation, arguments, options=options)
        if response.kind != expected_kind:
            raise ClientError(
                f"{operation} returned unexpected response kind {response.kind}"
            )
        return response

    async def begin_transaction(
        self,
        *,
        options: RequestOptions | None = None,
    ) -> AsyncTransaction:
        return await AsyncTransaction.begin(self, options=options)

    async def security_api_key_issue_start(
        self,
        arguments: dict[str, object],
        *,
        self_manage: bool = False,
        options: RequestOptions,
    ) -> Response:
        return await self._execute_expected(
            "security_api_key_issue_self_start" if self_manage else "security_api_key_issue_start",
            "security_api_key_started",
            arguments,
            options=options,
        )

    async def security_api_key_rotate_start(
        self,
        arguments: dict[str, object],
        *,
        self_manage: bool = False,
        options: RequestOptions,
    ) -> Response:
        return await self._execute_expected(
            "security_api_key_rotate_self_start" if self_manage else "security_api_key_rotate_start",
            "security_api_key_started",
            arguments,
            options=options,
        )

    async def security_api_key_activate(
        self,
        key_id: bytes,
        confirmation_digest: bytes,
        *,
        rotation: bool = False,
        self_manage: bool = False,
        options: RequestOptions,
    ) -> Response:
        operation = "security_api_key_rotate" if rotation else "security_api_key_issue"
        operation += "_self_activate" if self_manage else "_activate"
        return await self._execute_expected(
            operation,
            "security_api_key_activated",
            {
                "successor_key_id" if rotation else "key_id": key_id,
                "confirmation_digest": confirmation_digest,
            },
            options=options,
        )

    async def security_api_key_abort(
        self,
        key_id: bytes,
        *,
        rotation: bool = False,
        self_manage: bool = False,
        options: RequestOptions,
    ) -> Response:
        operation = "security_api_key_rotate" if rotation else "security_api_key_issue"
        operation += "_self_abort" if self_manage else "_abort"
        return await self._execute_expected(
            operation,
            "security_mutated",
            {"successor_key_id" if rotation else "key_id": key_id},
            options=options,
        )

    async def security_api_key_revoke(
        self,
        key_id: bytes,
        *,
        self_manage: bool = False,
        options: RequestOptions,
    ) -> Response:
        return await self._execute_expected(
            "security_api_key_revoke_self" if self_manage else "security_api_key_revoke",
            "security_mutated",
            {"key_id": key_id},
            options=options,
        )

    async def security_legacy_bearer_revoke(
        self,
        *,
        options: RequestOptions,
    ) -> Response:
        """Permanently revoke legacy-bearer compatibility as Owner."""

        return await self._execute_expected(
            "security_legacy_bearer_revoke",
            "security_mutated",
            options=options,
        )

    def security_principal_pages(
        self,
        *,
        cursor: dict[str, object] | None = None,
        limit: int = 1_000,
        options: RequestOptions | None = None,
    ) -> AsyncIterator[Response]:
        return self._security_pages(
            "security_principal_list",
            "security_principal_page",
            cursor,
            limit,
            options,
        )

    def security_role_pages(
        self,
        *,
        cursor: dict[str, object] | None = None,
        limit: int = 1_000,
        options: RequestOptions | None = None,
    ) -> AsyncIterator[Response]:
        return self._security_pages(
            "security_role_list",
            "security_role_page",
            cursor,
            limit,
            options,
        )

    def security_assignment_pages(
        self,
        *,
        cursor: dict[str, object] | None = None,
        limit: int = 1_000,
        options: RequestOptions | None = None,
    ) -> AsyncIterator[Response]:
        return self._security_pages(
            "security_assignment_list",
            "security_assignment_page",
            cursor,
            limit,
            options,
        )

    def security_key_pages(
        self,
        *,
        cursor: dict[str, object] | None = None,
        limit: int = 1_000,
        options: RequestOptions | None = None,
    ) -> AsyncIterator[Response]:
        return self._security_pages(
            "security_key_list",
            "security_key_page",
            cursor,
            limit,
            options,
        )

    def security_audit_pages(
        self,
        *,
        cursor: int | None = None,
        limit: int = 1_000,
        options: RequestOptions | None = None,
    ) -> AsyncIterator[Response]:
        return self._security_pages(
            "security_audit_read",
            "security_audit_page",
            cursor,
            limit,
            options,
        )

    async def _security_pages(
        self,
        operation: str,
        expected_kind: str,
        cursor: object | None,
        limit: int,
        options: RequestOptions | None,
    ) -> AsyncIterator[Response]:
        seen: set[str] = set()
        if cursor is not None:
            seen.add(self._cursor_fingerprint(operation, cursor))
        while True:
            response = await self._execute_expected(
                operation,
                expected_kind,
                {"cursor": cursor, "limit": limit},
                options=options,
            )
            value = response.value
            if (
                not isinstance(value, dict)
                or not isinstance(value.get("items"), list)
                or "next_cursor" not in value
            ):
                raise ClientError(f"{operation} returned a malformed page")
            next_cursor = value.get("next_cursor")
            if next_cursor is not None:
                fingerprint = self._cursor_fingerprint(operation, next_cursor)
                if fingerprint in seen:
                    raise ClientError(f"{operation} repeated a pagination cursor")
                seen.add(fingerprint)
            yield response
            if next_cursor is None:
                return
            cursor = next_cursor

    @staticmethod
    def _cursor_fingerprint(operation: str, cursor: object) -> str:
        try:
            return json.dumps(cursor, sort_keys=True, separators=(",", ":"))
        except (TypeError, ValueError) as error:
            raise ClientError(f"{operation} returned a malformed cursor") from error


class AsyncTransaction:
    """One session-local explicit transaction owned by an async client."""

    def __init__(self, client: AsyncHyphaeClient, handle: int) -> None:
        self._client = client
        self._handle = handle
        self._state = "active"
        self._transaction_id: int | None = None

    @classmethod
    async def begin(
        cls,
        client: AsyncHyphaeClient,
        *,
        options: RequestOptions | None = None,
    ) -> AsyncTransaction:
        response = await client._execute_expected(
            "transaction_begin",
            "explicit_transaction_status",
            options=options,
        )
        value = response.value
        if (
            not isinstance(value, dict)
            or value.get("state") != "active"
            or isinstance(value.get("handle"), bool)
            or not isinstance(value.get("handle"), int)
            or value["handle"] <= 0
        ):
            raise ClientError("transaction_begin returned an invalid active handle")
        return cls(client, value["handle"])

    @property
    def handle(self) -> int:
        return self._handle

    @property
    def state(self) -> str:
        return self._state

    @property
    def outcome_unknown(self) -> bool:
        return self._state == "outcome_unknown"

    @property
    def transaction_id(self) -> int | None:
        return self._transaction_id

    async def __aenter__(self) -> AsyncTransaction:
        self._require_active()
        return self

    async def __aexit__(
        self,
        exception_type: type[BaseException] | None,
        exception: BaseException | None,
        traceback: object,
    ) -> None:
        del exception_type, traceback
        if self._state != "active":
            return
        try:
            await self.rollback()
        except BaseException as cleanup_error:
            if exception is None:
                raise
            exception.add_note(
                "Native v2 transaction cleanup failed with "
                f"{type(cleanup_error).__name__}"
            )

    async def stage_sql(
        self,
        statement: str,
        parameters: list[object] | None = None,
        *,
        options: RequestOptions | None = None,
    ) -> Response:
        return await self._stage(
            "transaction_stage_sql",
            {"statement": statement, "parameters": parameters or []},
            options,
        )

    async def stage_structure(
        self,
        mutation: dict[str, object],
        *,
        options: RequestOptions | None = None,
    ) -> Response:
        return await self._stage(
            "transaction_stage_structure",
            {"mutation": mutation},
            options,
        )

    async def stage_search(
        self,
        mutation: dict[str, object],
        *,
        options: RequestOptions | None = None,
    ) -> Response:
        return await self._stage(
            "transaction_stage_search",
            {"mutation": mutation},
            options,
        )

    async def stage_vector(
        self,
        mutation: dict[str, object],
        *,
        options: RequestOptions | None = None,
    ) -> Response:
        return await self._stage(
            "transaction_stage_vector",
            {"mutation": mutation},
            options,
        )

    async def _stage(
        self,
        operation: str,
        arguments: dict[str, object],
        options: RequestOptions | None,
    ) -> Response:
        self._require_active()
        try:
            return await self._execute_transaction_expected(
                operation,
                "transaction_staged",
                {"handle": self._handle, **arguments},
                options=options,
            )
        except ProductError as error:
            if (
                error.code in {"cancelled", "deadline_exceeded"}
                and self._client._abort_invalidates_session
            ):
                self._state = "invalidated"
            raise
        except (asyncio.CancelledError, ClientError):
            if self._client._abort_invalidates_session:
                self._state = "invalidated"
            raise

    async def commit(
        self,
        *,
        options: RequestOptions | None = None,
    ) -> Response:
        self._require_active()
        try:
            response = await self._execute_transaction_expected(
                "transaction_commit",
                "transaction_committed",
                {"handle": self._handle},
                options=options,
            )
        except ProductError as error:
            if error.transaction_state == "outcome-unknown" or error.code in {
                "cancelled",
                "deadline_exceeded",
            }:
                self._state = "outcome_unknown"
                self._transaction_id = error.transaction_id
            raise
        except (asyncio.CancelledError, ClientError):
            self._state = "outcome_unknown"
            raise
        self._state = "committed"
        return response

    async def rollback(
        self,
        *,
        options: RequestOptions | None = None,
    ) -> Response:
        self._require_active()
        try:
            response = await self._execute_transaction_expected(
                "transaction_rollback",
                "transaction_rolled_back",
                {"handle": self._handle},
                options=options,
            )
        except ProductError as error:
            if error.code in {"cancelled", "deadline_exceeded"}:
                self._state = "invalidated"
            raise
        except (asyncio.CancelledError, ClientError):
            self._state = "invalidated"
            raise
        self._state = "rolled_back"
        return response

    async def _execute_transaction_expected(
        self,
        operation: str,
        expected_kind: str,
        arguments: dict[str, object],
        *,
        options: RequestOptions | None,
    ) -> Response:
        response = await self._client._execute_expected(
            operation,
            expected_kind,
            arguments,
            options=options,
        )
        if (
            not isinstance(response.value, dict)
            or response.value.get("handle") != self._handle
        ):
            raise ClientError(f"{operation} returned a mismatched transaction handle")
        return response

    def _require_active(self) -> None:
        if self._state != "active":
            raise ClientError(f"transaction is {self._state}")


__all__ = ["AsyncHyphaeClient", "AsyncTransaction"]
