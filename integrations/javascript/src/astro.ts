// SPDX-License-Identifier: Apache-2.0

import { HyphaeClient, type HyphaeClientOptions } from "@celiums/hyphae";
import type { MiddlewareHandler } from "astro";

export interface HyphaeAstroOptions extends HyphaeClientOptions {
  readonly baseUrl: string;
  /** Local name placed in `Astro.locals`; defaults to `hyphae`. */
  readonly localName?: string;
}

/** Create opt-in Astro middleware that exposes one public Hyphae client in locals. */
export function createHyphaeAstroMiddleware(options: HyphaeAstroOptions): MiddlewareHandler {
  const { baseUrl, localName = "hyphae", ...clientOptions } = options;
  if (!/^[$A-Z_a-z][$\w]*$/u.test(localName)) {
    throw new TypeError("Astro localName must be a JavaScript identifier");
  }
  const client = new HyphaeClient(baseUrl, clientOptions);
  return (context, next) => {
    const locals = context.locals as Record<string, unknown>;
    if (Object.hasOwn(locals, localName)) {
      throw new TypeError(`Astro.locals already defines ${localName}`);
    }
    Object.defineProperty(locals, localName, {
      value: client,
      enumerable: true,
      configurable: false,
      writable: false,
    });
    return next();
  };
}
