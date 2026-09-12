// SPDX-License-Identifier: Apache-2.0
import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { encodeProductRequest, decodeProductResponse } from "../dist/v2/protocol.js";

const fixture = name => new Uint8Array(readFileSync(new URL(`../../../compatibility/${name}`, import.meta.url)));
const search = { lexical: { query: "decisions", candidate_limit: 4, weight: 1 }, limit: 4 };

test("memory requests match Rust bytes and refuse older peers", () => {
  for (const [operation, args] of [
    ["memory_recall", {collections:[21n,22n], search, limit:2, provenance:new TextEncoder().encode("query-manifest")}],
    ["memory_enrich", {collection:21n, expected_envelope_digest:new Uint8Array(32).fill(9),
      idempotency_id:701n, document:{object_id:201n, text:"remember", doc_values:{}, vectors:{memory:[1,0]}}}],
  ]) {
    assert.deepEqual(encodeProductRequest(operation, args, {logicalTimeMicros:5n}, 7), fixture(`native-${operation.replaceAll("_", "-")}-v1.bin`));
    assert.throws(() => encodeProductRequest(operation, args, {}, 6), /minor/);
  }
});

test("memory response validates cross-domain identities, snapshots and every truncation", () => {
  const encoded = fixture("native-memory-result-v1.bin");
  const response = decodeProductResponse(encoded, 17n, 7);
  assert.equal(response.kind, "memory_recall");
  assert.equal(response.value.expiredFiltered, 1n);
  assert.deepEqual(response.value.memories.map(m => m.collection), [21n,22n]);
  assert.ok(response.value.memories.every(m => new TextDecoder().decode(m.envelope) === '{"text":"remember"}'));
  assert.throws(() => decodeProductResponse(encoded, 17n, 6), /minor/);
  for (let end = 0; end < encoded.length; end++) assert.throws(() => decodeProductResponse(encoded.slice(0,end), 17n, 7));
  const forged = encoded.slice();
  forged[120] ^= 1;
  assert.throws(() => decodeProductResponse(forged, 17n, 7), /snapshot/);
  const expired = encoded.slice();
  new DataView(expired.buffer).setBigUint64(expired.length - 8, 999n, true);
  assert.throws(() => decodeProductResponse(expired, 17n, 7), /expiry/);
});

test("memory request counts and provenance remain bounded", () => {
  const valid = {collections:[21n,22n], search, limit:2};
  for (const change of [{collections:[22n,21n]}, {collections:[21n,21n]},
    {collections:[]}, {limit:65}, {provenance:new Uint8Array(65537)}]) {
    assert.throws(() => encodeProductRequest("memory_recall", {...valid,...change}));
  }
});
