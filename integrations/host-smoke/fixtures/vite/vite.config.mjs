// SPDX-License-Identifier: Apache-2.0

import { realpathSync } from "node:fs";
import { dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { defineConfig } from "vite";

const root = realpathSync(dirname(fileURLToPath(import.meta.url)));

export default defineConfig({
  root,
});
