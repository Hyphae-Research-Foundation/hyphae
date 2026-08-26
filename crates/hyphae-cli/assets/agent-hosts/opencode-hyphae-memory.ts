// SPDX-License-Identifier: Apache-2.0

import type { Plugin } from "@opencode-ai/plugin";
import { spawn } from "node:child_process";

const binary = "__HYPHAE_BINARY__";

async function hook(event: string, cwd: string, payload: object): Promise<any | undefined> {
  const child = spawn(binary, ["agent", "hook", "--host", "opencode"], {
    stdio: ["pipe", "pipe", "ignore"],
  });
  child.stdin.end(JSON.stringify({ event, cwd, ...payload }));
  const chunks: Buffer[] = [];
  child.stdout.on("data", (chunk) => chunks.push(chunk));
  const status = await new Promise<number | null>((resolve) => child.on("close", resolve));
  if (status !== 0) return undefined;
  return JSON.parse(Buffer.concat(chunks).toString("utf8"));
}

export const HyphaeMemory: Plugin = async ({ directory, client }) => ({
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
      type: "text",
      text: result.context,
      synthetic: true,
      metadata: { source: "hyphae-agent-memory" },
    } as any);
  },
  "tool.execute.after": async (input) => {
    await hook("tool-complete", directory, { tool: input.tool, args: input.args });
  },
  event: async ({ event }) => {
    if (event.type === "session.idle") {
      const response = await client.session.messages({ path: { id: event.properties.sessionID } });
      const messages = response.data ?? [];
      const latest = [...messages].reverse().find((message: any) => message.info?.role === "assistant");
      const text = latest?.parts
        ?.filter((part: any) => part.type === "text")
        .map((part: any) => part.text)
        .join("\n");
      await hook("session.idle", directory, {
        message: text,
        harness: "opencode-cli",
        model: latest?.info ? `${latest.info.providerID}/${latest.info.modelID}` : undefined,
      });
    }
  },
});
