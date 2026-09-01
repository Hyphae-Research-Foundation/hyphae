// SPDX-License-Identifier: Apache-2.0

import { ClientError, DEFAULT_LIMITS, SensitiveBytes, type ProductErrorFields, type RequestOptions, type Response } from "./models.js";

export const MAX_PAYLOAD = 16 * 1024 * 1024;
export const FRAME_HEADER_SIZE = 32;
export const FRAME_KIND = {
  hello: 1,
  welcome: 2,
  prepare: 4,
  execute: 5,
  failure: 13,
  cancel: 14,
  deallocate: 16,
  data: 19,
  end: 20,
  windowUpdate: 21,
} as const;

const REQUEST_KIND: Readonly<Record<string, number>> = {
  capabilities: 1,
  sql_prepare: 2,
  sql_execute_prepared: 3,
  sql_execute: 4,
  structure_get: 5,
  structure_set: 6,
  structure_ttl: 7,
  transaction_status: 8,
  search: 9,
  admin_status: 10,
  admin_checkpoint: 11,
  sql_deallocate: 12,
  catalog_object: 13,
  catalog_object_named: 14,
  catalog_list: 15,
  catalog_dependencies: 16,
  catalog_describe: 17,
  catalog_resolve: 18,
  catalog_create: 19,
  admin_explain_sql: 20,
  doctor: 21,
  backup: 22,
  telemetry: 23,
  proof_verify: 24,
  search_collection: 25,
  search_ingest: 29,
  search_document_update: 30,
  search_document_delete: 31,
  structure_mutate: 26,
  structure_read: 27,
  restore: 28,
  transaction_begin: 32,
  transaction_stage_sql: 33,
  transaction_stage_structure: 34,
  transaction_stage_search: 35,
  transaction_stage_vector: 36,
  transaction_commit: 37,
  transaction_rollback: 38,
  transaction_status_by_idempotency: 39,
  explicit_transaction_status: 40,
  proof_generate: 41,
  security_status: 42,
  security_principal_list: 43,
  security_role_list: 44,
  security_assignment_list: 45,
  security_key_list: 46,
  security_audit_read: 47,
  security_principal_create: 48,
  security_principal_set_enabled: 49,
  security_custom_role_create: 50,
  security_built_in_assignment_create: 51,
  security_custom_assignment_create: 52,
  security_assignment_revoke: 53,
  catalog_visible_list: 54,
  security_api_key_issue_self_start: 55,
  security_api_key_issue_start: 56,
  security_api_key_issue_self_activate: 57,
  security_api_key_issue_activate: 58,
  security_api_key_rotate_self_start: 59,
  security_api_key_rotate_start: 60,
  security_api_key_rotate_self_activate: 61,
  security_api_key_rotate_activate: 62,
  security_api_key_issue_self_abort: 63,
  security_api_key_issue_abort: 64,
  security_api_key_rotate_self_abort: 65,
  security_api_key_rotate_abort: 66,
  security_api_key_revoke_self: 67,
  security_api_key_revoke: 68,
  security_legacy_bearer_revoke: 70,
};

const BUILT_IN_ROLES = ["owner", "admin", "operator", "developer", "writer", "reader", "auditor"] as const;
const PRODUCT_PERMISSIONS = ["audit.read", "backup.create", "backup.verify", "catalog.read", "catalog.write", "credential.self_manage", "data.read", "data.write", "discover", "maintain", "observe", "ownership.manage", "proof.generate", "proof.verify", "restore", "search.execute", "security.manage", "security.read"] as const;
const SECURITY_AUDIT_ACTIONS = ["bootstrap_owner", "activate_key", "create_principal", "create_custom_role", "assign_built_in_role", "assign_custom_role", "issue_key", "rotate_key", "revoke_key", "recover_owner", "migrate_legacy_bearer", "abort_key_rotation", "abort_key_issue", "set_principal_enabled", "revoke_assignment", "revoke_legacy_bearer"] as const;
const MAX_SECURITY_LIST_ROWS = 1_000;
const MAX_SECURITY_ASSIGNMENTS = 128;
const MAX_SECURITY_GRANTS = 128;
const MAX_CATALOG_VISIBLE_ITEMS = 4_096;
const API_KEY_BYTES = 102;
const MAX_PRODUCT_COUNT = 4_096;
const MAX_SQL_ROWS = 4_096;
const MAX_SQL_COLUMNS = 4_096;
const MAX_TELEMETRY_METRICS = 256;
const MAX_TELEMETRY_EVENTS = 1_024;
const MAX_SEARCH_HITS = 1_024;
const MAX_DOC_VALUES_PER_HIT = 64;
const MAX_SEARCH_FACETS = 8;
const MAX_SEARCH_FACET_BUCKETS = 10_000;
const MAX_SEARCH_AGGREGATIONS = 16;
const MAX_SEARCH_VECTOR_BRANCHES = 16;

const DEFAULT_PROOF_LIMITS = {
  result_items: 10_000n,
  candidate_items: 100_000n,
  evidence_bytes: 32n * 1024n * 1024n,
  max_proof_bytes: 64n * 1024n * 1024n,
  max_section_bytes: 32n * 1024n * 1024n,
  max_decoded_bytes: 48n * 1024n * 1024n,
  max_objects: 4_096n,
  max_hybrid_branches: 64n,
  max_witness_bytes: 4n * 1024n * 1024n * 1024n,
  max_entries: 65_536n,
  max_files: 32_768n,
  max_directories: 32_768n,
  max_path_bytes: 4_096n,
  max_file_bytes: 1024n * 1024n * 1024n,
  max_total_file_bytes: 3n * 1024n * 1024n * 1024n,
  max_witness_decoded_bytes: 3n * 1024n * 1024n * 1024n,
} as const;

export interface Frame {
  readonly kind: number;
  readonly streamId: number;
  readonly requestId: bigint;
  readonly payload: Uint8Array;
}

export function crc32c(encoded: Uint8Array): number {
  let crc = 0xffffffff;
  for (const byte of encoded) {
    crc ^= byte;
    for (let bit = 0; bit < 8; bit += 1) {
      crc = (crc >>> 1) ^ ((crc & 1) === 0 ? 0 : 0x82f63b78);
    }
  }
  return (crc ^ 0xffffffff) >>> 0;
}

export function encodeFrame(kind: number, streamId: number, requestId: bigint, payload: Uint8Array): Uint8Array {
  if (payload.byteLength > MAX_PAYLOAD) throw new ClientError("native frame payload is too large");
  const encoded = new Uint8Array(FRAME_HEADER_SIZE + payload.byteLength);
  encoded.set(new TextEncoder().encode("HYPHLCL1"));
  const view = new DataView(encoded.buffer);
  view.setUint16(8, 1, true);
  view.setUint8(10, kind);
  view.setUint32(12, streamId, true);
  view.setBigUint64(16, requestId, true);
  view.setUint32(24, payload.byteLength, true);
  encoded.set(payload, FRAME_HEADER_SIZE);
  view.setUint32(28, crc32c(encoded), true);
  return encoded;
}

export function decodeFrame(encoded: Uint8Array): Frame {
  if (encoded.byteLength < FRAME_HEADER_SIZE) throw new ClientError("native frame is truncated");
  const view = new DataView(encoded.buffer, encoded.byteOffset, encoded.byteLength);
  if (new TextDecoder().decode(encoded.subarray(0, 8)) !== "HYPHLCL1" ||
      view.getUint16(8, true) !== 1 || view.getUint8(11) !== 0) {
    throw new ClientError("native frame preamble is invalid");
  }
  const length = view.getUint32(24, true);
  if (length > MAX_PAYLOAD || encoded.byteLength !== FRAME_HEADER_SIZE + length) {
    throw new ClientError("native frame length is invalid");
  }
  const expected = view.getUint32(28, true);
  const checked = encoded.slice();
  checked.fill(0, 28, 32);
  if (crc32c(checked) !== expected) throw new ClientError("native frame CRC32C mismatch");
  return {
    kind: view.getUint8(10),
    streamId: view.getUint32(12, true),
    requestId: view.getBigUint64(16, true),
    payload: encoded.slice(FRAME_HEADER_SIZE),
  };
}

export function encodeHello(clientIdentity = "hyphae-typescript-sdk-v2", maximumMinor = 0): Uint8Array {
  if (!Number.isInteger(maximumMinor) || maximumMinor < 0 || maximumMinor > 6) throw new ClientError("native protocol minor is invalid");
  const names = [clientIdentity, "main", "public"].map((value) => new TextEncoder().encode(value));
  const encoded = new Uint8Array(58 + names.reduce((total, value) => total + value.byteLength, 0));
  encoded.set(new TextEncoder().encode("HYPHEL01"));
  const view = new DataView(encoded.buffer);
  view.setUint32(8, encoded.byteLength, true);
  view.setUint16(12, 1, true);
  view.setUint16(14, 1, true);
  view.setUint16(18, maximumMinor, true);
  view.setBigUint64(20, 0x7fn, true);
  view.setBigUint64(28, 0x7fn, true);
  view.setUint32(36, MAX_PAYLOAD, true);
  view.setUint32(40, 64, true);
  view.setUint32(44, 64 * 1024, true);
  view.setUint8(48, 1);
  let offset = 58;
  names.forEach((name, index) => {
    view.setUint16(52 + index * 2, name.byteLength, true);
    encoded.set(name, offset);
    offset += name.byteLength;
  });
  return encoded;
}

export function encodeAuthenticatedHello(
  apiKey: string | Uint8Array,
  clientIdentity = "hyphae-typescript-sdk-v2",
  maximumMinor = 6,
): Uint8Array {
  const authentication = typeof apiKey === "string" ? new TextEncoder().encode(apiKey) : apiKey.slice();
  if (authentication.byteLength !== API_KEY_BYTES) throw new ClientError("local API-key credential is invalid");
  const encoded = encodeHello(clientIdentity, maximumMinor);
  const authenticated = new Uint8Array(encoded.byteLength + authentication.byteLength);
  authenticated.set(encoded);
  authenticated.set(authentication, encoded.byteLength);
  const view = new DataView(authenticated.buffer);
  view.setUint32(8, authenticated.byteLength, true);
  view.setBigUint64(20, 0xffn, true);
  view.setBigUint64(28, 0xffn, true);
  view.setUint8(49, 1);
  view.setUint16(50, authentication.byteLength, true);
  authentication.fill(0);
  return authenticated;
}

/** Boolean, integer, string, and bytes doc values are minor-0 content;
 * future typed values raise the requirement here. */
function docValueRequiredMinor(_value: unknown): number {
  return 0;
}

function filterRequiredMinor(value: unknown, depth = 0): number {
  if (depth > 32 || typeof value !== "object" || value === null) return 0;
  const filter = value as Readonly<Record<string, unknown>>;
  if (filter.kind === "in" || filter.kind === "is_null" || filter.kind === "like") return 4;
  // Every current filter kind and operator is minor-0 content; future
  // operators and typed literals raise the requirement here.
  if (filter.kind === "compare") return docValueRequiredMinor(filter.value);
  if (filter.kind === "all" || filter.kind === "any") {
    const children = Array.isArray(filter.filters) ? filter.filters : [];
    return children.reduce((highest: number, child) => Math.max(highest, filterRequiredMinor(child, depth + 1)), 0);
  }
  if (filter.kind === "not") return filterRequiredMinor(filter.filter, depth + 1);
  return 0;
}

function documentRequiredMinor(value: unknown): number {
  if (typeof value !== "object" || value === null) return 0;
  const docValues = (value as Readonly<Record<string, unknown>>).doc_values;
  if (typeof docValues !== "object" || docValues === null) return 0;
  return Object.values(docValues).reduce((highest: number, entry) => Math.max(highest, docValueRequiredMinor(entry)), 0);
}

export function operationRequiredMinor(operation: string, args: Readonly<Record<string, unknown>> = {}): number {
  if (operation === "proof_generate") {
    const nested = args.operation;
    return typeof nested === "string" ? operationRequiredMinor(nested, (args.arguments ?? {}) as Readonly<Record<string, unknown>>) : 0;
  }
  if (["security_status", "security_principal_list", "security_role_list", "security_assignment_list", "security_key_list", "security_audit_read"].includes(operation)) return 1;
  if (["security_principal_create", "security_principal_set_enabled", "security_custom_role_create", "security_built_in_assignment_create", "security_custom_assignment_create", "security_assignment_revoke"].includes(operation)) return 2;
  if (operation === "catalog_visible_list" || operation.startsWith("security_api_key_") || operation === "security_legacy_bearer_revoke") return 3;
  if (operation === "structure_read") {
    const request = (typeof args.request === "object" && args.request !== null ? args.request : args) as Readonly<Record<string, unknown>>;
    const kind = String(request.kind ?? "");
    if (kind === "sorted_set_score_range" || kind === "hash_scan_reverse" || kind === "hash_scan_match") return 6;
  }
  if (operation === "search_collection") {
    const request = (typeof args.request === "object" && args.request !== null ? args.request : args) as Readonly<Record<string, unknown>>;
    const extended = request.fusion !== undefined || (request.parent_dedupe !== undefined && request.parent_dedupe !== null) || (request.rerank !== undefined && request.rerank !== null);
    const highlighted = request.highlight !== undefined && request.highlight !== null;
    return Math.max(highlighted ? 5 : 0, extended ? 4 : 0, filterRequiredMinor(request.filter));
  }
  if (operation === "search_ingest") {
    const batch = (typeof args.batch === "object" && args.batch !== null ? args.batch : args) as Readonly<Record<string, unknown>>;
    const documents = Array.isArray(batch.documents) ? batch.documents : [];
    return documents.reduce((highest: number, document) => Math.max(highest, documentRequiredMinor(document)), 0);
  }
  if (operation === "search_document_update") return documentRequiredMinor(args.document);
  return 0;
}

export function responseRequiredMinor(kind: number): number {
  if (kind >= 32 && kind <= 37) return 1;
  if (kind >= 38 && kind <= 41) return 2;
  if (kind >= 42 && kind <= 44) return 3;
  return 0;
}

export function decodeWelcome(encoded: Uint8Array): Readonly<Record<string, number | bigint>> {
  if (encoded.byteLength !== 94 || new TextDecoder().decode(encoded.subarray(0, 8)) !== "HYPWEL01") {
    throw new ClientError("native welcome is malformed");
  }
  const view = new DataView(encoded.buffer, encoded.byteOffset, encoded.byteLength);
  if (view.getUint32(8, true) !== 94 || view.getUint16(12, true) !== 1 || view.getUint16(14, true) > 4 || view.getBigUint64(24, true) === 0n) {
    throw new ClientError("native welcome values are invalid");
  }
  return {
    major: view.getUint16(12, true),
    minor: view.getUint16(14, true),
    capabilities: view.getBigUint64(16, true),
    sessionId: readU128(encoded, 24),
    maximumFramePayload: view.getUint32(40, true),
    maximumInFlight: view.getUint32(44, true),
    initialWindow: view.getUint32(48, true),
  };
}

export function encodeProductRequest(
  operation: string,
  args: Readonly<Record<string, unknown>>,
  options: RequestOptions = {},
  negotiatedMinor?: number,
): Uint8Array {
  const kind = REQUEST_KIND[operation];
  if (kind === undefined) throw new ClientError(`unsupported native operation: ${operation}`);
  if (negotiatedMinor !== undefined && negotiatedMinor < operationRequiredMinor(operation, args)) {
    throw new ClientError("native operation is unavailable at the negotiated protocol minor");
  }
  const limits = options.limits ?? DEFAULT_LIMITS;
  if (options.deadlineMicros !== undefined && options.deadlineMicros <= 0n) throw new ClientError("deadlineMicros must be positive");
  if (!Object.values(limits).every((value) => Number.isSafeInteger(value) && value > 0)) throw new ClientError("product limits must be positive safe integers");
  const securityMutation = (kind >= 48 && kind <= 53) || (kind >= 55 && kind <= 68) || kind === 70;
  if (securityMutation && options.idempotencyToken === undefined) throw new ClientError("security mutation requires a nonzero idempotencyToken");
  if (securityMutation && (options.durability ?? "strict") !== "strict") throw new ClientError("security mutation requires strict durability");
  const body = encodeOperation(operation, args);
  const extended = options.idempotencyToken !== undefined;
  const contextBytes = extended ? 80 : 64;
  const encoded = new Uint8Array(16 + contextBytes + body.byteLength);
  encoded.set(new TextEncoder().encode("HYPREQ01"));
  const view = new DataView(encoded.buffer);
  view.setUint32(8, encoded.byteLength, true);
  view.setUint16(12, kind, true);
  view.setBigInt64(16, options.logicalTimeMicros ?? 0n, true);
  view.setBigInt64(24, options.deadlineMicros ?? 0n, true);
  const offset = extended ? 16 : 0;
  if (extended) {
    const token = options.idempotencyToken ?? 0n;
    if (token <= 0n || token > (1n << 128n) - 1n) throw new ClientError("idempotencyToken must be a nonzero u128");
    view.setBigUint64(32, token & ((1n << 64n) - 1n), true);
    view.setBigUint64(40, token >> 64n, true);
  }
  view.setBigUint64(32 + offset, BigInt(limits.maxCount), true);
  view.setBigUint64(40 + offset, BigInt(limits.maxRequestBytes), true);
  view.setBigUint64(48 + offset, BigInt(limits.maxResponseBytes), true);
  view.setBigUint64(56 + offset, BigInt(limits.maxWorkUnits), true);
  view.setBigUint64(64 + offset, BigInt(limits.maxMemoryBytes), true);
  view.setUint8(72 + offset, ({ strict: 0, group: 1, memory: 2 } as const)[options.durability ?? "strict"]);
  if (extended) view.setUint8(89, 1);
  encoded.set(body, 16 + contextBytes);
  return encoded;
}

