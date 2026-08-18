// SPDX-License-Identifier: Apache-2.0

import { HyphaeClient, type HyphaeClientOptions } from "@celiums/hyphae";

export type HyphaeBrowserClientOptions = Omit<HyphaeClientOptions, "bearerToken">;

/** Create a browser client through a same-origin `/v1` reverse proxy. */
export function createHyphaeBrowserClient(
  baseUrl: string = browserOrigin(),
  options: HyphaeBrowserClientOptions = {},
): HyphaeClient {
  return new HyphaeClient(baseUrl, options);
}

function browserOrigin(): string {
  const origin = globalThis.location?.origin;
  if (origin === undefined || origin === "null") {
    throw new TypeError("an explicit same-origin base URL is required outside a browser");
  }
  return origin;
}
