// SPDX-License-Identifier: Apache-2.0

import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

import {
  ERROR_MEDIA_TYPE,
  FRAME_KIND,
  HyphaeClient,
  LocalTransport,
  PRODUCT_MEDIA_TYPE,
  ProductError,
  blake3,
  clearSensitiveBytes,
  ClientError,
  decodeFrame,
  decodeProductResponse,
  encodeFrame,
  encodeProductRequest,
  SensitiveBytes,
  windowsPipePath,
} from "../dist/v2/index.js";
import { decodeProductRequest, operationRequiredMinor } from "../dist/v2/protocol.js";

const fixtureUrl = new URL("../../../compatibility/native-protocol-v1-structure-get.bin", import.meta.url);

test("v2 search content at every current shape is minor zero", () => {
  // Every currently expressible search body is minor-0 content; the content
  // walk exists so future operators, typed doc values, and fusion methods
  // raise the requirement without new operations.
  assert.equal(
    operationRequiredMinor("search_collection", {
      filter: {
        kind: "not",
        filter: {
          kind: "all",
          filters: [
            { kind: "match_all" },
            { kind: "compare", field: "price", operator: "less_or_equal", value: 40n },
          ],
        },
      },
    }),
    0,
  );
  assert.equal(
    operationRequiredMinor("search_ingest", {
      documents: [{ object_id: 1n, text: "rust", doc_values: { flag: true, rank: 3n, name: "a" } }],
    }),
    0,
  );
  assert.equal(
    operationRequiredMinor("proof_generate", {
      operation: "search_collection",
      arguments: { filter: { kind: "match_all" } },
    }),
    0,
  );
  assert.equal(operationRequiredMinor("security_status"), 1);
});

test("v2 completion BLAKE3 matches published vectors", () => {
  const hex = (value) => Array.from(value, (byte) => byte.toString(16).padStart(2, "0")).join("");
  assert.equal(hex(blake3(new Uint8Array())), "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262");
  assert.equal(hex(blake3(new TextEncoder().encode("abc"))), "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85");
});

test("v2 sensitive byte cleanup overwrites in place", () => {
  const secret = new Uint8Array([1, 2, 3, 4]);
  clearSensitiveBytes(secret);
  assert.deepEqual(secret, new Uint8Array(4));
});

test("v2 HTTP requires TLS outside canonical loopback before bearer handling", () => {
  for (const origin of ["http://127.0.0.1:8787", "http://127.1.2.3", "http://localhost", "http://[::1]", "https://example.test"]) {
    assert.doesNotThrow(() => HyphaeClient.http(origin));
  }
  for (const origin of ["http://example.test", "http://localhost.example", "http://192.168.1.2", "http://[::ffff:127.0.0.1]"]) {
    assert.throws(() => HyphaeClient.http(origin, { bearerToken: "bad\nsecret" }), /requires HTTPS/);
  }
});

test("v2 sensitive wrapper is redacted, nonserializable, consumable, and terminal", () => {
  const secret = new SensitiveBytes(new Uint8Array([1, 2, 3]));
  assert.equal(String(secret), "SensitiveBytes([REDACTED])");
  assert.throws(() => JSON.stringify(secret), ClientError);
  assert.deepEqual(secret.consume(), new Uint8Array([1, 2, 3]));
  assert.throws(() => secret.consume(), /closed/);
});

test("v2 HTTP product error status is bounded", () => {
  const fields = {
    code: "invalid_request",
    category: "invalid-request",
    retry: "never",
    message: "invalid",
    transactionState: "none",
    details: {},
  };
  assert.throws(() => new ProductError(fields, 399), /status is invalid/);
  assert.throws(() => new ProductError(fields, 600), /status is invalid/);
  assert.doesNotThrow(() => new ProductError(fields, 400));
});

test("v2 catalog visible count is bounded before item allocation", () => {
  const response = new Uint8Array(24);
  response.set(new TextEncoder().encode("HYPRSP01"));
  const view = new DataView(response.buffer);
  view.setUint32(8, response.byteLength, true);
  view.setUint16(12, 42, true);
  view.setUint32(16, 0, true);
  view.setUint32(20, 0xffffffff, true);
  assert.throws(() => decodeProductResponse(response, 1n, 3), /item count/);
  view.setUint32(20, 1, true);
  assert.throws(() => decodeProductResponse(response, 1n, 3), /item count/);
});