export function decodeProductRequest(encoded: Uint8Array): {
  readonly operation: string;
  readonly args: Readonly<Record<string, unknown>>;
  readonly options: RequestOptions;
} {
  const [kind, payload] = envelope(encoded, "HYPREQ01");
  const extended = payload.byteLength >= 80 && payload[73] === 1 && payload.subarray(74, 80).every((value) => value === 0);
  const legacy = payload.byteLength >= 64 && payload.subarray(57, 64).every((value) => value === 0);
  if (!extended && !legacy) throw new ClientError("product request context is malformed");
  const operation = Object.entries(REQUEST_KIND).find((entry) => entry[1] === kind)?.[0];
  if (operation === undefined) throw new ClientError("unsupported product request kind");
  const view = new DataView(payload.buffer, payload.byteOffset, payload.byteLength);
  const offset = extended ? 16 : 0;
  const token = extended ? view.getBigUint64(24, true) << 64n | view.getBigUint64(16, true) : 0n;
  const options: RequestOptions = {
    logicalTimeMicros: view.getBigInt64(0, true),
    ...(view.getBigInt64(8, true) === 0n ? {} : { deadlineMicros: view.getBigInt64(8, true) }),
    ...(token === 0n ? {} : { idempotencyToken: token }),
    limits: {
      maxCount: Number(view.getBigUint64(16 + offset, true)),
      maxRequestBytes: Number(view.getBigUint64(24 + offset, true)),
      maxResponseBytes: Number(view.getBigUint64(32 + offset, true)),
      maxWorkUnits: Number(view.getBigUint64(40 + offset, true)),
      maxMemoryBytes: Number(view.getBigUint64(48 + offset, true)),
    },
    durability: (["strict", "group", "memory"] as const)[view.getUint8(56 + offset)] ?? "strict",
  };
  const securityMutation = (kind >= 48 && kind <= 53) || (kind >= 55 && kind <= 68) || kind === 70;
  if (securityMutation && token === 0n) throw new ClientError("security mutation requires a nonzero idempotencyToken");
  if (securityMutation && options.durability !== "strict") throw new ClientError("security mutation requires strict durability");
  return { operation, args: decodeOperation(operation, payload.subarray(extended ? 80 : 64)), options };
}

export function decodeProductResponse(encoded: Uint8Array, requestId: bigint, negotiatedMinor?: number): Response {
  const [kind, payload] = envelope(encoded, "HYPRSP01");
  if (negotiatedMinor !== undefined && negotiatedMinor < responseRequiredMinor(kind)) {
    throw new ClientError("native response is unavailable at the negotiated protocol minor");
  }
  const reader = new Reader(payload);
  if (kind === 1) {
    const value = {
      productApiVersion: reader.u16(),
      nativeDirectoryFormat: reader.u16(),
      logicalCatalogCodecVersion: reader.u16(),
      catalogTreeFormatVersion: reader.u16(),
      maxCatalogItems: reader.u64(),
      maxCatalogVisits: reader.u64(),
      maxCatalogBytes: reader.u64(),
      maxSqlStatementBytes: reader.u64(),
      maxSqlParameters: reader.u64(),
      maxSqlRows: reader.u64(),
    };
    reader.finish();
    return { kind: "capabilities", value, requestId };
  }
  if (kind === 2) {
    const value = {
      handle: reader.u64(),
      catalogVersion: reader.u64(),
      parameterCount: reader.u64(),
      maximumResultRows: reader.u64(),
    };
    reader.finish();
    return { kind: "prepared_sql", value, requestId };
  }
  if (kind === 3) {
    const flags = reader.u8();
    reader.zeroes(7);
    const value = {
      snapshot: (flags & 1) === 0 ? undefined : decodeSnapshot(reader),
      commit: (flags & 2) === 0 ? undefined : decodeCommitOutcome(reader),
      result: decodeSqlResult(reader),
    };
    reader.finish();
    return { kind: "sql", value, requestId };
  }
  if (kind === 4) {
    const present = reader.u8();
    reader.zeroes(3);
    if (present > 1) throw new ClientError("structure response is malformed");
    const value = present === 0 ? undefined : reader.bytes();
    reader.finish();
    return { kind: "structure_value", value, requestId };
  }
  if (kind === 5) {
    const value = decodeCommitOutcome(reader);
    reader.finish();
    return { kind: "structure_set", value, requestId };
  }
  if (kind === 6) {
    const tag = reader.u8();
    const value = tag === 0 ? { state: "missing" } : tag === 1 ? { state: "persistent" } :
      tag === 2 ? { state: "remaining", remainingMicros: reader.i64() } : undefined;
    if (value === undefined) throw new ClientError("structure TTL response is malformed");
    reader.finish();
    return { kind: "structure_ttl", value, requestId };
  }
  if (kind === 7) {
    const value = decodeTransactionStatus(reader);
    reader.finish();
    return { kind: "transaction_status", value, requestId };
  }
  if (kind === 8) {
    const count = reader.u32();
    reader.zeroes(4);
    const value = {
      documentsExamined: reader.u64(),
      sourceBytes: reader.u64(),
      tokenVisits: reader.u64(),
      tokenComparisons: reader.u64(),
      fuzzySteps: reader.u64(),
      hits: Array.from({ length: boundedCount(count, MAX_PRODUCT_COUNT, reader, 12, "search hit") }, () => ({ documentId: reader.bytes(), score: reader.f64() })),
    };
    reader.finish();
    return { kind: "search", value, requestId };
  }
  if (kind === 9) {
    const snapshot = decodeSnapshot(reader);
    const fields = Array.from({ length: 10 }, () => reader.u64());
    const value = { snapshot, fields };
    reader.finish();
    return { kind: "admin_status", value, requestId };
  }
  if (kind === 10) {
    const value = {
      transactionId: reader.u128(),
      visibleCsn: reader.u64(),
      manifestGeneration: reader.u64(),
      manifestDigest: reader.take(32),
      checkpointLsn: reader.u64(),
      parentDirectorySyncSupported: reader.boolean(),
    };
    reader.finish();
    return { kind: "admin_checkpoint", value, requestId };
  }
  if (kind === 11) {
    reader.finish();
    return { kind: "deallocated", value: undefined, requestId };
  }
  if (kind === 12) {
    const value = { snapshot: decodeSnapshot(reader), definition: reader.bytes() };
    reader.finish();
    return { kind: "catalog_object", value, requestId };
  }
  if (kind === 13 || kind === 14) {
    const value = decodeCatalogPage(reader, kind === 14);
    reader.finish();
    return { kind: kind === 13 ? "catalog_page" : "catalog_dependency_page", value, requestId };
  }
  if (kind === 15) {
    const present = reader.u8();
    reader.zeroes(3);
    if (present > 1) throw new ClientError("catalog definition response is malformed");
    const value = present === 0 ? undefined : reader.bytes();
    reader.finish();
    return { kind: "catalog_definition", value, requestId };
  }
  if (kind === 16) {
    const value = decodeCommitOutcome(reader);
    reader.finish();
    return { kind: "catalog_created", value, requestId };
  }
  if (kind === 42) {
    const cursor = reader.bytes();
    const count = reader.u32();
    if (count > MAX_CATALOG_VISIBLE_ITEMS || count > Math.floor(reader.remaining / 36)) {
      throw new ClientError("catalog visible page item count exceeds its bound");
    }
    const items = Array.from({ length: count }, () => {
      const id = reader.u128();
      const objectKind = reader.u8();
      const hasParent = reader.boolean();
      reader.zeroes(6);
      return { id, objectKind, parent: hasParent ? reader.u128() : undefined, name: decodeQualifiedName(reader) };
    });
    reader.finish();
    return { kind: "catalog_visible_page", value: { cursor: cursor.byteLength === 0 ? undefined : cursor, items }, requestId };
  }
  if (kind === 43) {
    const keyId = checkedNonzeroBytes(reader.take(16), "API key identity");
    const value = {
      keyId,
      principalId: decodeSecurityId(reader),
      predecessorKeyId: decodeOptionalApiKeyId(reader),
      authorizationEpoch: reader.u64(),
      commit: decodeCommitReceipt(reader),
      secret: decodeApiKeySecret(reader, keyId),
    };
    if (value.authorizationEpoch === 0n) throw new ClientError("authorization epoch is zero");
    reader.finish();
    return { kind: "security_api_key_started", value, requestId };
  }
  if (kind === 44) {
    const value = {
      keyId: checkedNonzeroBytes(reader.take(16), "API key identity"),
      predecessorKeyId: decodeOptionalApiKeyId(reader),
      overlapUntilMicros: decodeOptionalI64(reader),
      authorizationEpoch: decodeAuthorizationEpoch(reader),
      commit: decodeCommitReceipt(reader),
    };
    reader.finish();
    return { kind: "security_api_key_activated", value, requestId };
  }
  if (kind === 32) {
    const value = decodeSecurityStatus(reader);
    reader.finish();
    return { kind: "security_status", value, requestId };
  }
  if (kind >= 33 && kind <= 36) {
    const family = (["principal", "role", "assignment", "key"] as const)[kind - 33];
    if (family === undefined) throw new ClientError("security page family is invalid");
    const value = decodeSecurityPage(reader, family);
    reader.finish();
    return { kind: `security_${family}_page`, value, requestId };
  }
  if (kind === 37) {
    const value = decodeSecurityAuditPage(reader);
    reader.finish();
    return { kind: "security_audit_page", value, requestId };
  }
  if (kind >= 38 && kind <= 40) {
    const identity = ["principalId", "roleId", "assignmentId"][kind - 38];
    const responseKind = ["security_principal_mutated", "security_custom_role_mutated", "security_assignment_mutated"][kind - 38];
    if (identity === undefined || responseKind === undefined) throw new ClientError("security mutation response is invalid");
    const value = { [identity]: decodeSecurityId(reader), authorizationEpoch: decodeAuthorizationEpoch(reader), commit: decodeCommitReceipt(reader) };
    reader.finish();
    return { kind: responseKind, value, requestId };
  }
  if (kind === 41) {
    const value = { authorizationEpoch: decodeAuthorizationEpoch(reader), commit: decodeCommitReceipt(reader) };
    reader.finish();
    return { kind: "security_mutated", value, requestId };
  }
  if (kind === 17) {
    if (reader.u8() !== 0) throw new ClientError("explain response is not an SQL plan");
    reader.zeroes(3);
    const version = reader.u16();
    reader.zeroes(2);
    const visibleCsn = reader.u64();
    const catalogVersion = reader.u64();
    const executed = reader.boolean();
    reader.zeroes(7);
    const value = { version, visibleCsn, catalogVersion, executed, text: reader.text() };
    reader.finish();
    return { kind: "explain", value, requestId };
  }
  if (kind === 18) {
    const value = decodeDoctor(reader);
    reader.finish();
    return { kind: "doctor", value, requestId };
  }
  if (kind === 19) {
    const value = { path: reader.text(), visibleCsn: reader.u64(), checkpointDigest: reader.take(32), fileCount: reader.u64(), totalBytes: reader.u64() };
    reader.finish();
    return { kind: "backup", value, requestId };
  }
  if (kind === 20) {
    const value = decodeTelemetry(reader);
    reader.finish();
    return { kind: "telemetry", value, requestId };
  }
  if (kind === 21) {
    const proofKind = reader.u8();
    const semanticReexecutionPerformed = reader.boolean();
    reader.zeroes(6);
    const value = {
      proofKind,
      semanticReexecutionPerformed,
      anchorDigest: reader.take(32),
      proofDigest: reader.take(32),
      witnessDigest: reader.take(32),
      requestDigest: reader.take(32),
      resultDigest: reader.take(32),
      evidenceDigest: reader.take(32),
      fileCount: reader.u64(),
      directoryCount: reader.u64(),
      totalFileBytes: reader.u64(),
    };
    reader.finish();
    return { kind: "proof_verification", value, requestId };
  }
  if (kind === 22) {
    const value = decodeIntegratedSearch(reader);
    reader.finish();
    return { kind: "integrated_search", value, requestId };
  }
  if (kind === 26) {
    const snapshot = decodeSnapshot(reader);
    const hasCommit = reader.boolean();
    const idempotentReplay = reader.boolean();
    reader.zeroes(6);
    const value = { snapshot, documents: reader.u64(), idempotentReplay, commit: hasCommit ? decodeCommitReceipt(reader) : undefined };
    reader.finish();
    return { kind: "search_ingested", value, requestId };
  }
  if (kind === 31) {
    const response = decodeProductResponse(reader.bytes(), requestId, negotiatedMinor);
    const value = {
      response,
      proof: reader.bytes(),
      witness: reader.bytes(),
      trustedAnchor: reader.take(32),
    };
    reader.finish();
    return { kind: "proven", value, requestId };
  }
  if (kind === 23) {
    const value = decodeCommitOutcome(reader);
    reader.finish();
    return { kind: "structure_mutated", value, requestId };
  }
  if (kind === 24) {
    const value = { snapshot: decodeSnapshot(reader), result: decodeStructureRead(reader) };
    reader.finish();
    return { kind: "structure_read", value, requestId };
  }
  if (kind === 25) {
    const value = {
      dataPath: reader.text(),
      backup: { path: reader.text(), visibleCsn: reader.u64(), checkpointDigest: reader.take(32), fileCount: reader.u64(), totalBytes: reader.u64() },
      doctor: decodeDoctor(reader),
      phases: [] as number[],
    };
    const phaseCount = reader.u32();
    value.phases = Array.from({ length: boundedCount(phaseCount, 6, reader, 1, "restore phase") }, () => reader.u8());
    reader.finish();
    return { kind: "restore", value, requestId };
  }
  if (kind === 27) {
    const value = decodeExplicitTransactionStatus(reader);
    reader.finish();
    return { kind: "explicit_transaction_status", value, requestId };
  }
  if (kind === 28) {
    const value = {
      handle: reader.u64(),
      operationOrdinal: reader.u64(),
      changed: reader.boolean(),
      result: decodeTransactionStageResult(reader),
    };
    reader.finish();
    return { kind: "transaction_staged", value, requestId };
  }
  if (kind === 29) {
    const value = { handle: reader.u64(), stagedOperations: reader.u64(), commit: decodeCommitReceipt(reader) };
    reader.finish();
    return { kind: "transaction_committed", value, requestId };
  }
  if (kind === 30) {
    const value = { handle: reader.u64(), discardedOperations: reader.u64() };
    reader.finish();
    return { kind: "transaction_rolled_back", value, requestId };
  }
  return { kind: `response_${kind}`, value: payload.slice(), requestId };
}

function decodeCatalogPage(reader: Reader, dependencies: boolean): Readonly<Record<string, unknown>> {
  const snapshot = decodeSnapshot(reader);
  const cursor = decodeCursor(reader);
  const stops = ["exhausted", "item_limit", "visit_limit", "byte_limit"];
  const stopTag = reader.u8();
  reader.zeroes(7);
  const stop = stops[stopTag];
  if (stop === undefined) throw new ClientError("catalog page stop is invalid");
  const visited = reader.u64();
  const returnedBytes = reader.u64();
  const count = reader.u32();
  const bounded = boundedCount(count, MAX_PRODUCT_COUNT, reader, dependencies ? 33 : 48, "catalog page item");
  const items = dependencies
    ? Array.from({ length: bounded }, () => ({ dependent: reader.u128(), prerequisite: reader.u128(), kind: reader.u8() }))
    : Array.from({ length: bounded }, () => {
      const id = reader.u128();
      const objectKind = reader.u8();
      const hasParent = reader.boolean();
      reader.zeroes(6);
      return { id, objectKind, parent: hasParent ? reader.u128() : undefined, name: decodeQualifiedName(reader) };
    });
  return { snapshot, cursor, stop, visited, returnedBytes, items };
}

function decodeCursor(reader: Reader): Readonly<Record<string, unknown>> | undefined {
  const present = reader.boolean();
  reader.zeroes(7);
  return present ? { snapshot: decodeSnapshot(reader), after: reader.u128() } : undefined;
}

function decodeQualifiedName(reader: Reader): Readonly<Record<string, unknown>> {
  const component = (): Readonly<Record<string, string>> => ({ display: reader.text(), lookup: reader.text() });
  return { database: component(), schema: component(), object: component() };
}

function decodeDoctor(reader: Reader): Readonly<Record<string, unknown>> {
  const statuses = ["healthy", "busy", "corrupt", "io"];
  const status = statuses[reader.u8()];
  if (status === undefined) throw new ClientError("doctor status is invalid");
  const verifiedOpen = reader.boolean();
  const snapshotVerified = reader.boolean();
  const hasLineage = reader.boolean();
  const hasRecovery = reader.boolean();
  reader.zeroes(3);
  const telemetryRegistryVersion = reader.u16();
  reader.zeroes(2);
  const processStartIdentity = reader.u128();
  const sessionStartIdentity = reader.u128();
  const directoryLineage = hasLineage ? reader.take(24) : undefined;
  const recovery = hasRecovery ? {
    visibleCsn: reader.u64(),
    replayedTransactions: reader.u64(),
    pageTailBytesRemoved: reader.u64(),
    walTailBytesRemoved: reader.u64(),
    retainedWalBytes: reader.u64(),
    manifestCount: reader.u64(),
    blobCount: reader.u64(),
    openTimeMicros: reader.u64(),
  } : undefined;
  return { status, verifiedOpen, snapshotVerified, telemetryRegistryVersion, processStartIdentity, sessionStartIdentity, directoryLineage, recovery };
}

function decodeTelemetry(reader: Reader): Readonly<Record<string, unknown>> {
  const registryVersion = reader.u16();
  reader.zeroes(2);
  const processStartIdentity = reader.u128();
  const sessionStartIdentity = reader.u128();
  const capturedAtMicros = reader.i64();
  const catalogVersion = reader.u64();
  const droppedEvents = reader.u64();
  const metricCount = reader.u32();
  const eventCount = reader.u32();
  if (metricCount > MAX_TELEMETRY_METRICS || eventCount > MAX_TELEMETRY_EVENTS) {
    throw new ClientError("telemetry response count exceeds its bound");
  }
  const metrics = Array.from({ length: boundedCount(metricCount, MAX_TELEMETRY_METRICS, reader, 5, "telemetry metric") }, () => {
    const name = reader.text();
    const kind = reader.u8();
    if (kind === 0 || kind === 1) return { name, kind: kind === 0 ? "counter" : "gauge", value: reader.u64() };
    if (kind === 2) return { name, kind: "histogram", count: reader.u64(), sumMicros: reader.u64(), buckets: Array.from({ length: 11 }, () => reader.u64()) };
    throw new ClientError("telemetry metric kind is invalid");
  });
  const events = Array.from({ length: boundedCount(eventCount, MAX_TELEMETRY_EVENTS, reader, 16, "telemetry event") }, () => {
    const capturedAtMicros = reader.i64();
    const kind = reader.u8();
    const category = reader.u8();
    reader.zeroes(6);
    return { capturedAtMicros, kind, category };
  });
  return { registryVersion, processStartIdentity, sessionStartIdentity, capturedAtMicros, catalogVersion, droppedEvents, metrics, events };
}

