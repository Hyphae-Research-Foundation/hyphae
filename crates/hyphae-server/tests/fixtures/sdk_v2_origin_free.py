from __future__ import annotations

import os

from hyphae_sdk.v2 import HyphaeClient, RequestOptions


encoded = open(os.environ["HYPHAE_ARTIFACT"], "rb").read()
offset = 0
values = []
for _ in range(3):
    length = int.from_bytes(encoded[offset:offset + 8], "little")
    offset += 8
    values.append(encoded[offset:offset + length])
    offset += length
assert offset == len(encoded)

client = HyphaeClient.http(
    os.environ["HYPHAE_ORIGIN"], bearer_token=os.environ["HYPHAE_TOKEN"]
)
verified = client.verify_proof(*values, options=RequestOptions(request_id=20_124))
assert verified.kind == "proof_verification"
assert verified.value["semantic_reexecution_performed"]