test("v2 negotiated minor rejects unavailable operations before writing", () => {
  assert.throws(
    () => encodeProductRequest("catalog_visible_list", { item_limit: 1, visit_limit: 1, byte_limit: 1 }, {}, 2),
    /negotiated protocol minor/,
  );
  assert.throws(() => encodeProductRequest("security_status", {}, {}, 0), /negotiated protocol minor/);
});

test("v2 security tags 42 through 53 and 70 have strict/idempotent parity", () => {
  const cases = [
    ["security_status", {}, false],
    ["security_principal_list", { cursor: undefined, limit: 1 }, false],
    ["security_role_list", { cursor: undefined, limit: 1 }, false],
    ["security_assignment_list", { cursor: undefined, limit: 1 }, false],
    ["security_key_list", { cursor: undefined, limit: 1 }, false],
    ["security_audit_read", { cursor: undefined, limit: 1 }, false],
    ["security_principal_create", { display_name: "reader" }, true],
    ["security_principal_set_enabled", { principal_id: 1n, enabled: true }, true],
    ["security_custom_role_create", { display_name: "reader", grants: [{ permission: "data.read", scope: { kind: "instance" } }] }, true],
    ["security_built_in_assignment_create", { principal_id: 1n, role: "reader", scope: { kind: "instance" } }, true],
    ["security_custom_assignment_create", { principal_id: 1n, role_id: 2n }, true],
    ["security_assignment_revoke", { assignment_id: 3n }, true],
    ["security_legacy_bearer_revoke", {}, true],
  ];
  for (const [index, [operation, args, mutation]] of cases.entries()) {
    const options = mutation ? { idempotencyToken: 17n } : {};
    const encoded = encodeProductRequest(operation, args, options, 3);
    assert.equal(new DataView(encoded.buffer).getUint16(12, true), index === 12 ? 70 : 42 + index);
    if (mutation) {
      assert.throws(() => encodeProductRequest(operation, args, {}, 3), /idempotency/);
      assert.throws(() => encodeProductRequest(operation, args, { idempotencyToken: 17n, durability: "memory" }, 3), /strict durability/);
    }
  }
  const nonStrict = encodeProductRequest("security_legacy_bearer_revoke", {}, { idempotencyToken: 18n }, 3);
  nonStrict[88] = 2;
  assert.throws(() => decodeProductRequest(nonStrict), /strict durability/);
});

test("v2 security audit decodes a legacy-bearer target", () => {
  const eventBytes = 16 + 8 + 8 + 32 + 8 + 8 + 24 + 8;
  const encoded = new Uint8Array(16 + 8 + 24 + eventBytes);
  encoded.set(new TextEncoder().encode("HYPRSP01"));
  const view = new DataView(encoded.buffer);
  view.setUint32(8, encoded.byteLength, true);
  view.setUint16(12, 37, true);
  view.setUint32(16, 1, true);
  let offset = 48;
  encoded[offset + 15] = 1;
  offset += 16;
  view.setBigUint64(offset, 2n, true);
  offset += 8 + 8 + 32;
  view.setUint8(offset, 10);
  offset += 8;
  view.setUint32(offset, 1, true);
  offset += 8;
  view.setUint8(offset, 4);
  const response = decodeProductResponse(encoded, 27n, 3);
  assert.deepEqual(response.value.events[0].targets, [{ kind: "legacy_bearer" }]);
});

test("v2 HTTP deadline remains active while the response body stalls", async () => {
  const client = HyphaeClient.http("https://example.test", {
    fetch: async (_url, options) => new Response(new ReadableStream({
      start(controller) {
        options.signal.addEventListener("abort", () => controller.error(options.signal.reason), { once: true });
      },
    }), {
      status: 200,
      headers: { "content-type": PRODUCT_MEDIA_TYPE, "x-hyphae-protocol-minor": "3", "x-hyphae-request-id": "91" },
    }),
  });
  await assert.rejects(
    client.capabilities({ requestId: 91n, deadlineMicros: BigInt(Date.now() + 20) * 1000n }),
    (error) => error instanceof ProductError && error.fields.code === "deadline_exceeded",
  );
});