function decodeSnapshot(reader: Reader): Readonly<Record<string, unknown>> {
  return {
    directoryLineage: reader.take(24),
    visibleCsn: reader.u64(),
    catalogVersion: reader.u64(),
    rootDigest: reader.take(32),
    logicalTimeMicros: reader.i64(),
  };
}

function decodeCommitReceipt(reader: Reader): Readonly<Record<string, unknown>> {
  const transactionId = reader.u128();
  const commitCsn = reader.u64();
  const catalogVersion = reader.u64();
  const commitLsn = reader.u64();
  const walBlockDigest = reader.take(32);
  const durability = reader.u8();
  reader.zeroes(7);
  const durabilityCohortSize = reader.u64();
  const durabilityCohortPosition = reader.u64();
  if (transactionId === 0n || commitCsn === 0n || catalogVersion === 0n || commitLsn === 0n ||
      walBlockDigest.every((byte) => byte === 0) || durability > 2 || durabilityCohortSize === 0n ||
      durabilityCohortPosition >= durabilityCohortSize) {
    throw new ClientError("commit receipt is noncanonical");
  }
  return { transactionId, commitCsn, catalogVersion, commitLsn, walBlockDigest, durability, durabilityCohortSize, durabilityCohortPosition };
}

function decodeOptionalApiKeyId(reader: Reader): Uint8Array | undefined {
  const present = reader.boolean();
  reader.zeroes(7);
  const value = reader.take(16);
  if (!present && value.some((byte) => byte !== 0)) throw new ClientError("optional API key identity is malformed");
  return present ? checkedNonzeroBytes(value, "optional API key identity") : undefined;
}

function decodeOptionalI64(reader: Reader): bigint | undefined {
  const present = reader.boolean();
  reader.zeroes(7);
  const value = reader.i64();
  if (!present && value !== 0n) throw new ClientError("optional instant is malformed");
  return present ? value : undefined;
}

function decodeAuthorizationEpoch(reader: Reader): bigint {
  const value = reader.u64();
  if (value === 0n) throw new ClientError("authorization epoch is zero");
  return value;
}

function decodeSecurityStatus(reader: Reader): Readonly<Record<string, unknown>> {
  const bootstrapped = reader.boolean();
  reader.zeroes(7);
  const authorizationEpoch = reader.u64();
  const names = ["principals", "assignments", "customRoles", "customAssignments", "keys", "pendingKeys", "auditEvents"] as const;
  const counts: Record<string, bigint> = {};
  for (const name of names) counts[name] = reader.u64();
  const empty = Object.values(counts).every((value) => value === 0n);
  if ((bootstrapped && (authorizationEpoch === 0n || counts.principals === 0n || counts.assignments === 0n)) ||
      (!bootstrapped && (authorizationEpoch !== 0n || !empty)) || (counts.principals ?? 0n) > 4_096n ||
      (counts.assignments ?? 0n) + (counts.customAssignments ?? 0n) > (counts.principals ?? 0n) * 128n ||
      (counts.customRoles ?? 0n) > 1_024n || (counts.keys ?? 0n) > (counts.principals ?? 0n) * 64n ||
      (counts.pendingKeys ?? 0n) > (counts.keys ?? 0n) ||
      (counts.auditEvents ?? 0n) > 100_000n) {
    throw new ClientError("security status is invalid");
  }
  return { bootstrapped, authorizationEpoch, ...counts };
}

type SecurityFamily = "principal" | "role" | "assignment" | "key";

function decodeSecurityPage(reader: Reader, family: SecurityFamily): Readonly<Record<string, unknown>> {
  const authorizationEpoch = decodeAuthorizationEpoch(reader);
  const count = reader.u32();
  reader.zeroes(4);
  if (count > MAX_SECURITY_LIST_ROWS || count > reader.remaining) throw new ClientError("security page item count is invalid");
  const nextCursor = decodeSecurityCursor(reader, family);
  if (nextCursor !== undefined && nextCursor.authorization_epoch !== authorizationEpoch) {
    throw new ClientError("security page cursor epoch differs from its page");
  }
  const decoders = {
    principal: decodeSecurityPrincipal,
    role: decodeSecurityRole,
    assignment: decodeSecurityAssignment,
    key: decodeSecurityKey,
  } as const;
  const items = Array.from({ length: count }, () => decoders[family](reader));
  return { authorizationEpoch, items, nextCursor };
}

function decodeSecurityPrincipal(reader: Reader): Readonly<Record<string, unknown>> {
  const id = decodeSecurityId(reader);
  const enabled = reader.boolean();
  reader.zeroes(7);
  return { id, displayName: decodeSecurityText(reader), enabled };
}

function decodeSecurityRole(reader: Reader): Readonly<Record<string, unknown>> {
  const kind = reader.u8();
  if (kind === 0) {
    const role = decodeBuiltInRole(reader);
    reader.zeroes(6);
    return { kind: "built_in", role, displayName: role, grants: [] };
  }
  if (kind === 1) {
    reader.zeroes(7);
    return { kind: "custom", id: decodeSecurityId(reader), displayName: decodeSecurityText(reader), grants: decodeSecurityGrants(reader) };
  }
  throw new ClientError("security role kind is invalid");
}

function decodeSecurityAssignment(reader: Reader): Readonly<Record<string, unknown>> {
  const id = decodeSecurityId(reader);
  const principalId = decodeSecurityId(reader);
  const kind = reader.u8();
  if (kind === 0) {
    const role = decodeBuiltInRole(reader);
    reader.zeroes(6);
    return { id, principalId, kind: "built_in", role, scope: decodeProductScope(reader) };
  }
  if (kind === 1) {
    reader.zeroes(7);
    return { id, principalId, kind: "custom", roleId: decodeSecurityId(reader) };
  }
  throw new ClientError("security assignment kind is invalid");
}

function decodeSecurityKey(reader: Reader): Readonly<Record<string, unknown>> {
  const id = checkedNonzeroBytes(reader.take(16), "API key identity");
  const principalId = decodeSecurityId(reader);
  const flags = reader.u8();
  reader.zeroes(7);
  if ((flags & ~3) !== 0) throw new ClientError("security key flags are invalid");
  const label = decodeSecurityText(reader);
  const roleCount = reader.u32();
  reader.zeroes(4);
  if (roleCount > BUILT_IN_ROLES.length) throw new ClientError("security key role count is invalid");
  const roles = Array.from({ length: roleCount }, () => decodeBuiltInRole(reader));
  const customCount = reader.u32();
  reader.zeroes(4);
  if (customCount > MAX_SECURITY_ASSIGNMENTS) throw new ClientError("security key custom-role count is invalid");
  const customRoles = Array.from({ length: customCount }, () => decodeSecurityId(reader));
  const permissionCeilingBits = reader.u64();
  if (permissionCeilingBits >> BigInt(PRODUCT_PERMISSIONS.length) !== 0n) throw new ClientError("security key permission ceiling has unknown bits");
  const scopeCount = reader.u32();
  reader.zeroes(4);
  if (scopeCount === 0 || scopeCount > MAX_SECURITY_ASSIGNMENTS) throw new ClientError("security key scope count is invalid");
  const scopeCeiling = Array.from({ length: scopeCount }, () => decodeProductScope(reader));
  const createdAtMicros = reader.i64();
  const expiresAtMicros = decodeFixedOptionalI64(reader);
  const publishedEpoch = decodeAuthorizationEpoch(reader);
  const predecessorId = decodeOptionalApiKeyId(reader);
  const successorId = decodeOptionalApiKeyId(reader);
  const overlapUntilMicros = decodeFixedOptionalI64(reader);
  const rotationOverlapMicros = decodeOptionalU64(reader);
  return {
    id, principalId, label, active: (flags & 1) !== 0, revoked: (flags & 2) !== 0, roles, customRoles,
    permissionCeiling: PRODUCT_PERMISSIONS.filter((_, index) => (permissionCeilingBits & 1n << BigInt(index)) !== 0n),
    scopeCeiling, createdAtMicros, expiresAtMicros, publishedEpoch, predecessorId, successorId,
    overlapUntilMicros, rotationOverlapMicros,
  };
}

function decodeSecurityAuditPage(reader: Reader): Readonly<Record<string, unknown>> {
  const count = reader.u32();
  reader.zeroes(4);
  if (count > MAX_SECURITY_LIST_ROWS || count > reader.remaining) throw new ClientError("security audit page count is invalid");
  const nextCursor = decodeOptionalSecurityId(reader);
  const events = Array.from({ length: count }, () => decodeSecurityAuditEvent(reader));
  return { events, nextCursor };
}

function decodeSecurityAuditEvent(reader: Reader): Readonly<Record<string, unknown>> {
  const id = decodeSecurityId(reader);
  const commitCsn = reader.u64();
  if (commitCsn === 0n) throw new ClientError("security audit commit CSN is zero");
  const hasActor = reader.boolean();
  reader.zeroes(7);
  const principalWire = reader.take(16);
  const keyWire = reader.take(16);
  const actorPrincipalId = hasActor ? securityIdFromBytes(principalWire) : undefined;
  const actorKeyId = hasActor ? checkedNonzeroBytes(keyWire, "security audit actor key") : undefined;
  if (!hasActor && (principalWire.some((byte) => byte !== 0) || keyWire.some((byte) => byte !== 0))) throw new ClientError("absent security audit actor is noncanonical");
  const action = SECURITY_AUDIT_ACTIONS[reader.u8()];
  const result = reader.u8();
  reader.zeroes(6);
  if (action === undefined || result !== 0) throw new ClientError("security audit action or result is invalid");
  const targetCount = reader.u32();
  reader.zeroes(4);
  if (targetCount === 0 || targetCount > MAX_SECURITY_ASSIGNMENTS) throw new ClientError("security audit target count is invalid");
  const targets = Array.from({ length: targetCount }, () => {
    const tag = reader.u8();
    reader.zeroes(7);
    const raw = reader.take(16);
    if (tag === 4 && raw.every((byte) => byte === 0)) return { kind: "legacy_bearer" };
    const kind = (["principal", "role", "assignment", "key"] as const)[tag];
    if (kind === undefined) throw new ClientError("security audit target is invalid");
    return { kind, id: kind === "key" ? checkedNonzeroBytes(raw, "API key identity") : securityIdFromBytes(raw) };
  });
  const metadataCount = reader.u32();
  reader.zeroes(4);
  if (metadataCount > MAX_SECURITY_ASSIGNMENTS) throw new ClientError("security audit metadata count is invalid");
  const metadata = Array.from({ length: metadataCount }, () => {
    const kind = (["expires_at_micros", "rotation_overlap_until_micros"] as const)[reader.u8()];
    reader.zeroes(7);
    if (kind === undefined) throw new ClientError("security audit metadata is invalid");
    return { kind, value: reader.i64() };
  });
  return { id, commitCsn, actorPrincipalId, actorKeyId, action, result: "succeeded", targets, metadata };
}

function decodeFixedOptionalI64(reader: Reader): bigint | undefined {
  const present = reader.boolean();
  reader.zeroes(7);
  const value = reader.i64();
  if (!present && value !== 0n) throw new ClientError("absent optional instant is noncanonical");
  return present ? value : undefined;
}

function decodeOptionalU64(reader: Reader): bigint | undefined {
  const present = reader.boolean();
  reader.zeroes(7);
  const value = reader.u64();
  if (!present && value !== 0n) throw new ClientError("absent optional integer is noncanonical");
  return present ? value : undefined;
}

function decodeCommitOutcome(reader: Reader): Readonly<Record<string, unknown>> {
  const tag = reader.u8();
  reader.zeroes(7);
  if (tag === 0) return { state: "committed", receipt: decodeCommitReceipt(reader) };
  if (tag === 1) return { state: "outcome_unknown", transactionId: reader.u128() };
  throw new ClientError("commit outcome is malformed");
}

function decodeTransactionStatus(reader: Reader): Readonly<Record<string, unknown>> {
  const tag = reader.u8();
  if (tag === 0) return { state: "unknown" };
  if (tag === 1) return { state: "committed", receipt: decodeCommitReceipt(reader) };
  if (tag === 2 || tag === 3) return { state: tag === 2 ? "rolled_back" : "outcome_unknown", transactionId: reader.u128() };
  throw new ClientError("transaction status is malformed");
}

function decodeExplicitTransactionStatus(reader: Reader): Readonly<Record<string, unknown>> {
  const tag = reader.u8();
  if (tag === 0) return { state: "unknown" };
  if (tag === 1) {
    const handle = reader.u64();
    const readCsn = reader.u64();
    const stagedOperations = reader.u64();
    const durabilityTag = reader.u8();
    const durability = ["strict", "group", "memory"][durabilityTag];
    if (durability === undefined) throw new ClientError("explicit transaction durability is invalid");
    return { handle, state: "active", readCsn: readCsn === 0n ? undefined : readCsn, stagedOperations, durability };
  }
  if (tag === 2) {
    return { state: "committed", handle: reader.u64(), stagedOperations: reader.u64(), receipt: decodeCommitReceipt(reader) };
  }
  if (tag === 3) {
    return { state: "rolled_back", handle: reader.u64(), discardedOperations: reader.u64() };
  }
  if (tag === 4) {
    return { state: "outcome_unknown", handle: reader.u64(), transactionId: reader.u128(), stagedOperations: reader.u64() };
  }
  throw new ClientError("explicit transaction status is malformed");
}

function decodeTransactionStageResult(reader: Reader): Readonly<Record<string, unknown>> {
  const tag = reader.u8();
  if (tag === 0) return { kind: "sql", result: decodeSqlResult(reader) };
  if (tag === 1) return { kind: "structure", result: decodeStructureMutationResult(reader) };
  if (tag === 2) return { kind: "search" };
  if (tag === 3) return { kind: "vector", changed: reader.boolean() };
  throw new ClientError("transaction stage result is malformed");
}

function decodeStructureMutationResult(reader: Reader): Readonly<Record<string, unknown>> {
  const tag = reader.u8();
  if (tag === 0) return { kind: "unit" };
  if (tag === 1) return { kind: "integer", value: reader.i64() };
  if (tag === 2) return { kind: "boolean", value: reader.boolean() };
  if (tag === 3) return { kind: "count", value: reader.u64() };
  if (tag === 4) return { kind: "value", value: reader.boolean() ? reader.bytes() : undefined };
  if (tag === 5) return { kind: "stream_id", value: reader.u64() };
  if (tag === 6) return { kind: "score", value: reader.f64() };
  if (tag === 7) return { kind: "popped_entry", entry: reader.boolean() ? { member: reader.bytes(), score: reader.f64() } : undefined };
  throw new ClientError("structure mutation result is malformed");
}

function decodeSqlResult(reader: Reader): Readonly<Record<string, unknown>> {
  const tag = reader.u8();
  if (tag === 0) {
    const hasObject = reader.boolean();
    reader.zeroes(6);
    return { kind: "command", rowsAffected: reader.u64(), objectId: hasObject ? reader.u128() : undefined };
  }
  if (tag === 1) {
    reader.zeroes(7);
    const columnCount = reader.u32();
    const rowCount = reader.u32();
    if (columnCount > MAX_SQL_COLUMNS || rowCount > MAX_SQL_ROWS || columnCount > Math.floor(reader.remaining / 4)) {
      throw new ClientError("SQL row or column count exceeds its bound");
    }
    const columns = Array.from({ length: columnCount }, () => reader.text());
    if (rowCount > 0 && columnCount > 0 && BigInt(rowCount) * BigInt(columnCount) > BigInt(reader.remaining)) {
      throw new ClientError("SQL cell count exceeds its envelope bound");
    }
    const rows = Array.from({ length: rowCount }, () => Array.from({ length: columnCount }, () => decodeValue(reader, 0)));
    return { kind: "rows", columns, rows };
  }
  throw new ClientError("SQL result is malformed");
}

function decodeValue(reader: Reader, depth: number): unknown {
  if (depth > 8) throw new ClientError("SQL value nesting is too deep");
  const tag = reader.u8();
  if (tag === 0) return null;
  if (tag === 1) return reader.boolean();
  if (tag === 2) return reader.i64();
  if (tag === 3) return reader.u64();
  if (tag === 5) return reader.f32();
  if (tag === 6) return reader.f64();
  if (tag === 7) return reader.text();
  if (tag === 8) return reader.bytes();
  if (tag === 9) return { dateDays: reader.i32() };
  if (tag === 10) return { timeNanos: reader.u64() };
  if (tag === 11) return { timestampMicros: reader.i64() };
  if (tag === 12) return { months: reader.i32(), days: reader.i32(), nanoseconds: reader.i64() };
  if (tag === 13) return { uuid: reader.take(16) };
  if (tag === 14) return Array.from({ length: readBoundedCount(reader, MAX_PRODUCT_COUNT, 1, "SQL array value") }, () => decodeValue(reader, depth + 1));
  if (tag === 15) return { map: Array.from({ length: readBoundedCount(reader, MAX_PRODUCT_COUNT, 2, "SQL map value") }, () => [decodeValue(reader, depth + 1), decodeValue(reader, depth + 1)]) };
  if (tag === 16) return { vector: Array.from({ length: readBoundedCount(reader, MAX_PRODUCT_COUNT, 4, "SQL vector value") }, () => reader.f32()) };
  if (tag === 17) return { json: reader.text() };
  throw new ClientError("SQL value kind is unsupported");
}

