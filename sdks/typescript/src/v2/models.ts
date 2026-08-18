// SPDX-License-Identifier: Apache-2.0

export interface ProductLimits {
  readonly maxCount: number;
  readonly maxRequestBytes: number;
  readonly maxResponseBytes: number;
  readonly maxWorkUnits: number;
  readonly maxMemoryBytes: number;
}

export const DEFAULT_LIMITS: ProductLimits = {
  maxCount: 4096,
  maxRequestBytes: 16 * 1024 * 1024,
  maxResponseBytes: 16 * 1024 * 1024,
  maxWorkUnits: 1_000_000,
  maxMemoryBytes: 64 * 1024 * 1024,
};

export type Durability = "strict" | "group" | "memory";
export type TransactionState = "none" | "active" | "rolled-back" | "committed" | "outcome-unknown";

export interface ProductErrorFields {
  readonly code: string;
  readonly category: string;
  readonly retry: string;
  readonly message: string;
  readonly requestId?: bigint;
  readonly traceId?: bigint;
  readonly objectId?: bigint;
  readonly transactionState: TransactionState;
  readonly transactionId?: bigint;
  readonly limit?: { readonly kind: string; readonly configured: bigint; readonly observed: bigint };
  readonly sourceSpan?: { readonly start: number; readonly end: number };
  readonly details: Readonly<Record<string, unknown>>;
}

export class ProductError extends Error {
  readonly fields: ProductErrorFields;
  readonly status: number | undefined;

  constructor(fields: ProductErrorFields, status?: number) {
    if (status !== undefined && (!Number.isInteger(status) || status < 400 || status > 599)) {
      throw new ClientError("HTTP product error status is invalid");
    }
    super(fields.message);
    this.name = "ProductError";
    this.fields = fields;
    this.status = status;
  }
}

export function productError(code: "cancelled" | "deadline_exceeded", requestId: bigint): ProductError {
  const fields = code === "cancelled"
    ? { category: "cancelled", retry: "same-request", message: "native product request was cancelled" }
    : { category: "deadline", retry: "same-request", message: "native product request deadline exceeded" };
  return new ProductError({ code, ...fields, requestId, transactionState: "none", details: {} });
}

export class ClientError extends Error {
  constructor(message: string, options?: ErrorOptions) {
    super(message, options);
    this.name = "ClientError";
  }
}

/** One-time secret bytes that never stringify or serialize in clear text. */
export class SensitiveBytes {
  #value: Uint8Array | undefined;

  constructor(value: Uint8Array) {
    this.#value = value.slice();
  }

  toString(): string {
    return "SensitiveBytes([REDACTED])";
  }

  toJSON(): never {
    throw new ClientError("sensitive bytes are not serializable");
  }

  [Symbol.for("nodejs.util.inspect.custom")](): string {
    return this.toString();
  }

  /** Returns one copy and immediately zeroizes and closes the wrapper. */
  consume(): Uint8Array {
    const value = this.#value;
    if (value === undefined) throw new ClientError("sensitive bytes are closed");
    const exposed = value.slice();
    value.fill(0);
    this.#value = undefined;
    return exposed;
  }

  close(): void {
    this.#value?.fill(0);
    this.#value = undefined;
  }
}

export interface RequestOptions {
  readonly requestId?: bigint;
  readonly logicalTimeMicros?: bigint;
  readonly deadlineMicros?: bigint;
  readonly idempotencyToken?: bigint;
  readonly limits?: ProductLimits;
  readonly durability?: Durability;
  readonly signal?: AbortSignal;
}

export interface Response<T = unknown> {
  readonly kind: string;
  readonly value: T;
  readonly requestId: bigint;
}

export interface Transport {
  execute(
    operation: string,
    args: Readonly<Record<string, unknown>>,
    options?: RequestOptions,
  ): Promise<Response>;
  close?(): Promise<void> | void;
}
