// SPDX-License-Identifier: Apache-2.0

import { HttpTransport, type HttpTransportOptions } from "./http.js";
import { LocalTransport, type LocalConnector, type LocalTransportOptions } from "./local.js";
import type { RequestOptions, Response, Transport } from "./models.js";
import { nodeLocalConnector } from "./node-local.js";

/** Equivalent high-level Native v2 API over local and HTTP transports. */
export class HyphaeClient {
  readonly #transport: Transport;

  constructor(transport: Transport) {
    this.#transport = transport;
  }

  static http(baseUrl: string, options: HttpTransportOptions = {}): HyphaeClient {
    return new HyphaeClient(new HttpTransport(baseUrl, options));
  }

  static local(endpoint: string, connector: LocalConnector = nodeLocalConnector, clientIdentity = "hyphae-typescript-sdk-v2"): HyphaeClient {
    return new HyphaeClient(new LocalTransport(endpoint, connector, clientIdentity));
  }

  static localWithOptions(endpoint: string, options: LocalTransportOptions = {}, connector: LocalConnector = nodeLocalConnector): HyphaeClient {
    return new HyphaeClient(new LocalTransport(endpoint, connector, options));
  }

  static localAuthenticated(endpoint: string, apiKey: string, connector: LocalConnector = nodeLocalConnector, clientIdentity = "hyphae-typescript-sdk-v2"): HyphaeClient {
    return HyphaeClient.localWithOptions(endpoint, { clientIdentity, apiKey }, connector);
  }

  execute(operation: string, args: Readonly<Record<string, unknown>> = {}, options: RequestOptions = {}): Promise<Response> {
    return this.#transport.execute(operation, args, options);
  }

  async close(): Promise<void> {
    await this.#transport.close?.();
  }

  capabilities(options: RequestOptions = {}): Promise<Response> {
    return this.execute("capabilities", {}, options);
  }

  catalog(action: string, args: Readonly<Record<string, unknown>>, options: RequestOptions = {}): Promise<Response> {
    return this.execute(`catalog_${action}`, args, options);
  }

  catalogObject(objectId: bigint, options: RequestOptions = {}): Promise<Response> {
    return this.execute("catalog_object", { id: objectId }, options);
  }

  catalogList(request: Readonly<Record<string, unknown>>, options: RequestOptions = {}): Promise<Response> {
    return this.execute("catalog_list", request, options);
  }

  /** Lists current visible catalog objects; cursor values are opaque Uint8Array tokens. */
  catalogVisibleList(request: Readonly<Record<string, unknown>>, options: RequestOptions = {}): Promise<Response> {
    return this.execute("catalog_visible_list", request, options);
  }

  sql(statement: string, parameters: readonly unknown[] = [], options: RequestOptions = {}): Promise<Response> {
    return this.execute("sql_execute", { statement, parameters }, options);
  }

  prepareSql(statement: string, options: RequestOptions = {}): Promise<Response> {
    return this.execute("sql_prepare", { statement }, options);
  }

  executePrepared(handle: bigint, parameters: readonly unknown[] = [], options: RequestOptions = {}): Promise<Response> {
    return this.execute("sql_execute_prepared", { handle, parameters }, options);
  }

  deallocatePrepared(handle: bigint, options: RequestOptions = {}): Promise<Response> {
    return this.execute("sql_deallocate", { handle }, options);
  }

  structureGet(key: Uint8Array, options: RequestOptions = {}): Promise<Response> {
    return this.execute("structure_get", { key }, options);
  }

  structureSet(
    key: Uint8Array,
    value: Uint8Array,
    expiresAtMicros?: bigint,
    options: RequestOptions = {},
  ): Promise<Response> {
    return this.execute("structure_set", {
      key,
      value,
      ...(expiresAtMicros === undefined ? {} : { expires_at_micros: expiresAtMicros }),
    }, options);
  }

  structureTtl(key: Uint8Array, options: RequestOptions = {}): Promise<Response> {
    return this.execute("structure_ttl", { key }, options);
  }

  structureMutate(mutations: ReadonlyArray<Readonly<Record<string, unknown>>>, options: RequestOptions = {}): Promise<Response> {
    return this.execute("structure_mutate", { mutations }, options);
  }

  structureRead(request: Readonly<Record<string, unknown>>, options: RequestOptions = {}): Promise<Response> {
    return this.execute("structure_read", request, options);
  }

  search(index: bigint, query: Readonly<Record<string, unknown>>, limit: number, options: RequestOptions = {}): Promise<Response> {
    return this.execute("search", { index, query, limit }, options);
  }