export function decodeProductError(encoded: Uint8Array): ProductErrorFields {
  if (encoded.byteLength < 20 || new TextDecoder().decode(encoded.subarray(0, 8)) !== "HYPERR01") {
    throw new ClientError("HYPERR01 envelope is malformed");
  }
  const view = new DataView(encoded.buffer, encoded.byteOffset, encoded.byteLength);
  if (view.getUint32(8, true) !== encoded.byteLength) throw new ClientError("HYPERR01 length is invalid");
  const categories = ["invalid-request", "not-found", "conflict", "limit", "deadline", "cancelled", "authorization", "corruption", "unavailable", "io", "internal"];
  const retries = ["never", "same-request", "new-snapshot", "after-backoff", "after-recovery", "unknown-commit"];
  const states = ["none", "active", "rolled-back", "committed", "outcome-unknown"] as const;
  const flags = view.getUint8(15);
  let offset = 20;
  const [code, codeEnd] = takeText(encoded, offset, view.getUint8(16));
  offset = codeEnd;
  const [message, messageEnd] = takeText(encoded, offset, view.getUint16(17, true));
  offset = messageEnd;
  const identities: Array<bigint | undefined> = [];
  for (let bit = 0; bit < 3; bit += 1) {
    identities.push((flags & (1 << bit)) === 0 ? undefined : readU128(encoded, offset));
    if ((flags & (1 << bit)) !== 0) offset += 16;
  }
  let limit: ProductErrorFields["limit"];
  if ((flags & 8) !== 0) {
    const length = encoded[offset];
    if (length === undefined) throw new ClientError("HYPERR01 limit is truncated");
    offset += 1;
    const [kind, end] = takeText(encoded, offset, length);
    offset = end;
    limit = { kind, configured: new DataView(encoded.buffer, encoded.byteOffset + offset).getBigUint64(0, true), observed: new DataView(encoded.buffer, encoded.byteOffset + offset).getBigUint64(8, true) };
    offset += 16;
  }
  let sourceSpan: ProductErrorFields["sourceSpan"];
  if ((flags & 16) !== 0) {
    sourceSpan = { start: view.getUint32(offset, true), end: view.getUint32(offset + 4, true) };
    offset += 8;
  }
  const details: Record<string, unknown> = {};
  let transactionId: bigint | undefined;
  let previous = 0;
  for (let index = 0; index < view.getUint8(19); index += 1) {
    const tag = view.getUint16(offset, true);
    const length = view.getUint16(offset + 2, true);
    offset += 4;
    if (tag <= previous || offset + length > encoded.byteLength) throw new ClientError("HYPERR01 details are noncanonical");
    const value = encoded.subarray(offset, offset + length);
    offset += length;
    previous = tag;
    if (tag === 1) details.sqlSubcode = new TextDecoder().decode(value);
    else if (tag === 2) transactionId = readU128(value, 0);
    else details[`unknown_${tag}`] = value;
  }
  if (offset !== encoded.byteLength) throw new ClientError("HYPERR01 has trailing bytes");
  const category = categories[view.getUint8(12)];
  const retry = retries[view.getUint8(13)];
  const transactionState = states[view.getUint8(14)];
  if (category === undefined || retry === undefined || transactionState === undefined) throw new ClientError("HYPERR01 discriminant is invalid");
  return {
    code,
    category,
    retry,
    message,
    ...(identities[0] === undefined ? {} : { requestId: identities[0] }),
    ...(identities[1] === undefined ? {} : { traceId: identities[1] }),
    ...(identities[2] === undefined ? {} : { objectId: identities[2] }),
    transactionState,
    ...(transactionId === undefined ? {} : { transactionId }),
    ...(limit === undefined ? {} : { limit }),
    ...(sourceSpan === undefined ? {} : { sourceSpan }),
    details,
  };
}

export function encodeCancel(reason = 1): Uint8Array {
  const encoded = new Uint8Array(16);
  encoded.set(new TextEncoder().encode("HYPCAN01"));
  new DataView(encoded.buffer).setUint32(8, reason, true);
  return encoded;
}

export function encodeWindowUpdate(increment: bigint): Uint8Array {
  if (increment <= 0n) throw new ClientError("window update must be positive");
  const encoded = new Uint8Array(16);
  encoded.set(new TextEncoder().encode("HYPWIN01"));
  new DataView(encoded.buffer).setBigUint64(8, increment, true);
  return encoded;
}

export function decodeEnd(encoded: Uint8Array): { readonly totalBytes: bigint; readonly digest: Uint8Array } {
  if (encoded.byteLength !== 56 || new TextDecoder().decode(encoded.subarray(0, 8)) !== "HYPEND01" || encoded[12] !== 1) {
    throw new ClientError("native completion is malformed");
  }
  return { totalBytes: new DataView(encoded.buffer, encoded.byteOffset).getBigUint64(16, true), digest: encoded.slice(24) };
}

/** Dependency-free one-shot BLAKE3 used by mandatory provisional completion checks. */
export function blake3(input: Uint8Array): Uint8Array {
  const iv = [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19];
  const permutation = [2, 6, 3, 10, 7, 0, 4, 13, 1, 11, 12, 5, 9, 14, 15, 8];
  type Output = { readonly cv: number[]; readonly words: number[]; readonly counter: bigint; readonly length: number; readonly flags: number };
  const rotate = (value: number, count: number): number => (value >>> count) | (value << (32 - count));
  const compress = (cv: readonly number[], words: readonly number[], counter: bigint, length: number, flags: number): number[] => {
    const state = [...cv, ...iv.slice(0, 4), Number(counter & 0xffffffffn), Number(counter >> 32n), length, flags];
    let message = [...words];
    const mix = (a: number, b: number, c: number, d: number, x: number, y: number): void => {
      state[a] = ((state[a] ?? 0) + (state[b] ?? 0) + x) >>> 0;
      state[d] = rotate((state[d] ?? 0) ^ (state[a] ?? 0), 16) >>> 0;
      state[c] = ((state[c] ?? 0) + (state[d] ?? 0)) >>> 0;
      state[b] = rotate((state[b] ?? 0) ^ (state[c] ?? 0), 12) >>> 0;
      state[a] = ((state[a] ?? 0) + (state[b] ?? 0) + y) >>> 0;
      state[d] = rotate((state[d] ?? 0) ^ (state[a] ?? 0), 8) >>> 0;
      state[c] = ((state[c] ?? 0) + (state[d] ?? 0)) >>> 0;
      state[b] = rotate((state[b] ?? 0) ^ (state[c] ?? 0), 7) >>> 0;
    };
    for (let round = 0; round < 7; round += 1) {
      mix(0, 4, 8, 12, message[0] ?? 0, message[1] ?? 0);
      mix(1, 5, 9, 13, message[2] ?? 0, message[3] ?? 0);
      mix(2, 6, 10, 14, message[4] ?? 0, message[5] ?? 0);
      mix(3, 7, 11, 15, message[6] ?? 0, message[7] ?? 0);
      mix(0, 5, 10, 15, message[8] ?? 0, message[9] ?? 0);
      mix(1, 6, 11, 12, message[10] ?? 0, message[11] ?? 0);
      mix(2, 7, 8, 13, message[12] ?? 0, message[13] ?? 0);
      mix(3, 4, 9, 14, message[14] ?? 0, message[15] ?? 0);
      if (round !== 6) message = permutation.map((index) => message[index] ?? 0);
    }
    return [
      ...Array.from({ length: 8 }, (_, index) => ((state[index] ?? 0) ^ (state[index + 8] ?? 0)) >>> 0),
      ...Array.from({ length: 8 }, (_, index) => ((state[index + 8] ?? 0) ^ (cv[index] ?? 0)) >>> 0),
    ];
  };
  const words = (block: Uint8Array): number[] => {
    const padded = new Uint8Array(64);
    padded.set(block);
    const view = new DataView(padded.buffer);
    return Array.from({ length: 16 }, (_, index) => view.getUint32(index * 4, true));
  };
  const chainingValue = (output: Output): number[] => compress(output.cv, output.words, output.counter, output.length, output.flags).slice(0, 8);
  const root = (output: Output): Uint8Array => {
    const values = compress(output.cv, output.words, 0n, output.length, output.flags | 8);
    const encoded = new Uint8Array(64);
    const view = new DataView(encoded.buffer);
    values.forEach((value, index) => view.setUint32(index * 4, value, true));
    return encoded.slice(0, 32);
  };
  const chunkOutput = (chunk: Uint8Array, counter: bigint): Output => {
    const blocks: Uint8Array[] = [];
    for (let offset = 0; offset < chunk.byteLength; offset += 64) blocks.push(chunk.subarray(offset, offset + 64));
    if (blocks.length === 0) blocks.push(new Uint8Array());
    let cv = [...iv];
    for (let index = 0; index + 1 < blocks.length; index += 1) {
      const block = blocks[index] ?? new Uint8Array();
      cv = compress(cv, words(block), counter, block.byteLength, index === 0 ? 1 : 0).slice(0, 8);
    }
    const final = blocks[blocks.length - 1] ?? new Uint8Array();
    return { cv, words: words(final), counter, length: final.byteLength, flags: 2 | (blocks.length === 1 ? 1 : 0) };
  };
  const parentOutput = (left: readonly number[], right: readonly number[]): Output => ({ cv: [...iv], words: [...left, ...right], counter: 0n, length: 64, flags: 4 });
  const chunks: Uint8Array[] = [];
  for (let offset = 0; offset < input.byteLength; offset += 1024) chunks.push(input.subarray(offset, offset + 1024));
  if (chunks.length === 0) chunks.push(new Uint8Array());
  const stack: number[][] = [];
  for (let index = 0; index + 1 < chunks.length; index += 1) {
    let value = chainingValue(chunkOutput(chunks[index] ?? new Uint8Array(), BigInt(index)));
    let total = index + 1;
    while ((total & 1) === 0) {
      value = chainingValue(parentOutput(stack.pop() ?? [], value));
      total >>>= 1;
    }
    stack.push(value);
  }
  let output = chunkOutput(chunks[chunks.length - 1] ?? new Uint8Array(), BigInt(chunks.length - 1));
  while (stack.length > 0) output = parentOutput(stack.pop() ?? [], chainingValue(output));
  return root(output);
}

function encodeOperation(operation: string, args: Readonly<Record<string, unknown>>): Uint8Array {
  if (["capabilities", "admin_status", "admin_checkpoint", "telemetry", "transaction_begin", "security_status", "security_legacy_bearer_revoke"].includes(operation)) return new Uint8Array();
  if (operation === "structure_get" || operation === "structure_ttl") return bytes(requireBytes(args.key));
  if (operation === "structure_set") {
    const key = bytes(requireBytes(args.key));
    const value = bytes(requireBytes(args.value));
    const expiry = args.expires_at_micros;
    const suffix = new Uint8Array(expiry === undefined || expiry === null ? 8 : 16);
    const view = new DataView(suffix.buffer);
    if (expiry !== undefined && expiry !== null) {
      view.setUint8(0, 1);
      view.setBigInt64(8, BigInt(expiry as bigint | number), true);
    }
    return join(key, value, suffix);
  }
  if (operation === "sql_prepare" || operation === "admin_explain_sql") return bytes(new TextEncoder().encode(String(args.statement)));
  if (operation === "sql_deallocate") return u64(BigInt(args.handle as bigint | number));
  if (operation === "sql_execute_prepared") return join(u64(BigInt(args.handle as bigint | number)), encodeValues(args.parameters ?? []));
  if (operation === "sql_execute") return join(bytes(new TextEncoder().encode(String(args.statement))), encodeValues(args.parameters ?? []));
  if (operation === "transaction_status") return u128(BigInt(args.transaction_id as bigint | number));
  if (operation === "transaction_stage_sql") return join(u64(BigInt(args.handle as bigint | number)), bytes(new TextEncoder().encode(String(args.statement))), encodeValues(args.parameters ?? []));
  if (operation === "transaction_stage_structure") return join(u64(BigInt(args.handle as bigint | number)), encodeStructureMutation(args.mutation));
  if (operation === "transaction_stage_search") return join(u64(BigInt(args.handle as bigint | number)), encodeTransactionSearchMutation(args.mutation));
  if (operation === "transaction_stage_vector") return join(u64(BigInt(args.handle as bigint | number)), encodeTransactionVectorMutation(args.mutation));
  if (operation === "transaction_commit" || operation === "transaction_rollback" || operation === "explicit_transaction_status") return u64(BigInt(args.handle as bigint | number));
  if (operation === "transaction_status_by_idempotency") return u128(BigInt(args.idempotency_token as bigint | number));
  if (operation === "doctor") return new Uint8Array();
  if (operation === "backup") {
    const limits = args.limits as Readonly<Record<string, number>>;
    return join(
      bytes(new TextEncoder().encode(String(args.destination))),
      u64(BigInt(limits.max_files ?? 0)),
      u64(BigInt(limits.max_directories ?? 0)),
      u64(BigInt(limits.max_total_bytes ?? 0)),
      u64(BigInt(limits.max_path_bytes ?? 0)),
      u64(BigInt(limits.max_manifest_bytes ?? 0)),
    );
  }
  if (operation === "search") return join(u128(BigInt(args.index as bigint | number)), u64(BigInt(args.limit as number)), encodeQuery(args.query));
  if (operation === "catalog_object" || operation === "catalog_describe") return u128(BigInt(args.id as bigint | number));
  if (operation === "catalog_object_named" || operation === "catalog_resolve") return encodeQualifiedName(args.name);
  if (operation === "catalog_list") {
    const parent = args.parent;
    const prefix = new Uint8Array(8);
    prefix[0] = parent === undefined || parent === null ? 0 : 1;
    prefix[1] = args.kind === undefined || args.kind === null ? 0 : catalogKindTag(String(args.kind));
    return join(
      prefix,
      ...(parent === undefined || parent === null ? [] : [u128(BigInt(parent as bigint | number))]),
      encodeCursor(args.cursor),
      u64(BigInt(args.item_limit as number)),
      u64(BigInt(args.visit_limit as number)),
      u64(BigInt(args.byte_limit as number)),
    );
  }
  if (operation === "catalog_visible_list") {
    const parent = args.parent;
    const cursor = args.cursor;
    if (cursor !== undefined && !(cursor instanceof Uint8Array)) {
      throw new ClientError("catalog visible cursor must be a Uint8Array");
    }
    const prefix = new Uint8Array(8);
    prefix[0] = parent === undefined || parent === null ? 0 : 1;
    prefix[1] = args.kind === undefined || args.kind === null ? 0 : catalogKindTag(String(args.kind));
    return join(
      prefix,
      ...(parent === undefined || parent === null ? [] : [u128(BigInt(parent as bigint | number))]),
      bytes(cursor instanceof Uint8Array ? cursor : new Uint8Array()),
      u64(BigInt(args.item_limit as number)),
      u64(BigInt(args.visit_limit as number)),
      u64(BigInt(args.byte_limit as number)),
    );
  }
  if (["security_principal_list", "security_role_list", "security_assignment_list", "security_key_list"].includes(operation)) {
    const family = operation.slice("security_".length, -"_list".length);
    return join(encodeSecurityCursor(args.cursor, family), securityLimit(args.limit));
  }
  if (operation === "security_audit_read") return join(optionalSecurityId(args.cursor), securityLimit(args.limit));
  if (operation === "security_principal_create") return securityText(args.display_name);
  if (operation === "security_principal_set_enabled") {
    if (typeof args.enabled !== "boolean") throw new ClientError("security principal enabled state must be boolean");
    return join(securityId(args.principal_id), Uint8Array.of(Number(args.enabled)), new Uint8Array(7));
  }
  if (operation === "security_custom_role_create") return join(securityText(args.display_name), encodeSecurityGrants(args.grants));
  if (operation === "security_built_in_assignment_create") {
    return join(securityId(args.principal_id), Uint8Array.of(builtInRoleTag(args.role)), new Uint8Array(7), encodeProductScope(args.scope));
  }
  if (operation === "security_custom_assignment_create") return join(securityId(args.principal_id), securityId(args.role_id));
  if (operation === "security_assignment_revoke") return securityId(args.assignment_id);
  if (operation === "security_api_key_issue_self_start" || operation === "security_api_key_issue_start") {
    return join(
      securityId(args.principal_id),
      securityText(args.label),
      builtInRoles(args.roles),
      securityIds(args.custom_roles ?? []),
      u64(permissionBits(args.permission_ceiling)),
      productScopes(args.scope_ceiling),
      optionalI64(args.expires_at_micros),
    );
  }
  if (operation === "security_api_key_issue_self_activate" || operation === "security_api_key_issue_activate") {
    return join(apiKeyId(args.key_id), confirmationDigest(args.confirmation_digest));
  }
  if (operation === "security_api_key_rotate_self_start" || operation === "security_api_key_rotate_start") {
    return join(
      apiKeyId(args.predecessor_key_id),
      securityText(args.label),
      u64(BigInt(args.overlap_seconds as bigint | number)),
      optionalI64(args.expires_at_micros),
    );
  }
  if (operation === "security_api_key_rotate_self_activate" || operation === "security_api_key_rotate_activate") {
    return join(apiKeyId(args.successor_key_id), confirmationDigest(args.confirmation_digest));
  }
  if (["security_api_key_issue_self_abort", "security_api_key_issue_abort", "security_api_key_revoke_self", "security_api_key_revoke"].includes(operation)) {
    return apiKeyId(args.key_id);
  }
  if (operation === "security_api_key_rotate_self_abort" || operation === "security_api_key_rotate_abort") {
    return apiKeyId(args.successor_key_id);
  }
  if (operation === "catalog_dependencies") {
    const direction = new Uint8Array(8);
    direction[0] = args.direction === "outgoing" ? 0 : 1;
    return join(
      u128(BigInt(args.object as bigint | number)),
      direction,
      encodeCursor(args.cursor),
      u64(BigInt(args.item_limit as number)),
      u64(BigInt(args.visit_limit as number)),
      u64(BigInt(args.byte_limit as number)),
    );
  }
  if (operation === "catalog_create") return bytes(requireBytes(args.definition));
  if (operation === "proof_verify") {
    const anchor = requireBytes(args.trusted_anchor);
    if (anchor.byteLength !== 32) throw new ClientError("trusted anchor must contain 32 bytes");
    return join(bytes(requireBytes(args.proof)), bytes(requireBytes(args.witness)), anchor);
  }
  if (operation === "search_collection") return encodeSearchCollection(args);
  if (operation === "search_ingest") return join(u128(BigInt(args.collection as bigint | number)), encodeSearchBatch(args.batch));
  if (operation === "search_document_update") return join(u128(BigInt(args.collection as bigint | number)), u128(BigInt(args.idempotency_id as bigint | number)), encodeSearchDocument(args.document));
  if (operation === "search_document_delete") return join(u128(BigInt(args.collection as bigint | number)), u128(BigInt(args.idempotency_id as bigint | number)), u128(BigInt(args.object_id as bigint | number)));
  if (operation === "structure_mutate") {
    const mutations = args.mutations;
    if (!Array.isArray(mutations) || mutations.length === 0 || mutations.length > 4096) {
      throw new ClientError("structure mutations must be a nonempty bounded array");
    }
    return join(u32(mutations.length), ...mutations.map((value) => encodeStructureMutation(value)));
  }
  if (operation === "structure_read") return encodeStructureRead(args);
  if (operation === "restore") {
    const limits = args.limits as Readonly<Record<string, number>>;
    return join(
      bytes(new TextEncoder().encode(String(args.backup))),
      bytes(new TextEncoder().encode(String(args.destination))),
      u64(BigInt(limits.max_files ?? 0)),
      u64(BigInt(limits.max_directories ?? 0)),
      u64(BigInt(limits.max_total_bytes ?? 0)),
      u64(BigInt(limits.max_path_bytes ?? 0)),
      u64(BigInt(limits.max_manifest_bytes ?? 0)),
      i64(BigInt((args.doctor_logical_time_micros as bigint | number | undefined) ?? 0)),
    );
  }
  if (operation === "proof_generate") {
    const nestedOperation = String(args.operation);
    const nestedKind = REQUEST_KIND[nestedOperation];
    if (nestedKind === undefined || nestedOperation === "proof_generate") throw new ClientError("nested proof operation is invalid");
    const nestedArgs = (args.arguments ?? {}) as Readonly<Record<string, unknown>>;
    const limits = { ...DEFAULT_PROOF_LIMITS, ...(args.limits as Readonly<Record<string, bigint | number>> | undefined) };
    return join(
      u16(nestedKind),
      u16(0),
      bytes(encodeOperation(nestedOperation, nestedArgs)),
      ...Object.keys(DEFAULT_PROOF_LIMITS).map((name) => u64(BigInt(limits[name as keyof typeof limits]))),
    );
  }
  throw new ClientError(`binary operation encoder is not implemented for ${operation}`);
}

