// SPDX-License-Identifier: GPL-3.0-only

import { realpathSync } from "node:fs";
import { dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { defineConfig } from "vite";

const root = realpathSync(dirname(fileURLToPath(import.meta.url)));

export default defineConfig({
  root,
});