  searchCollection(collection: bigint, request: Readonly<Record<string, unknown>>, options: RequestOptions = {}): Promise<Response> {
    return this.execute("search_collection", { collection, request }, options);
  }

  searchIngest(collection: bigint, batch: Readonly<Record<string, unknown>>, options: RequestOptions = {}): Promise<Response> {
    return this.execute("search_ingest", { collection, batch }, options);
  }

  searchDocumentUpdate(collection: bigint, idempotencyId: bigint, document: Readonly<Record<string, unknown>>, options: RequestOptions = {}): Promise<Response> {
    return this.execute("search_document_update", { collection, idempotency_id: idempotencyId, document }, options);
  }

  searchDocumentDelete(collection: bigint, idempotencyId: bigint, objectId: bigint, options: RequestOptions = {}): Promise<Response> {
    return this.execute("search_document_delete", { collection, idempotency_id: idempotencyId, object_id: objectId }, options);
  }

  admin(action: string, args: Readonly<Record<string, unknown>> = {}, options: RequestOptions = {}): Promise<Response> {
    return this.execute(`admin_${action}`, args, options);
  }

  telemetry(options: RequestOptions = {}): Promise<Response> {
    return this.execute("telemetry", {}, options);
  }

  doctor(path: string, logicalTimeMicros: bigint, options: RequestOptions = {}): Promise<Response> {
    return this.execute("doctor", { path, logical_time_micros: logicalTimeMicros }, options);
  }

  backup(destination: string, limits: Readonly<Record<string, number>>, options: RequestOptions = {}): Promise<Response> {
    return this.execute("backup", { destination, limits }, options);
  }

  restore(backup: string, destination: string, limits: Readonly<Record<string, number>>, doctorLogicalTimeMicros = 0n, options: RequestOptions = {}): Promise<Response> {
    return this.execute("restore", { backup, destination, limits, doctor_logical_time_micros: doctorLogicalTimeMicros }, options);
  }

  verifyProof(proof: Uint8Array, witness: Uint8Array, trustedAnchor: Uint8Array, options: RequestOptions = {}): Promise<Response> {
    return this.execute("proof_verify", { proof, witness, trusted_anchor: trustedAnchor }, options);
  }

  prove(operation: string, args: Readonly<Record<string, unknown>>, limits: Readonly<Record<string, bigint | number>> = {}, options: RequestOptions = {}): Promise<Response> {
    return this.execute("proof_generate", { operation, arguments: args, limits }, options);
  }

  proveSql(statement: string, parameters: readonly unknown[] = [], limits: Readonly<Record<string, bigint | number>> = {}, options: RequestOptions = {}): Promise<Response> {
    return this.prove("sql_execute", { statement, parameters }, limits, options);
  }

  transactionStatus(transactionId: bigint, options: RequestOptions = {}): Promise<Response> {
    return this.execute("transaction_status", { transaction_id: transactionId }, options);
  }

  transactionBegin(options: RequestOptions = {}): Promise<Response> {
    return this.execute("transaction_begin", {}, options);
  }

  transactionStageSql(handle: bigint, statement: string, parameters: readonly unknown[] = [], options: RequestOptions = {}): Promise<Response> {
    return this.execute("transaction_stage_sql", { handle, statement, parameters }, options);
  }

  transactionStageStructure(handle: bigint, mutation: Readonly<Record<string, unknown>>, options: RequestOptions = {}): Promise<Response> {
    return this.execute("transaction_stage_structure", { handle, mutation }, options);
  }

  transactionStageSearch(handle: bigint, mutation: Readonly<Record<string, unknown>>, options: RequestOptions = {}): Promise<Response> {
    return this.execute("transaction_stage_search", { handle, mutation }, options);
  }

  transactionStageVector(handle: bigint, mutation: Readonly<Record<string, unknown>>, options: RequestOptions = {}): Promise<Response> {
    return this.execute("transaction_stage_vector", { handle, mutation }, options);
  }

  transactionCommit(handle: bigint, options: RequestOptions = {}): Promise<Response> {
    return this.execute("transaction_commit", { handle }, options);
  }

  transactionRollback(handle: bigint, options: RequestOptions = {}): Promise<Response> {
    return this.execute("transaction_rollback", { handle }, options);
  }

  explicitTransactionStatus(handle: bigint, options: RequestOptions = {}): Promise<Response> {
    return this.execute("explicit_transaction_status", { handle }, options);
  }

