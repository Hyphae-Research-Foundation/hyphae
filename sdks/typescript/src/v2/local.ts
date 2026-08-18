// SPDX-License-Identifier: Apache-2.0

import { ClientError, ProductError, productError, type RequestOptions, type Response, type Transport } from "./models.js";
import {
  FRAME_HEADER_SIZE,
  FRAME_KIND,
  blake3,
  decodeEnd,
  decodeFrame,
  decodeProductError,
  decodeProductResponse,
  decodeWelcome,
  encodeAuthenticatedHello,
  encodeCancel,
  encodeFrame,
  encodeHello,
  encodeProductRequest,
  encodeWindowUpdate,
} from "./protocol.js";

/** Runtime-injected exact byte stream for AF_UNIX sockets or Windows named pipes. */
export interface LocalByteStream {
  readExact(length: number, signal?: AbortSignal): Promise<Uint8Array>;
  write(encoded: Uint8Array, signal?: AbortSignal): Promise<void>;
  close(): Promise<void> | void;
}

export type LocalConnector = (endpoint: string, signal?: AbortSignal) => Promise<LocalByteStream>;

export interface LocalTransportOptions {
  readonly clientIdentity?: string;
  readonly apiKey?: string;
  readonly maximumPending?: number;
}

/**
 * Native-local transport entry point.
 *
 * Node runtimes supply a connector backed by `node:net` for AF_UNIX paths and
 * bare Windows named-pipe namespaces. The connector carries exact HYPHLCL1 bytes;
 * it must not add length prefixes, JSON, or another wrapper protocol.
 */
export class LocalTransport implements Transport {
  readonly #endpoint: string;
  readonly #connector: LocalConnector;
  readonly #clientIdentity: string;
  #apiKey: Uint8Array | undefined;
  #stream: LocalByteStream | undefined;
  #nextStreamId = 1;
  #active: Promise<void> = Promise.resolve();
  #initialWindow = 64 * 1024;
  #maximumFramePayload = 16 * 1024 * 1024;
  #negotiatedMinor: number | undefined;
  readonly #maximumPending: number;
  #pending = 0;
  #closed = false;

  constructor(endpoint: string, connector: LocalConnector, options: LocalTransportOptions | string = {}) {
    const { clientIdentity = "hyphae-typescript-sdk-v2", apiKey, maximumPending = 64 } =
      typeof options === "string" ? { clientIdentity: options } : options;
    if (endpoint.length === 0) throw new ClientError("local endpoint must not be empty");
    if (typeof connector !== "function") throw new ClientError("a platform local connector is required");
    if (clientIdentity.length === 0 || new TextEncoder().encode(clientIdentity).byteLength > 4096) throw new ClientError("local client identity is invalid");
    if (!Number.isSafeInteger(maximumPending) || maximumPending <= 0 || maximumPending > 4096) throw new ClientError("local pending request bound is invalid");
    this.#endpoint = endpoint;
    this.#connector = connector;
    this.#clientIdentity = clientIdentity;
    if (apiKey !== undefined) {
      const encoded = new TextEncoder().encode(apiKey);
      encodeAuthenticatedHello(encoded, clientIdentity);
      this.#apiKey = encoded;
    }
    this.#maximumPending = maximumPending;
  }

