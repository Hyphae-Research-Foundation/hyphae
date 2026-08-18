// SPDX-License-Identifier: Apache-2.0

import type { Plugin, UserConfig } from "vite";

export interface HyphaeViteOptions {
  /** Root loopback/public Hyphae origin used only by the local dev proxy. */
  readonly target: string;
  /** Configure `/v1` dev and preview proxies; defaults to true. */
  readonly proxy?: boolean;
}

/** Add an opt-in same-origin `/v1` development proxy without embedding secrets. */
export function hyphaeVite(options: HyphaeViteOptions): Plugin {
  const target = normalizeOrigin(options.target);
  const proxy = options.proxy ?? true;
  return {
    name: "hyphae-vite",
    enforce: "pre",
    config(): UserConfig {
      if (!proxy) return {};
      const entry = { target, changeOrigin: false };
      return {
        server: { proxy: { "/v1": entry } },
        preview: { proxy: { "/v1": entry } },
      };
    },
  };
}

function normalizeOrigin(value: string): string {
  let parsed: URL;
  try {
    parsed = new URL(value);
  } catch (cause) {
    throw new TypeError("Vite Hyphae target must be a root HTTP(S) origin", { cause });
  }
  if ((parsed.protocol !== "http:" && parsed.protocol !== "https:") ||
      parsed.username !== "" || parsed.password !== "" || parsed.search !== "" ||
      parsed.hash !== "" || (parsed.pathname !== "" && parsed.pathname !== "/")) {
    throw new TypeError("Vite Hyphae target must be a root HTTP(S) origin");
  }
  parsed.pathname = "/";
  return parsed.origin;
}
