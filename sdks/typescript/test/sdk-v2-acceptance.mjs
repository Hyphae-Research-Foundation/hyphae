import assert from "node:assert/strict";
import { writeFile } from "node:fs/promises";

import { DEFAULT_LIMITS, HyphaeClient, ProductError } from "../dist/v2/index.js";

const local = HyphaeClient.local(process.env.HYPHAE_SOCKET);
const deniedLocal = HyphaeClient.local(
  process.env.HYPHAE_SOCKET,
  undefined,
  process.env.HYPHAE_DENIED_IDENTITY,
);
const http = HyphaeClient.http(process.env.HYPHAE_ORIGIN, { bearerToken: process.env.HYPHAE_TOKEN });
const deniedHttp = HyphaeClient.http(process.env.HYPHAE_ORIGIN);

const fields = (error) => {
  assert(error instanceof ProductError);
  return error.fields;
};
const failures = [
  ["sql_invalid_syntax", (client, options) => client.sql("SELEC bad", [], options)],
  ["catalog_object_not_found", (client, options) => client.catalogObject(999n, options)],
  ["limit_exceeded", (client, options) => client.sql("SELECT id FROM proof_items", [], options)],
];
for (const [offset, [code, call]] of failures.entries()) {
  const requestId = 30_100n + BigInt(offset);
  const options = { requestId, limits: code === "limit_exceeded" ? { ...DEFAULT_LIMITS, maxRequestBytes: 1 } : DEFAULT_LIMITS };
  const errors = [];
  for (const client of [local, http]) {
    try {
      await call(client, options);
      throw new Error(`${code} accepted`);
    } catch (error) {
      errors.push(fields(error));
    }
  }
  assert.deepEqual(errors[0], errors[1]);
  assert.equal(errors[0].code, code);
  assert.equal(errors[0].requestId, requestId);
}

const expiredErrors = [];
for (const client of [local, http]) {
  try {
    await client.proveSql(
      "SELECT label FROM proof_items WHERE id = ?",
      [7n],
      {},
      { requestId: 30_110n, deadlineMicros: BigInt(Date.now()) * 1000n + 100n },
    );
    throw new Error("expired request accepted");
  } catch (error) {
    expiredErrors.push(fields(error));
  }
}
assert.deepEqual(expiredErrors[0], expiredErrors[1]);
assert.equal(expiredErrors[0].code, "deadline_exceeded");

const cancelledErrors = [];
for (const [transport, client] of [["local", local], ["http", http]]) {
  const controller = new AbortController();
  if (transport === "local") queueMicrotask(() => controller.abort());
  else controller.abort();
  try {
    await client.proveSql(
      "SELECT label FROM proof_items WHERE id = ?",
      [7n],
      {},
      { requestId: 30_111n, signal: controller.signal },
    );
    throw new Error("cancelled request accepted");
  } catch (error) {
    cancelledErrors.push(fields(error));
  }
}
assert.deepEqual(cancelledErrors[0], cancelledErrors[1]);
assert.equal(cancelledErrors[0].code, "cancelled");

const authorizationErrors = [];
for (const client of [deniedLocal, deniedHttp]) {
  try {
    await client.structureGet(new TextEncoder().encode("denied"), { requestId: 30_112n });
    throw new Error("unauthorized request accepted");
  } catch (error) {
    authorizationErrors.push(fields(error));
  }
}
assert.deepEqual(authorizationErrors[0], authorizationErrors[1]);
assert.equal(authorizationErrors[0].code, "authorization_denied");

const proven = await local.proveSql("SELECT label FROM proof_items WHERE id = ?", [7n], {}, { requestId: 30_120n });
assert.equal(proven.kind, "proven");
assert.equal(new TextDecoder().decode(proven.value.proof.slice(0, 8)), "HYNPRF02");
assert.equal(new TextDecoder().decode(proven.value.witness.slice(0, 8)), "HYNWIT02");
const verified = await http.verifyProof(
  proven.value.proof,
  proven.value.witness,
  proven.value.trustedAnchor,
  { requestId: 30_121n },
);
assert.equal(verified.kind, "proof_verification");
assert.equal(verified.value.semanticReexecutionPerformed, true);
{
  const values = [proven.value.proof, proven.value.witness, proven.value.trustedAnchor];
  const encoded = new Uint8Array(values.reduce((total, value) => total + 8 + value.byteLength, 0));
  const view = new DataView(encoded.buffer);
  let offset = 0;
  for (const value of values) {
    view.setBigUint64(offset, BigInt(value.byteLength), true);
    offset += 8;
    encoded.set(value, offset);
    offset += value.byteLength;
  }
  await writeFile(process.env.HYPHAE_TYPESCRIPT_ARTIFACT, encoded);
}

const httpProven = await http.proveSql("SELECT label FROM proof_items WHERE id = ?", [7n], {}, { requestId: 30_122n });
assert.equal(new TextDecoder().decode(httpProven.value.proof.slice(0, 8)), "HYNPRF02");
assert.equal(new TextDecoder().decode(httpProven.value.witness.slice(0, 8)), "HYNWIT02");
const proofLocal = HyphaeClient.local(process.env.HYPHAE_SOCKET);
const localVerified = await proofLocal.verifyProof(
  httpProven.value.proof,
  httpProven.value.witness,
  httpProven.value.trustedAnchor,
  { requestId: 30_123n },
);
assert.equal(localVerified.value.semanticReexecutionPerformed, true);
await proofLocal.close();

await local.close();
await deniedLocal.close();
