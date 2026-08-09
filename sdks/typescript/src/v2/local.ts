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
  #stream: LocalByteStream | undefined;
  #nextStreamId = 1;
  #active: Promise<void> = Promise.resolve();
  #initialWindow = 64 * 1024;
  #maximumFramePayload = 16 * 1024 * 1024;

  constructor(endpoint: string, connector: LocalConnector, clientIdentity = "hyphae-typescript-sdk-v2") {
    if (endpoint.length === 0) throw new ClientError("local endpoint must not be empty");
    if (typeof connector !== "function") throw new ClientError("a platform local connector is required");
    if (clientIdentity.length === 0 || new TextEncoder().encode(clientIdentity).byteLength > 4096) throw new ClientError("local client identity is invalid");
    this.#endpoint = endpoint;
    this.#connector = connector;
    this.#clientIdentity = clientIdentity;
  }

  async execute(
    operation: string,
    args: Readonly<Record<string, unknown>>,
    options: RequestOptions = {},
  ): Promise<Response> {
    const prior = this.#active;
    let release = (): void => {};
    this.#active = new Promise<void>((resolve) => { release = resolve; });
    await prior;
    try {
      return await this.#executeSerial(operation, args, options);
    } finally {
      release();
    }
  }

  async close(): Promise<void> {
    const stream = this.#stream;
    this.#stream = undefined;
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
    try {
      const handshakeId = requestId === (1n << 64n) - 1n ? 1n : requestId + 1n;
      const stream = await this.#connection(handshakeId, deadlineController.signal);
      const streamId = this.#nextStreamId;
      this.#nextStreamId = this.#nextStreamId === 0xffffffff ? 1 : this.#nextStreamId + 1;
      const payload = encodeProductRequest(operation, args, options);
      const kind = operation === "sql_prepare" ? FRAME_KIND.prepare : operation === "sql_deallocate" ? FRAME_KIND.deallocate : FRAME_KIND.execute;
      await this.#writeFrame(stream, kind, streamId, requestId, payload, deadlineController.signal);
      const chunks: Uint8Array[] = [];
      let length = 0;
      let credited = 0;
      const maximum = options.limits?.maxResponseBytes ?? 16 * 1024 * 1024;
      for (;;) {
        if (deadlineController.signal.aborted) {
          await this.#writeFrame(stream, FRAME_KIND.cancel, streamId, requestId, encodeCancel());
          await this.close();
          if (options.signal !== undefined) throw productError("cancelled", requestId);
          throw productError("deadline_exceeded", requestId);
        }
        const frame = await readFrame(stream, this.#maximumFramePayload, deadlineController.signal);
        if (frame.streamId !== streamId || frame.requestId !== requestId) {
          await this.close();
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
          return decodeProductResponse(encoded, requestId);
        }
        await this.close();
        throw new ClientError("local server returned an invalid response frame");
      }
    } finally {
      if (deadlineTimer !== undefined) clearTimeout(deadlineTimer);
      options.signal?.removeEventListener("abort", cancelDeadline);
    }
  }

  async #connection(requestId: bigint, signal?: AbortSignal): Promise<LocalByteStream> {
    if (this.#stream !== undefined) return this.#stream;
    const stream = await this.#connector(this.#endpoint, signal);
    try {
      await stream.write(encodeFrame(FRAME_KIND.hello, 0, requestId, encodeHello(this.#clientIdentity)), signal);
      const frame = await readFrame(stream, 16 * 1024 * 1024, signal);
      if (frame.kind === FRAME_KIND.failure) throw new ProductError(decodeProductError(frame.payload));
      if (frame.kind !== FRAME_KIND.welcome || frame.streamId !== 0 || frame.requestId !== requestId) {
        throw new ClientError("local handshake response mismatch");
      }
      const welcome = decodeWelcome(frame.payload);
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
    await stream.write(encodeFrame(kind, streamId, requestId, payload), signal);
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
  const header = await stream.readExact(FRAME_HEADER_SIZE, signal);
  const length = new DataView(header.buffer, header.byteOffset).getUint32(24, true);
  if (length > maximumPayload) throw new ClientError("local frame exceeds the negotiated maximum");
  return decodeFrame(join([header, await stream.readExact(length, signal)], FRAME_HEADER_SIZE + length));
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