test("v2 HTTP deadline wins even when injected fetch ignores AbortSignal", async () => {
  const never = new Promise(() => {});
  const client = HyphaeClient.http("https://example.test", { fetch: async () => never });
  await assert.rejects(
    client.capabilities({ requestId: 911n, deadlineMicros: BigInt(Date.now() + 20) * 1000n }),
    (error) => error instanceof ProductError && error.fields.code === "deadline_exceeded",
  );
});

test("v2 HTTP pending requests have a finite reject-on-full bound", async () => {
  let release;
  const blocked = new Promise((resolve) => { release = resolve; });
  const client = HyphaeClient.http("https://example.test", {
    maximumPending: 1,
    fetch: async () => blocked,
  });
  const first = client.capabilities({ requestId: 92n });
  await assert.rejects(client.capabilities({ requestId: 93n }), /queue is full/);
  release(new Response(new Uint8Array(), {
    status: 500,
    headers: { "content-type": ERROR_MEDIA_TYPE, "x-hyphae-protocol-minor": "3", "x-hyphae-request-id": "92" },
  }));
  await assert.rejects(first);
});

test("v2 local aborted read disconnects the contaminated stream before reuse", async () => {
  const pending = () => new Promise(() => {});
  let connections = 0;
  let closed = 0;
  const connector = async () => {
    connections += 1;
    let reads = 0;
    return {
      async write() {},
      async readExact() {
        reads += 1;
        if (reads === 1) {
          const welcome = new Uint8Array(94);
          welcome.set(new TextEncoder().encode("HYPWEL01"));
          const welcomeView = new DataView(welcome.buffer);
          welcomeView.setUint32(8, 94, true);
          welcomeView.setUint16(12, 1, true);
          welcomeView.setUint16(14, 3, true);
          welcomeView.setBigUint64(16, 0x7fn, true);
          welcomeView.setBigUint64(24, 1n, true);
          welcomeView.setUint32(40, 16 * 1024 * 1024, true);
          welcomeView.setUint32(44, 64, true);
          welcomeView.setUint32(48, 64 * 1024, true);
          return encodeFrame(FRAME_KIND.welcome, 0, connections === 1 ? 913n : 915n, welcome).slice(0, 32);
        }
        if (reads === 2) {
          const welcome = new Uint8Array(94);
          welcome.set(new TextEncoder().encode("HYPWEL01"));
          const view = new DataView(welcome.buffer);
          view.setUint32(8, 94, true);
          view.setUint16(12, 1, true);
          view.setUint16(14, 3, true);
          view.setBigUint64(16, 0x7fn, true);
          view.setBigUint64(24, 1n, true);
          view.setUint32(40, 16 * 1024 * 1024, true);
          view.setUint32(44, 64, true);
          view.setUint32(48, 64 * 1024, true);
          return welcome;
        }
        return pending();
      },
      async close() { closed += 1; },
    };
  };
  const transport = new LocalTransport("unused", connector);
  const aborted = new AbortController();
  const first = transport.execute("capabilities", {}, { requestId: 912n, signal: aborted.signal });
  setTimeout(() => aborted.abort(), 10);
  await assert.rejects(first, (error) => error instanceof ProductError && error.fields.code === "cancelled");
  const second = transport.execute("capabilities", {}, { requestId: 914n, deadlineMicros: BigInt(Date.now() + 20) * 1000n });
  await assert.rejects(second, (error) => error instanceof ProductError && error.fields.code === "deadline_exceeded");
  assert.equal(connections, 2);
  assert.equal(closed, 2);
});

