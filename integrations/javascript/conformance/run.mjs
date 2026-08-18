// SPDX-License-Identifier: Apache-2.0

import assert from "node:assert/strict";

import { createHyphaeAstroMiddleware } from "../dist/astro.js";
import { createHyphaeNextClientFromEnv } from "../dist/next.js";
import { createHyphaeBrowserClient } from "../dist/vite-client.js";

const baseUrl = process.env.HYPHAE_BASE_URL ?? "http://127.0.0.1:8787";
const bearerToken = process.env.HYPHAE_BEARER_TOKEN;
const clientOptions = bearerToken === undefined ? {} : { bearerToken };

const locals = {};
const middleware = createHyphaeAstroMiddleware({ baseUrl, ...clientOptions });
const sentinel = new Response("next");
assert.equal(await middleware({ locals }, async () => sentinel), sentinel);
assert.equal((await locals.hyphae.capabilities()).value.api_version, "v1");

const nextClient = createHyphaeNextClientFromEnv({
  HYPHAE_BASE_URL: baseUrl,
  ...(bearerToken === undefined ? {} : { HYPHAE_BEARER_TOKEN: bearerToken }),
});
assert.equal((await nextClient.readiness()).value.status, "ready");

const viteClient = createHyphaeBrowserClient(baseUrl);
assert.equal((await viteClient.liveness()).value.status, "live");

console.log(JSON.stringify({
  version: 1,
  adapters: ["astro", "next", "vite"],
  status: "ok",
}));
