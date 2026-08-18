// SPDX-License-Identifier: Apache-2.0

/** Parse Hyphae JSON without rounding integer tokens outside JavaScript's safe range. */
export function parseHyphaeJson(encoded: string): unknown {
  return new Parser(encoded).parse();
}

/** Serialize integer-safe Hyphae JSON, including `bigint` as unquoted JSON integers. */
export function stringifyHyphaeJson(value: unknown): string {
  const active = new Set<object>();
  const encoded = encode(value, active, false);
  if (encoded === undefined) {
    throw new TypeError("Hyphae JSON root cannot be undefined");
  }
  return encoded;
}

class Parser {
  readonly #encoded: string;
  #offset = 0;

  constructor(encoded: string) {
    this.#encoded = encoded;
  }

  parse(): unknown {
    const value = this.#value();
    this.#whitespace();
    if (this.#offset !== this.#encoded.length) this.#fail();
    return value;
  }

  #value(): unknown {
    this.#whitespace();
    const token = this.#encoded[this.#offset];
    if (token === '"') return this.#string();
    if (token === "{") return this.#object();
    if (token === "[") return this.#array();
    if (token === "t") return this.#literal("true", true);
    if (token === "f") return this.#literal("false", false);
    if (token === "n") return this.#literal("null", null);
    if (token === "-" || (token !== undefined && token >= "0" && token <= "9")) {
      return this.#integer();
    }
    return this.#fail();
  }

  #object(): Record<string, unknown> {
    this.#offset += 1;
    const value: Record<string, unknown> = {};
    this.#whitespace();
    if (this.#take("}")) return value;
    for (;;) {
      this.#whitespace();
      if (this.#encoded[this.#offset] !== '"') this.#fail();
      const key = this.#string();
      this.#whitespace();
      if (!this.#take(":")) this.#fail();
      Object.defineProperty(value, key, {
        value: this.#value(),
        enumerable: true,
        configurable: true,
        writable: true,
      });
      this.#whitespace();
      if (this.#take("}")) return value;
      if (!this.#take(",")) this.#fail();
    }
  }

  #array(): unknown[] {
    this.#offset += 1;
    const value: unknown[] = [];
    this.#whitespace();
    if (this.#take("]")) return value;
    for (;;) {
      value.push(this.#value());
      this.#whitespace();
      if (this.#take("]")) return value;
      if (!this.#take(",")) this.#fail();
    }
  }

  #string(): string {
    const start = this.#offset;
    this.#offset += 1;
    let escaped = false;
    while (this.#offset < this.#encoded.length) {
      const token = this.#encoded[this.#offset];
      this.#offset += 1;
      if (escaped) {
        escaped = false;
      } else if (token === "\\") {
        escaped = true;
      } else if (token === '"') {
        return JSON.parse(this.#encoded.slice(start, this.#offset)) as string;
      }
    }
    return this.#fail();
  }

  #integer(): number | bigint {
    const start = this.#offset;
    if (this.#take("-")) {
      if (this.#offset === this.#encoded.length) this.#fail();
    }
    if (this.#take("0")) {
      const next = this.#encoded[this.#offset];
      if (next !== undefined && next >= "0" && next <= "9") this.#fail();
    } else {
      const first = this.#encoded[this.#offset];
      if (first === undefined || first < "1" || first > "9") this.#fail();
      this.#offset += 1;
      while (true) {
        const next = this.#encoded[this.#offset];
        if (next === undefined || next < "0" || next > "9") break;
        this.#offset += 1;
      }
    }
    const token = this.#encoded.slice(start, this.#offset);
    const integer = BigInt(token);
    const number = Number(integer);
    return Number.isSafeInteger(number) ? number : integer;
  }

  #literal<T>(encoded: string, value: T): T {
    if (!this.#encoded.startsWith(encoded, this.#offset)) this.#fail();
    this.#offset += encoded.length;
    return value;
  }

  #whitespace(): void {
    while (/\s/u.test(this.#encoded[this.#offset] ?? "") &&
           /[\u0009\u000a\u000d\u0020]/u.test(this.#encoded[this.#offset] ?? "")) {
      this.#offset += 1;
    }
  }

  #take(token: string): boolean {
    if (this.#encoded[this.#offset] !== token) return false;
    this.#offset += 1;
    return true;
  }

  #fail(): never {
    throw new SyntaxError(`invalid Hyphae JSON at byte-like offset ${this.#offset}`);
  }
}

function encode(value: unknown, active: Set<object>, inArray: boolean): string | undefined {
  if (value === null) return "null";
  switch (typeof value) {
    case "boolean":
      return value ? "true" : "false";
    case "string":
      return JSON.stringify(value);
    case "number":
      if (!Number.isSafeInteger(value)) {
        throw new TypeError("Hyphae JSON numbers must be safe integers; use bigint for larger integers");
      }
      return String(value);
    case "bigint":
      return value.toString(10);
    case "undefined":
      if (inArray) throw new TypeError("Hyphae JSON arrays cannot contain undefined");
      return undefined;
    case "object": {
      if (active.has(value)) throw new TypeError("Hyphae JSON cannot contain cycles");
      active.add(value);
      try {
        if (Array.isArray(value)) {
          return `[${value.map((item) => encode(item, active, true)).join(",")}]`;
        }
        const properties: string[] = [];
        for (const [key, child] of Object.entries(value)) {
          const encoded = encode(child, active, false);
          if (encoded !== undefined) properties.push(`${JSON.stringify(key)}:${encoded}`);
        }
        return `{${properties.join(",")}}`;
      } finally {
        active.delete(value);
      }
    }
    default:
      throw new TypeError(`unsupported Hyphae JSON value: ${typeof value}`);
  }
}
