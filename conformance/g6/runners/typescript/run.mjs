#!/usr/bin/env node
// SPDX-License-Identifier: GPL-3.0-only
// TypeScript SDK G6 lane runner over native-local or HTTP v2.

import { execFileSync, spawn } from "node:child_process";
import { existsSync, readFileSync, rmSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { DEFAULT_LIMITS, HyphaeClient, ProductError } from "../../../../sdks/typescript/dist/v2/index.js";

const lane = process.env.HYPHAE_G6_LANE;
const here = dirname(fileURLToPath(import.meta.url));
const runner = resolve(here, "../rust/Cargo.toml");
const work = process.env.HYPHAE_G6_WORK;
const corpus = JSON.parse(readFileSync(process.env.HYPHAE_G6_CORPUS, "utf8"));
if (corpus.schema !== "hyphae-native-g6-corpus-v1") throw new Error("unsupported G6 corpus");
const data = resolve(work, `lane-${lane}`);
execFileSync("cargo", ["run", "--quiet", "--locked", "--manifest-path", runner, "--", "restore", resolve(work, "seed-backup"), data]);
const endpoint = resolve(work, `${lane}.sock`);
const portFile = resolve(work, `${lane}.port`);
rmSync(endpoint, { force: true });
rmSync(portFile, { force: true });
const server = spawn("cargo", ["run", "--quiet", "--locked", "--manifest-path", runner, "--", "serve", data, endpoint, portFile], { stdio: ["ignore", "ignore", "pipe"] });

const hex = (value) => Buffer.from(value).toString("hex");
const normalize = (value) => {
  if (value === undefined) return null;
  if (typeof value === "bigint") return value.toString();
  if (value instanceof Uint8Array) return hex(value);
  if (Array.isArray(value)) return value.map(normalize);
  if (value !== null && typeof value === "object") return Object.fromEntries(Object.entries(value).sort(([left], [right]) => left.localeCompare(right)).map(([key, child]) => [key, normalize(child)]));
  return value;
};
const options = (requestId) => ({ requestId: BigInt(requestId), logicalTimeMicros: 1700000000000000n });
const snapshot = (value) => ({ directory_lineage: hex(value.directoryLineage), catalog_version: Number(value.catalogVersion), visible_csn: value.visibleCsn === 0n ? null : Number(value.visibleCsn), root_digest: hex(value.rootDigest) });
const add = (cases, id, outcome) => cases.push({ id, outcome: normalize(outcome) });

function searchRequest(mode) {
  const vectors = [];
  if (["exact", "hybrid", "named-vectors"].includes(mode)) vectors.push({ target: "exact", query: [0, 0], candidate_limit: 4, weight: 1, execution: { kind: "exact" } });
  if (["ann", "named-vectors"].includes(mode)) vectors.push({ target: "ann", query: [0, 0], candidate_limit: 4, weight: 1, execution: { kind: "ann", ef_search: 8, exact_rerank: 4 } });
  const value = { lexical: ["lexical", "hybrid", "filter", "facet", "metric"].includes(mode) ? { query: "rust", candidate_limit: 8, weight: 1 } : undefined, vectors, limit: 8 };
  if (mode === "filter") value.filter = { kind: "compare", field: "category", operator: "equal", value: "book" };
  if (mode === "facet") value.facets = [{ field: "category", limit: 8 }];
  if (mode === "metric") value.aggregations = [{ name: "count", kind: "count" }];
  return value;
}

const errorOutcome = (error) => ({ code: error.fields.code, category: error.fields.category, retry: error.fields.retry, transaction_state: error.fields.transactionState, request_id: error.fields.requestId });
const backupOutcome = (value) => ({ visible_csn: Number(value.visibleCsn), checkpoint_digest: value.checkpointDigest, file_count: Number(value.fileCount), total_bytes: Number(value.totalBytes) });
const backupLimits = { max_files: 16384, max_directories: 16384, max_total_bytes: 256 * 1024 * 1024 * 1024, max_path_bytes: 4096, max_manifest_bytes: 4 * 1024 * 1024 };

async function executeCases(client, denied) {
  const cases = [];
  const initial = (await client.admin("status", {}, options(6000))).value;
  const start = snapshot(initial.snapshot);
  const capabilities = (await client.capabilities(options(6001))).value;
  add(cases, "capabilities/capabilities", { product_api_version: capabilities.productApiVersion, directory_format: capabilities.nativeDirectoryFormat });
  const page = (await client.catalogList({ item_limit: 64, visit_limit: 128, byte_limit: 65536 }, options(6002))).value;
  add(cases, "catalog/catalog-list", { snapshot: snapshot(page.snapshot), object_ids: page.items.map((item) => item.id) });
  for (const [id, objectId] of [["catalog/catalog-describe", 10n], ["catalog/catalog-dependencies", 15n]]) {
    const definition = (await client.execute("catalog_describe", { id: objectId }, options(6003))).value;
    add(cases, id, { object_id: objectId, present: definition !== undefined });
  }
  const ddl = (await client.sql("CREATE TABLE g6_lane (id BIGINT PRIMARY KEY)", [], options(6004))).value;
  add(cases, "sql/sql-ddl", { rows_affected: Number(ddl.result.rowsAffected), object_id: ddl.result.objectId, commit_csn: Number(ddl.commit.receipt.commitCsn) });
  const dml = (await client.sql("INSERT INTO g6_lane (id) VALUES (?)", [1n], options(6005))).value;
  add(cases, "sql/sql-dml", { rows_affected: Number(dml.result.rowsAffected), object_id: dml.result.objectId, commit_csn: Number(dml.commit.receipt.commitCsn) });
  const selected = (await client.sql("SELECT id, label FROM g6_items WHERE id = ?", [1n], options(6006))).value;
  add(cases, "sql/sql-prepared", { columns: selected.result.columns, rows: selected.result.rows.map((row) => row.map((value) => typeof value === "bigint" && value <= BigInt(Number.MAX_SAFE_INTEGER) && value >= BigInt(Number.MIN_SAFE_INTEGER) ? Number(value) : value)), snapshot: snapshot(selected.snapshot) });
  const explained = (await client.admin("explain_sql", { statement: "SELECT id, label FROM g6_items WHERE id = 1" }, options(6007))).value;
  add(cases, "sql/sql-explain", { version: explained.version, text: explained.text });
  const scalar = (await client.structureGet(new TextEncoder().encode("g6-scalar"), options(6008))).value;
  add(cases, "structures/scalar", { family: "scalar", value: scalar, snapshot: null });
  const reads = [
    ["hash", { kind: "hash_get", key: { keyspace: 10n, key: new TextEncoder().encode("hash") }, field: new TextEncoder().encode("field") }],
    ["set", { kind: "set_members", key: { keyspace: 11n, key: new TextEncoder().encode("set") }, limit: 10 }],
    ["list", { kind: "list_range", key: { keyspace: 12n, key: new TextEncoder().encode("list") }, start: 0, stop: -1 }],
    ["sorted-set", { kind: "sorted_set_range", key: { keyspace: 13n, key: new TextEncoder().encode("zset") }, start: 0, stop: -1 }],
    ["stream", { kind: "stream_range", key: { keyspace: 14n, key: new TextEncoder().encode("stream") }, start: 0n, end: (1n << 64n) - 1n, limit: 10 }],
  ];
  for (const [offset, [family, read]] of reads.entries()) {
    const result = (await client.structureRead(read, options(6010 + offset))).value;
    if (family === "stream") result.result.entries = result.result.entries.map((entry) => ({ ...entry, id: Number(entry.id) }));
    add(cases, `structures/${family}`, { family, value: result.result, snapshot: snapshot(result.snapshot) });
  }
  for (const [offset, mode] of ["lexical", "exact", "ann", "hybrid", "named-vectors", "filter", "facet", "metric"].entries()) {
    const result = (await client.searchCollection(17n, searchRequest(mode), options(6020 + offset))).value;
    add(cases, `search/${mode}`, { mode, snapshot: snapshot(result.snapshot), object_ids: result.hits.map((hit) => hit.objectId), approximate: result.approximate });
  }
  const transaction = (await client.transactionStatus(2n, options(6030))).value;
  add(cases, "transactions/commit-status", { status: transaction.state.replaceAll("_", "-"), transaction_id: "2" });
  const committed = (await client.sql("UPDATE g6_items SET label = ? WHERE id = ?", ["beta", 1n], options(6033))).value;
  add(cases, "transactions/atomic-batch", { staged_operations: 1, commit_csn: Number(committed.commit.receipt.commitCsn) });
  const status = (await client.admin("status", {}, options(6034))).value;
  add(cases, "administration/status", { snapshot: snapshot(status.snapshot) });
  const telemetry = (await client.telemetry(options(6035))).value;
  add(cases, "administration/telemetry", { registry_version: telemetry.registryVersion, metric_names: telemetry.metrics.map((metric) => metric.name) });
  const doctor = (await client.execute("doctor", {}, options(6036))).value;
  add(cases, "administration/doctor", { status: doctor.status, snapshot_verified: doctor.snapshotVerified });
  if (lane.endsWith("-http")) {
    const proven = (await client.proveSql("SELECT id, label FROM g6_items WHERE id = ?", [1n], {}, options(6040))).value;
    add(cases, "proofs/generate", { kind: "sql", anchor_digest: proven.trustedAnchor, proof_digest: proven.proof.slice(32, 64), result_digest: proven.proof.slice(0, 32) });
  }
  const backupPath = resolve(work, `${lane}-corpus-backup`);
  const restoredPath = resolve(work, `${lane}-corpus-restored`);
  const backup = (await client.backup(backupPath, backupLimits, options(6050))).value;
  add(cases, "backup/create", backupOutcome(backup));
  const restored = (await client.restore(backupPath, restoredPath, backupLimits, 1700000000000000n, options(6052))).value;
  add(cases, "backup/restore", { visible_csn: Number(restored.backup.visibleCsn), checkpoint_digest: restored.backup.checkpointDigest, doctor_status: restored.doctor.status, snapshot_verified: restored.doctor.snapshotVerified });
  add(cases, "backup/doctor-after-restore", { status: restored.doctor.status, snapshot_verified: restored.doctor.snapshotVerified });
  const failures = ["syntax", "not-found"];
  for (const [offset, name] of failures.entries()) {
    try {
      if (name === "syntax") await client.sql("SELEC bad", [], options(6100 + offset));
      else await client.search(999n, { kind: "term", value: "missing" }, 1, options(6100 + offset));
      throw new Error(`TypeScript failure case ${name} succeeded`);
    } catch (error) {
      if (!(error instanceof ProductError)) throw error;
      add(cases, `failures/${name}`, errorOutcome(error));
    }
  }
  const controller = new AbortController();
  controller.abort();
  const productFailures = [
    ["limit", { ...options(6110), limits: { ...DEFAULT_LIMITS, maxRequestBytes: 1 } }],
    ["deadline", { ...options(6111), deadlineMicros: 1n }],
    ["cancellation", { ...options(6112), signal: controller.signal }],
  ];
  for (const [name, failureOptions] of productFailures) {
    try {
      await client.sql("SELECT id FROM g6_items", [], failureOptions);
      throw new Error(`TypeScript failure case ${name} succeeded`);
    } catch (error) {
      if (!(error instanceof ProductError)) throw error;
      add(cases, `failures/${name}`, errorOutcome(error));
    }
  }
  try {
    await denied.structureGet(new TextEncoder().encode("g6-scalar"), options(6113));
    throw new Error("TypeScript authorization case succeeded");
  } catch (error) {
    if (!(error instanceof ProductError)) throw error;
    add(cases, "failures/authorization", errorOutcome(error));
  }
  return { start, cases };
}

try {
  for (let attempt = 0; attempt < 400; attempt += 1) {
    if (server.exitCode !== null) throw new Error("G6 server exited before SDK execution");
    if (existsSync(portFile)) {
      await new Promise((resolveWait) => setTimeout(resolveWait, 100));
      break;
    }
    await new Promise((resolveWait) => setTimeout(resolveWait, 25));
  }
  let client;
  let denied;
  if (lane.endsWith("-local")) {
    client = HyphaeClient.local(endpoint);
    denied = HyphaeClient.local(endpoint, undefined, "hyphae-g6-conformance-denied");
  } else {
    const origin = `http://127.0.0.1:${readFileSync(portFile, "utf8").trim()}`;
    client = HyphaeClient.http(origin, { bearerToken: "0123456789abcdef0123456789abcdef" });
    denied = HyphaeClient.http(origin);
  }
  const { start, cases } = await executeCases(client, denied);
  await client.close();
  await denied.close();
  const coverage = ["capabilities", "catalog", "sql", "structures", "search", "transactions", "administration"];
  if (lane.endsWith("-http")) coverage.push("proofs");
  coverage.push("backup", "failures");
  process.stdout.write(`${JSON.stringify({ schema: "hyphae-native-g6-transcript-v1", lane, adapter: "typescript", transport: lane.endsWith("-local") ? "native-local" : "http-v2", start, cases, coverage, status: "passed" })}\n`);
} finally {
  server.kill("SIGTERM");
  await new Promise((resolveExit) => server.exitCode !== null ? resolveExit() : server.once("exit", resolveExit));
}