test("v2 decoders reject oversized counts before Array.from allocation", () => {
  const cases = [
    [8, (encoded, view) => view.setUint32(16, 0xffffffff, true), "search hit"],
    [13, (encoded, view) => view.setUint32(128, 0xffffffff, true), "catalog page item"],
    [20, (encoded, view) => view.setUint32(76, 0xffffffff, true), "telemetry response"],
    [22, (encoded, view) => view.setUint32(96, 0xffffffff, true), "integrated search hit"],
    [24, (encoded, view) => {
      encoded[96] = 1;
      view.setUint32(97, 0xffffffff, true);
    }, "structure value"],
    [3, (encoded, view) => {
      encoded[24] = 1;
      view.setUint32(32, 0xffffffff, true);
    }, "SQL row or column"],
  ];
  for (const [kind, mutate, message] of cases) {
    const encoded = new Uint8Array(320);
    encoded.set(new TextEncoder().encode("HYPRSP01"));
    const view = new DataView(encoded.buffer);
    view.setUint32(8, encoded.byteLength, true);
    view.setUint16(12, kind, true);
    mutate(encoded, view);
    assert.throws(() => decodeProductResponse(encoded, 1n, 3), new RegExp(message));
  }
});

test("v2 security start receipts require canonical matching key secrets", () => {
  const secret = "hyp1_01010101010101010101010101010101_" + "02".repeat(32);
  const encoded = new Uint8Array(16 + 16 + 16 + 24 + 8 + 96 + 4 + secret.length);
  encoded.set(new TextEncoder().encode("HYPRSP01"));
  const view = new DataView(encoded.buffer);
  view.setUint32(8, encoded.byteLength, true);
  view.setUint16(12, 43, true);
  encoded.fill(1, 16, 48);
  view.setBigUint64(72, 1n, true);
  view.setBigUint64(80, 1n, true);
  view.setBigUint64(96, 1n, true);
  view.setBigUint64(104, 1n, true);
  view.setBigUint64(112, 1n, true);
  encoded.fill(3, 120, 152);
  view.setBigUint64(160, 1n, true);
  view.setBigUint64(168, 0n, true);
  view.setUint32(176, secret.length, true);
  encoded.set(new TextEncoder().encode(secret), 180);
  assert.doesNotThrow(() => decodeProductResponse(encoded, 1n, 3));
  encoded[180 + 5] = "f".charCodeAt(0);
  assert.throws(() => decodeProductResponse(encoded, 1n, 3), /identity differs/);
  encoded[180 + 5] = "0".charCodeAt(0);
  encoded[180] = 0xff;
  assert.throws(() => decodeProductResponse(encoded, 1n, 3), /noncanonical/);
});