  async execute(
    operation: string,
    args: Readonly<Record<string, unknown>>,
    options: RequestOptions = {},
  ): Promise<Response> {
    if (this.#closed) throw new ClientError("local transport is closed");
    if (this.#pending >= this.#maximumPending) throw new ClientError("local pending request queue is full");
    this.#pending += 1;
    const prior = this.#active;
    let release = (): void => {};
    this.#active = new Promise<void>((resolve) => { release = resolve; });
    let admitted = false;
    try {
      await waitForTurn(prior, options);
      admitted = true;
      return await this.#executeSerial(operation, args, options);
    } finally {
      this.#pending -= 1;
      if (admitted) release();
      else void prior.then(release, release);
    }
  }

  async close(): Promise<void> {
    if (this.#closed) return;
    this.#closed = true;
    const stream = this.#stream;
    this.#stream = undefined;
    this.#negotiatedMinor = undefined;
    this.#apiKey?.fill(0);
    this.#apiKey = undefined;
    await stream?.close();
  }

  async #disconnect(): Promise<void> {
    const stream = this.#stream;
    this.#stream = undefined;
    this.#negotiatedMinor = undefined;
    await stream?.close();
  }

  async #executeSerial(
    operation: string,
    args: Readonly<Record<string, unknown>>,
    options: RequestOptions,
  ): Promise<Response> {
    const requestId = checkedRequestId(options.requestId);
    if (options.signal?.aborted === true) throw productError("cancelled", requestId);
    if (options.deadlineMicros !== undefined && options.deadlineMicros <= BigInt(Date.now()) * 1000n) {
      throw productError("deadline_exceeded", requestId);
    }
    const deadlineController = new AbortController();
    const cancelDeadline = (): void => deadlineController.abort(options.signal?.reason);
    options.signal?.addEventListener("abort", cancelDeadline, { once: true });
    const deadlineTimer = localDeadlineTimer(options.deadlineMicros, deadlineController);
    let stream: LocalByteStream | undefined;
    let streamId: number | undefined;
    try {
      const handshakeId = requestId === (1n << 64n) - 1n ? 1n : requestId + 1n;
      const terminalReplay = operation === "security_api_key_revoke_self"
        || operation === "security_api_key_rotate_self_activate";
      const freshTerminalReplay = terminalReplay && this.#stream === undefined;
      const terminalPayload = freshTerminalReplay
        ? encodeProductRequest(operation, args, options, 3)
        : undefined;
      stream = await this.#connection(
        handshakeId,
        deadlineController.signal,
        terminalPayload === undefined
          ? undefined
          : { streamId: 1, requestId, payload: terminalPayload },
      );
      if (terminalPayload !== undefined) this.#nextStreamId = 2;
      streamId = this.#nextStreamId;
      this.#nextStreamId = this.#nextStreamId === 0xffffffff ? 1 : this.#nextStreamId + 1;
      const payload = encodeProductRequest(operation, args, options, this.#negotiatedMinor);
      const kind = operation === "sql_prepare" ? FRAME_KIND.prepare : operation === "sql_deallocate" ? FRAME_KIND.deallocate : FRAME_KIND.execute;
      if (!freshTerminalReplay) {
        await this.#writeFrame(stream, kind, streamId, requestId, payload, deadlineController.signal);
      }
      const chunks: Uint8Array[] = [];
      let length = 0;
      let credited = 0;
      const maximum = options.limits?.maxResponseBytes ?? 16 * 1024 * 1024;
      for (;;) {
        if (deadlineController.signal.aborted) {
          throw deadlineController.signal.reason ?? new ClientError("local request interrupted");
        }
        const frame = await readFrame(stream, this.#maximumFramePayload, deadlineController.signal);
        if (frame.streamId !== streamId || frame.requestId !== requestId) {
          await this.#disconnect();
          throw new ClientError("local response correlation mismatch");
        }
        if (frame.kind === FRAME_KIND.failure) throw new ProductError(decodeProductError(frame.payload));
        if (frame.kind === FRAME_KIND.data) {
          length += frame.payload.byteLength;
          if (length > maximum) throw new ClientError("local response exceeds the configured maximum");
          chunks.push(frame.payload);
          credited += frame.payload.byteLength;
          if (credited >= Math.max(1, Math.floor(this.#initialWindow / 2))) {
            await this.#writeFrame(stream, FRAME_KIND.windowUpdate, streamId, requestId, encodeWindowUpdate(BigInt(credited)), deadlineController.signal);
            credited = 0;
          }
          continue;
        }
        if (frame.kind === FRAME_KIND.end) {
          const encoded = join(chunks, length);
          const completion = decodeEnd(frame.payload);
          if (completion.totalBytes !== BigInt(length) || !equal(completion.digest, blake3(encoded))) {
            throw new ClientError("local provisional response completion mismatch");
          }
          return decodeProductResponse(encoded, requestId, this.#negotiatedMinor);
        }
        await this.#disconnect();
        throw new ClientError("local server returned an invalid response frame");
      }
    } catch (cause) {
      if (deadlineController.signal.aborted) {
        if (stream !== undefined && streamId !== undefined) {
          await this.#writeFrame(stream, FRAME_KIND.cancel, streamId, requestId, encodeCancel()).catch(() => {});
        }
        await this.#disconnect().catch(() => {});
        throw productError(Boolean(options.signal?.aborted) ? "cancelled" : "deadline_exceeded", requestId);
      }
      if (stream !== undefined) {
        await this.#disconnect().catch(() => {});
      }
      throw cause;
    } finally {
      if (deadlineTimer !== undefined) clearTimeout(deadlineTimer);
      options.signal?.removeEventListener("abort", cancelDeadline);
    }
  }

  async #connection(
    requestId: bigint,
    signal?: AbortSignal,
    terminalRequest?: { readonly streamId: number; readonly requestId: bigint; readonly payload: Uint8Array },
  ): Promise<LocalByteStream> {
    if (this.#stream !== undefined) return this.#stream;
    const connecting = this.#connector(this.#endpoint, signal);
    let stream: LocalByteStream;
    try {
      stream = await abortable(connecting, signal);
    } catch (error) {
      if (signal?.aborted === true) {
        void connecting.then((late) => late.close()).catch(() => {});
      }
      throw error;
    }
    try {
      const hello = this.#apiKey === undefined
        ? encodeHello(this.#clientIdentity, 3)
        : encodeAuthenticatedHello(this.#apiKey, this.#clientIdentity, 3);
      try {
        await abortable(stream.write(encodeFrame(FRAME_KIND.hello, 0, requestId, hello), signal), signal);
        if (terminalRequest !== undefined) {
          await abortable(
            stream.write(
              encodeFrame(
                FRAME_KIND.execute,
                terminalRequest.streamId,
                terminalRequest.requestId,
                terminalRequest.payload,
              ),
              signal,
            ),
            signal,
          );
        }
      } finally {
        hello.fill(0);
      }
      const frame = await readFrame(stream, 16 * 1024 * 1024, signal);
      if (frame.kind === FRAME_KIND.failure) throw new ProductError(decodeProductError(frame.payload));
      if (frame.kind !== FRAME_KIND.welcome || frame.streamId !== 0 || frame.requestId !== requestId) {
        throw new ClientError("local handshake response mismatch");
      }
      const welcome = decodeWelcome(frame.payload);
      if (this.#apiKey !== undefined && (BigInt(welcome.capabilities ?? 0) & 0x80n) === 0n) {
        throw new ClientError("local server downgraded managed API-key authentication");
      }
      this.#negotiatedMinor = Number(welcome.minor);
      this.#initialWindow = Number(welcome.initialWindow);
      const maximumFramePayload = Number(welcome.maximumFramePayload);
      if (!Number.isInteger(maximumFramePayload) || maximumFramePayload <= 0 || maximumFramePayload > 16 * 1024 * 1024) {
        throw new ClientError("local handshake frame limit is invalid");
      }
      this.#maximumFramePayload = maximumFramePayload;
      this.#stream = stream;
      return stream;
    } catch (error) {
      await stream.close();
      throw error;
    }
  }

  async #writeFrame(
    stream: LocalByteStream,
    kind: number,
    streamId: number,
    requestId: bigint,
    payload: Uint8Array,
    signal?: AbortSignal,
  ): Promise<void> {
    if (payload.byteLength > this.#maximumFramePayload) {
      throw new ClientError("local frame exceeds the negotiated maximum");
    }
    await abortable(stream.write(encodeFrame(kind, streamId, requestId, payload), signal), signal);
  }

}

async function waitForTurn(prior: Promise<void>, options: RequestOptions): Promise<void> {
  if (options.signal?.aborted === true) throw productError("cancelled", checkedRequestId(options.requestId));
  if (options.deadlineMicros !== undefined && options.deadlineMicros <= BigInt(Date.now()) * 1000n) {
    throw productError("deadline_exceeded", checkedRequestId(options.requestId));
  }
  const controller = new AbortController();
  const cancel = (): void => controller.abort("cancelled");
  options.signal?.addEventListener("abort", cancel, { once: true });
  const timer = localDeadlineTimer(options.deadlineMicros, controller);
  try {
    await Promise.race([
      prior,
      new Promise<never>((_, reject) => controller.signal.addEventListener("abort", () => {
        reject(productError(options.signal?.aborted === true ? "cancelled" : "deadline_exceeded", checkedRequestId(options.requestId)));
      }, { once: true })),
    ]);
  } finally {
    if (timer !== undefined) clearTimeout(timer);
    options.signal?.removeEventListener("abort", cancel);
  }
}

function localDeadlineTimer(
  deadlineMicros: bigint | undefined,
  controller: AbortController,
): ReturnType<typeof setTimeout> | undefined {
  if (deadlineMicros === undefined) return undefined;
  const remaining = deadlineMicros - BigInt(Date.now()) * 1000n;
  return setTimeout(() => controller.abort(), Math.max(0, Number((remaining + 999n) / 1000n)));
}

async function readFrame(stream: LocalByteStream, maximumPayload: number, signal?: AbortSignal) {
  const header = await abortable(stream.readExact(FRAME_HEADER_SIZE, signal), signal);
  const length = new DataView(header.buffer, header.byteOffset).getUint32(24, true);
  if (length > maximumPayload) throw new ClientError("local frame exceeds the negotiated maximum");
  return decodeFrame(join([header, await abortable(stream.readExact(length, signal), signal)], FRAME_HEADER_SIZE + length));
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

function checkedRequestId(value?: bigint): bigint {
  const requestId = value ?? (BigInt(Date.now()) << 16n) | BigInt(Math.floor(Math.random() * 65536));
  if (requestId <= 0n || requestId > (1n << 64n) - 1n) throw new ClientError("request ID must be a nonzero u64");
  return requestId;
}

function join(values: readonly Uint8Array[], length: number): Uint8Array {
  const output = new Uint8Array(length);
  let offset = 0;
  for (const value of values) {
    output.set(value, offset);
    offset += value.byteLength;
  }
  return output;
}

function equal(left: Uint8Array, right: Uint8Array): boolean {
  return left.byteLength === right.byteLength && left.every((value, index) => value === right[index]);
}
