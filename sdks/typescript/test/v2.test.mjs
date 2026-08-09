// SPDX-License-Identifier: Apache-2.0

import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

import {
  ERROR_MEDIA_TYPE,
  FRAME_KIND,
  HyphaeClient,
  PRODUCT_MEDIA_TYPE,
  blake3,
  decodeFrame,
  decodeProductResponse,
  encodeFrame,
  encodeProductRequest,
  windowsPipePath,
} from "../dist/v2/index.js";
import { decodeProductRequest } from "../dist/v2/protocol.js";

const fixtureUrl = new URL("../../../compatibility/native-protocol-v1-structure-get.bin", import.meta.url);

test("v2 completion BLAKE3 matches published vectors", () => {
  const hex = (value) => Array.from(value, (byte) => byte.toString(16).padStart(2, "0")).join("");
  assert.equal(hex(blake3(new Uint8Array())), "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262");
  assert.equal(hex(blake3(new TextEncoder().encode("abc"))), "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85");
});

test("v2 shared binary fixture decodes and reencodes exactly", async () => {
  const fixture = new Uint8Array(await readFile(fixtureUrl));
  const frame = decodeFrame(fixture);
  const request = decodeProductRequest(frame.payload);
  assert.equal(request.operation, "structure_get");
  assert.deepEqual(request.args.key, new TextEncoder().encode("shared-key"));
  assert.deepEqual(
    encodeFrame(frame.kind, frame.streamId, frame.requestId, encodeProductRequest(request.operation, request.args, request.options)),
    fixture,
  );
});

test("v2 independent encoder matches shared fixture", async () => {
  const fixture = new Uint8Array(await readFile(fixtureUrl));
  const payload = encodeProductRequest("structure_get", { key: new TextEncoder().encode("shared-key") }, {
    logicalTimeMicros: 1_700_000_000_000_000n,
    deadlineMicros: 1_700_000_000_500_000n,
  });
  assert.deepEqual(encodeFrame(FRAME_KIND.execute, 7, 42n, payload), fixture);
});

test("v2 transaction and catalog requests round trip", () => {
  const cases = [
    ["transaction_begin", {}],
    ["transaction_stage_vector", { handle: 7n, mutation: { kind: "delete", index: 11n, object_id: 13n } }],
    ["transaction_commit", { handle: 7n }],
    ["transaction_status_by_idempotency", { idempotency_token: 23n }],
    ["catalog_create", { definition: new TextEncoder().encode("HYCOBJ02-canonical") }],
  ];
  for (const [operation, args] of cases) {
    const decoded = decodeProductRequest(encodeProductRequest(operation, args));
    assert.equal(decoded.operation, operation);
    assert.deepEqual(decoded.args, args);
  }
});

test("v2 all structure read requests round trip", () => {
  const key = { keyspace: 7n, key: new TextEncoder().encode("key") };
  const cases = [
    { kind: "string_get", key },
    { kind: "counter_get", key },
    { kind: "ttl", key, family: "hash" },
    { kind: "hash_get", key, field: new TextEncoder().encode("field") },
    { kind: "hash_field_ttl", key, field: new TextEncoder().encode("field") },
    { kind: "hash_scan", key, start_after: new TextEncoder().encode("field"), limit: 10n },
    { kind: "hash_length", key },
    { kind: "list_range", key, start: -2n, stop: 4n },
    { kind: "list_length", key },
    { kind: "set_contains", key, member: new TextEncoder().encode("member") },
    { kind: "set_members", key, start_after: new TextEncoder().encode("member"), limit: 10n },
    { kind: "set_cardinality", key },
    { kind: "set_algebra", keyspace: 7n, operation: "intersection", keys: [new TextEncoder().encode("a"), new TextEncoder().encode("b")], output_member_limit: 10n, visit_limit: 20n },
    { kind: "sorted_set_score", key, member: new TextEncoder().encode("member") },
    { kind: "sorted_set_rank", key, member: new TextEncoder().encode("member"), order: "descending" },
    { kind: "sorted_set_range", key, start: -2n, stop: 4n, order: "descending" },
    { kind: "sorted_set_cardinality", key },
    { kind: "stream_range", key, start: 2n, end: 4n, limit: 10n },
  ];
  for (const args of cases) {
    const decoded = decodeProductRequest(encodeProductRequest("structure_read", args));
    assert.equal(decoded.operation, "structure_read");
    assert.deepEqual(decoded.args, args);
  }
});

test("Windows local endpoint normalization never doubles the pipe prefix", () => {
  assert.equal(windowsPipePath("hyphae-test"), "\\\\.\\pipe\\hyphae-test");
  assert.equal(windowsPipePath("\\\\.\\pipe\\hyphae-test"), "\\\\.\\pipe\\hyphae-test");
  assert.throws(() => windowsPipePath("\\\\server\\pipe\\hyphae-test"), /local named-pipe namespace/);
});

test("v2 high-level API is transport independent", async () => {
  const calls = [];
  const client = new HyphaeClient({
    async execute(operation, args, options) {
      calls.push({ operation, args, options });
      return { kind: "fake", value: args, requestId: 9n };
    },
  });
  const response = await client.structureGet(new TextEncoder().encode("key"), { requestId: 9n });
  assert.equal(response.requestId, 9n);
  assert.equal(calls[0].operation, "structure_get");
});

