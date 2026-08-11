// SPDX-License-Identifier: AGPL-3.0-only

import type {
  CapabilitiesV1,
  CommitReceiptV1,
  DefineLexicalIndexRequestV1,
  DefineVectorSpaceRequestV1,
  DeleteRequestV1,
  DeleteVectorsRequestV1,
  ErrorV1,
  ExactRetrievalRequestV1,
  ExactRetrievalResponseV1,
  GetRequestV1,
  GetResponseV1,
  HealthV1,
  HybridRetrievalRequestV1,
  HybridRetrievalResponseV1,
  LexicalRetrievalRequestV1,
  LexicalRetrievalResponseV1,
  ProofV1,
  PutRequestV1,
  PutVectorsRequestV1,
  QueryRequestV1,
  QueryResponseV1,
  RetrievalProofV1,
} from "./generated.js";
import { parseHyphaeJson, stringifyHyphaeJson } from "./json.js";

const DEFAULT_RESPONSE_BYTES = 32 * 1024 * 1024;
const DEFAULT_WITNESS_BYTES = 512 * 1024 * 1024;
const DEFAULT_TIMEOUT_MS = 60_000;
const MAX_TIMER_SLICE_MS = 2_147_483_647;

class Deadline {
  readonly #expiresAt: number;
  readonly #controller = new AbortController();
  #timer: ReturnType<typeof setTimeout> | undefined;

  constructor(timeoutMs: number) {
    this.#expiresAt = performance.now() + timeoutMs;
    this.#schedule();
  }

  get signal(): AbortSignal {
    return this.#controller.signal;
  }

  elapsed(): boolean {
    return this.#controller.signal.aborted || performance.now() >= this.#expiresAt;
  }

  throwIfElapsed(): void {
    if (this.elapsed()) {
      this.#expire();
      throw deadlineError();
    }
  }

  async race<T>(operation: PromiseLike<T>): Promise<T> {
    this.throwIfElapsed();
    let rejectDeadline: (() => void) | undefined;
    const expiration = new Promise<never>((_resolve, reject) => {
      rejectDeadline = () => reject(deadlineError());
      this.#controller.signal.addEventListener("abort", rejectDeadline, { once: true });
    });
    try {
      const result = await Promise.race([Promise.resolve(operation), expiration]);
      this.throwIfElapsed();
      return result;
    } finally {
      if (rejectDeadline !== undefined) {
        this.#controller.signal.removeEventListener("abort", rejectDeadline);
      }
    }
  }

  close(): void {
    if (this.#timer !== undefined) {
      clearTimeout(this.#timer);
      this.#timer = undefined;
    }
    this.#expire();
  }

  #expire(): void {
    if (!this.#controller.signal.aborted) {
      this.#controller.abort();
    }
  }

  #schedule(): void {
    const remaining = this.#expiresAt - performance.now();
    if (remaining <= 0) {
      this.#expire();
      return;
    }
    this.#timer = setTimeout(
      () => this.#schedule(),
      Math.min(Math.ceil(remaining), MAX_TIMER_SLICE_MS),
    );
  }
}

export interface HyphaeClientOptions {
  readonly bearerToken?: string;
  readonly timeoutMs?: number;
  readonly responseBytes?: number;
  readonly witnessBytes?: number;
  readonly fetch?: typeof globalThis.fetch;
}

export interface ApiResponse<T> {
  readonly value: T;
  readonly requestId: string;
}

export class HyphaeApiError extends Error {
  readonly status: number;
  readonly code: string;
  readonly requestId: string;

  constructor(status: number, envelope: ErrorV1) {
    super(`Hyphae API returned HTTP ${status} ${envelope.code} (request ${envelope.request_id})`);
    this.name = "HyphaeApiError";
    this.status = status;
    this.code = envelope.code;
    this.requestId = envelope.request_id;
  }
}

export class HyphaeClientError extends Error {
  constructor(message: string, options?: ErrorOptions) {
    super(message, options);
    this.name = "HyphaeClientError";
  }
}

export class HyphaeClient {
  readonly #origin: URL;
  readonly #bearerToken: string | undefined;
  readonly #timeoutMs: number;
  readonly #responseBytes: number;
  readonly #witnessBytes: number;
  readonly #fetch: typeof globalThis.fetch;

