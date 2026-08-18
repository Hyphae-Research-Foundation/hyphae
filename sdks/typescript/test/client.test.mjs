// SPDX-License-Identifier: Apache-2.0

import assert from "node:assert/strict";
import test from "node:test";

import {
  HyphaeApiError,
  HyphaeClient,
  HyphaeClientError,
  parseHyphaeJson,
  stringifyHyphaeJson,
} from "../dist/index.js";

test("client rejects non-origin URLs and unsafe secrets", () => {
  assert.throws(() => new HyphaeClient("file:///tmp/hyphae"), HyphaeClientError);
  assert.throws(() => new HyphaeClient("https://example.test/prefix"), HyphaeClientError);
  assert.throws(
    () => new HyphaeClient("https://example.test", { bearerToken: "bad\nsecret" }),
    HyphaeClientError,
  );
  for (const timeoutMs of [Number.NaN, Number.POSITIVE_INFINITY, Number.NEGATIVE_INFINITY]) {
    assert.throws(
      () => new HyphaeClient("https://example.test", { timeoutMs }),
      HyphaeClientError,
    );
  }
});

test("client decodes a correlated bounded JSON response", async () => {
  const client = new HyphaeClient("https://example.test", {
    fetch: async () => new Response('{"status":"live"}', {
      status: 200,
      headers: { "content-type": "application/json", "x-request-id": "request-1" },
    }),
  });
  assert.deepEqual(await client.liveness(), { value: { status: "live" }, requestId: "request-1" });
});

test("client exposes stable API errors", async () => {
  const client = new HyphaeClient("https://example.test", {
    fetch: async () => new Response(
      '{"code":"idempotency_conflict","message":"conflict","request_id":"request-2"}',
      {
        status: 409,
        headers: { "content-type": "application/json", "x-request-id": "request-2" },
      },
    ),
  });
  await assert.rejects(
    client.put({ records: [] }),
    (error) => error instanceof HyphaeApiError && error.status === 409 &&
      error.code === "idempotency_conflict" && error.requestId === "request-2",
  );
});

test("client enforces the streaming byte bound", async () => {
  const client = new HyphaeClient("https://example.test", {
    responseBytes: 4,
    fetch: async () => new Response('{"status":"live"}', {
      status: 200,
      headers: { "content-type": "application/json", "x-request-id": "request-3" },
    }),
  });
  await assert.rejects(client.liveness(), HyphaeClientError);
});

test("a false Content-Length cannot bypass the streaming byte bound", async () => {
  const client = new HyphaeClient("https://example.test", {
    responseBytes: 4,
    fetch: async () => new Response("12345", {
      status: 200,
      headers: {
        "content-length": "1",
        "content-type": "application/json",
        "x-request-id": "request-false-length",
      },
    }),
  });
  await assert.rejects(
    client.liveness(),
    (error) => error instanceof HyphaeClientError &&
      error.message === "Hyphae response exceeded local limit 4 bytes",
  );
});

test("body AbortError is normalized to HyphaeClientError", async () => {
  const client = new HyphaeClient("https://example.test", {
    fetch: async () => new Response(new ReadableStream({
      start(controller) {
        controller.error(new DOMException("aborted", "AbortError"));
      },
    }), {
      status: 200,
      headers: { "content-type": "application/json", "x-request-id": "request-abort" },
    }),
  });
  await assert.rejects(
    client.liveness(),
    (error) => error instanceof HyphaeClientError &&
      error.message === "Hyphae HTTP transport failed",
  );
});

test("complete deadline rejects an injected fetch that ignores its signal", async () => {
  let timer;
  const client = new HyphaeClient("https://example.test", {
    timeoutMs: 20,
    fetch: async () => new Promise((resolve) => {
      timer = setTimeout(() => resolve(new Response()), 10_000);
    }),
  });
  const started = performance.now();
  try {
    await assert.rejects(
      client.liveness(),
      (error) => error instanceof HyphaeClientError &&
        error.message === "Hyphae HTTP request/response deadline elapsed",
    );
  } finally {
    clearTimeout(timer);
  }
  assert.ok(performance.now() - started < 500);
});

