// SPDX-License-Identifier: GPL-3.0-only
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";

import { HyphaeClient } from "../dist/v2/index.js";

const encoded = new Uint8Array(await readFile(process.env.HYPHAE_ARTIFACT));
const view = new DataView(encoded.buffer, encoded.byteOffset, encoded.byteLength);
const values = [];
let offset = 0;
for (let index = 0; index < 3; index += 1) {
  const length = Number(view.getBigUint64(offset, true));
  offset += 8;
  values.push(encoded.slice(offset, offset + length));
  offset += length;
}
assert.equal(offset, encoded.byteLength);

const client = HyphaeClient.http(process.env.HYPHAE_ORIGIN, {
  bearerToken: process.env.HYPHAE_TOKEN,
});
const [proof, witness, trustedAnchor] = values;
assert.notEqual(proof, undefined);
assert.notEqual(witness, undefined);
assert.notEqual(trustedAnchor, undefined);
const verified = await client.verifyProof(proof, witness, trustedAnchor, { requestId: 30_124n });
assert.equal(verified.kind, "proof_verification");
assert.equal(verified.value.semanticReexecutionPerformed, true);