  constructor(baseUrl: string, options: HyphaeClientOptions = {}) {
    let origin: URL;
    try {
      origin = new URL(baseUrl);
    } catch (cause) {
      throw new HyphaeClientError("invalid Hyphae base URL", { cause });
    }
    if ((origin.protocol !== "http:" && origin.protocol !== "https:") ||
        origin.username !== "" || origin.password !== "" || origin.search !== "" ||
        origin.hash !== "" || (origin.pathname !== "" && origin.pathname !== "/")) {
      throw new HyphaeClientError("Hyphae base URL must be a root HTTP(S) origin");
    }
    const timeoutMs = options.timeoutMs ?? DEFAULT_TIMEOUT_MS;
    const responseBytes = options.responseBytes ?? DEFAULT_RESPONSE_BYTES;
    const witnessBytes = options.witnessBytes ?? DEFAULT_WITNESS_BYTES;
    if (![timeoutMs, responseBytes, witnessBytes].every(Number.isSafeInteger) ||
        timeoutMs <= 0 || responseBytes <= 0 || witnessBytes <= 0) {
      throw new HyphaeClientError("client timeout and response limits must be positive safe integers");
    }
    if (options.bearerToken !== undefined &&
        (options.bearerToken.length === 0 || /[\r\n]/u.test(options.bearerToken))) {
      throw new HyphaeClientError("invalid bearer token for an HTTP authorization header");
    }
    const fetchFunction = options.fetch ?? globalThis.fetch;
    if (typeof fetchFunction !== "function") {
      throw new HyphaeClientError("this runtime does not provide fetch");
    }
    origin.pathname = "/";
    this.#origin = origin;
    this.#bearerToken = options.bearerToken;
    this.#timeoutMs = timeoutMs;
    this.#responseBytes = responseBytes;
    this.#witnessBytes = witnessBytes;
    this.#fetch = fetchFunction;
  }

  capabilities(): Promise<ApiResponse<CapabilitiesV1>> {
    return this.#json("v1/capabilities", false);
  }

  liveness(): Promise<ApiResponse<HealthV1>> {
    return this.#json("v1/health/live", false);
  }

  readiness(): Promise<ApiResponse<HealthV1>> {
    return this.#json("v1/health/ready", false);
  }

  put(request: PutRequestV1): Promise<ApiResponse<CommitReceiptV1>> {
    return this.#json("v1/kv/put", true, request);
  }

  delete(request: DeleteRequestV1): Promise<ApiResponse<CommitReceiptV1>> {
    return this.#json("v1/kv/delete", true, request);
  }

  get(request: GetRequestV1): Promise<ApiResponse<GetResponseV1>> {
    return this.#json("v1/kv/get", true, request);
  }

  query(request: QueryRequestV1): Promise<ApiResponse<QueryResponseV1>> {
    return this.#json("v1/query", true, request);
  }

  defineVectorSpace(request: DefineVectorSpaceRequestV1): Promise<ApiResponse<CommitReceiptV1>> {
    return this.#json("v1/vector-spaces/define", true, request);
  }

  putVectors(request: PutVectorsRequestV1): Promise<ApiResponse<CommitReceiptV1>> {
    return this.#json("v1/vectors/put", true, request);
  }

  deleteVectors(request: DeleteVectorsRequestV1): Promise<ApiResponse<CommitReceiptV1>> {
    return this.#json("v1/vectors/delete", true, request);
  }

  retrieveExact(request: ExactRetrievalRequestV1): Promise<ApiResponse<ExactRetrievalResponseV1>> {
    return this.#json("v1/retrieve/exact", true, request);
  }

  defineLexicalIndex(request: DefineLexicalIndexRequestV1): Promise<ApiResponse<CommitReceiptV1>> {
    return this.#json("v1/lexical-indexes/define", true, request);
  }

  retrieveLexical(
    request: LexicalRetrievalRequestV1,
  ): Promise<ApiResponse<LexicalRetrievalResponseV1>> {
    return this.#json("v1/retrieve/lexical", true, request);
  }

  retrieveHybrid(
    request: HybridRetrievalRequestV1,
  ): Promise<ApiResponse<HybridRetrievalResponseV1>> {
    return this.#json("v1/retrieve/hybrid", true, request);
  }

