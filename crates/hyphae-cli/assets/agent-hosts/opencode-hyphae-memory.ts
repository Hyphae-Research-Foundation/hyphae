// SPDX-License-Identifier: Apache-2.0

import type { Plugin } from "@opencode-ai/plugin";
import { spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { homedir } from "node:os";

const configuration = (() => {
  try { return JSON.parse(readFileSync(join(process.env.XDG_CONFIG_HOME ?? join(homedir(), ".config"), "hyphae/opencode-agent-memory.json"), "utf8")); }
  catch { return undefined; }
})();
const binary: string = configuration?.binary ?? "";

async function hook(event: string, cwd: string, payload: object): Promise<any | undefined> {
  if (!binary) return undefined;
  try {
    const input = JSON.stringify({ event, cwd, ...payload });
    if (Buffer.byteLength(input) > 1024 * 1024) return undefined;
    return await new Promise((resolve) => {
      const child = spawn(binary, ["agent", "hook", "--host", "opencode"], { stdio: ["pipe", "pipe", "ignore"] });
      const chunks: Buffer[] = [];
      let bytes = 0;
      let finished = false;
      const finish = (value?: any) => { if (!finished) { finished = true; clearTimeout(timer); resolve(value); } };
      const timer = setTimeout(() => { child.kill(); finish(); }, 1500);
      child.on("error", () => finish());
      child.stdin.on("error", () => finish());
      child.stdout.on("data", (chunk) => { bytes += chunk.length; if (bytes > 64 * 1024) { child.kill(); finish(); } else chunks.push(chunk); });
      child.on("close", (code) => {
        if (code !== 0) return finish();
        try { finish(JSON.parse(Buffer.concat(chunks).toString("utf8"))); } catch { finish(); }
      });
      child.stdin.end(input);
    });
  } catch { return undefined; }
}

export const HyphaeMemory: Plugin = async ({ directory, client }) => configuration ? ({
  config: async (config) => {
    const command = [binary, "mcp", "--profile", "memory", "--endpoint", configuration.endpoint];
    if (configuration.allow_write) command.push("--allow-write");
    config.mcp = { ...config.mcp, "hyphae-memory": { type: "local", command, enabled: true,
      environment: { HYPHAE_NATIVE_API_KEY_FILE: configuration.credential_file } } };
  },
  "chat.message": async (input, output) => {
    const prompt = output.parts
      .filter((part: any) => part.type === "text" && !part.synthetic)
      .map((part: any) => part.text)
      .join("\n");
    const result = await hook("prompt", directory, {
      prompt,
      harness: "opencode-cli",
      model: input.model ? `${input.model.providerID}/${input.model.modelID}` : undefined,
    });
    if (typeof result?.context !== "string") return;
    output.parts.push({
      id: "prt_" + randomUUID().replaceAll("-", ""),
      sessionID: input.sessionID,
      messageID: output.message.id,
      type: "text",
      text: result.context,
      synthetic: true,
      metadata: { source: "hyphae-agent-memory" },
    });
  },
  "tool.execute.after": async (input, output) => {
    await hook("tool-complete", directory, { tool: input.tool, args: input.args, success: (output.metadata as any)?.exit === 0 });
  },
  event: async ({ event }) => {
    if (event.type === "session.idle") {
      try {
      const response = await client.session.messages({ path: { id: event.properties.sessionID }, query: {limit: 16}, signal: AbortSignal.timeout(1200) });
      const messages = response.data ?? [];
      const latest = [...messages].reverse().find((message: any) => message.info?.role === "assistant");
      const text = latest?.parts
        ?.filter((part: any) => part.type === "text")
        .map((part: any) => part.text)
        .join("\n");
      await hook("session.idle", directory, {
        message: text,
        harness: "opencode-cli",
        model: latest?.info.role === "assistant" ? `${latest.info.providerID}/${latest.info.modelID}` : undefined,
      });
      } catch { /* Host events continue when memory is unavailable. */ }
    }
  },
}) : ({});
