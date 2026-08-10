// SPDX-License-Identifier: GPL-3.0-only

import { ClientError, ProductError, productError, type RequestOptions, type Response, type Transport } from "./models.js";
import { decodeProductError, decodeProductResponse, encodeProductRequest } from "./protocol.js";

export const PRODUCT_MEDIA_TYPE = "application/vnd.hyphae.product-v1";
export const ERROR_MEDIA_TYPE = "application/vnd.hyphae.error-v1";

export interface HttpTransportOptions {
  readonly bearerToken?: string;
  readonly responseBytes?: number;
  readonly fetch?: typeof globalThis.fetch;
}

/** Bounded binary-envelope transport for HTTP `/v2/execute`. */
export class HttpTransport implements Transport {
  readonly #origin: URL;
  readonly #bearerToken: string | undefined;
  readonly #responseBytes: number;
  readonly #fetch: typeof globalThis.fetch;
  #sessionId: string | undefined;

  constructor(baseUrl: string, options: HttpTransportOptions = {}) {
    let origin: URL;
    try {
      origin = new URL(baseUrl);
    } catch (cause) {
      throw new ClientError("invalid Hyphae v2 base URL", { cause });
    }
    if ((origin.protocol !== "http:" && origin.protocol !== "https:") || origin.username !== "" ||
        origin.password !== "" || origin.pathname !== "/" || origin.search !== "" || origin.hash !== "") {
      throw new ClientError("base URL must be a root HTTP(S) origin");
    }
    if (options.bearerToken !== undefined &&
        (options.bearerToken.length === 0 || /[\r\n]/u.test(options.bearerToken))) {
      throw new ClientError("invalid bearer token");
    }
    this.#responseBytes = options.responseBytes ?? 16 * 1024 * 1024;
    if (!Number.isSafeInteger(this.#responseBytes) || this.#responseBytes <= 0 || this.#responseBytes > 16 * 1024 * 1024) {
      throw new ClientError("invalid HTTP v2 response bound");
    }
    this.#origin = origin;
    this.#bearerToken = options.bearerToken;
    this.#fetch = options.fetch ?? globalThis.fetch;
  }

  async execute(
    operation: string,
    args: Readonly<Record<string, unknown>>,
    options: RequestOptions = {},
  ): Promise<Response> {
    const requestId = checkedRequestId(options.requestId);
    if (options.signal?.aborted === true) throw productError("cancelled", requestId);
    if (options.deadlineMicros !== undefined && options.deadlineMicros <= BigInt(Date.now()) * 1000n) {
      throw productError("deadline_exceeded", requestId);
    }
    const body = encodeProductRequest(operation, args, options);
    const controller = new AbortController();
    const cancel = (): void => controller.abort(options.signal?.reason);
    options.signal?.addEventListener("abort", cancel, { once: true });
    const timeout = deadlineTimeout(options.deadlineMicros, controller);
    const headers = new Headers({
      accept: `${PRODUCT_MEDIA_TYPE}, ${ERROR_MEDIA_TYPE}`,
      "content-type": PRODUCT_MEDIA_TYPE,
      "x-hyphae-request-id": requestId.toString(),
    });
    if (options.deadlineMicros !== undefined) headers.set("x-hyphae-deadline-micros", options.deadlineMicros.toString());
    if (this.#bearerToken !== undefined) headers.set("authorization", `Bearer ${this.#bearerToken}`);
    if (this.#sessionId !== undefined) headers.set("x-hyphae-session-id", this.#sessionId);
    let response: globalThis.Response;
    try {
      response = await this.#fetch(new URL("/v2/execute", this.#origin), {
        method: "POST",
        headers,
        body: body.buffer.slice(body.byteOffset, body.byteOffset + body.byteLength) as ArrayBuffer,
        redirect: "error",
        signal: controller.signal,
      });
    } catch (cause) {
      if (controller.signal.aborted && options.deadlineMicros !== undefined &&
          options.deadlineMicros <= BigInt(Date.now()) * 1000n) {
        throw productError("deadline_exceeded", requestId);
      }
      if (options.signal !== undefined) {
        throw productError("cancelled", requestId);
      }
      throw new ClientError("Hyphae HTTP v2 transport failed", { cause });
    } finally {
      if (timeout !== undefined) clearTimeout(timeout);
      options.signal?.removeEventListener("abort", cancel);
    }
    const maximum = Math.min(this.#responseBytes, options.limits?.maxResponseBytes ?? this.#responseBytes);
    this.#sessionId = response.headers.get("x-hyphae-session-id") ?? this.#sessionId;
    if (response.headers.get("x-hyphae-request-id") !== requestId.toString()) {
      throw new ClientError("HTTP v2 response request ID mismatch");
    }
    const encoded = await readBounded(response, maximum, options.signal);
    const mediaType = response.headers.get("content-type")?.split(";", 1)[0]?.trim().toLowerCase();
    if (response.ok) {
      if (response.status !== 200 || mediaType !== PRODUCT_MEDIA_TYPE) {
        throw new ClientError("HTTP v2 returned an unexpected status or media type");
      }
      return decodeProductResponse(encoded, requestId);
    }
    if (mediaType !== ERROR_MEDIA_TYPE) {
      throw new ClientError("HTTP v2 failure did not contain a typed product error");
    }
    throw new ProductError(decodeProductError(encoded), response.status);
  }
}

function deadlineTimeout(
  deadlineMicros: bigint | undefined,
  controller: AbortController,
): ReturnType<typeof setTimeout> | undefined {
  if (deadlineMicros === undefined) return undefined;
  const remainingMicros = deadlineMicros - BigInt(Date.now()) * 1000n;
  const delay = remainingMicros <= 0n ? 0 : Number((remainingMicros + 999n) / 1000n);
  return setTimeout(() => controller.abort(new ClientError("Hyphae v2 request deadline elapsed")), delay);
}

async function readBounded(response: globalThis.Response, maximum: number, signal?: AbortSignal): Promise<Uint8Array> {
  const declared = response.headers.get("content-length");
  if (declared !== null && (!/^\d+$/u.test(declared) || Number(declared) > maximum)) {
    throw new ClientError("HTTP v2 response exceeds the configured maximum");
  }
  if (response.body === null) return new Uint8Array();
  const reader = response.body.getReader();
  const chunks: Uint8Array[] = [];
  let length = 0;
  try {
    for (;;) {
      signal?.throwIfAborted();
      const item = await reader.read();
      if (item.done) break;
      length += item.value.byteLength;
      if (length > maximum) throw new ClientError("HTTP v2 response exceeds the configured maximum");
      chunks.push(item.value);
    }
  } finally {
    reader.releaseLock();
  }
  const encoded = new Uint8Array(length);
  let offset = 0;
  for (const chunk of chunks) {
    encoded.set(chunk, offset);
    offset += chunk.byteLength;
  }
  if (declared !== null && Number(declared) !== encoded.byteLength) {
    throw new ClientError("HTTP v2 response length differs from Content-Length");
  }
  return encoded;
}

function checkedRequestId(value?: bigint): bigint {
  const requestId = value ?? (BigInt(Date.now()) << 16n) | BigInt(Math.floor(Math.random() * 65536));
  if (requestId <= 0n || requestId > (1n << 64n) - 1n) throw new ClientError("request ID must be a nonzero u64");
  return requestId;
}