function catalogKindTag(kind: string): number {
  const kinds = ["database", "schema", "relation", "secondary_index", "keyspace", "structure", "search_collection", "analyzer", "cross_engine_link"];
  const index = kinds.indexOf(kind);
  if (index < 0) throw new ClientError("catalog object kind is invalid");
  return index + 1;
}

function encodeSearchCollection(args: Readonly<Record<string, unknown>>): Uint8Array {
  const request = args.request as Readonly<Record<string, unknown>>;
  const lexical = request.lexical as Readonly<Record<string, unknown>> | undefined;
  const vectors = (request.vectors ?? []) as ReadonlyArray<Readonly<Record<string, unknown>>>;
  const lexicalFlag = new Uint8Array(8);
  lexicalFlag[0] = lexical === undefined ? 0 : 1;
  const vectorCount = new Uint8Array(4);
  new DataView(vectorCount.buffer).setUint32(0, vectors.length, true);
  return join(
    u128(BigInt(args.collection as bigint | number)),
    lexicalFlag,
    ...(lexical === undefined ? [] : [bytes(new TextEncoder().encode(String(lexical.query))), u64(BigInt(lexical.candidate_limit as number)), u32(Number(lexical.weight))]),
    vectorCount,
    ...vectors.map(encodeIntegratedVector),
    encodeSearchFilter((request.filter ?? { kind: "match_all" }) as Readonly<Record<string, unknown>>),
    encodeSorts((request.sort ?? []) as ReadonlyArray<Readonly<Record<string, unknown>>>),
    encodeFacets((request.facets ?? []) as ReadonlyArray<Readonly<Record<string, unknown>>>),
    encodeAggregations((request.aggregations ?? []) as ReadonlyArray<Readonly<Record<string, unknown>>>),
    u64(BigInt(request.limit as number)),
    // Content-derived tagged sections in ascending tag order: an absent
    // section is the default and keeps the exact historical bytes.
    ...(request.fusion === undefined ? [] : request.fusion === "weighted_score" ? [Uint8Array.of(1, 1)] : request.fusion === "relative_score" ? [Uint8Array.of(1, 2)] : (() => { throw new ClientError("integrated fusion method is invalid"); })()),
    ...encodeParentDedupe(request.parent_dedupe),
    ...encodeRerank(request.rerank),
    ...encodeHighlight(request.highlight),
    ...(request.autocut === undefined || request.autocut === null ? [] : [join(Uint8Array.of(5), u32(Number(request.autocut)))]),
    ...(request.offset === undefined || request.offset === null || Number(request.offset) === 0 ? [] : [join(Uint8Array.of(6), u32(Number(request.offset)))]),
    ...encodeRangeFacets(request.range_facets),
    ...encodeDistanceCutoffs(request.vectors as ReadonlyArray<Readonly<Record<string, unknown>>>),
    ...encodeLexicalOperator(lexical),
  );
}

function encodeLexicalOperator(lexical: Readonly<Record<string, unknown>> | undefined): Uint8Array[] {
  const operator = lexical?.operator as Readonly<Record<string, unknown>> | string | undefined;
  if (operator === undefined || operator === null) {
    if (lexical?.prefix === true) return [Uint8Array.of(9, 2)];
    return [];
  }
  if (lexical?.prefix === true) throw new ClientError("lexical prefix excludes the operator");
  if (operator === "and") return [Uint8Array.of(9, 0)];
  if (typeof operator === "object" && typeof operator.minimum_match === "number") {
    return [join(Uint8Array.of(9, 1), u32(Number(operator.minimum_match)))];
  }
  throw new ClientError("lexical operator is invalid");
}

function encodeDistanceCutoffs(vectors: ReadonlyArray<Readonly<Record<string, unknown>>>): Uint8Array[] {
  const cutoffs = vectors
    .map((branch, ordinal) => [ordinal, branch.max_distance] as const)
    .filter(([, cutoff]) => cutoff !== undefined && cutoff !== null);
  if (cutoffs.length === 0) return [];
  return [join(
    Uint8Array.of(8),
    u32(cutoffs.length),
    ...cutoffs.flatMap(([ordinal, cutoff]) => [u32(ordinal), f64(Number(cutoff))]),
  )];
}

function encodeRangeFacets(raw: unknown): Uint8Array[] {
  if (raw === undefined || raw === null) return [];
  const facets = raw as ReadonlyArray<Readonly<Record<string, unknown>>>;
  if (!Array.isArray(facets) || facets.length === 0) return [];
  const bound = (value: unknown): Uint8Array => {
    if (value === undefined || value === null) return Uint8Array.of(0);
    return join(Uint8Array.of(1), f64(Number(value)));
  };
  return [join(
    Uint8Array.of(7),
    u32(facets.length),
    ...facets.flatMap((facet) => {
      const ranges = facet.ranges as ReadonlyArray<Readonly<Record<string, unknown>>>;
      if (!Array.isArray(ranges) || ranges.length === 0) throw new ClientError("range facet needs ranges");
      return [
        bytes(new TextEncoder().encode(String(facet.field))),
        u32(ranges.length),
        ...ranges.flatMap((range) => [bound(range.lower), bound(range.upper)]),
      ];
    }),
  )];
}

function encodeHighlight(value: unknown): Uint8Array[] {
  if (value === undefined || value === null) return [];
  const highlight = value as Readonly<Record<string, unknown>>;
  const maxFragments = highlight.max_fragments;
  const fragmentBytes = highlight.fragment_bytes;
  if (typeof maxFragments !== "number" || !Number.isInteger(maxFragments) || maxFragments < 1 || maxFragments > 4
    || typeof fragmentBytes !== "number" || !Number.isInteger(fragmentBytes) || fragmentBytes < 16 || fragmentBytes > 512) {
    throw new ClientError("integrated highlight budget is invalid");
  }
  return [Uint8Array.of(4), u32(maxFragments), u32(fragmentBytes)];
}

function encodeRerank(value: unknown): Uint8Array[] {
  if (value === undefined || value === null) return [];
  const rerank = value as Readonly<Record<string, unknown>>;
  const attestation = rerank.attestation;
  const scores = rerank.scores;
  if (!(attestation instanceof Uint8Array) || attestation.byteLength === 0 || attestation.byteLength > 4096 || !Array.isArray(scores) || scores.length === 0 || scores.length > 256) {
    throw new ClientError("integrated rerank stage is invalid");
  }
  const encodedScores = scores.map((entry) => {
    const scored = entry as Readonly<Record<string, unknown>>;
    if (typeof scored.object_id !== "bigint" && typeof scored.object_id !== "number") {
      throw new ClientError("integrated rerank stage is invalid");
    }
    if (typeof scored.score !== "number") {
      throw new ClientError("integrated rerank stage is invalid");
    }
    const encoded = new Uint8Array(24);
    const view = new DataView(encoded.buffer);
    const objectId = BigInt(scored.object_id as number | bigint);
    view.setBigUint64(0, objectId & 0xffffffffffffffffn, true);
    view.setBigUint64(8, objectId >> 64n, true);
    view.setFloat64(16, scored.score, true);
    return encoded;
  });
  return [Uint8Array.of(3), bytes(attestation), u32(scores.length), ...encodedScores];
}

function encodeParentDedupe(value: unknown): Uint8Array[] {
  if (value === undefined || value === null) return [];
  const dedupe = value as Readonly<Record<string, unknown>>;
  const field = dedupe.field;
  const firstK = dedupe.first_k;
  if (typeof field !== "string" || field.length === 0 || typeof firstK !== "number" || !Number.isInteger(firstK) || firstK < 1 || firstK > 100) {
    throw new ClientError("integrated parent dedupe is invalid");
  }
  return [Uint8Array.of(2), bytes(new TextEncoder().encode(field)), u32(firstK)];
}

function encodeIntegratedVector(vector: Readonly<Record<string, unknown>>): Uint8Array {
  const values = vector.query as readonly number[];
  const dimension = new Uint8Array(4);
  new DataView(dimension.buffer).setUint32(0, values.length, true);
  const encodedValues = new Uint8Array(values.length * 4);
  values.forEach((value, index) => new DataView(encodedValues.buffer).setFloat32(index * 4, value, true));
  const execution = vector.execution as Readonly<Record<string, unknown>> | undefined;
  const header = new Uint8Array(8);
  if (execution === undefined) header[0] = 0;
  else if (execution.kind === "exact") header[0] = 1;
  else if (execution.kind === "ann") header[0] = 2;
  else if (execution.kind === "adaptive") header[0] = 3;
  else throw new ClientError("integrated vector execution is invalid");
  header[1] = execution?.exact_rerank === undefined ? 0 : 1;
  const suffix = execution === undefined || execution.kind === "exact" ? [] : execution.kind === "ann" ? [
    u64(BigInt(execution.ef_search as number)), u64(BigInt((execution.exact_rerank as number | undefined) ?? 0)),
  ] : [
    u64(BigInt(execution.exact_candidate_threshold as number)), u64(BigInt(execution.ef_search as number)),
    u64(BigInt((execution.exact_rerank as number | undefined) ?? 0)),
  ];
  return join(bytes(new TextEncoder().encode(String(vector.target))), dimension, encodedValues,
    u64(BigInt(vector.candidate_limit as number)), u32(Number(vector.weight)), header, ...suffix);
}

function encodeSearchBatch(raw: unknown): Uint8Array {
  const batch = raw as Readonly<Record<string, unknown>>;
  const documents = batch.documents as ReadonlyArray<Readonly<Record<string, unknown>>>;
  return join(u128(BigInt(batch.idempotency_id as bigint | number)), u32(documents.length), ...documents.map(encodeSearchDocument));
}

function encodeSearchDocument(raw: unknown): Uint8Array {
  const document = raw as Readonly<Record<string, unknown>>;
  const values = document.doc_values as Readonly<Record<string, unknown>> ?? {};
  const vectors = document.vectors as Readonly<Record<string, readonly number[]>> ?? {};
  const valueEntries = Object.entries(values).sort(([left], [right]) => left.localeCompare(right));
  const vectorEntries = Object.entries(vectors).sort(([left], [right]) => left.localeCompare(right));
  return join(
    u128(BigInt(document.object_id as bigint | number)),
    bytes(new TextEncoder().encode(String(document.text))),
    u32(valueEntries.length),
    ...valueEntries.flatMap(([name, value]) => [bytes(new TextEncoder().encode(name)), encodeDocValue(value)]),
    u32(vectorEntries.length),
    ...vectorEntries.flatMap(([name, vector]) => {
      const encoded = new Uint8Array(vector.length * 4);
      vector.forEach((value, index) => new DataView(encoded.buffer).setFloat32(index * 4, value, true));
      return [bytes(new TextEncoder().encode(name)), u32(vector.length), encoded];
    }),
  );
}

function encodeSearchFilter(filter: Readonly<Record<string, unknown>>, depth = 0): Uint8Array {
  if (depth > 32) throw new ClientError("integrated filter is too deep");
  if (filter.kind === "match_all") return Uint8Array.of(0);
  if (filter.kind === "exists") return join(Uint8Array.of(1), bytes(new TextEncoder().encode(String(filter.field))));
  if (filter.kind === "compare") {
    const operators = ["equal", "not_equal", "less", "less_or_equal", "greater", "greater_or_equal"];
    const operator = operators.indexOf(String(filter.operator));
    if (operator < 0) throw new ClientError("integrated comparison operator is invalid");
    return join(Uint8Array.of(2), bytes(new TextEncoder().encode(String(filter.field))), Uint8Array.of(operator), encodeDocValue(filter.value));
  }
  if (filter.kind === "all" || filter.kind === "any") {
    const filters = filter.filters;
    if (!Array.isArray(filters)) throw new ClientError("integrated filter children must be an array");
    return join(Uint8Array.of(filter.kind === "all" ? 3 : 4), u32(filters.length), ...filters.map((value) => encodeSearchFilter(value as Readonly<Record<string, unknown>>, depth + 1)));
  }
  if (filter.kind === "not") return join(Uint8Array.of(5), encodeSearchFilter(filter.filter as Readonly<Record<string, unknown>>, depth + 1));
  if (filter.kind === "in") {
    const members = filter.values;
    if (!Array.isArray(members) || members.length < 1 || members.length > 256) {
      throw new ClientError("integrated membership set is invalid");
    }
    return join(Uint8Array.of(6), bytes(new TextEncoder().encode(String(filter.field))), u32(members.length), ...members.map((value) => encodeDocValue(value)));
  }
  if (filter.kind === "is_null") return join(Uint8Array.of(7), bytes(new TextEncoder().encode(String(filter.field))));
  if (filter.kind === "like") {
    return join(Uint8Array.of(8), bytes(new TextEncoder().encode(String(filter.field))), bytes(new TextEncoder().encode(String(filter.pattern))));
  }
  throw new ClientError("integrated filter kind is invalid");
}

function encodeDocValue(value: unknown): Uint8Array {
  if (typeof value === "boolean") return Uint8Array.of(0, Number(value));
  if (typeof value === "bigint") return join(Uint8Array.of(1), i64(value));
  if (typeof value === "number" && Number.isSafeInteger(value)) return join(Uint8Array.of(1), i64(BigInt(value)));
  if (typeof value === "number" && Number.isFinite(value)) return join(Uint8Array.of(4), f64(canonicalFloat(value)));
  if (typeof value === "object" && value !== null && typeof (value as { float?: unknown }).float === "number") {
    return join(Uint8Array.of(4), f64(canonicalFloat((value as { float: number }).float)));
  }
  if (typeof value === "string") return join(Uint8Array.of(2), bytes(new TextEncoder().encode(value)));
  if (value instanceof Uint8Array) return join(Uint8Array.of(3), bytes(value));
  throw new ClientError("integrated doc value is invalid");
}

/** Collapses NaN payloads and signed zero to the canonical forms. */
function canonicalFloat(value: number): number {
  if (Number.isNaN(value)) return Number.NaN;
  return value === 0 ? 0 : value;
}

function decodeDocValue(reader: Reader): unknown {
  const tag = reader.u8();
  if (tag === 0) return reader.boolean();
  if (tag === 1) return reader.i64();
  if (tag === 2) return reader.text();
  if (tag === 3) return reader.bytes();
  if (tag === 4) return reader.f64();
  throw new ClientError("integrated doc value is invalid");
}

function encodeSorts(sorts: ReadonlyArray<Readonly<Record<string, unknown>>>): Uint8Array {
  return join(u32(sorts.length), ...sorts.map((sort) => {
    const source = sort.source as Readonly<Record<string, unknown>>;
    const sourceBytes = source.kind === "score" ? Uint8Array.of(0) : source.kind === "field" ?
      join(Uint8Array.of(1), bytes(new TextEncoder().encode(String(source.field)))) : undefined;
    if (sourceBytes === undefined) throw new ClientError("integrated sort source is invalid");
    const direction = ["ascending", "descending"].indexOf(String(sort.direction));
    const missing = ["first", "last"].indexOf(String(sort.missing));
    if (direction < 0 || missing < 0) throw new ClientError("integrated sort policy is invalid");
    return join(sourceBytes, Uint8Array.of(direction, missing));
  }));
}

function encodeFacets(facets: ReadonlyArray<Readonly<Record<string, unknown>>>): Uint8Array {
  return join(u32(facets.length), ...facets.flatMap((facet) => [bytes(new TextEncoder().encode(String(facet.field))), u64(BigInt(facet.limit as number))]));
}

function encodeAggregations(aggregations: ReadonlyArray<Readonly<Record<string, unknown>>>): Uint8Array {
  return join(u32(aggregations.length), ...aggregations.map((aggregation) => {
    const kind = ["count", "sum", "min", "max", "average"].indexOf(String(aggregation.kind));
    if (kind < 0) throw new ClientError("integrated aggregation is invalid");
    return join(bytes(new TextEncoder().encode(String(aggregation.name))), Uint8Array.of(kind),
      ...(kind === 0 ? [] : [bytes(new TextEncoder().encode(String(aggregation.field)))]));
  }));
}

function decodeAggregationValue(reader: Reader): Readonly<Record<string, unknown>> {
  const tag = reader.u8();
  if (tag === 0) return { kind: "count", value: reader.u64() };
  if (tag === 1) return { kind: "integer", value: reader.boolean() ? reader.i128() : undefined };
  if (tag === 2) return { kind: "value", value: reader.boolean() ? decodeDocValue(reader) : undefined };
  if (tag === 3) return { kind: "float", value: reader.boolean() ? reader.f64() : undefined };
  throw new ClientError("integrated aggregation value is invalid");
}