test("complete deadline cancels stalled success, error, and witness bodies", async () => {
  const cases = [
    {
      response: (body) => new Response(body, {
        status: 200,
        headers: { "content-type": "application/json", "x-request-id": "request-success" },
      }),
      operation: (client) => client.liveness(),
    },
    {
      response: (body) => new Response(body, {
        status: 422,
        headers: { "content-type": "application/json", "x-request-id": "request-error" },
      }),
      operation: (client) => client.liveness(),
    },
    {
      response: (body) => new Response(body, {
        status: 200,
        headers: { "digest": "blake3=abc", "x-request-id": "request-witness" },
      }),
      operation: (client) => client.downloadWitness({
        checkpoint_sequence: 1,
        snapshot_digest: "abc",
        witness: { file_bytes: 8, path: "/v1/witnesses/1/abc" },
      }),
    },
  ];
  for (const entry of cases) {
    let cancelled = false;
    const body = new ReadableStream({
      cancel() {
        cancelled = true;
      },
    });
    const client = new HyphaeClient("https://example.test", {
      timeoutMs: 20,
      fetch: async () => entry.response(body),
    });
    await assert.rejects(
      entry.operation(client),
      (error) => error instanceof HyphaeClientError &&
        error.message === "Hyphae HTTP request/response deadline elapsed",
    );
    await new Promise((resolve) => setTimeout(resolve, 0));
    assert.equal(cancelled, true);
  }
});

test("complete deadline includes synchronous request serialization", async () => {
  let fetchCalled = false;
  const client = new HyphaeClient("https://example.test", {
    timeoutMs: 20,
    fetch: async () => {
      fetchCalled = true;
      return new Response(
        '{"status":"committed","transaction_id":"t","commit_sequence":1,"commit_digest":"d","transaction_digest":"x"}',
        {
          status: 200,
          headers: { "content-type": "application/json", "x-request-id": "request-late" },
        },
      );
    },
  });
  const request = {};
  Object.defineProperty(request, "records", {
    enumerable: true,
    get() {
      const stop = performance.now() + 75;
      while (performance.now() < stop) {
        // Deliberately consume the complete deadline before fetch starts.
      }
      return [];
    },
  });
  await assert.rejects(
    client.put(request),
    (error) => error instanceof HyphaeClientError &&
      error.message === "Hyphae HTTP request/response deadline elapsed",
  );
  assert.equal(fetchCalled, false);
});

test("client disables redirect following at the fetch boundary", async () => {
  let redirect;
  const client = new HyphaeClient("https://example.test", {
    fetch: async (_url, options) => {
      redirect = options?.redirect;
      return new Response('{"status":"live"}', {
        status: 200,
        headers: { "content-type": "application/json", "x-request-id": "request-redirect" },
      });
    },
  });
  await client.liveness();
  assert.equal(redirect, "error");
});

test("Hyphae JSON preserves every signed 64-bit integer", () => {
  const decoded = parseHyphaeJson('{"minimum":-9223372036854775808,"maximum":9223372036854775807}');
  assert.deepEqual(decoded, {
    minimum: -9223372036854775808n,
    maximum: 9223372036854775807n,
  });
  assert.equal(
    stringifyHyphaeJson(decoded),
    '{"minimum":-9223372036854775808,"maximum":9223372036854775807}',
  );
  assert.throws(() => stringifyHyphaeJson({ rounded: 9007199254740992 }), TypeError);
});

test("client emits bigint request values as exact integer tokens", async () => {
  let body;
  const client = new HyphaeClient("https://example.test", {
    fetch: async (_url, options) => {
      body = options?.body;
      return new Response(
        '{"status":"committed","transaction_id":"t","commit_sequence":1,"commit_digest":"d","transaction_digest":"x"}',
        {
          status: 200,
          headers: { "content-type": "application/json", "x-request-id": "request-4" },
        },
      );
    },
  });
  await client.put({ records: [{ key_hex: "61", value: 9223372036854775807n }] });
  assert.equal(body, '{"records":[{"key_hex":"61","value":9223372036854775807}]}');
});
