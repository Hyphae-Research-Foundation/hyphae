// SPDX-License-Identifier: Apache-2.0

import { ClientError, ProductError, productError, type RequestOptions, type Response, type Transport } from "./models.js";
import { decodeProductError, decodeProductResponse, encodeProductRequest, operationRequiredMinor } from "./protocol.js";

/** Every protocol minor this build speaks, ascending. The request offers the
 * whole set and the server echoes its selection, which must be a member. */
const SUPPORTED_PROTOCOL_MINORS: readonly number[] = [3];

export const PRODUCT_MEDIA_TYPE = "application/vnd.hyphae.product-v1";
export const ERROR_MEDIA_TYPE = "application/vnd.hyphae.error-v1";

export interface HttpTransportOptions {
  readonly bearerToken?: string;
  readonly responseBytes?: number;
  readonly fetch?: typeof globalThis.fetch;
  readonly maximumPending?: number;
}

/** Bounded binary-envelope transport for HTTP `/v2/execute`. */
export class HttpTransport implements Transport {
  readonly #origin: URL;
  #bearerToken: Uint8Array | undefined;
  readonly #responseBytes: number;
  readonly #fetch: typeof globalThis.fetch;
  #sessionId: string | undefined;
  readonly #maximumPending: number;
  #pending = 0;
  #closed = false;
  #negotiatedMinor = 3;

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
    if (origin.protocol === "http:" && !isCanonicalLoopback(origin)) {
      throw new ClientError("Hyphae HTTP v2 requires HTTPS outside canonical loopback");
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
    this.#bearerToken = options.bearerToken === undefined ? undefined : new TextEncoder().encode(options.bearerToken);
    this.#fetch = options.fetch ?? globalThis.fetch;
    this.#maximumPending = options.maximumPending ?? 64;
    if (!Number.isSafeInteger(this.#maximumPending) || this.#maximumPending <= 0 || this.#maximumPending > 4096) {
      throw new ClientError("invalid HTTP v2 pending request bound");
    }
  }

  async execute(
    operation: string,
    args: Readonly<Record<string, unknown>>,
    options: RequestOptions = {},
  ): Promise<Response> {
    if (this.#closed) throw new ClientError("HTTP v2 transport is closed");
    if (this.#pending >= this.#maximumPending) throw new ClientError("HTTP v2 pending request queue is full");
    this.#pending += 1;
    try {
      return await this.#execute(operation, args, options);
    } finally {
      this.#pending -= 1;
    }
  }

  async #execute(
    operation: string,
    args: Readonly<Record<string, unknown>>,
    options: RequestOptions,
  ): Promise<Response> {
    const requestId = checkedRequestId(options.requestId);
    if (options.signal?.aborted === true) throw productError("cancelled", requestId);
    if (options.deadlineMicros !== undefined && options.deadlineMicros <= BigInt(Date.now()) * 1000n) {
      throw productError("deadline_exceeded", requestId);
    }
    const body = encodeProductRequest(operation, args, options, this.#negotiatedMinor);
    const keyLifecycle = operation.startsWith("security_api_key_") || operation === "security_legacy_bearer_revoke";
    const oneTimeSecret = operation.endsWith("_start") && operation.startsWith("security_api_key_");
    const controller = new AbortController();
    const cancel = (): void => controller.abort(options.signal?.reason);
    options.signal?.addEventListener("abort", cancel, { once: true });
    const timeout = deadlineTimeout(options.deadlineMicros, controller, requestId);
    const headers = new Headers({
      accept: `${PRODUCT_MEDIA_TYPE}, ${ERROR_MEDIA_TYPE}`,
      "content-type": PRODUCT_MEDIA_TYPE,
      "x-hyphae-protocol-minor": SUPPORTED_PROTOCOL_MINORS.join(","),
      "x-hyphae-request-id": requestId.toString(),
    });
    if (options.deadlineMicros !== undefined) headers.set("x-hyphae-deadline-micros", options.deadlineMicros.toString());
    if (this.#bearerToken !== undefined) headers.set("authorization", `Bearer ${new TextDecoder().decode(this.#bearerToken)}`);
    if (this.#sessionId !== undefined) headers.set("x-hyphae-session-id", this.#sessionId);
    let response: globalThis.Response;
    try {
      const fetching = this.#fetch(new URL(keyLifecycle ? "/v2/security/keys" : "/v2/execute", this.#origin), {
        method: "POST",
        headers,
        body: body.buffer.slice(body.byteOffset, body.byteOffset + body.byteLength) as ArrayBuffer,
        redirect: "error",
        signal: controller.signal,
      });
      void fetching.then((late) => {
        if (controller.signal.aborted) void late.body?.cancel(controller.signal.reason).catch(() => {});
      }, () => {});
      response = await abortable(fetching, controller.signal);
      const minor = response.headers.get("x-hyphae-protocol-minor");
      if (minor === null || !/^[0-9]{1,3}$/u.test(minor) || !SUPPORTED_PROTOCOL_MINORS.includes(Number(minor))) {
        throw new ClientError("HTTP v2 protocol minor is missing or unsupported");
      }
      const responseRequestId = response.headers.get("x-hyphae-request-id");
      const responseSessionId = response.headers.get("x-hyphae-session-id");
      if (responseRequestId !== requestId.toString()) {
        throw new ClientError("HTTP v2 response request ID mismatch");
      }
      if (responseSessionId !== null && !/^(?!0{32}$)[0-9a-f]{32}$/u.test(responseSessionId)) {
        throw new ClientError("HTTP v2 response session ID is invalid");
      }
      const negotiatedMinor = Number(minor);
      if (negotiatedMinor < operationRequiredMinor(operation, args)) {
        throw new ClientError("native operation is unavailable at the negotiated protocol minor");
      }
      const maximum = Math.min(this.#responseBytes, options.limits?.maxResponseBytes ?? this.#responseBytes);
      if (oneTimeSecret && response.ok && (response.headers.get("cache-control") !== "no-store, private, max-age=0" ||
          response.headers.get("pragma") !== "no-cache" || response.headers.has("content-encoding"))) {
        throw new ClientError("HTTP API-key secret response is not cache-safe");
      }
      this.#negotiatedMinor = negotiatedMinor;
      this.#sessionId = responseSessionId ?? this.#sessionId;
      const encoded = await readBounded(response, maximum, controller.signal);
      const mediaType = response.headers.get("content-type")?.split(";", 1)[0]?.trim().toLowerCase();
      if (response.ok) {
        if (response.status !== 200 || mediaType !== PRODUCT_MEDIA_TYPE) {
          throw new ClientError("HTTP v2 returned an unexpected status or media type");
        }
        return decodeProductResponse(encoded, requestId, this.#negotiatedMinor);
      }
      if (mediaType !== ERROR_MEDIA_TYPE) {
        throw new ClientError("HTTP v2 failure did not contain a typed product error");
      }
      throw new ProductError(decodeProductError(encoded), response.status);
    } catch (cause) {
      if (controller.signal.aborted && options.deadlineMicros !== undefined &&
          options.deadlineMicros <= BigInt(Date.now()) * 1000n) {
        throw productError("deadline_exceeded", requestId);
      }
      if (options.signal !== undefined && Boolean(options.signal.aborted)) {
        throw productError("cancelled", requestId);
      }
      if (cause instanceof ProductError || cause instanceof ClientError) throw cause;
      throw new ClientError("Hyphae HTTP v2 transport failed", { cause });
    } finally {
      if (timeout !== undefined) clearTimeout(timeout);
      options.signal?.removeEventListener("abort", cancel);
    }
  }

  close(): void {
    if (this.#closed) return;
    this.#closed = true;
    this.#bearerToken?.fill(0);
    this.#bearerToken = undefined;
    this.#sessionId = undefined;
  }
}

function deadlineTimeout(
  deadlineMicros: bigint | undefined,
  controller: AbortController,
  requestId: bigint,
): ReturnType<typeof setTimeout> | undefined {
  if (deadlineMicros === undefined) return undefined;
  const remainingMicros = deadlineMicros - BigInt(Date.now()) * 1000n;
  const delay = remainingMicros <= 0n ? 0 : Number((remainingMicros + 999n) / 1000n);
  return setTimeout(() => controller.abort(productError("deadline_exceeded", requestId)), delay);
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
      const item = await abortableRead(reader, signal);
      if (item.done) break;
      length += item.value.byteLength;
      if (length > maximum) throw new ClientError("HTTP v2 response exceeds the configured maximum");
      chunks.push(item.value);
    }
  } finally {
    if (signal?.aborted === true) await reader.cancel(signal.reason).catch(() => {});
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

async function abortableRead(
  reader: ReadableStreamDefaultReader<Uint8Array>,
  signal?: AbortSignal,
): Promise<ReadableStreamReadResult<Uint8Array>> {
  return abortable(reader.read(), signal);
}

async function abortable<T>(operation: Promise<T>, signal?: AbortSignal): Promise<T> {
  if (signal === undefined) return operation;
  signal.throwIfAborted();
  let abort: (() => void) | undefined;
  const interrupted = new Promise<never>((_, reject) => {
    abort = (): void => reject(signal.reason ?? new DOMException("aborted", "AbortError"));
    signal.addEventListener("abort", abort, { once: true });
  });
  try {
    return await Promise.race([operation, interrupted]);
  } finally {
    if (abort !== undefined) signal.removeEventListener("abort", abort);
  }
}

function isCanonicalLoopback(origin: URL): boolean {
  const host = origin.hostname.toLowerCase();
  if (host === "localhost" || host === "[::1]" || host === "::1") return true;
  if (!/^\d{1,3}(?:\.\d{1,3}){3}$/u.test(host)) return false;
  const octets = host.split(".").map(Number);
  return octets.length === 4 && octets[0] === 127 && octets.every((octet) => octet >= 0 && octet <= 255);
}

function checkedRequestId(value?: bigint): bigint {
  const requestId = value ?? (BigInt(Date.now()) << 16n) | BigInt(Math.floor(Math.random() * 65536));
  if (requestId <= 0n || requestId > (1n << 64n) - 1n) throw new ClientError("request ID must be a nonzero u64");
  return requestId;
}