function encodeStructureMutation(raw: unknown): Uint8Array {
  if (typeof raw !== "object" || raw === null) throw new ClientError("structure mutation is invalid");
  const value = raw as Readonly<Record<string, unknown>>;
  const aliases: Readonly<Record<string, readonly [string, string | undefined]>> = {
    create_hash: ["create", "hash"],
    create_set: ["create", "set"],
    create_list: ["create", "list"],
    create_sorted_set: ["create", "sorted_set"],
    create_stream: ["create", "stream"],
    list_push_tail: ["list_push", "right"],
    stream_append: ["stream_add", undefined],
  };
  const tags: Readonly<Record<string, number>> = {
    string_set: 0,
    string_delete: 1,
    counter_add: 2,
    create: 3,
    delete: 4,
    expire: 5,
    hash_set: 6,
    hash_delete: 7,
    hash_counter_add: 8,
    hash_expire_field: 9,
    list_push: 10,
    list_pop: 11,
    set_add: 12,
    set_remove: 13,
    sorted_set_add: 14,
    sorted_set_remove: 15,
    stream_add: 16,
    sorted_set_increment: 17,
    sorted_set_pop: 18,
    string_set_conditional: 19,
    string_append: 20,
    string_set_range: 21,
    hash_set_if_absent: 22,
    set_pop: 23,
  };
  const originalKind = String(value.kind);
  const [kind, implied] = aliases[originalKind] ?? [originalKind, undefined];
  const tag = tags[kind];
  if (tag === undefined) throw new ClientError("structure mutation kind is invalid");
  const parts = [Uint8Array.of(tag), encodeStructureKey(value.key)];
  if (kind === "string_set") {
    const expiry = value.expires_at_micros;
    parts.push(bytes(requireBytes(value.value)), Uint8Array.of(expiry === undefined || expiry === null ? 0 : 1));
    if (expiry !== undefined && expiry !== null) parts.push(i64(BigInt(expiry as bigint | number)));
  }
  else if (kind === "counter_add") parts.push(i64(BigInt(value.delta as bigint | number)));
  else if (kind === "create" || kind === "delete") parts.push(Uint8Array.of(structureFamilyTag(implied ?? value.family)));
  else if (kind === "expire") parts.push(Uint8Array.of(structureFamilyTag(value.family)), i64(BigInt(value.expires_at_micros as bigint | number)));
  else if (kind === "hash_set") parts.push(bytes(requireBytes(value.field)), bytes(requireBytes(value.value)));
  else if (kind === "hash_delete") parts.push(bytes(requireBytes(value.field)));
  else if (kind === "hash_counter_add") parts.push(bytes(requireBytes(value.field)), i64(BigInt(value.delta as bigint | number)));
  else if (kind === "hash_expire_field") parts.push(bytes(requireBytes(value.field)), i64(BigInt(value.expires_at_micros as bigint | number)));
  else if (kind === "list_push") parts.push(Uint8Array.of(listSideTag(implied ?? value.side)), bytes(requireBytes(value.value)));
  else if (kind === "list_pop") parts.push(Uint8Array.of(listSideTag(value.side)));
  else if (kind === "set_add" || kind === "set_remove" || kind === "sorted_set_remove") parts.push(bytes(requireBytes(value.member)));
  else if (kind === "sorted_set_add") parts.push(f64(Number(value.score)), bytes(requireBytes(value.member)));
  else if (kind === "sorted_set_increment") parts.push(f64(Number(value.delta)), bytes(requireBytes(value.member)));
  else if (kind === "sorted_set_pop") parts.push(Uint8Array.of(sortedSetEndTag(value.end ?? "lowest")));
  else if (kind === "string_set_conditional") {
    const expiry = value.expires_at_micros;
    parts.push(bytes(requireBytes(value.value)), Uint8Array.of(expiry === undefined || expiry === null ? 0 : 1));
    if (expiry !== undefined && expiry !== null) parts.push(i64(BigInt(expiry as bigint | number)));
    parts.push(Uint8Array.of(setConditionTag(value.condition ?? "if_absent")));
  }
  else if (kind === "string_append") parts.push(bytes(requireBytes(value.suffix)));
  else if (kind === "string_set_range") parts.push(u32(Number(value.offset)), bytes(requireBytes(value.patch)));
  else if (kind === "hash_set_if_absent") parts.push(bytes(requireBytes(value.field)), bytes(requireBytes(value.value)));
  else if (kind === "set_pop") parts.push(u64(BigInt(value.seed as bigint | number)));
  else if (kind === "stream_add") {
    const fields = value.fields;
    if (!Array.isArray(fields) || fields.length === 0 || fields.length > 4096) throw new ClientError("stream fields must be a nonempty bounded array");
    parts.push(u32(fields.length), ...fields.flatMap((entry) => {
      if (!Array.isArray(entry) || entry.length !== 2) throw new ClientError("stream field entry is invalid");
      return [bytes(requireBytes(entry[0])), bytes(requireBytes(entry[1]))];
    }));
  }
  return join(...parts);
}

function structureFamilyTag(raw: unknown): number {
  const families: Readonly<Record<string, number>> = { string: 1, counter: 2, hash: 3, list: 4, set: 5, sorted_set: 6, stream: 7 };
  const tag = families[String(raw)];
  if (tag === undefined) throw new ClientError("structure family is invalid");
  return tag;
}

function setConditionTag(raw: unknown): number {
  if (raw === "if_absent") return 0;
  if (raw === "if_present") return 1;
  throw new ClientError("string set condition is invalid");
}

function sortedSetEndTag(raw: unknown): number {
  if (raw === "lowest") return 0;
  if (raw === "highest") return 1;
  throw new ClientError("sorted-set pop end is invalid");
}

function listSideTag(raw: unknown): number {
  if (raw === "left") return 0;
  if (raw === "right") return 1;
  throw new ClientError("list side is invalid");
}

function encodeTransactionSearchMutation(raw: unknown): Uint8Array {
  if (typeof raw !== "object" || raw === null) throw new ClientError("transaction search mutation is invalid");
  const value = raw as Readonly<Record<string, unknown>>;
  const tags: Readonly<Record<string, number>> = { index: 0, replace: 1, delete: 2 };
  const kind = String(value.kind);
  const tag = tags[kind];
  if (tag === undefined) throw new ClientError("transaction search mutation kind is invalid");
  return join(
    Uint8Array.of(tag),
    u128(BigInt(value.index as bigint | number)),
    bytes(requireBytes(value.document_id)),
    ...(kind === "delete" ? [] : [bytes(new TextEncoder().encode(String(value.text)))]),
  );
}

function encodeTransactionVectorMutation(raw: unknown): Uint8Array {
  if (typeof raw !== "object" || raw === null) throw new ClientError("transaction vector mutation is invalid");
  const value = raw as Readonly<Record<string, unknown>>;
  const kind = String(value.kind);
  if (kind !== "upsert" && kind !== "delete") throw new ClientError("transaction vector mutation kind is invalid");
  const prefix = [
    Uint8Array.of(kind === "upsert" ? 0 : 1),
    u128(BigInt(value.index as bigint | number)),
    u128(BigInt(value.object_id as bigint | number)),
  ];
  if (kind === "delete") return join(...prefix);
  const vector = value.vector;
  if (!Array.isArray(vector) || vector.length === 0) throw new ClientError("transaction vector must be a nonempty array");
  return join(...prefix, u32(vector.length), ...vector.map((item) => f32(Number(item))));
}

function encodeStructureRead(value: Readonly<Record<string, unknown>>): Uint8Array {
  const tags: Readonly<Record<string, number>> = {
    string_get: 0, counter_get: 1, ttl: 2, hash_get: 3, hash_field_ttl: 4,
    hash_scan: 5, hash_length: 6, list_range: 7, list_length: 8, set_contains: 9,
    set_members: 10, set_cardinality: 11, set_algebra: 12, sorted_set_score: 13,
    sorted_set_rank: 14, sorted_set_range: 15, sorted_set_cardinality: 16, stream_range: 17,
    sorted_set_score_range: 18, hash_scan_reverse: 19, hash_scan_match: 20,
    key_scan_match: 21, string_range: 22, set_random_members: 23,
  };
  const kind = String(value.kind);
  const tag = tags[kind];
  if (tag === undefined) throw new ClientError("structure read kind is invalid");
  if (kind === "set_algebra") {
    const operations: Readonly<Record<string, number>> = { union: 0, intersection: 1, difference: 2 };
    const operation = operations[String(value.operation)];
    const keys = value.keys;
    if (operation === undefined) throw new ClientError("set algebra operation is invalid");
    if (!Array.isArray(keys) || keys.length === 0) throw new ClientError("set algebra keys must be a nonempty array");
    return join(Uint8Array.of(tag), u128(BigInt(value.keyspace as bigint | number)), Uint8Array.of(operation),
      u32(keys.length), ...keys.map((key) => bytes(requireBytes(key))),
      u64(BigInt(value.output_member_limit as number)), u64(BigInt(value.visit_limit as number)));
  }
  if (kind === "key_scan_match") {
    const parts = [Uint8Array.of(tag), u128(BigInt(value.keyspace as bigint | number)), bytes(requireBytes(value.pattern))];
    const cursor = value.start_after;
    parts.push(Uint8Array.of(cursor === undefined ? 0 : 1));
    if (cursor !== undefined) parts.push(bytes(requireBytes(cursor)));
    parts.push(
      u64(BigInt(value.output_limit as number)),
      u64(BigInt(value.visit_limit as number)),
      u64(BigInt(value.match_step_limit as number)),
    );
    return join(...parts);
  }
  const parts = [Uint8Array.of(tag), encodeStructureKey(value.key)];
  if (kind === "ttl") parts.push(Uint8Array.of(structureFamilyTag(value.family)));
  else if (kind === "hash_get" || kind === "hash_field_ttl") parts.push(bytes(requireBytes(value.field)));
  else if (kind === "hash_scan" || kind === "set_members") {
    const cursor = value.start_after;
    parts.push(Uint8Array.of(cursor === undefined ? 0 : 1));
    if (cursor !== undefined) parts.push(bytes(requireBytes(cursor)));
    parts.push(u64(BigInt(value.limit as number)));
  } else if (kind === "set_contains" || kind === "sorted_set_score") parts.push(bytes(requireBytes(value.member)));
  else if (kind === "sorted_set_rank") parts.push(bytes(requireBytes(value.member)), Uint8Array.of(sortedOrderTag(value.order ?? "ascending")));
  else if (kind === "list_range" || kind === "sorted_set_range") {
    parts.push(i64(BigInt(value.start as bigint | number)), i64(BigInt(value.stop as bigint | number)));
    if (kind === "sorted_set_range") parts.push(Uint8Array.of(sortedOrderTag(value.order ?? "ascending")));
  }
  else if (kind === "stream_range") parts.push(u64(BigInt(value.start as bigint | number)), u64(BigInt(value.end as bigint | number)), u64(BigInt(value.limit as number)));
  else if (kind === "string_range") parts.push(i64(BigInt(value.start as bigint | number)), i64(BigInt(value.end as bigint | number)));
  else if (kind === "set_random_members") parts.push(u64(BigInt(value.seed as bigint | number)), u64(BigInt(value.count as number)));
  else if (kind === "sorted_set_score_range") {
    parts.push(
      encodeScoreBound(value.lower),
      encodeScoreBound(value.upper),
      u64(BigInt(value.offset as number ?? 0)),
      u64(BigInt(value.limit as number)),
      Uint8Array.of(sortedOrderTag(value.order ?? "ascending")),
    );
  } else if (kind === "hash_scan_reverse") {
    const cursor = value.start_before;
    parts.push(Uint8Array.of(cursor === undefined ? 0 : 1));
    if (cursor !== undefined) parts.push(bytes(requireBytes(cursor)));
    parts.push(u64(BigInt(value.limit as number)));
  } else if (kind === "hash_scan_match") {
    parts.push(bytes(requireBytes(value.pattern)));
    const cursor = value.start_after;
    parts.push(Uint8Array.of(cursor === undefined ? 0 : 1));
    if (cursor !== undefined) parts.push(bytes(requireBytes(cursor)));
    parts.push(
      u64(BigInt(value.output_limit as number)),
      u64(BigInt(value.visit_limit as number)),
      u64(BigInt(value.match_step_limit as number)),
    );
  }
  return join(...parts);
}

/** Encodes one canonical score endpoint: unbounded, or ±inclusive f64. */
function encodeScoreBound(raw: unknown): Uint8Array {
  if (raw === undefined || raw === null) return Uint8Array.of(0);
  const bound = raw as { inclusive?: number; exclusive?: number };
  if (typeof bound.inclusive === "number") {
    const encoded = new Uint8Array(9);
    encoded[0] = 1;
    new DataView(encoded.buffer).setFloat64(1, bound.inclusive, true);
    return encoded;
  }
  if (typeof bound.exclusive === "number") {
    const encoded = new Uint8Array(9);
    encoded[0] = 2;
    new DataView(encoded.buffer).setFloat64(1, bound.exclusive, true);
    return encoded;
  }
  throw new ClientError("sorted-set score bound is invalid");
}

function sortedOrderTag(raw: unknown): number {
  if (raw === "ascending") return 0;
  if (raw === "descending") return 1;
  throw new ClientError("sorted-set order is invalid");
}

function encodeStructureKey(raw: unknown): Uint8Array {
  if (typeof raw !== "object" || raw === null) throw new ClientError("structure key is invalid");
  const value = raw as Readonly<Record<string, unknown>>;
  return join(u128(BigInt(value.keyspace as bigint | number)), bytes(requireBytes(value.key)));
}

function decodeStructureRead(reader: Reader): Readonly<Record<string, unknown>> {
  const tag = reader.u8();
  if (tag === 0) return { kind: "value", value: reader.boolean() ? reader.bytes() : undefined };
  if (tag === 1) return { kind: "values", values: Array.from({ length: readBoundedCount(reader, MAX_PRODUCT_COUNT, 4, "structure value") }, () => reader.bytes()) };
  if (tag === 2) return { kind: "counter", value: reader.boolean() ? reader.i64() : undefined };
  if (tag === 3) {
    const state = ["missing", "persistent", "remaining"][reader.u8()];
    if (state === undefined) throw new ClientError("structure TTL response is invalid");
    return { kind: "ttl", value: { state, ...(state === "remaining" ? { remainingMicros: reader.i64() } : {}) } };
  }
  if (tag === 4) return { kind: "hash_entries", entries: Array.from({ length: readBoundedCount(reader, MAX_PRODUCT_COUNT, 8, "hash entry") }, () => ({ field: reader.bytes(), value: reader.bytes() })) };
  if (tag === 5) return { kind: "count", value: reader.u64() };
  if (tag === 6) return { kind: "boolean", value: reader.boolean() };
  if (tag === 7) return { kind: "set_algebra", members: Array.from({ length: readBoundedCount(reader, MAX_PRODUCT_COUNT, 4, "set member") }, () => reader.bytes()), visited: reader.u64() };
  if (tag === 8) return { kind: "sorted_set_score", value: reader.boolean() ? reader.f64() : undefined };
  if (tag === 9) return { kind: "sorted_set_rank", value: reader.boolean() ? reader.u64() : undefined };
  if (tag === 10) return { kind: "sorted_set_entries", entries: Array.from({ length: readBoundedCount(reader, MAX_PRODUCT_COUNT, 12, "sorted-set entry") }, () => ({ member: reader.bytes(), score: reader.f64() })) };
  if (tag === 11) return { kind: "stream_entries", entries: Array.from({ length: readBoundedCount(reader, MAX_PRODUCT_COUNT, 12, "stream entry") }, () => ({ id: reader.u64(), fields: Array.from({ length: readBoundedCount(reader, MAX_PRODUCT_COUNT, 8, "stream field") }, () => [reader.bytes(), reader.bytes()]) })) };
  if (tag === 12) {
    const entries = Array.from({ length: readBoundedCount(reader, MAX_PRODUCT_COUNT, 8, "hash entry") }, () => ({ field: reader.bytes(), value: reader.bytes() }));
    const continuation = reader.boolean() ? reader.bytes() : undefined;
    const stop = ["exhausted", "output_limit", "visit_limit"][reader.u8()];
    if (stop === undefined) throw new ClientError("hash page stop reason is invalid");
    return { kind: "hash_page", entries, continuation, stop, visited: reader.u64(), matchSteps: reader.u64() };
  }
  if (tag === 13) {
    const families: readonly string[] = ["", "string", "counter", "hash", "list", "set", "sorted_set", "stream"];
    const entries = Array.from({ length: readBoundedCount(reader, MAX_PRODUCT_COUNT, 5, "key entry") }, () => {
      const key = reader.bytes();
      const family = families[reader.u8()];
      if (family === undefined || family === "") throw new ClientError("key page family is invalid");
      return { key, family };
    });
    const continuation = reader.boolean() ? reader.bytes() : undefined;
    const stop = ["exhausted", "output_limit", "visit_limit"][reader.u8()];
    if (stop === undefined) throw new ClientError("key page stop reason is invalid");
    return { kind: "key_page", entries, continuation, stop, visited: reader.u64(), matchSteps: reader.u64() };
  }
  throw new ClientError("structure read response is invalid");
}

function decodeIntegratedSearch(reader: Reader): Readonly<Record<string, unknown>> {
  const snapshot = decodeSnapshot(reader);
  const hits = Array.from({ length: readBoundedCount(reader, MAX_SEARCH_HITS, 28, "integrated search hit") }, () => {
    const objectId = reader.u128();
    const score = reader.f64();
    const docValues: Record<string, unknown> = {};
    const valueCount = reader.u32();
    if (valueCount > MAX_DOC_VALUES_PER_HIT || valueCount > Math.floor(reader.remaining / 5)) {
      throw new ClientError("integrated doc-value count exceeds its bound");
    }
    for (let index = 0; index < valueCount; index += 1) {
      const name = reader.text();
      const tag = reader.u8();
      docValues[name] = tag === 0 ? reader.boolean() : tag === 1 ? reader.i64() : tag === 2 ? reader.text() : tag === 3 ? reader.bytes() : tag === 4 ? reader.f64() : undefined;
      if (docValues[name] === undefined) throw new ClientError("integrated doc value is invalid");
    }
    return { objectId, score, docValues };
  });
  const facets = Array.from({ length: readBoundedCount(reader, MAX_SEARCH_FACETS, 8, "integrated facet") }, () => ({
    field: reader.text(),
    buckets: Array.from({ length: readBoundedCount(reader, MAX_SEARCH_FACET_BUCKETS, 10, "facet bucket") }, () => ({ value: decodeDocValue(reader), count: reader.u64() })),
  }));
  const aggregations = Array.from({ length: readBoundedCount(reader, MAX_SEARCH_AGGREGATIONS, 5, "search aggregation") }, () => ({ name: reader.text(), value: decodeAggregationValue(reader) }));
  const strategies = ["exact_filtered", "adaptive_exact_filtered", "filter_aware_ann", "adaptive_filter_aware_ann"];
  const vectorBranches = Array.from({ length: readBoundedCount(reader, MAX_SEARCH_VECTOR_BRANCHES, 36, "search vector branch") }, () => {
    const target = reader.text();
    const strategy = strategies[reader.u8()];
    if (strategy === undefined) throw new ClientError("integrated vector strategy is invalid");
    const approximate = reader.boolean();
    const exactReranked = reader.boolean();
    reader.zeroes(5);
    return { target, strategy, approximate, exactReranked, eligibleDocuments: reader.u64(), candidateCount: reader.u64(), visitedNodes: reader.u64() };
  });
  const approximate = reader.boolean();
  reader.zeroes(7);
  const totalDocuments = reader.u64();
  const eligibleDocuments = reader.u64();
  const lexicalCandidates = reader.u64();
  const retrievalCandidates = reader.u64();
  const matchedCandidates = reader.u64();
  if (reader.remaining > 0) {
    // Content-derived response tail: per-hit highlight fragments.
    if (reader.u8() !== 1) throw new ClientError("integrated response section is invalid");
    for (const hit of hits) {
      const fragmentCount = reader.u32();
      if (fragmentCount > 4) throw new ClientError("integrated highlight fragments are unbounded");
      const fragments = Array.from({ length: fragmentCount }, () => reader.text());
      if (fragments.some((fragment) => new TextEncoder().encode(fragment).byteLength > 512)) {
        throw new ClientError("integrated highlight fragments are unbounded");
      }
      (hit as Record<string, unknown>).fragments = fragments;
    }
  }
  return { snapshot, hits, facets, aggregations, vectorBranches, approximate, totalDocuments, eligibleDocuments,
    lexicalCandidates, retrievalCandidates, matchedCandidates };
}