test("v2 high-level API exposes explicit transactions", async () => {
  const calls = [];
  const client = new HyphaeClient({
    async execute(operation, args, options) {
      calls.push({ operation, args, options });
      return { kind: "fake", value: args, requestId: options.requestId ?? 1n };
    },
  });
  await client.transactionBegin({ requestId: 20n });
  await client.transactionStageVector(7n, { kind: "delete", index: 11n, object_id: 13n }, { requestId: 21n });
  await client.explicitTransactionStatus(7n, { requestId: 22n });
  await client.transactionStatusByIdempotency(23n, { requestId: 23n });
  assert.deepEqual(calls.map(({ operation }) => operation), [
    "transaction_begin",
    "transaction_stage_vector",
    "explicit_transaction_status",
    "transaction_status_by_idempotency",
  ]);
});

test("v2 transaction stage response decodes a typed result", () => {
  const encoded = new Uint8Array(35);
  encoded.set(new TextEncoder().encode("HYPRSP01"));
  const view = new DataView(encoded.buffer);
  view.setUint32(8, encoded.byteLength, true);
  view.setUint16(12, 28, true);
  view.setBigUint64(16, 7n, true);
  view.setBigUint64(24, 1n, true);
  view.setUint8(32, 1);
  view.setUint8(33, 3);
  view.setUint8(34, 1);
  assert.deepEqual(decodeProductResponse(encoded, 24n), {
    kind: "transaction_staged",
    value: {
      handle: 7n,
      operationOrdinal: 1n,
      changed: true,
      result: { kind: "vector", changed: true },
    },
    requestId: 24n,
  });
});

test("v2 transaction stage requests match canonical wire kinds", () => {
  const vector = encodeProductRequest("transaction_stage_vector", {
    handle: 7n,
    mutation: { kind: "delete", index: 11n, object_id: 13n },
  }, { requestId: 25n });
  const vectorView = new DataView(vector.buffer, vector.byteOffset, vector.byteLength);
  assert.equal(vectorView.getUint16(12, true), 36);
  assert.equal(vectorView.getBigUint64(80, true), 7n);
  assert.equal(vectorView.getUint8(88), 1);
  assert.equal(vectorView.getBigUint64(89, true), 11n);
  assert.equal(vectorView.getBigUint64(105, true), 13n);

  const structure = encodeProductRequest("transaction_stage_structure", {
    handle: 7n,
    mutation: { kind: "create_hash", key: { keyspace: 17n, key: new TextEncoder().encode("hash") } },
  }, { requestId: 26n });
  const structureView = new DataView(structure.buffer, structure.byteOffset, structure.byteLength);
  assert.equal(structureView.getUint16(12, true), 34);
  assert.equal(structureView.getBigUint64(80, true), 7n);
  assert.equal(structureView.getUint8(88), 3);
  assert.equal(structureView.getBigUint64(89, true), 17n);
  assert.equal(structureView.getUint32(105, true), 4);
  assert.deepEqual(structure.slice(109, 113), new TextEncoder().encode("hash"));
  assert.equal(structureView.getUint8(113), 3);
});

test("integrated search exposes only the logical collection identity", async () => {
  const calls = [];
  const client = new HyphaeClient({
    async execute(operation, args) {
      calls.push({ operation, args });
      return { kind: "fake", value: args, requestId: 10n };
    },
  });
  await client.searchCollection(13n, { limit: 1, vectors: [] });
  await client.searchIngest(13n, { idempotency_id: 7n, documents: [{ object_id: 21n, text: "hello" }] });
  assert.equal(calls[0].args.collection, 13n);
  assert.equal("binding" in calls[0].args, false);
  assert.equal(calls[1].operation, "search_ingest");
});

test("v2 HTTP client uses /v2 and validates correlation", async () => {
  const capabilities = new Uint8Array(16 + 56);
  capabilities.set(new TextEncoder().encode("HYPRSP01"));
  const view = new DataView(capabilities.buffer);
  view.setUint32(8, capabilities.byteLength, true);
  view.setUint16(12, 1, true);
  view.setUint16(16, 1, true);
  view.setUint16(18, 1, true);
  view.setUint16(20, 2, true);
  view.setUint16(22, 6, true);
  let seen;
  const client = HyphaeClient.http("https://example.test", {
    fetch: async (url, options) => {
      seen = { url: String(url), contentType: options.headers.get("content-type") };
      return new Response(capabilities, {
        status: 200,
        headers: {
          "content-type": PRODUCT_MEDIA_TYPE,
          "x-hyphae-request-id": "17",
        },
      });
    },
  });
  const response = await client.capabilities({ requestId: 17n });
  assert.equal(response.kind, "capabilities");
  assert.deepEqual(seen, { url: "https://example.test/v2/execute", contentType: PRODUCT_MEDIA_TYPE });
  assert.equal(ERROR_MEDIA_TYPE, "application/vnd.hyphae.error-v1");
});
