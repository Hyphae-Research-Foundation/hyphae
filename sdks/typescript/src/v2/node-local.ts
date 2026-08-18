// SPDX-License-Identifier: Apache-2.0

import { createConnection, type Socket } from "node:net";

import { ClientError } from "./models.js";
import type { LocalByteStream, LocalConnector } from "./local.js";

/** Node.js AF_UNIX and Windows named-pipe connector carrying exact HYPHLCL1 bytes. */
export const nodeLocalConnector: LocalConnector = async (endpoint, signal) => {
  signal?.throwIfAborted();
  if (process.platform === "win32") endpoint = windowsPipePath(endpoint);
  const socket = await new Promise<Socket>((resolve, reject) => {
    const candidate = createConnection(endpoint);
    const onError = (error: Error): void => {
      candidate.destroy();
      reject(new ClientError("native-local connection failed", { cause: error }));
    };
    const onAbort = (): void => {
      candidate.destroy();
      reject(new ClientError("native-local connection was cancelled"));
    };
    candidate.once("error", onError);
    signal?.addEventListener("abort", onAbort, { once: true });
    candidate.once("connect", () => {
      candidate.off("error", onError);
      signal?.removeEventListener("abort", onAbort);
      resolve(candidate);
    });
  });
  return new NodeLocalByteStream(socket);
};

export function windowsPipePath(endpoint: string): string {
  const prefix = "\\\\.\\pipe\\";
  if (endpoint.toLowerCase().startsWith(prefix.toLowerCase())) endpoint = endpoint.slice(prefix.length);
  if (endpoint.length === 0 || endpoint.startsWith("\\\\")) {
    throw new ClientError("Windows local endpoint must be a local named-pipe namespace");
  }
  return prefix + endpoint;
}

class NodeLocalByteStream implements LocalByteStream {
  readonly #socket: Socket;
  #buffer = new Uint8Array();
  #ended = false;
  #error: Error | undefined;
  readonly #waiters = new Set<() => void>();

  constructor(socket: Socket) {
    this.#socket = socket;
    socket.on("data", (chunk: Buffer) => {
      const joined = new Uint8Array(this.#buffer.byteLength + chunk.byteLength);
      joined.set(this.#buffer);
      joined.set(chunk, this.#buffer.byteLength);
      this.#buffer = joined;
      this.#wake();
    });
    socket.once("end", () => {
      this.#ended = true;
      this.#wake();
    });
    socket.once("error", (error) => {
      this.#error = error;
      this.#wake();
    });
  }

  async readExact(length: number, signal?: AbortSignal): Promise<Uint8Array> {
    while (this.#buffer.byteLength < length) {
      signal?.throwIfAborted();
      if (this.#error !== undefined) throw new ClientError("native-local read failed", { cause: this.#error });
      if (this.#ended) throw new ClientError("native-local stream ended before completion");
      await new Promise<void>((resolve, reject) => {
        const wake = (): void => {
          signal?.removeEventListener("abort", abort);
          resolve();
        };
        const abort = (): void => {
          this.#waiters.delete(wake);
          reject(new ClientError("native-local read was cancelled"));
        };
        this.#waiters.add(wake);
        signal?.addEventListener("abort", abort, { once: true });
      });
    }
    const value = this.#buffer.slice(0, length);
    this.#buffer = this.#buffer.slice(length);
    return value;
  }

  async write(encoded: Uint8Array, signal?: AbortSignal): Promise<void> {
    signal?.throwIfAborted();
    await new Promise<void>((resolve, reject) => {
      this.#socket.write(encoded, (error?: Error | null) => {
        if (error === null || error === undefined) resolve();
        else reject(new ClientError("native-local write failed", { cause: error }));
      });
    });
  }

  async close(): Promise<void> {
    if (this.#socket.destroyed) return;
    await new Promise<void>((resolve) => {
      this.#socket.once("close", () => resolve());
      this.#socket.destroy();
    });
  }

  #wake(): void {
    for (const waiter of this.#waiters) waiter();
    this.#waiters.clear();
  }
}
