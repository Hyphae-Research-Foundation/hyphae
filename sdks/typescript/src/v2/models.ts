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

export type ProductDocValue = boolean | bigint | number | string | Uint8Array | { readonly float: number };

export const CATALOG_OBJECT_KINDS = [
  "database",
  "schema",
  "relation",
  "secondary_index",
  "keyspace",
  "structure",
  "search_collection",
  "analyzer",
  "cross_engine_link",
  "embedding_profile",
] as const;
export type CatalogObjectKind = typeof CATALOG_OBJECT_KINDS[number];

export const CATALOG_DEPENDENCY_KINDS = [
  "parent",
  "secondary_index_relation",
  "foreign_key",
  "analyzer",
  "link_endpoint",
  "relation_schema",
  "embedding_profile",
] as const;
export type CatalogDependencyKind = typeof CATALOG_DEPENDENCY_KINDS[number];

/** One complete image whose map names use canonical UTF-8 byte order on wire. */
export interface ProductDocument {
  readonly object_id: bigint;
  readonly text: string;
  readonly doc_values?: Readonly<Record<string, ProductDocValue>>;
  readonly vectors?: Readonly<Record<string, readonly number[]>>;
}

/** One document whose vectors are produced from catalog-bound profiles. */
export interface EmbedAndIngestDocument {
  readonly object_id: bigint;
  readonly text: string;
  readonly doc_values?: Readonly<Record<string, ProductDocValue>>;
}

export interface EmbedAndIngestBatch {
  readonly idempotency_id: bigint;
  readonly documents: readonly EmbedAndIngestDocument[];
}

/** The selected executor reported by an embed-and-ingest response. */
export interface EmbeddingExecutionProfile {
  readonly embeddingProfile: bigint;
  readonly backend: "cpu" | "cuda";
  readonly device: string;
  readonly driver: string;
  readonly runtime: string;
  readonly precision: "f32";
  readonly kernels: readonly string[];
  readonly fallback: boolean;
}

export interface EmbedAndIngestResult {
  readonly snapshot: Readonly<Record<string, unknown>>;
  readonly documents: bigint;
  readonly idempotentReplay: boolean;
  readonly executionProfile: EmbeddingExecutionProfile;
  readonly commit: Readonly<Record<string, unknown>> | undefined;
}

export type ProductTransactionSearchMutation =
  | {
      readonly kind: "index";
      readonly index: bigint;
      readonly document_id: Uint8Array;
      readonly text: string;
    }
  | {
      readonly kind: "replace";
      readonly index: bigint;
      readonly document_id: Uint8Array;
      readonly text: string;
    }
  | {
      readonly kind: "delete";
      readonly index: bigint;
      readonly document_id: Uint8Array;
    }
  | {
      readonly kind: "document";
      readonly collection: bigint;
      readonly document: ProductDocument;
    };

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