function encodeQualifiedName(raw: unknown): Uint8Array {
  if (typeof raw !== "object" || raw === null) throw new ClientError("qualified catalog name is invalid");
  const name = raw as Readonly<Record<string, unknown>>;
  return join(...["database", "schema", "object"].flatMap((key) => {
    const component = name[key];
    if (typeof component !== "object" || component === null) throw new ClientError("qualified catalog name component is invalid");
    const value = component as Readonly<Record<string, unknown>>;
    return [bytes(new TextEncoder().encode(String(value.display))), bytes(new TextEncoder().encode(String(value.lookup)))];
  }));
}

function encodeCursor(raw: unknown): Uint8Array {
  if (raw === undefined || raw === null) return new Uint8Array(8);
  if (typeof raw !== "object") throw new ClientError("catalog cursor is invalid");
  const cursor = raw as Readonly<Record<string, unknown>>;
  const prefix = new Uint8Array(8);
  prefix[0] = 1;
  return join(prefix, encodeSnapshot(cursor.snapshot), u128(BigInt(cursor.after as bigint | number)));
}

function encodeSnapshot(raw: unknown): Uint8Array {
  if (typeof raw !== "object" || raw === null) throw new ClientError("snapshot identity is invalid");
  const snapshot = raw as Readonly<Record<string, unknown>>;
  const lineage = requireBytes(snapshot.directory_lineage);
  const root = requireBytes(snapshot.root_digest);
  if (lineage.byteLength !== 24 || root.byteLength !== 32) throw new ClientError("snapshot digest widths are invalid");
  return join(
    lineage,
    u64(BigInt((snapshot.visible_csn as bigint | number | undefined) ?? 0)),
    u64(BigInt(snapshot.catalog_version as bigint | number)),
    root,
    i64(BigInt(snapshot.logical_time_micros as bigint | number)),
  );
}

function encodeValues(raw: unknown): Uint8Array {
  if (!Array.isArray(raw) || raw.length > 4096) throw new ClientError("SQL parameters must be a bounded array");
  const count = new Uint8Array(4);
  new DataView(count.buffer).setUint32(0, raw.length, true);
  return join(count, ...raw.map((value) => encodeValue(value, 0)));
}

function encodeValue(value: unknown, depth: number): Uint8Array {
  if (depth > 8) throw new ClientError("SQL parameter nesting is too deep");
  if (value === null) return Uint8Array.of(0);
  if (typeof value === "boolean") return Uint8Array.of(1, Number(value));
  if (typeof value === "number") {
    if (!Number.isSafeInteger(value)) throw new ClientError("SQL numeric parameters must be safe integers or bigint");
    return join(Uint8Array.of(2), i64(BigInt(value)));
  }
  if (typeof value === "bigint") {
    if (value >= -(1n << 63n) && value < 1n << 63n) return join(Uint8Array.of(2), i64(value));
    if (value >= 0n && value < 1n << 64n) return join(Uint8Array.of(3), u64(value));
    throw new ClientError("SQL integer is outside the signed/unsigned 64-bit domain");
  }
  if (typeof value === "string") return join(Uint8Array.of(7), bytes(new TextEncoder().encode(value)));
  if (value instanceof Uint8Array) return join(Uint8Array.of(8), bytes(value));
  if (Array.isArray(value)) return join(Uint8Array.of(14), encodeValues(value));
  if (typeof value === "object" && value !== null) {
    const entries = Object.entries(value);
    const count = new Uint8Array(4);
    new DataView(count.buffer).setUint32(0, entries.length, true);
    return join(Uint8Array.of(15), count, ...entries.flatMap(([key, child]) => [encodeValue(key, depth + 1), encodeValue(child, depth + 1)]));
  }
  throw new ClientError("unsupported SQL parameter type");
}

function encodeQuery(raw: unknown, depth = 0): Uint8Array {
  if (depth > 8 || typeof raw !== "object" || raw === null) throw new ClientError("search query is invalid");
  const query = raw as Readonly<Record<string, unknown>>;
  if (query.kind === "term" || query.kind === "phrase" || query.kind === "prefix") {
    const tag = query.kind === "term" ? 0 : query.kind === "phrase" ? 1 : 2;
    return join(Uint8Array.of(tag), bytes(new TextEncoder().encode(String(query.value))));
  }
  if (query.kind === "fuzzy") return join(Uint8Array.of(3, Number(query.max_distance)), bytes(new TextEncoder().encode(String(query.term))));
  if (query.kind === "boolean") {
    const groups = [query.must ?? [], query.should ?? [], query.must_not ?? []];
    if (!groups.every(Array.isArray)) throw new ClientError("boolean search clauses must be arrays");
    const counts = new Uint8Array(12);
    const view = new DataView(counts.buffer);
    groups.forEach((group, index) => view.setUint32(index * 4, (group as unknown[]).length, true));
    return join(Uint8Array.of(4), counts, ...groups.flatMap((group) => (group as unknown[]).map((child) => encodeQuery(child, depth + 1))));
  }
  throw new ClientError("unsupported search query kind");
}

function decodeOperation(operation: string, encoded: Uint8Array): Readonly<Record<string, unknown>> {
  if (["capabilities", "admin_status", "admin_checkpoint", "telemetry", "transaction_begin", "security_status", "security_legacy_bearer_revoke"].includes(operation)) {
    if (encoded.byteLength !== 0) throw new ClientError("parameterless request has trailing bytes");
    return {};
  }
  if (operation === "structure_get" || operation === "structure_ttl") {
    const [key, offset] = takeBytes(encoded, 0);
    if (offset !== encoded.byteLength) throw new ClientError("structure request has trailing bytes");
    return { key };
  }
  const reader = new Reader(encoded);
  let args: Readonly<Record<string, unknown>>;
  if (operation === "catalog_create") args = { definition: reader.bytes() };
  else if (operation === "catalog_visible_list") {
    const hasParent = reader.boolean();
    const kind = reader.u8();
    reader.zeroes(6);
    args = {
      parent: hasParent ? reader.u128() : undefined,
      kind: kind === 0 ? undefined : kind,
      cursor: reader.bytes(),
      item_limit: reader.u64(),
      visit_limit: reader.u64(),
      byte_limit: reader.u64(),
    };
  }
  else if (operation === "transaction_stage_sql") {
    const handle = reader.u64();
    const statement = reader.text();
    const parameters = Array.from({ length: readBoundedCount(reader, MAX_PRODUCT_COUNT, 1, "SQL parameter") }, () => decodeValue(reader, 0));
    args = { handle, statement, parameters };
  }
  else if (operation === "transaction_stage_structure") args = { handle: reader.u64(), mutation: decodeStructureMutation(reader) };
  else if (operation === "transaction_stage_search") args = { handle: reader.u64(), mutation: decodeTransactionSearchMutation(reader) };
  else if (operation === "transaction_stage_vector") args = { handle: reader.u64(), mutation: decodeTransactionVectorMutation(reader) };
  else if (["transaction_commit", "transaction_rollback", "explicit_transaction_status"].includes(operation)) args = { handle: reader.u64() };
  else if (operation === "transaction_status_by_idempotency") args = { idempotency_token: reader.u128() };
  else if (["security_principal_list", "security_role_list", "security_assignment_list", "security_key_list"].includes(operation)) {
    const family = operation.slice("security_".length, -"_list".length);
    args = { cursor: decodeSecurityCursor(reader, family), limit: decodeSecurityLimit(reader) };
  }
  else if (operation === "security_audit_read") args = { cursor: decodeOptionalSecurityId(reader), limit: decodeSecurityLimit(reader) };
  else if (operation === "security_principal_create") args = { display_name: decodeSecurityText(reader) };
  else if (operation === "security_principal_set_enabled") {
    args = { principal_id: decodeSecurityId(reader), enabled: reader.boolean() };
    reader.zeroes(7);
  }
  else if (operation === "security_custom_role_create") args = { display_name: decodeSecurityText(reader), grants: decodeSecurityGrants(reader) };
  else if (operation === "security_built_in_assignment_create") {
    const principal_id = decodeSecurityId(reader);
    const role = decodeBuiltInRole(reader);
    reader.zeroes(7);
    args = { principal_id, role, scope: decodeProductScope(reader) };
  }
  else if (operation === "security_custom_assignment_create") args = { principal_id: decodeSecurityId(reader), role_id: decodeSecurityId(reader) };
  else if (operation === "security_assignment_revoke") args = { assignment_id: decodeSecurityId(reader) };
  else if (operation === "security_api_key_issue_self_start" || operation === "security_api_key_issue_start") {
    const principal_id = decodeSecurityId(reader);
    const label = decodeSecurityText(reader);
    const roleCount = reader.u32();
    reader.zeroes(4);
    if (roleCount > BUILT_IN_ROLES.length) throw new ClientError("API key role count is invalid");
    const roles = Array.from({ length: roleCount }, () => decodeBuiltInRole(reader));
    const customCount = reader.u32();
    reader.zeroes(4);
    if (customCount > MAX_SECURITY_ASSIGNMENTS) throw new ClientError("API key custom role count is invalid");
    const custom_roles = Array.from({ length: customCount }, () => decodeSecurityId(reader));
    const bits = reader.u64();
    if (bits >> BigInt(PRODUCT_PERMISSIONS.length) !== 0n) throw new ClientError("API key permission ceiling is invalid");
    const scopeCount = reader.u32();
    reader.zeroes(4);
    if (scopeCount === 0 || scopeCount > MAX_SECURITY_ASSIGNMENTS) throw new ClientError("API key scope ceiling is invalid");
    args = {
      principal_id,
      label,
      roles,
      custom_roles,
      permission_ceiling: PRODUCT_PERMISSIONS.filter((_, index) => (bits & 1n << BigInt(index)) !== 0n),
      scope_ceiling: Array.from({ length: scopeCount }, () => decodeProductScope(reader)),
      expires_at_micros: decodeFixedOptionalI64(reader),
    };
  }
  else if (operation === "security_api_key_issue_self_activate" || operation === "security_api_key_issue_activate") {
    args = { key_id: reader.take(16), confirmation_digest: reader.take(32) };
  }
  else if (operation === "security_api_key_rotate_self_activate" || operation === "security_api_key_rotate_activate") {
    args = { successor_key_id: reader.take(16), confirmation_digest: reader.take(32) };
  }
  else if (operation === "security_api_key_rotate_self_start" || operation === "security_api_key_rotate_start") {
    args = {
      predecessor_key_id: checkedNonzeroBytes(reader.take(16), "API key identity"),
      label: decodeSecurityText(reader),
      overlap_seconds: reader.u64(),
      expires_at_micros: decodeFixedOptionalI64(reader),
    };
  }
  else if (["security_api_key_issue_self_abort", "security_api_key_issue_abort", "security_api_key_revoke_self", "security_api_key_revoke"].includes(operation)) {
    args = { key_id: reader.take(16) };
  }
  else if (operation === "security_api_key_rotate_self_abort" || operation === "security_api_key_rotate_abort") {
    args = { successor_key_id: reader.take(16) };
  }
  else if (operation === "structure_read") args = decodeStructureReadRequest(reader);
  else throw new ClientError(`binary operation decoder is not implemented for ${operation}`);
  reader.finish();
  return args;
}

function decodeStructureKey(reader: Reader): Readonly<Record<string, unknown>> {
  return { keyspace: reader.u128(), key: reader.bytes() };
}

function decodeStructureReadRequest(reader: Reader): Readonly<Record<string, unknown>> {
  const kinds = ["string_get", "counter_get", "ttl", "hash_get", "hash_field_ttl", "hash_scan", "hash_length", "list_range", "list_length", "set_contains", "set_members", "set_cardinality", "set_algebra", "sorted_set_score", "sorted_set_rank", "sorted_set_range", "sorted_set_cardinality", "stream_range"];
  const kind = kinds[reader.u8()];
  if (kind === undefined) throw new ClientError("structure read kind is invalid");
  if (kind === "set_algebra") {
    const keyspace = reader.u128();
    const operation = ["union", "intersection", "difference"][reader.u8()];
    if (operation === undefined) throw new ClientError("set algebra operation is invalid");
    return { kind, keyspace, operation, keys: Array.from({ length: readBoundedCount(reader, MAX_PRODUCT_COUNT, 4, "set algebra key") }, () => reader.bytes()), output_member_limit: reader.u64(), visit_limit: reader.u64() };
  }
  const result: Record<string, unknown> = { kind, key: decodeStructureKey(reader) };
  if (kind === "ttl") {
    const family = [undefined, "string", "counter", "hash", "list", "set", "sorted_set", "stream"][reader.u8()];
    if (family === undefined) throw new ClientError("structure family is invalid");
    result.family = family;
  }
  else if (kind === "hash_get" || kind === "hash_field_ttl") result.field = reader.bytes();
  else if (kind === "hash_scan" || kind === "set_members") {
    result.start_after = reader.boolean() ? reader.bytes() : undefined;
    result.limit = reader.u64();
  }
  else if (kind === "set_contains" || kind === "sorted_set_score") result.member = reader.bytes();
  else if (kind === "sorted_set_rank") {
    result.member = reader.bytes();
    const order = ["ascending", "descending"][reader.u8()];
    if (order === undefined) throw new ClientError("sorted-set order is invalid");
    result.order = order;
  }
  else if (kind === "list_range" || kind === "sorted_set_range") {
    result.start = reader.i64();
    result.stop = reader.i64();
    if (kind === "sorted_set_range") {
      const order = ["ascending", "descending"][reader.u8()];
      if (order === undefined) throw new ClientError("sorted-set order is invalid");
      result.order = order;
    }
  }
  else if (kind === "stream_range") {
    result.start = reader.u64();
    result.end = reader.u64();
    result.limit = reader.u64();
  }
  return result;
}

function decodeStructureMutation(reader: Reader): Readonly<Record<string, unknown>> {
  const kinds = ["string_set", "string_delete", "counter_add", "create", "delete", "expire", "hash_set", "hash_delete", "hash_counter_add", "hash_expire_field", "list_push", "list_pop", "set_add", "set_remove", "sorted_set_add", "sorted_set_remove", "stream_add"];
  const kind = kinds[reader.u8()];
  if (kind === undefined) throw new ClientError("structure mutation kind is invalid");
  const result: Record<string, unknown> = { kind, key: decodeStructureKey(reader) };
  const families = [undefined, "string", "counter", "hash", "list", "set", "sorted_set", "stream"];
  if (kind === "string_set") {
    result.value = reader.bytes();
    result.expires_at_micros = reader.boolean() ? reader.i64() : null;
  }
  else if (kind === "counter_add") result.delta = reader.i64();
  else if (kind === "create" || kind === "delete") {
    const family = families[reader.u8()];
    if (family === undefined) throw new ClientError("structure family is invalid");
    result.family = family;
  }
  else if (kind === "expire") {
    const family = families[reader.u8()];
    if (family === undefined) throw new ClientError("structure family is invalid");
    result.family = family;
    result.expires_at_micros = reader.i64();
  }
  else if (["hash_set", "hash_delete", "hash_counter_add", "hash_expire_field"].includes(kind)) {
    result.field = reader.bytes();
    if (kind === "hash_set") result.value = reader.bytes();
    else if (kind === "hash_counter_add") result.delta = reader.i64();
    else if (kind === "hash_expire_field") result.expires_at_micros = reader.i64();
  }
  else if (kind === "list_push" || kind === "list_pop") {
    const side = ["left", "right"][reader.u8()];
    if (side === undefined) throw new ClientError("list side is invalid");
    result.side = side;
    if (kind === "list_push") result.value = reader.bytes();
  }
  else if (kind === "set_add" || kind === "set_remove" || kind === "sorted_set_remove") result.member = reader.bytes();
  else if (kind === "sorted_set_add") {
    result.score = reader.f64();
    result.member = reader.bytes();
  }
  else if (kind === "stream_add") result.fields = Array.from({ length: readBoundedCount(reader, MAX_PRODUCT_COUNT, 8, "stream field") }, () => [reader.bytes(), reader.bytes()]);
  return result;
}

function decodeTransactionSearchMutation(reader: Reader): Readonly<Record<string, unknown>> {
  const kind = ["index", "replace", "delete"][reader.u8()];
  if (kind === undefined) throw new ClientError("transaction search mutation kind is invalid");
  return { kind, index: reader.u128(), document_id: reader.bytes(), ...(kind === "delete" ? {} : { text: reader.text() }) };
}

function decodeTransactionVectorMutation(reader: Reader): Readonly<Record<string, unknown>> {
  const kind = ["upsert", "delete"][reader.u8()];
  if (kind === undefined) throw new ClientError("transaction vector mutation kind is invalid");
  return { kind, index: reader.u128(), object_id: reader.u128(), ...(kind === "delete" ? {} : { vector: Array.from({ length: readBoundedCount(reader, MAX_PRODUCT_COUNT, 4, "vector dimension") }, () => reader.f32()) }) };
}

