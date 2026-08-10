// SPDX-License-Identifier: GPL-3.0-only
import assert from "node:assert/strict";

import { HyphaeClient, ProductError } from "../dist/v2/index.js";

const clients = [
  HyphaeClient.local(process.env.HYPHAE_SOCKET),
  HyphaeClient.http(process.env.HYPHAE_ORIGIN, { bearerToken: process.env.HYPHAE_TOKEN }),
];
const errors = [];
for (const [index, client] of clients.entries()) {
  try {
    await client.structureSet(
      new TextEncoder().encode(`unknown-${index}`),
      new TextEncoder().encode("t".repeat(9000)),
      undefined,
      { requestId: 30_132n },
    );
    throw new Error("unknown commit was acknowledged");
  } catch (error) {
    assert(error instanceof ProductError);
    assert.equal(error.fields.code, "unknown_commit");
    assert.equal(error.fields.category, "unavailable");
    assert.equal(error.fields.retry, "unknown-commit");
    assert.equal(error.fields.transactionState, "outcome-unknown");
    assert.notEqual(error.fields.transactionId, undefined);
    assert.equal(error.fields.requestId, 30_132n);
    const { transactionId, details, ...comparable } = error.fields;
    errors.push(comparable);
  }
}
assert.deepEqual(errors[0], errors[1]);
for (const client of clients) await client.close();
