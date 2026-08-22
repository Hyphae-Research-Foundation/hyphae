// SPDX-License-Identifier: Apache-2.0
import assert from "node:assert/strict";
import test from "node:test";

import { blake3 } from "@noble/hashes/blake3.js";
import { DeclaredProviderRecord } from "../dist/providers.js";

test("declared envelope matches the cross-language golden", () => {
  const record = new DeclaredProviderRecord(
    "openai",
    "text-embedding-3-small",
    blake3(new TextEncoder().encode("request")),
    blake3(new TextEncoder().encode("response")),
  );
  const expectedHead = new Uint8Array([
    ...new TextEncoder().encode("HYATTS01"),
    2,
    6, 0,
    ...new TextEncoder().encode("openai"),
    22, 0,
    ...new TextEncoder().encode("text-embedding-3-small"),
  ]);
  const envelope = record.envelope();
  assert.deepEqual(envelope.subarray(0, expectedHead.byteLength), expectedHead);
  assert.deepEqual(
    envelope.subarray(expectedHead.byteLength, expectedHead.byteLength + 32),
    blake3(new TextEncoder().encode("request")),
  );
  assert.deepEqual(
    envelope.subarray(expectedHead.byteLength + 32),
    blake3(new TextEncoder().encode("response")),
  );
  assert.equal(record.envelopeHex().length, envelope.byteLength * 2);
});

test("unbounded names fail closed", () => {
  const record = new DeclaredProviderRecord("", "m", new Uint8Array(32), new Uint8Array(32));
  assert.throws(() => record.envelope());
});
