// SPDX-License-Identifier: Apache-2.0

import { HyphaeClient, type HyphaeClientOptions } from "@celiums/hyphae";

export interface HyphaeNextOptions extends HyphaeClientOptions {
  readonly baseUrl: string;
}

export interface HyphaeNextEnvironment {
  readonly HYPHAE_BASE_URL?: string;
  readonly HYPHAE_BEARER_TOKEN?: string;
}

/** Construct a Next server-only client from explicit non-public configuration. */
export function createHyphaeNextClient(options: HyphaeNextOptions): HyphaeClient {
  assertServerRuntime();
  const { baseUrl, ...clientOptions } = options;
  return new HyphaeClient(baseUrl, clientOptions);
}

/** Construct a Next server-only client from runtime environment values. */
export function createHyphaeNextClientFromEnv(
  environment: HyphaeNextEnvironment = processEnvironment(),
): HyphaeClient {
  assertServerRuntime();
  const baseUrl = environment.HYPHAE_BASE_URL;
  if (baseUrl === undefined || baseUrl.length === 0) {
    throw new TypeError("HYPHAE_BASE_URL is required when the Next integration is enabled");
  }
  return new HyphaeClient(baseUrl, {
    ...(environment.HYPHAE_BEARER_TOKEN === undefined
      ? {}
      : { bearerToken: environment.HYPHAE_BEARER_TOKEN }),
  });
}

function assertServerRuntime(): void {
  if (typeof globalThis.window !== "undefined") {
    throw new TypeError("the Next Hyphae adapter is server-only");
  }
}

function processEnvironment(): HyphaeNextEnvironment {
  const runtime = globalThis as typeof globalThis & {
    process?: { env?: HyphaeNextEnvironment };
  };
  return runtime.process?.env ?? {};
}