test("v2 local cancellation removes a queued request from the finite bound", async () => {
  const connector = async (_endpoint, signal) => new Promise((_resolve, reject) => {
    signal.addEventListener("abort", () => reject(signal.reason), { once: true });
  });
  const transport = new LocalTransport("unused", connector, { maximumPending: 2 });
  const firstAbort = new AbortController();
  const first = transport.execute("capabilities", {}, { requestId: 94n, signal: firstAbort.signal });
  const queuedAbort = new AbortController();
  const queued = transport.execute("capabilities", {}, { requestId: 95n, signal: queuedAbort.signal });
  await Promise.resolve();
  await assert.rejects(transport.execute("capabilities", {}, { requestId: 96n }), /queue is full/);
  queuedAbort.abort();
  await assert.rejects(queued, (error) => error instanceof ProductError && error.fields.code === "cancelled");
  const alreadyAborted = new AbortController();
  alreadyAborted.abort();
  await assert.rejects(
    transport.execute("capabilities", {}, { requestId: 97n, signal: alreadyAborted.signal }),
    (error) => error instanceof ProductError && error.fields.code === "cancelled",
  );
  firstAbort.abort();
  await assert.rejects(first);
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

test("v2 attested rerank request matches the cross-language golden", () => {
  const hex = (value) => Array.from(value, (byte) => byte.toString(16).padStart(2, "0")).join("");
  const name = (value) => {
    const encoded = new TextEncoder().encode(value);
    return [encoded.byteLength & 0xff, encoded.byteLength >> 8, ...encoded];
  };
  const envelope = Uint8Array.from([
    ...new TextEncoder().encode("HYATTS01"), 2,
    ...name("openai"), ...name("text-embedding-3-small"),
    ...Array(32).fill(3), ...Array(32).fill(4),
  ]);
  const args = {
    collection: 13n,
    request: {
      lexical: { query: "rust", candidate_limit: 4, weight: 1 },
      vectors: [],
      limit: 4,
      rerank: {
        attestation: envelope,
        scores: [
          { object_id: 201n, score: 0.75 },
          { object_id: 202n, score: 0.25 },
        ],
      },
    },
  };
  const options = { logicalTimeMicros: 10n, durability: "memory" };
  assert.throws(() => encodeProductRequest("search_collection", args, options, 3), /protocol minor/);
  const encoded = encodeProductRequest("search_collection", args, options, 4);
  // The same digest is pinned by the Rust protocol goldens and the Python
  // suite for this identically composed request.
  assert.equal(hex(blake3(encoded)), "f61fd68c170b8cf0841678aeda0819f7ff98869486b51ea10c104e8e2d4cee04");
});

test("v2 highlighted request matches the cross-language golden", () => {
  const hex = (value) => Array.from(value, (byte) => byte.toString(16).padStart(2, "0")).join("");
  const args = {
    collection: 13n,
    request: {
      lexical: { query: "rust", candidate_limit: 4, weight: 1 },
      vectors: [],
      limit: 4,
      highlight: { max_fragments: 2, fragment_bytes: 64 },
    },
  };
  const options = { logicalTimeMicros: 10n, durability: "memory" };
  assert.throws(() => encodeProductRequest("search_collection", args, options, 4), /protocol minor/);
  const encoded = encodeProductRequest("search_collection", args, options, 5);
  // The same digest is pinned by the Rust protocol goldens and the Python
  // suite for this identically composed request.
  assert.equal(hex(blake3(encoded)), "1438488e4d12a342a71d1cab17bad2fecf6ddc46ecb8e73970fc6f037e5e1443");
});

test("v2 integrated search response decodes with and without fragments", () => {
  const fromHex = (hex) => Uint8Array.from(hex.match(/../gu), (byte) => Number.parseInt(byte, 16));
  // Both payloads are Rust-encoded goldens for the same one-hit result; the
  // second carries the minor-5 content-derived fragments tail.
  const plain = fromHex(
    "4859505253503031bc0000001600000001010101010101010101010101010101" +
    "0101010101010101000000000000000003000000000000000404040404040404" +
    "0404040404040404040404040404040404040404040404040500000000000000" +
    "01000000c9000000000000000000000000000000000000000000f83f00000000" +
    "0000000000000000000000000000000000000000010000000000000001000000" +
    "00000000010000000000000001000000000000000100000000000000",
  );
  const fragmented = fromHex(
    "4859505253503031d20000001600000001010101010101010101010101010101" +
    "0101010101010101000000000000000003000000000000000404040404040404" +
    "0404040404040404040404040404040404040404040404040500000000000000" +
    "01000000c9000000000000000000000000000000000000000000f83f00000000" +
    "0000000000000000000000000000000000000000010000000000000001000000" +
    "0000000001000000000000000100000000000000010000000000000001010000" +
    "000d00000072757374206461746162617365",
  );
  for (const [payload, fragments] of [[plain, undefined], [fragmented, ["rust database"]]]) {
    const response = decodeProductResponse(payload, 1n, 5);
    assert.equal(response.kind, "integrated_search");
    const hit = response.value.hits[0];
    assert.equal(hit.objectId, 201n);
    assert.deepEqual(hit.fragments, fragments);
  }
});

test("v2 transaction and catalog requests round trip", () => {
  const cases = [
    ["transaction_begin", {}],
    ["transaction_stage_vector", { handle: 7n, mutation: { kind: "delete", index: 11n, object_id: 13n } }],
    ["transaction_commit", { handle: 7n }],
    ["transaction_status_by_idempotency", { idempotency_token: 23n }],
    ["catalog_create", { definition: new TextEncoder().encode("HYCOBJ02-canonical") }],
    ["catalog_visible_list", { parent: undefined, kind: undefined, cursor: new TextEncoder().encode("opaque"), item_limit: 2n, visit_limit: 8n, byte_limit: 4096n }],
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

test("v2 high-level API exposes security metadata writes and legacy revoke", async () => {
  const calls = [];
  const client = new HyphaeClient({
    async execute(operation, args, options) {
      calls.push({ operation, args, options });
      return { kind: "fake", value: args, requestId: 1n };
    },
  });
  const options = { idempotencyToken: 77n };
  await client.securityPrincipalCreate("reader", options);
  await client.securityPrincipalSetEnabled(1n, true, options);
  await client.securityCustomRoleCreate("custom", [{ permission: "data.read", scope: { kind: "instance" } }], options);
  await client.securityBuiltInAssignmentCreate(1n, "reader", { kind: "instance" }, options);
  await client.securityCustomAssignmentCreate(1n, 2n, options);
  await client.securityAssignmentRevoke(3n, options);
  await client.securityLegacyBearerRevoke(options);
  assert.deepEqual(calls.map(({ operation }) => operation), [
    "security_principal_create", "security_principal_set_enabled", "security_custom_role_create",
    "security_built_in_assignment_create", "security_custom_assignment_create",
    "security_assignment_revoke", "security_legacy_bearer_revoke",
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
      seen = { url: String(url), contentType: options.headers.get("content-type"), minor: options.headers.get("x-hyphae-protocol-minor") };
      return new Response(capabilities, {
        status: 200,
        headers: {
          "content-type": PRODUCT_MEDIA_TYPE,
          "x-hyphae-protocol-minor": "3",
          "x-hyphae-request-id": "17",
        },
      });
    },
  });
  const response = await client.capabilities({ requestId: 17n });
  assert.equal(response.kind, "capabilities");
  assert.deepEqual(seen, { url: "https://example.test/v2/execute", contentType: PRODUCT_MEDIA_TYPE, minor: "3,4,5,6" });
  assert.equal(ERROR_MEDIA_TYPE, "application/vnd.hyphae.error-v1");
});

test("v2 HTTP rejects a missing or nonexact selected minor before session retention and decoding", async () => {
  const capabilities = new Uint8Array(16 + 56);
  capabilities.set(new TextEncoder().encode("HYPRSP01"));
  const view = new DataView(capabilities.buffer);
  view.setUint32(8, capabilities.byteLength, true);
  view.setUint16(12, 1, true);
  view.setUint16(16, 1, true);
  view.setUint16(18, 1, true);
  view.setUint16(20, 2, true);
  view.setUint16(22, 6, true);
  for (const minor of [undefined, "2", "garbage"]) {
    const headers = {
      "content-type": PRODUCT_MEDIA_TYPE,
      "x-hyphae-request-id": "18",
      "x-hyphae-session-id": "1".repeat(32),
    };
    if (minor !== undefined) headers["x-hyphae-protocol-minor"] = minor;
    let calls = 0;
    let retainedSession;
    const client = HyphaeClient.http("https://example.test", {
      fetch: async (_url, options) => {
        calls += 1;
        if (calls === 1) return new Response(null, { status: 200, headers });
        retainedSession = options.headers.get("x-hyphae-session-id");
        return new Response(capabilities, {
          status: 200,
          headers: {
            "content-type": PRODUCT_MEDIA_TYPE,
            "x-hyphae-protocol-minor": "3",
            "x-hyphae-request-id": "19",
          },
        });
      },
    });
    await assert.rejects(client.capabilities({ requestId: 18n }), /protocol minor/);
    assert.equal((await client.capabilities({ requestId: 19n })).kind, "capabilities");
    assert.equal(retainedSession, null);
  }
});

test("v2 HTTP rejects swapped correlation before retaining a session", async () => {
  const capabilities = new Uint8Array(16 + 56);
  capabilities.set(new TextEncoder().encode("HYPRSP01"));
  const view = new DataView(capabilities.buffer);
  view.setUint32(8, capabilities.byteLength, true);
  view.setUint16(12, 1, true);
  view.setUint16(16, 1, true);
  view.setUint16(18, 1, true);
  view.setUint16(20, 2, true);
  view.setUint16(22, 6, true);
  let calls = 0;
  let retainedSession;
  const client = HyphaeClient.http("https://example.test", {
    fetch: async (_url, options) => {
      calls += 1;
      if (calls === 1) {
        return new Response(capabilities, {
          status: 200,
          headers: {
            "content-type": PRODUCT_MEDIA_TYPE,
            "x-hyphae-protocol-minor": "3",
            "x-hyphae-request-id": "99",
            "x-hyphae-session-id": "1".repeat(32),
          },
        });
      }
      retainedSession = options.headers.get("x-hyphae-session-id");
      return new Response(capabilities, {
        status: 200,
        headers: {
          "content-type": PRODUCT_MEDIA_TYPE,
          "x-hyphae-protocol-minor": "3",
          "x-hyphae-request-id": "19",
        },
      });
    },
  });
  await assert.rejects(client.capabilities({ requestId: 18n }), /request ID mismatch/);
  assert.equal((await client.capabilities({ requestId: 19n })).kind, "capabilities");
  assert.equal(retainedSession, null);
});

test("v2 HTTP routes all API-key lifecycle phases through the dedicated family", async () => {
  const operations = [
    ["security_api_key_issue_self_start", { principal_id: 1n, label: "issue", roles: [], custom_roles: [], permission_ceiling: ["credential.self_manage"], scope_ceiling: [{ kind: "instance" }] }],
    ["security_api_key_issue_start", { principal_id: 1n, label: "issue", roles: [], custom_roles: [], permission_ceiling: ["credential.self_manage"], scope_ceiling: [{ kind: "instance" }] }],
    ["security_api_key_issue_self_activate", { key_id: new Uint8Array(16).fill(1), confirmation_digest: new Uint8Array(32).fill(2) }],
    ["security_api_key_issue_activate", { key_id: new Uint8Array(16).fill(1), confirmation_digest: new Uint8Array(32).fill(2) }],
    ["security_api_key_rotate_self_start", { predecessor_key_id: new Uint8Array(16).fill(1), label: "rotate", overlap_seconds: 0n }],
    ["security_api_key_rotate_start", { predecessor_key_id: new Uint8Array(16).fill(1), label: "rotate", overlap_seconds: 0n }],
    ["security_api_key_rotate_self_activate", { successor_key_id: new Uint8Array(16).fill(1), confirmation_digest: new Uint8Array(32).fill(2) }],
    ["security_api_key_rotate_activate", { successor_key_id: new Uint8Array(16).fill(1), confirmation_digest: new Uint8Array(32).fill(2) }],
    ["security_api_key_issue_self_abort", { key_id: new Uint8Array(16).fill(1) }],
    ["security_api_key_issue_abort", { key_id: new Uint8Array(16).fill(1) }],
    ["security_api_key_rotate_self_abort", { successor_key_id: new Uint8Array(16).fill(1) }],
    ["security_api_key_rotate_abort", { successor_key_id: new Uint8Array(16).fill(1) }],
    ["security_api_key_revoke_self", { key_id: new Uint8Array(16).fill(1) }],
    ["security_api_key_revoke", { key_id: new Uint8Array(16).fill(1) }],
  ];
  const seen = [];
  let expectedRequestId = "";
  const client = HyphaeClient.http("https://example.test", {
    fetch: async (url, options) => {
      seen.push({ url: String(url), minor: options.headers.get("x-hyphae-protocol-minor") });
      return new Response(new Uint8Array(), {
        status: 500,
        headers: {
          "content-type": ERROR_MEDIA_TYPE,
          "x-hyphae-protocol-minor": "3",
          "x-hyphae-request-id": expectedRequestId,
        },
      });
    },
  });
  for (const [index, [operation, args]] of operations.entries()) {
    expectedRequestId = String(101 + index);
    await assert.rejects(client.execute(operation, args, {
      requestId: BigInt(101 + index),
      idempotencyToken: BigInt(101 + index),
    }));
  }
  assert.deepEqual(seen, operations.map(() => ({
    url: "https://example.test/v2/security/keys",
    minor: "3,4,5,6",
  })));
});
