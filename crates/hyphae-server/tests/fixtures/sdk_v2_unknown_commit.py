# SPDX-License-Identifier: Apache-2.0
from __future__ import annotations

import os

from hyphae_sdk.v2 import HyphaeClient, ProductError, RequestOptions

clients = [
    HyphaeClient.local(os.environ["HYPHAE_SOCKET"]),
    HyphaeClient.http(os.environ["HYPHAE_ORIGIN"], bearer_token=os.environ["HYPHAE_TOKEN"]),
]
errors = []
for client, row_id in zip(clients, (9, 14)):
    try:
        client.structure_set(
            f"unknown-{row_id}".encode(), b"p" * 9000,
            options=RequestOptions(request_id=20_132),
        )
        raise AssertionError("unknown commit was acknowledged")
    except ProductError as error:
        assert error.code == "unknown_commit"
        assert error.category == "unavailable"
        assert error.retry == "unknown-commit"
        assert error.transaction_state == "outcome-unknown"
        assert error.transaction_id is not None
        assert error.request_id == 20_132
        values = dict(error.fields.__dict__)
        values.pop("transaction_id")
        errors.append(values)
assert errors[0] == errors[1]
for client in clients:
    client.close()
