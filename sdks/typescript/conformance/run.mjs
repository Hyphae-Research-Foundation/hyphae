// SPDX-License-Identifier: Apache-2.0

import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";

import { HyphaeApiError, HyphaeClient } from "../dist/index.js";

const fixtureUrl = new URL("../../../conformance/v1/cases.json", import.meta.url);
const fixture = JSON.parse(await readFile(fixtureUrl, "utf8"));
assert.equal(fixture.version, 1);

const client = new HyphaeClient(process.env.HYPHAE_BASE_URL ?? "http://127.0.0.1:8787", {
  ...(process.env.HYPHAE_BEARER_TOKEN === undefined
    ? {}
    : { bearerToken: process.env.HYPHAE_BEARER_TOKEN }),
});

assert.equal((await client.liveness()).value.status, "live");
const capabilities = (await client.capabilities()).value;
assert.equal(capabilities.api_version, "v1");
assert.deepEqual([...capabilities.features].sort(), capabilities.features);
assert.equal(new Set(capabilities.features).size, capabilities.features.length);

assert.equal((await client.put(fixture.put_request)).value.status, "committed");
assert.equal((await client.put(fixture.put_request)).value.status, "existing");
await assert.rejects(
  client.put(fixture.conflict_put_request),
  (error) => error instanceof HyphaeApiError && error.code === "idempotency_conflict",
);

const present = (await client.get(fixture.present_get_request)).value;
assert.equal(present.found, true);
assert.equal(present.record?.key_hex, "61");
const witness = (await client.downloadWitness(present.proof)).value;
assert.equal(new TextDecoder().decode(witness.slice(0, 8)), "HYSNAP01");

const absent = (await client.get(fixture.absent_get_request)).value;
assert.equal(absent.found, false);
assert.equal(absent.record ?? null, null);

const firstPage = (await client.query(fixture.query_request)).value;
assert.deepEqual(firstPage.rows.map((record) => record.key_hex), fixture.expected.first_page_keys);
assert.equal(firstPage.matched_records, fixture.expected.matched_records);
assert.deepEqual(firstPage.aggregation, fixture.expected.aggregation);
const secondPage = (await client.query({
  ...fixture.query_request,
  cursor: firstPage.next_cursor,
})).value;
assert.deepEqual(secondPage.rows.map((record) => record.key_hex), fixture.expected.second_page_keys);
assert.equal(secondPage.next_cursor ?? null, null);

assert.equal(
  (await client.defineVectorSpace(fixture.define_vector_space_request)).value.status,
  "committed",
);
assert.equal(
  (await client.defineVectorSpace(fixture.define_vector_space_request)).value.status,
  "existing",
);
await assert.rejects(
  client.putVectors(fixture.invalid_put_vectors_request),
  (error) => error instanceof HyphaeApiError && error.code === "invalid_request",
);
assert.equal((await client.putVectors(fixture.put_vectors_request)).value.status, "committed");
assert.equal(
  (await client.defineLexicalIndex(fixture.define_lexical_index_request)).value.status,
  "committed",
);

const exact = (await client.retrieveExact(fixture.exact_retrieval_request)).value;
assert.equal(exact.outcome.status, "matches");
assert.deepEqual(
  exact.outcome.matches?.map((match) => match.key_hex),
  fixture.expected.exact_retrieval_keys,
);
assert.equal(
  new TextDecoder().decode((await client.downloadRetrievalWitness(exact.proof)).value.slice(0, 8)),
  "HYSNAP01",
);
const ambiguous = (await client.retrieveExact(fixture.ambiguous_exact_retrieval_request)).value;
assert.equal(ambiguous.outcome.status, "abstained");
assert.equal(ambiguous.outcome.abstention?.reason, fixture.expected.ambiguous_exact_reason);
await assert.rejects(
  client.retrieveExact(fixture.wrong_dimension_exact_retrieval_request),
  (error) => error instanceof HyphaeApiError && error.code === "invalid_request",
);

const lexical = (await client.retrieveLexical(fixture.lexical_retrieval_request)).value;
assert.equal(lexical.outcome.status, "matches");
assert.equal(lexical.outcome.matches?.[0]?.key_hex, fixture.expected.lexical_first_key);
await assert.rejects(
  client.retrieveLexical(fixture.invalid_lexical_retrieval_request),
  (error) => error instanceof HyphaeApiError && error.code === "invalid_request",
);

const hybrid = (await client.retrieveHybrid(fixture.hybrid_retrieval_request)).value;
assert.equal(hybrid.outcome.status, "matches");
assert.equal(hybrid.outcome.matches?.[0]?.key_hex, fixture.expected.hybrid_first_key);
assert.equal((await client.deleteVectors(fixture.delete_vectors_request)).value.status, "committed");

await client.delete(fixture.delete_request);
assert.equal((await client.get({ key_hex: "62" })).value.found, false);

console.log(JSON.stringify({ client: "typescript", status: "ok" }));