function envelope(encoded: Uint8Array, expectedMagic: string): readonly [number, Uint8Array] {
  if (encoded.byteLength < 16) throw new ClientError("product envelope is truncated");
  const view = new DataView(encoded.buffer, encoded.byteOffset, encoded.byteLength);
  if (new TextDecoder().decode(encoded.subarray(0, 8)) !== expectedMagic ||
      view.getUint32(8, true) !== encoded.byteLength || view.getUint16(14, true) !== 0) {
    throw new ClientError("product envelope is malformed");
  }
  return [view.getUint16(12, true), encoded.subarray(16)];
}

class Reader {
  readonly #encoded: Uint8Array;
  #offset = 0;

  constructor(encoded: Uint8Array) {
    this.#encoded = encoded;
  }

  get remaining(): number {
    return this.#encoded.byteLength - this.#offset;
  }

  take(length: number): Uint8Array {
    if (length < 0 || this.#offset + length > this.#encoded.byteLength) throw new ClientError("product response is truncated");
    const value = this.#encoded.slice(this.#offset, this.#offset + length);
    this.#offset += length;
    return value;
  }

  zeroes(length: number): void {
    if (this.take(length).some((value) => value !== 0)) throw new ClientError("product response reserved bytes are nonzero");
  }

  boolean(): boolean {
    const value = this.u8();
    if (value > 1) throw new ClientError("product response boolean is invalid");
    return value === 1;
  }

  bytes(): Uint8Array {
    const length = this.u32();
    if (length > MAX_PAYLOAD) throw new ClientError("product response bytes exceed the protocol maximum");
    return this.take(length);
  }

  text(): string {
    return new TextDecoder("utf-8", { fatal: true }).decode(this.bytes());
  }

  u8(): number { return this.take(1)[0] ?? 0; }
  u16(): number { return this.view(2).getUint16(0, true); }
  u32(): number { return this.view(4).getUint32(0, true); }
  i32(): number { return this.view(4).getInt32(0, true); }
  u64(): bigint { return this.view(8).getBigUint64(0, true); }
  i64(): bigint { return this.view(8).getBigInt64(0, true); }
  i128(): bigint {
    const view = this.view(16);
    const value = view.getBigUint64(0, true) | (view.getBigUint64(8, true) << 64n);
    return value >= 1n << 127n ? value - (1n << 128n) : value;
  }
  u128(): bigint {
    const view = this.view(16);
    return view.getBigUint64(0, true) | (view.getBigUint64(8, true) << 64n);
  }
  f32(): number { return this.view(4).getFloat32(0, true); }
  f64(): number { return this.view(8).getFloat64(0, true); }

  finish(): void {
    if (this.#offset !== this.#encoded.byteLength) throw new ClientError("product response has trailing bytes");
  }

  view(length: number): DataView {
    const value = this.take(length);
    return new DataView(value.buffer, value.byteOffset, value.byteLength);
  }
}

function bytes(value: Uint8Array): Uint8Array {
  const encoded = new Uint8Array(4 + value.byteLength);
  new DataView(encoded.buffer).setUint32(0, value.byteLength, true);
  encoded.set(value, 4);
  return encoded;
}

function checkedBytes(value: unknown, length: number, name: string): Uint8Array {
  if (!(value instanceof Uint8Array) || value.byteLength !== length || value.every((byte) => byte === 0)) {
    throw new ClientError(`${name} is invalid`);
  }
  return value;
}

function apiKeyId(value: unknown): Uint8Array { return checkedBytes(value, 16, "API key identity"); }
function confirmationDigest(value: unknown): Uint8Array { return checkedBytes(value, 32, "API key confirmation digest"); }

function securityId(value: unknown): Uint8Array {
  const id = BigInt(value as bigint | number);
  if (id <= 0n || id >= 1n << 128n) throw new ClientError("security identity is invalid");
  const encoded = u128(id);
  encoded.reverse();
  return encoded;
}

function securityText(value: unknown): Uint8Array {
  if (typeof value !== "string") throw new ClientError("security text is invalid");
  const encoded = new TextEncoder().encode(value);
  if (encoded.byteLength === 0 || encoded.byteLength > 128 || /[\u0000-\u001f\u007f]/u.test(value)) throw new ClientError("security text is invalid");
  return bytes(encoded);
}

function builtInRoles(value: unknown): Uint8Array {
  if (!Array.isArray(value) || value.length > 7) throw new ClientError("API key roles are invalid");
  const roles = ["owner", "admin", "operator", "developer", "writer", "reader", "auditor"];
  return join(u32(value.length), new Uint8Array(4), new Uint8Array(value.map((role) => {
    const tag = roles.indexOf(String(role));
    if (tag < 0) throw new ClientError("API key role is invalid");
    return tag;
  })));
}

function securityIds(value: unknown): Uint8Array {
  if (!Array.isArray(value) || value.length > 128) throw new ClientError("API key custom roles are invalid");
  return join(u32(value.length), new Uint8Array(4), ...value.map(securityId));
}

function permissionBits(value: unknown): bigint {
  if (!Array.isArray(value) || value.length === 0) throw new ClientError("API key permission ceiling is invalid");
  const permissions = ["audit.read", "backup.create", "backup.verify", "catalog.read", "catalog.write", "credential.self_manage", "data.read", "data.write", "discover", "maintain", "observe", "ownership.manage", "proof.generate", "proof.verify", "restore", "search.execute", "security.manage", "security.read"];
  return [...new Set(value.map(String))].reduce((bits, permission) => {
    const tag = permissions.indexOf(permission);
    if (tag < 0) throw new ClientError("API key permission ceiling is invalid");
    return bits | 1n << BigInt(tag);
  }, 0n);
}

function productScopes(value: unknown): Uint8Array {
  if (!Array.isArray(value) || value.length === 0 || value.length > 128) throw new ClientError("API key scope ceiling is invalid");
  return join(u32(value.length), new Uint8Array(4), ...value.map((scope) => {
    if (typeof scope !== "object" || scope === null) throw new ClientError("API key scope is invalid");
    const record = scope as Record<string, unknown>;
    const encoded = new Uint8Array(24);
    if (record.kind === "instance") return encoded;
    encoded[0] = record.kind === "catalog_subtree" ? 1 : record.kind === "catalog_object" ? 2 : 255;
    if (encoded[0] === 255) throw new ClientError("API key scope is invalid");
    encoded.set(u128(BigInt(record.object_id as bigint | number)), 8);
    return encoded;
  }));
}

function optionalI64(value: unknown): Uint8Array {
  const encoded = new Uint8Array(16);
  if (value !== undefined && value !== null) {
    encoded[0] = 1;
    new DataView(encoded.buffer).setBigInt64(8, BigInt(value as bigint | number), true);
  }
  return encoded;
}

function securityLimit(value: unknown): Uint8Array {
  const limit = Number(value ?? MAX_SECURITY_LIST_ROWS);
  if (!Number.isSafeInteger(limit) || limit <= 0 || limit > MAX_SECURITY_LIST_ROWS) throw new ClientError("security list limit is invalid");
  return u64(BigInt(limit));
}

function builtInRoleTag(value: unknown): number {
  const tag = BUILT_IN_ROLES.indexOf(String(value) as typeof BUILT_IN_ROLES[number]);
  if (tag < 0) throw new ClientError("built-in security role is invalid");
  return tag;
}

function encodeProductScope(value: unknown): Uint8Array {
  if (typeof value !== "object" || value === null) throw new ClientError("security scope is invalid");
  const scope = value as Readonly<Record<string, unknown>>;
  const encoded = new Uint8Array(24);
  if (scope.kind === "instance") return encoded;
  encoded[0] = scope.kind === "catalog_subtree" ? 1 : scope.kind === "catalog_object" ? 2 : 255;
  if (encoded[0] === 255) throw new ClientError("security scope kind is invalid");
  encoded.set(u128(BigInt(scope.object_id as bigint | number)), 8);
  return encoded;
}

function encodeSecurityGrants(value: unknown): Uint8Array {
  if (!Array.isArray(value) || value.length === 0 || value.length > MAX_SECURITY_GRANTS) throw new ClientError("custom-role grants are invalid");
  return join(u32(value.length), new Uint8Array(4), ...value.map((raw) => {
    if (typeof raw !== "object" || raw === null) throw new ClientError("custom-role grant is invalid");
    const grant = raw as Readonly<Record<string, unknown>>;
    const permission = PRODUCT_PERMISSIONS.indexOf(String(grant.permission) as typeof PRODUCT_PERMISSIONS[number]);
    if (permission < 0) throw new ClientError("custom-role permission is invalid");
    return join(Uint8Array.of(permission), new Uint8Array(7), encodeProductScope(grant.scope));
  }));
}

function encodeSecurityCursor(value: unknown, family: string): Uint8Array {
  if (value === undefined || value === null) return new Uint8Array(40);
  if (typeof value !== "object") throw new ClientError("security cursor is invalid");
  const cursor = value as Readonly<Record<string, unknown>>;
  const epoch = BigInt(cursor.authorization_epoch as bigint | number);
  if (epoch <= 0n || epoch >= 1n << 64n) throw new ClientError("security cursor authorization epoch is invalid");
  const tags: Readonly<Record<string, Readonly<Record<string, number>>>> = {
    principal: { principal: 1 }, role: { built_in_role: 2, custom_role: 3 }, assignment: { assignment: 4 }, key: { key: 5 },
  };
  const tag = tags[family]?.[String(cursor.kind)];
  if (tag === undefined) throw new ClientError("security cursor family is invalid");
  const payload = cursor.kind === "built_in_role"
    ? join(Uint8Array.of(builtInRoleTag(cursor.after)), new Uint8Array(15))
    : cursor.kind === "key" ? apiKeyId(cursor.after) : securityId(cursor.after);
  return join(Uint8Array.of(1), new Uint8Array(7), u64(epoch), Uint8Array.of(tag), new Uint8Array(7), payload);
}

function optionalSecurityId(value: unknown): Uint8Array {
  return value === undefined || value === null
    ? new Uint8Array(24)
    : join(Uint8Array.of(1), new Uint8Array(7), securityId(value));
}

function decodeSecurityLimit(reader: Reader): bigint {
  const value = reader.u64();
  if (value === 0n || value > BigInt(MAX_SECURITY_LIST_ROWS)) throw new ClientError("security list limit is invalid");
  return value;
}

function decodeSecurityId(reader: Reader): bigint {
  return securityIdFromBytes(reader.take(16));
}

function securityIdFromBytes(value: Uint8Array): bigint {
  const reversed = value.slice().reverse();
  const id = readU128(reversed, 0);
  if (id === 0n) throw new ClientError("security identity is zero");
  return id;
}

function checkedNonzeroBytes(value: Uint8Array, name: string): Uint8Array {
  if (value.every((byte) => byte === 0)) throw new ClientError(`${name} is zero`);
  return value;
}

function decodeApiKeySecret(reader: Reader, expectedId: Uint8Array): SensitiveBytes {
  const secret = reader.bytes();
  let text: string;
  try {
    text = new TextDecoder("utf-8", { fatal: true }).decode(secret);
  } catch (cause) {
    secret.fill(0);
    throw new ClientError("API key secret is noncanonical", { cause });
  }
  if (secret.byteLength !== API_KEY_BYTES || !/^hyp1_[0-9a-f]{32}_[0-9a-f]{64}$/u.test(text)) {
    secret.fill(0);
    throw new ClientError("API key secret is noncanonical");
  }
  const encodedId = hexBytes(text.slice(5, 37));
  if (!equalBytes(encodedId, expectedId) || encodedId.every((byte) => byte === 0)) {
    secret.fill(0);
    throw new ClientError("API key secret identity differs from its receipt");
  }
  const wrapped = new SensitiveBytes(secret);
  secret.fill(0);
  return wrapped;
}

function hexBytes(value: string): Uint8Array {
  const encoded = new Uint8Array(value.length / 2);
  for (let index = 0; index < encoded.length; index += 1) {
    encoded[index] = Number.parseInt(value.slice(index * 2, index * 2 + 2), 16);
  }
  return encoded;
}

function equalBytes(left: Uint8Array, right: Uint8Array): boolean {
  return left.byteLength === right.byteLength && left.every((value, index) => value === right[index]);
}

function readBoundedCount(reader: Reader, maximum: number, minimumBytes: number, name: string): number {
  return boundedCount(reader.u32(), maximum, reader, minimumBytes, name);
}

function boundedCount(count: number, maximum: number, reader: Reader, minimumBytes: number, name: string): number {
  if (count > maximum || count > Math.floor(reader.remaining / minimumBytes)) {
    throw new ClientError(`${name} count exceeds its bound`);
  }
  return count;
}

function decodeSecurityText(reader: Reader): string {
  const value = reader.text();
  if (value.length === 0 || new TextEncoder().encode(value).byteLength > 128 || /[\u0000-\u001f\u007f]/u.test(value)) throw new ClientError("security text is invalid");
  return value;
}

function decodeBuiltInRole(reader: Reader): typeof BUILT_IN_ROLES[number] {
  const role = BUILT_IN_ROLES[reader.u8()];
  if (role === undefined) throw new ClientError("built-in security role is invalid");
  return role;
}

function decodeProductScope(reader: Reader): Readonly<Record<string, unknown>> {
  const kind = reader.u8();
  reader.zeroes(7);
  const objectId = reader.u128();
  if (kind === 0) {
    if (objectId !== 0n) throw new ClientError("instance scope has a nonzero identity");
    return { kind: "instance" };
  }
  if ((kind !== 1 && kind !== 2) || objectId === 0n) throw new ClientError("security object scope is invalid");
  return { kind: kind === 1 ? "catalog_subtree" : "catalog_object", object_id: objectId };
}

function decodeSecurityGrants(reader: Reader): ReadonlyArray<Readonly<Record<string, unknown>>> {
  const count = reader.u32();
  reader.zeroes(4);
  if (count === 0 || count > MAX_SECURITY_GRANTS) throw new ClientError("custom-role grant count is invalid");
  return Array.from({ length: count }, () => {
    const permission = PRODUCT_PERMISSIONS[reader.u8()];
    reader.zeroes(7);
    if (permission === undefined) throw new ClientError("custom-role permission is invalid");
    return { permission, scope: decodeProductScope(reader) };
  });
}

function decodeSecurityCursor(reader: Reader, family: string): Readonly<Record<string, unknown>> | undefined {
  const present = reader.boolean();
  reader.zeroes(7);
  const authorization_epoch = reader.u64();
  const tag = reader.u8();
  reader.zeroes(7);
  const payload = reader.take(16);
  if (!present) {
    if (authorization_epoch !== 0n || tag !== 0 || payload.some((byte) => byte !== 0)) throw new ClientError("absent security cursor is noncanonical");
    return undefined;
  }
  const expected: Readonly<Record<string, Readonly<Record<number, string>>>> = {
    principal: { 1: "principal" }, role: { 2: "built_in_role", 3: "custom_role" }, assignment: { 4: "assignment" }, key: { 5: "key" },
  };
  const kind = expected[family]?.[tag];
  if (authorization_epoch === 0n || kind === undefined) throw new ClientError("security cursor is invalid");
  const after = kind === "built_in_role" ? BUILT_IN_ROLES[payload[0] ?? 255]
    : kind === "key" ? checkedNonzeroBytes(payload, "security key cursor") : securityIdFromBytes(payload);
  if (after === undefined) throw new ClientError("security role cursor is invalid");
  return { authorization_epoch, kind, after };
}

function decodeOptionalSecurityId(reader: Reader): bigint | undefined {
  const present = reader.boolean();
  reader.zeroes(7);
  const payload = reader.take(16);
  if (!present) {
    if (payload.some((byte) => byte !== 0)) throw new ClientError("absent security identity is noncanonical");
    return undefined;
  }
  return securityIdFromBytes(payload);
}

function takeBytes(encoded: Uint8Array, offset: number): readonly [Uint8Array, number] {
  if (offset + 4 > encoded.byteLength) throw new ClientError("length-prefixed bytes are truncated");
  const length = new DataView(encoded.buffer, encoded.byteOffset + offset).getUint32(0, true);
  const start = offset + 4;
  if (length > MAX_PAYLOAD || start + length > encoded.byteLength) throw new ClientError("length-prefixed bytes are invalid");
  return [encoded.slice(start, start + length), start + length];
}

function takeText(encoded: Uint8Array, offset: number, length: number): readonly [string, number] {
  if (offset + length > encoded.byteLength) throw new ClientError("text is truncated");
  return [new TextDecoder("utf-8", { fatal: true }).decode(encoded.subarray(offset, offset + length)), offset + length];
}

function readU128(encoded: Uint8Array, offset: number): bigint {
  const view = new DataView(encoded.buffer, encoded.byteOffset + offset, 16);
  return view.getBigUint64(0, true) | (view.getBigUint64(8, true) << 64n);
}

function u128(value: bigint): Uint8Array {
  const encoded = new Uint8Array(16);
  const view = new DataView(encoded.buffer);
  view.setBigUint64(0, value & ((1n << 64n) - 1n), true);
  view.setBigUint64(8, value >> 64n, true);
  return encoded;
}

function i64(value: bigint): Uint8Array {
  const encoded = new Uint8Array(8);
  new DataView(encoded.buffer).setBigInt64(0, value, true);
  return encoded;
}

function u64(value: bigint): Uint8Array {
  const encoded = new Uint8Array(8);
  new DataView(encoded.buffer).setBigUint64(0, value, true);
  return encoded;
}

function f64(value: number): Uint8Array {
  const encoded = new Uint8Array(8);
  new DataView(encoded.buffer).setFloat64(0, value, true);
  return encoded;
}

function f32(value: number): Uint8Array {
  const encoded = new Uint8Array(4);
  new DataView(encoded.buffer).setFloat32(0, value, true);
  return encoded;
}

function u32(value: number): Uint8Array {
  const encoded = new Uint8Array(4);
  new DataView(encoded.buffer).setUint32(0, value, true);
  return encoded;
}

function u16(value: number): Uint8Array {
  const encoded = new Uint8Array(2);
  new DataView(encoded.buffer).setUint16(0, value, true);
  return encoded;
}

function requireBytes(value: unknown): Uint8Array {
  if (!(value instanceof Uint8Array)) throw new ClientError("operation requires Uint8Array bytes");
  return value;
}

function join(...values: ReadonlyArray<Uint8Array>): Uint8Array {
  const output = new Uint8Array(values.reduce((total, value) => total + value.byteLength, 0));
  let offset = 0;
  for (const value of values) {
    output.set(value, offset);
    offset += value.byteLength;
  }
  return output;
}