  transactionStatusByIdempotency(idempotencyToken: bigint, options: RequestOptions = {}): Promise<Response> {
    return this.execute("transaction_status_by_idempotency", { idempotency_token: idempotencyToken }, options);
  }

  securityPrincipalCreate(displayName: string, options: RequestOptions): Promise<Response> {
    return this.execute("security_principal_create", { display_name: displayName }, options);
  }

  securityPrincipalSetEnabled(principalId: bigint, enabled: boolean, options: RequestOptions): Promise<Response> {
    return this.execute("security_principal_set_enabled", { principal_id: principalId, enabled }, options);
  }

  securityCustomRoleCreate(displayName: string, grants: ReadonlyArray<Readonly<Record<string, unknown>>>, options: RequestOptions): Promise<Response> {
    return this.execute("security_custom_role_create", { display_name: displayName, grants }, options);
  }

  securityBuiltInAssignmentCreate(principalId: bigint, role: string, scope: Readonly<Record<string, unknown>>, options: RequestOptions): Promise<Response> {
    return this.execute("security_built_in_assignment_create", { principal_id: principalId, role, scope }, options);
  }

  securityCustomAssignmentCreate(principalId: bigint, roleId: bigint, options: RequestOptions): Promise<Response> {
    return this.execute("security_custom_assignment_create", { principal_id: principalId, role_id: roleId }, options);
  }

  securityAssignmentRevoke(assignmentId: bigint, options: RequestOptions): Promise<Response> {
    return this.execute("security_assignment_revoke", { assignment_id: assignmentId }, options);
  }

  securityApiKeyIssueStart(args: Readonly<Record<string, unknown>>, selfManage = false, options: RequestOptions): Promise<Response> {
    return this.execute(selfManage ? "security_api_key_issue_self_start" : "security_api_key_issue_start", args, options);
  }

  securityApiKeyRotateStart(args: Readonly<Record<string, unknown>>, selfManage = false, options: RequestOptions): Promise<Response> {
    return this.execute(selfManage ? "security_api_key_rotate_self_start" : "security_api_key_rotate_start", args, options);
  }

  securityApiKeyActivate(keyId: Uint8Array, confirmationDigest: Uint8Array, rotation = false, selfManage = false, options: RequestOptions): Promise<Response> {
    const operation = `security_api_key_${rotation ? "rotate" : "issue"}_${selfManage ? "self_" : ""}activate`;
    return this.execute(operation, { [rotation ? "successor_key_id" : "key_id"]: keyId, confirmation_digest: confirmationDigest }, options);
  }

  securityApiKeyAbort(keyId: Uint8Array, rotation = false, selfManage = false, options: RequestOptions): Promise<Response> {
    const operation = `security_api_key_${rotation ? "rotate" : "issue"}_${selfManage ? "self_" : ""}abort`;
    return this.execute(operation, { [rotation ? "successor_key_id" : "key_id"]: keyId }, options);
  }

  securityApiKeyRevoke(keyId: Uint8Array, selfManage = false, options: RequestOptions): Promise<Response> {
    return this.execute(selfManage ? "security_api_key_revoke_self" : "security_api_key_revoke", { key_id: keyId }, options);
  }

  securityLegacyBearerRevoke(options: RequestOptions): Promise<Response> {
    return this.execute("security_legacy_bearer_revoke", {}, options);
  }

  securityStatus(options: RequestOptions = {}): Promise<Response> {
    return this.execute("security_status", {}, options);
  }

  securityPrincipalList(request: Readonly<Record<string, unknown>>, options: RequestOptions = {}): Promise<Response> {
    return this.execute("security_principal_list", request, options);
  }

  securityRoleList(request: Readonly<Record<string, unknown>>, options: RequestOptions = {}): Promise<Response> {
    return this.execute("security_role_list", request, options);
  }

  securityAssignmentList(request: Readonly<Record<string, unknown>>, options: RequestOptions = {}): Promise<Response> {
    return this.execute("security_assignment_list", request, options);
  }

  securityKeyList(request: Readonly<Record<string, unknown>>, options: RequestOptions = {}): Promise<Response> {
    return this.execute("security_key_list", request, options);
  }

  securityAuditRead(request: Readonly<Record<string, unknown>>, options: RequestOptions = {}): Promise<Response> {
    return this.execute("security_audit_read", request, options);
  }

}

/** Overwrites sensitive bytes in place. */
export function clearSensitiveBytes(value: Uint8Array): void {
  value.fill(0);
}