  async downloadWitness(proof: ProofV1): Promise<ApiResponse<Uint8Array>> {
    return this.#downloadWitness(proof);
  }

  async downloadRetrievalWitness(
    proof: RetrievalProofV1,
  ): Promise<ApiResponse<Uint8Array>> {
    return this.#downloadWitness(proof);
  }

  async #downloadWitness(
    proof: ProofV1 | RetrievalProofV1,
  ): Promise<ApiResponse<Uint8Array>> {
    const deadline = new Deadline(this.#timeoutMs);
    let response: Response | undefined;
    try {
      const expectedPath = `/v1/witnesses/${proof.checkpoint_sequence}/${proof.snapshot_digest}`;
      if (proof.witness.path !== expectedPath) {
        throw new HyphaeClientError("proof contains a noncanonical witness reference");
      }
      const expectedBytes = typeof proof.witness.file_bytes === "bigint"
        ? proof.witness.file_bytes
        : BigInt(proof.witness.file_bytes);
      if (expectedBytes < 0n || expectedBytes > BigInt(this.#witnessBytes)) {
        throw new HyphaeClientError(
          `Hyphae response exceeded local limit ${this.#witnessBytes} bytes`,
        );
      }
      deadline.throwIfElapsed();
      response = await this.#request(expectedPath.slice(1), true, deadline);
      if (!response.ok) {
        throw await this.#apiError(response, deadline);
      }
      if (response.status !== 200) {
        throw new HyphaeClientError(`Hyphae returned unexpected success status ${response.status}`);
      }
      const requestId = singleHeader(response.headers, "x-request-id");
      if (requestId === undefined) {
        throw new HyphaeClientError("Hyphae response has no single valid X-Request-Id header");
      }
      if (singleHeader(response.headers, "digest") !== `blake3=${proof.snapshot_digest}`) {
        throw new HyphaeClientError("downloaded witness digest header differs from the proof");
      }
      const value = await readBounded(response, this.#witnessBytes, deadline);
      if (BigInt(value.byteLength) !== expectedBytes) {
        throw new HyphaeClientError("downloaded witness length differs from the proof");
      }
      deadline.throwIfElapsed();
      return { value, requestId };
    } finally {
      cancelUnusedBody(response);
      deadline.close();
    }
  }

  async #json<T>(path: string, authenticated: boolean, body?: unknown): Promise<ApiResponse<T>> {
    const deadline = new Deadline(this.#timeoutMs);
    let response: Response | undefined;
    try {
      response = await this.#request(path, authenticated, deadline, body);
      if (!response.ok) {
        throw await this.#apiError(response, deadline);
      }
      if (response.status !== 200) {
        throw new HyphaeClientError(`Hyphae returned unexpected success status ${response.status}`);
      }
      requireJson(response.headers);
      const requestId = singleHeader(response.headers, "x-request-id");
      if (requestId === undefined) {
        throw new HyphaeClientError("Hyphae response has no single valid X-Request-Id header");
      }
      const encoded = await readBounded(response, this.#responseBytes, deadline);
      try {
        const value = parseHyphaeJson(
          new TextDecoder("utf-8", { fatal: true }).decode(encoded),
        ) as T;
        deadline.throwIfElapsed();
        return { value, requestId };
      } catch (cause) {
        deadline.throwIfElapsed();
        throw new HyphaeClientError(
          "Hyphae response violated the version 1 JSON contract",
          { cause },
        );
      }
    } finally {
      cancelUnusedBody(response);
      deadline.close();
    }
  }

  async #apiError(response: Response, deadline: Deadline): Promise<HyphaeApiError> {
    requireJson(response.headers);
    const requestId = singleHeader(response.headers, "x-request-id");
    if (requestId === undefined) {
      throw new HyphaeClientError("Hyphae response has no single valid X-Request-Id header");
    }
    const encoded = await readBounded(response, this.#responseBytes, deadline);
    let envelope: ErrorV1;
    try {
      envelope = parseHyphaeJson(new TextDecoder("utf-8", { fatal: true }).decode(encoded)) as ErrorV1;
      deadline.throwIfElapsed();
    } catch (cause) {
      deadline.throwIfElapsed();
      throw new HyphaeClientError("Hyphae error response violated the version 1 JSON contract", { cause });
    }
    if (typeof envelope !== "object" || envelope === null ||
        typeof envelope.code !== "string" || typeof envelope.message !== "string" ||
        typeof envelope.request_id !== "string") {
      throw new HyphaeClientError("Hyphae error response violated the version 1 JSON contract");
    }
    if (envelope.request_id !== requestId) {
      throw new HyphaeClientError("Hyphae error envelope request ID differs from its response header");
    }
    deadline.throwIfElapsed();
    return new HyphaeApiError(response.status, envelope);
  }

  async #request(
    path: string,
    authenticated: boolean,
    deadline: Deadline,
    body?: unknown,
  ): Promise<Response> {
    deadline.throwIfElapsed();
    const headers = new Headers();
    if (authenticated && this.#bearerToken !== undefined) {
      headers.set("authorization", `Bearer ${this.#bearerToken}`);
    }
    let method = "GET";
    let encoded: string | undefined;
    if (body !== undefined) {
      method = "POST";
      headers.set("content-type", "application/json");
      encoded = stringifyHyphaeJson(body);
    }
    deadline.throwIfElapsed();
    const endpoint = new URL(path, this.#origin);
    try {
      return await deadline.race(
        this.#fetch(endpoint, {
          method,
          headers,
          ...(encoded === undefined ? {} : { body: encoded }),
          redirect: "error",
          signal: deadline.signal,
        }),
      );
    } catch (cause) {
      if (cause instanceof HyphaeClientError) throw cause;
      if (deadline.elapsed()) throw deadlineError(cause);
      throw new HyphaeClientError("Hyphae HTTP transport failed", { cause });
    }
  }
}

function requireJson(headers: Headers): void {
  const contentType = singleHeader(headers, "content-type");
  const mediaType = contentType?.split(";", 1)[0]?.trim().toLowerCase();
  if (mediaType !== "application/json" &&
      !(mediaType?.startsWith("application/") === true && mediaType.endsWith("+json"))) {
    throw new HyphaeClientError("Hyphae response did not use a JSON content type");
  }
}

function singleHeader(headers: Headers, name: string): string | undefined {
  const value = headers.get(name);
  return value === null || value.length === 0 || value.includes(",") ? undefined : value;
}

function deadlineError(cause?: unknown): HyphaeClientError {
  return new HyphaeClientError(
    "Hyphae HTTP request/response deadline elapsed",
    cause === undefined ? undefined : { cause },
  );
}

function cancelUnusedBody(response: Response | undefined): void {
  if (response !== undefined && response.body !== null && !response.bodyUsed) {
    void response.body.cancel().catch(() => {});
  }
}

async function readBounded(
  response: Response,
  maximum: number,
  deadline: Deadline,
): Promise<Uint8Array> {
  const declared = response.headers.get("content-length");
  if (declared !== null && (!/^\d+$/u.test(declared) || Number(declared) > maximum)) {
    throw new HyphaeClientError(`Hyphae response exceeded local limit ${maximum} bytes`);
  }
  if (response.body === null) {
    return new Uint8Array();
  }
  const reader = response.body.getReader();
  const chunks: Uint8Array[] = [];
  let length = 0;
  let completed = false;
  try {
    for (;;) {
      let result: ReadableStreamReadResult<Uint8Array>;
      try {
        result = await deadline.race(reader.read());
      } catch (cause) {
        if (cause instanceof HyphaeClientError) throw cause;
        if (deadline.elapsed()) throw deadlineError(cause);
        throw new HyphaeClientError("Hyphae HTTP transport failed", { cause });
      }
      if (result.done) break;
      length += result.value.byteLength;
      if (length > maximum) {
        throw new HyphaeClientError(`Hyphae response exceeded local limit ${maximum} bytes`);
      }
      chunks.push(result.value);
    }
    completed = true;
  } finally {
    reader.releaseLock();
    if (!completed) {
      void response.body.cancel().catch(() => {});
    }
  }
  deadline.throwIfElapsed();
  const joined = new Uint8Array(length);
  let offset = 0;
  for (const chunk of chunks) {
    joined.set(chunk, offset);
    offset += chunk.byteLength;
  }
  deadline.throwIfElapsed();
  return joined;
}
