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
  decodeProductRequest,
  encodeFrame,
  encodeProductRequest,
  windowsPipePath,
} from "../dist/v2/index.js";

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
