// SPDX-License-Identifier: Apache-2.0

import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";
import { spawn } from "node:child_process";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { homedir } from "node:os";

type Config = {
  binary: string;
  endpoint: string;
  credential_file: string;
  allow_write: boolean;
};

const configHome = process.env.XDG_CONFIG_HOME ?? join(homedir(), ".config");
const config = JSON.parse(
  readFileSync(join(configHome, "hyphae/pi-agent-memory.json"), "utf8"),
) as Config;

let nextId = 2;

async function call(name: string, args: object): Promise<unknown> {
  const command = ["mcp", "--profile", "memory", "--endpoint", config.endpoint];
  if (config.allow_write) command.push("--allow-write");
  const child = spawn(config.binary, command, {
    env: { ...process.env, HYPHAE_NATIVE_API_KEY_FILE: config.credential_file },
    stdio: ["pipe", "pipe", "pipe"],
  });
  const id = nextId++;
  const messages = [
    { jsonrpc: "2.0", id: 1, method: "initialize", params: { protocolVersion: "2025-06-18", capabilities: {}, clientInfo: { name: "pi", version: "1" } } },
    { jsonrpc: "2.0", method: "notifications/initialized", params: {} },
    { jsonrpc: "2.0", id, method: "tools/call", params: { name, arguments: args } },
  ];
  child.stdin.end(messages.map((message) => JSON.stringify(message)).join("\n") + "\n");
  const stdout: Buffer[] = [];
  const stderr: Buffer[] = [];
  child.stdout.on("data", (chunk) => stdout.push(chunk));
  child.stderr.on("data", (chunk) => stderr.push(chunk));
  const status = await new Promise<number | null>((resolve, reject) => {
    child.on("error", reject);
    child.on("close", resolve);
  });
  if (status !== 0) throw new Error(Buffer.concat(stderr).toString("utf8") || `hyphae exited ${status}`);
  const response = Buffer.concat(stdout).toString("utf8").trim().split("\n").map((line) => JSON.parse(line)).find((message) => message.id === id);
  if (!response?.result?.structuredContent) throw new Error("Hyphae returned no structured content");
  return response.result.structuredContent;
}

function result(value: unknown) {
  return { content: [{ type: "text" as const, text: JSON.stringify(value, null, 2) }], details: value };
}

export default function (pi: ExtensionAPI) {
  async function proactive(event: string, payload: object): Promise<string | undefined> {
    const child = spawn(config.binary, ["agent", "hook", "--host", "pi"], {
      stdio: ["pipe", "pipe", "ignore"],
    });
    child.stdin.end(JSON.stringify({ event, cwd: process.cwd(), ...payload }));
    const chunks: Buffer[] = [];
    child.stdout.on("data", (chunk) => chunks.push(chunk));
    const status = await new Promise<number | null>((resolve) => child.on("close", resolve));
    if (status !== 0) return undefined;
    const value = JSON.parse(Buffer.concat(chunks).toString("utf8"));
    return typeof value.context === "string" ? value.context : undefined;
  }

  pi.on("before_agent_start", async (event) => {
    const context = await proactive("prompt", {
      prompt: event.prompt,
      harness: "pi-cli",
      model: ctx.model ? `${ctx.model.provider}/${ctx.model.id}` : undefined,
    });
    if (!context) return;
    return { systemPrompt: `${event.systemPrompt}\n\n${context}` };
  });
  pi.on("tool_result", async (event) => {
    if (event.isError) return;
    await proactive("tool-complete", { tool: event.toolName, args: event.input });
  });
  pi.on("agent_settled", async (_event, ctx) => {
    const entries = ctx.sessionManager.getBranch();
    const latest = [...entries].reverse().find((entry: any) => entry.type === "message" && entry.message?.role === "assistant") as any;
    const message = Array.isArray(latest?.message?.content)
      ? latest.message.content.filter((part: any) => part.type === "text").map((part: any) => part.text).join("\n")
      : latest?.message?.content;
    const provider = latest?.message?.provider;
    const model = latest?.message?.model;
    await proactive("agent.settled", {
      message,
      harness: "pi-cli",
      model: provider && model ? `${provider}/${model}` : undefined,
    });
  });

  pi.registerTool({
    name: "hyphae_memory_recall",
    label: "Recall Hyphae Memory",
    description: "Recall local Agent Memory for one project.",
    parameters: Type.Object({ project: Type.String(), query: Type.String(), limit: Type.Optional(Type.Number()), kind: Type.Optional(Type.String()), prove: Type.Optional(Type.Boolean()) }),
    async execute(_id, params) { return result(await call("hyphae_memory_recall", params)); },
  });
  pi.registerTool({
    name: "hyphae_memory_status",
    label: "Hyphae Memory Status",
    description: "Return redacted local Agent Memory status.",
    parameters: Type.Object({}),
    async execute(_id, params) { return result(await call("hyphae_memory_status", params)); },
  });
  if (config.allow_write) {
    pi.registerTool({
      name: "hyphae_memory_store",
      label: "Store Hyphae Memory",
      description: "Store one local project or global memory.",
      parameters: Type.Object({ project: Type.String(), text: Type.String(), kind: Type.Optional(Type.String()), scope: Type.Optional(Type.String()), agent: Type.Optional(Type.String()), ttl: Type.Optional(Type.Number()) }),
      async execute(_id, params) { return result(await call("hyphae_memory_store", params)); },
    });
    pi.registerTool({
      name: "hyphae_memory_journal",
      label: "Journal Hyphae Reflection",
      description: "Write one first-person model reflection with harness and model provenance, separate from work memory.",
      parameters: Type.Object({ project: Type.String(), text: Type.String(), harness: Type.String(), model: Type.String(), ttl: Type.Optional(Type.Number()) }),
      async execute(_id, params) { return result(await call("hyphae_memory_journal", params)); },
    });
    pi.registerTool({
      name: "hyphae_memory_forget",
      label: "Forget Hyphae Memory",
      description: "Permanently forget one memory owned by a project.",
      parameters: Type.Object({ project: Type.String(), id: Type.String() }),
      async execute(_id, params) { return result(await call("hyphae_memory_forget", params)); },
    });
  }
}
