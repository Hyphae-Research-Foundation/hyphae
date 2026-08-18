// SPDX-License-Identifier: Apache-2.0

import assert from "node:assert/strict";
import test from "node:test";

import { createHyphaeAstroMiddleware } from "../dist/astro.js";
import {
  createHyphaeNextClient,
  createHyphaeNextClientFromEnv,
} from "../dist/next.js";
import { createHyphaeBrowserClient } from "../dist/vite-client.js";
import { hyphaeVite } from "../dist/vite.js";

test("Astro middleware attaches an explicit client without owning host state", async () => {
  const middleware = createHyphaeAstroMiddleware({ baseUrl: "http://127.0.0.1:8787" });
  const locals = {};
  const expected = new Response("next");
  const response = await middleware({ locals }, async () => expected);
  assert.equal(response, expected);
  assert.equal(typeof locals.hyphae.capabilities, "function");
  assert.throws(
    () => middleware({ locals }, async () => expected),
    /already defines/u,
  );
});

test("Next adapter requires explicit server-side enablement", () => {
  assert.throws(() => createHyphaeNextClientFromEnv({}), /HYPHAE_BASE_URL/u);
  const explicit = createHyphaeNextClient({ baseUrl: "http://127.0.0.1:8787" });
  const environment = createHyphaeNextClientFromEnv({
    HYPHAE_BASE_URL: "http://127.0.0.1:8787",
  });
  assert.equal(typeof explicit.query, "function");
  assert.equal(typeof environment.get, "function");
});

test("Vite plugin proxies only /v1 and never accepts a bearer token", () => {
  const plugin = hyphaeVite({ target: "http://127.0.0.1:8787" });
  const config = plugin.config({}, { command: "serve", mode: "test" });
  assert.deepEqual(config.server.proxy, {
    "/v1": { target: "http://127.0.0.1:8787", changeOrigin: false },
  });
  assert.throws(
    () => hyphaeVite({ target: "https://example.test/prefix" }),
    /root HTTP\(S\) origin/u,
  );
  const client = createHyphaeBrowserClient("http://127.0.0.1:8787");
  assert.equal(typeof client.liveness, "function");
});
