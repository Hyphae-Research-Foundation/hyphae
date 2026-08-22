// SPDX-License-Identifier: Apache-2.0

/**
 * Optional model-provider layer with declared-provider attestation records.
 *
 * Every provider call returns its payload together with a
 * `DeclaredProviderRecord` whose envelope replicates the core `HYATTS01`
 * attestation format byte-exactly, so the engine's pure verifier accepts
 * it. The record proves what was sent and received — never that the
 * provider computed it deterministically; locally attested models carry the
 * replayable `AttestedLocal` class instead.
 *
 * Digests need the optional `@noble/hashes` peer dependency; constructing a
 * record without it throws a clear error. Transport is global `fetch`.
 */

import { ClientError } from "./v2/models.js";

const ATTESTATION_MAGIC = new TextEncoder().encode("HYATTS01");
const MAX_NAME_BYTES = 256;

type Blake3 = (data: Uint8Array) => Uint8Array;

let blake3Implementation: Blake3 | undefined;

async function digestFn(): Promise<Blake3> {
  if (blake3Implementation !== undefined) return blake3Implementation;
  try {
    const module = await import("@noble/hashes/blake3.js");
    blake3Implementation = (data: Uint8Array) => module.blake3(data);
    return blake3Implementation;
  } catch {
    throw new ClientError(
      "the providers layer needs the optional @noble/hashes peer dependency",
    );
  }
}

function name(value: string): Uint8Array {
  const encoded = new TextEncoder().encode(value);
  if (encoded.byteLength === 0 || encoded.byteLength > MAX_NAME_BYTES) {
    throw new ClientError("attestation name is unbounded");
  }
  const framed = new Uint8Array(2 + encoded.byteLength);
  framed[0] = encoded.byteLength & 0xff;
  framed[1] = encoded.byteLength >> 8;
  framed.set(encoded, 2);
  return framed;
}

function concat(parts: readonly Uint8Array[]): Uint8Array {
  const total = parts.reduce((sum, part) => sum + part.byteLength, 0);
  const joined = new Uint8Array(total);
  let offset = 0;
  for (const part of parts) {
    joined.set(part, offset);
    offset += part.byteLength;
  }
  return joined;
}

/** One declared-provider attestation, envelope-compatible with HYATTS01. */
export class DeclaredProviderRecord {
  readonly provider: string;
  readonly model: string;
  readonly requestDigest: Uint8Array;
  readonly responseDigest: Uint8Array;

  constructor(provider: string, model: string, requestDigest: Uint8Array, responseDigest: Uint8Array) {
    if (requestDigest.byteLength !== 32 || responseDigest.byteLength !== 32) {
      throw new ClientError("attestation digests must be 32 bytes");
    }
    this.provider = provider;
    this.model = model;
    this.requestDigest = requestDigest;
    this.responseDigest = responseDigest;
  }

  /** Returns the canonical HYATTS01 declared-provider envelope. */
  envelope(): Uint8Array {
    return concat([
      ATTESTATION_MAGIC,
      Uint8Array.of(2),
      name(this.provider),
      name(this.model),
      this.requestDigest,
      this.responseDigest,
    ]);
  }

  /** Returns the canonical envelope as lowercase hex. */
  envelopeHex(): string {
    return Array.from(this.envelope(), (byte) => byte.toString(16).padStart(2, "0")).join("");
  }
}

async function postJson(
  url: string,
  payload: Readonly<Record<string, unknown>>,
  headers: Readonly<Record<string, string>>,
): Promise<{ requestBytes: Uint8Array; responseBytes: Uint8Array }> {
  const requestBytes = new TextEncoder().encode(canonicalJson(payload));
  const response = await fetch(url, {
    method: "POST",
    headers: { "content-type": "application/json", ...headers },
    body: requestBytes.slice().buffer as ArrayBuffer,
  });
  if (!response.ok) {
    throw new ClientError(`provider request failed with status ${response.status}`);
  }
  const responseBytes = new Uint8Array(await response.arrayBuffer());
  return { requestBytes, responseBytes };
}

/** Deterministic JSON with sorted keys, matching the Python provider layer. */
function canonicalJson(value: unknown): string {
  if (Array.isArray(value)) {
    return `[${value.map(canonicalJson).join(",")}]`;
  }
  if (typeof value === "object" && value !== null) {
    const entries = Object.entries(value as Record<string, unknown>).sort(([left], [right]) =>
      left < right ? -1 : left > right ? 1 : 0,
    );
    return `{${entries.map(([key, entry]) => `${JSON.stringify(key)}:${canonicalJson(entry)}`).join(",")}}`;
  }
  return JSON.stringify(value);
}

/** Local Ollama embeddings with declared-provider attestation records. */
export class OllamaProvider {
  readonly #baseUrl: string;

  constructor(baseUrl = "http://127.0.0.1:11434") {
    this.#baseUrl = baseUrl.replace(/\/+$/u, "");
  }

  /** Embeds `texts` and returns vectors with the attestation record. */
  async embed(model: string, texts: readonly string[]): Promise<{ vectors: number[][]; record: DeclaredProviderRecord }> {
    if (texts.length === 0) throw new ClientError("no texts to embed");
    const blake3 = await digestFn();
    const { requestBytes, responseBytes } = await postJson(
      `${this.#baseUrl}/api/embed`,
      { input: [...texts], model },
      {},
    );
    const decoded = JSON.parse(new TextDecoder().decode(responseBytes)) as { embeddings?: unknown };
    const vectors = decoded.embeddings;
    if (!Array.isArray(vectors) || vectors.length !== texts.length) {
      throw new ClientError("provider returned an unexpected embedding shape");
    }
    return {
      vectors: vectors as number[][],
      record: new DeclaredProviderRecord("ollama", model, blake3(requestBytes), blake3(responseBytes)),
    };
  }
}

/** OpenAI embeddings with declared-provider attestation records. */
export class OpenAiProvider {
  readonly #apiKey: string;
  readonly #baseUrl: string;

  constructor(apiKey: string, baseUrl = "https://api.openai.com/v1") {
    if (apiKey.length === 0) throw new ClientError("an OpenAI API key is required");
    this.#apiKey = apiKey;
    this.#baseUrl = baseUrl.replace(/\/+$/u, "");
  }

  /** Embeds `texts` and returns vectors with the attestation record. */
  async embed(model: string, texts: readonly string[]): Promise<{ vectors: number[][]; record: DeclaredProviderRecord }> {
    if (texts.length === 0) throw new ClientError("no texts to embed");
    const blake3 = await digestFn();
    const { requestBytes, responseBytes } = await postJson(
      `${this.#baseUrl}/embeddings`,
      { input: [...texts], model },
      { authorization: `Bearer ${this.#apiKey}` },
    );
    const decoded = JSON.parse(new TextDecoder().decode(responseBytes)) as { data?: unknown };
    const data = decoded.data;
    if (!Array.isArray(data) || data.length !== texts.length) {
      throw new ClientError("provider returned an unexpected embedding shape");
    }
    const vectors = data.map((entry) => (entry as { embedding?: unknown }).embedding);
    if (vectors.some((vector) => !Array.isArray(vector))) {
      throw new ClientError("provider returned an unexpected embedding shape");
    }
    return {
      vectors: vectors as number[][],
      record: new DeclaredProviderRecord("openai", model, blake3(requestBytes), blake3(responseBytes)),
    };
  }
}
