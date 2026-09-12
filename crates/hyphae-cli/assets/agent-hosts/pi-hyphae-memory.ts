// SPDX-License-Identifier: Apache-2.0

import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";
import { spawn } from "node:child_process";
import { StringDecoder } from "node:string_decoder";
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
const config = (() => {
  try { return JSON.parse(readFileSync(join(configHome, "hyphae/pi-agent-memory.json"), "utf8")) as Config; }
  catch { return undefined; }
})();

let nextId = 2;

async function call(name: string, args: object, signal?: AbortSignal): Promise<unknown> {
  if (!config) throw new Error("Hyphae Memory is not configured");
  const command = ["mcp", "--profile", "memory", "--endpoint", config.endpoint];
  if (config.allow_write) command.push("--allow-write");
  const id = nextId++;
  const messages = [
    { jsonrpc: "2.0", id: 1, method: "initialize", params: { protocolVersion: "2025-06-18", capabilities: {}, clientInfo: { name: "pi", version: "1" } } },
    { jsonrpc: "2.0", method: "notifications/initialized", params: {} },
    { jsonrpc: "2.0", id, method: "tools/call", params: { name, arguments: args } },
  ];
  const input = messages.map((message) => JSON.stringify(message)).join("\n") + "\n";
  if (Buffer.byteLength(input) > 1024 * 1024 || signal?.aborted) throw new Error("Memory request cancelled or too large");
  const configured = config;
  return new Promise((resolve, reject) => {
    const child = spawn(configured.binary, command, {
      env: { ...process.env, HYPHAE_NATIVE_API_KEY_FILE: configured.credential_file },
      stdio: ["pipe", "pipe", "ignore"],
    });
    const chunks: Buffer[] = [];
    let bytes = 0;
    let done = false;
    let initialized = false;
    let incoming = "";
    const decoder = new StringDecoder("utf8");
    const finish = (error?: Error, value?: unknown) => {
      if (done) return;
      done = true; clearTimeout(timer); signal?.removeEventListener("abort", abort);
      if (error) { child.kill("SIGKILL"); reject(error); } else resolve(value);
    };
    const abort = () => finish(new Error("Memory request cancelled"));
    const timeout = (args as {prove?: boolean}).prove ? 120000 : 10000;
    const timer = setTimeout(() => finish(new Error("Memory request timed out")), timeout);
    signal?.addEventListener("abort", abort, {once:true});
    if (signal?.aborted) { abort(); return; }
    child.on("error", () => finish(new Error("Memory runtime unavailable")));
    child.stdin.on("error", () => finish(new Error("Memory runtime disconnected")));
    child.stdout.on("data", chunk => {
      bytes += chunk.length;
      if (bytes > 4 * 1024 * 1024) finish(new Error("Memory response exceeded its bound"));
      else if (!done) {
        chunks.push(chunk);
        incoming += decoder.write(chunk);
        let boundary;
        while ((boundary = incoming.indexOf("\n")) >= 0) {
          const line = incoming.slice(0,boundary); incoming = incoming.slice(boundary+1);
          try {
            const message = JSON.parse(line);
            if (message.id === 1 && !initialized) {
              if (message.error || !message.result) throw new Error();
              initialized = true;
              child.stdin.write(messages.slice(1).map(message => JSON.stringify(message)).join("\n")+"\n");
            } else if (message.id === id) {
              // EOF is cancellation in the MCP server. Close only after the
              // result arrives, so a completed write cannot lose its receipt.
              child.stdin.end();
            }
          } catch { finish(new Error("Memory initialization failed")); }
        }
      }
    });
    child.on("close", code => {
      if (code !== 0) return finish(new Error("Memory operation failed"));
      try {
        const response = Buffer.concat(chunks).toString("utf8").trim().split("\n").map(line => JSON.parse(line)).find(message => message.id === id);
        if (response?.error || response?.result?.isError || !response?.result?.structuredContent) throw new Error();
        finish(undefined, response.result.structuredContent);
      } catch { finish(new Error("Memory returned an invalid response")); }
    });
    child.stdin.write(JSON.stringify(messages[0])+"\n");
  });
}

function result(value: unknown) {
  return { content: [{ type: "text" as const, text: JSON.stringify(value, null, 2) }], details: value };
}

export default function (pi: ExtensionAPI) {
  if (!config) return;
  const configured = config;
  async function proactive(event: string, payload: object): Promise<string | undefined> {
    try {
      const input = JSON.stringify({ event, cwd: process.cwd(), ...payload });
      if (Buffer.byteLength(input) > 1024 * 1024) return undefined;
      return await new Promise((resolve) => {
        const child = spawn(configured.binary, ["agent", "hook", "--host", "pi"], {stdio:["pipe","pipe","ignore"]});
        const chunks: Buffer[] = [];
        let bytes = 0;
        let done = false;
        const finish = (value?: string) => { if (!done) { done = true; clearTimeout(timer); resolve(value); } };
        const timer = setTimeout(() => { child.kill(); finish(); }, 1500);
        child.on("error", () => finish()); child.stdin.on("error", () => finish());
        child.stdout.on("data", chunk => { bytes += chunk.length; if (bytes > 64 * 1024) { child.kill(); finish(); } else chunks.push(chunk); });
        child.on("close", code => {
          if (code !== 0) return finish();
          try { const value = JSON.parse(Buffer.concat(chunks).toString("utf8")); finish(typeof value.context === "string" ? value.context : undefined); }
          catch { finish(); }
        });
        child.stdin.end(input);
      });
    } catch { return undefined; }
  }

  pi.on("before_agent_start", async (event, ctx) => {
    const context = await proactive("prompt", {
      prompt: event.prompt,
      cwd: ctx.cwd,
      harness: "pi-cli",
      model: ctx.model ? `${ctx.model.provider}/${ctx.model.id}` : undefined,
    });
    if (!context) return;
    return { message: { customType: "hyphae-memory", content: context, display: false } };
  });
  pi.on("tool_result", async (event, ctx) => {
    if (event.isError) return;
    await proactive("tool-complete", { cwd: ctx.cwd, tool: event.toolName, args: event.input, success: true });
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
      cwd: ctx.cwd,
      harness: "pi-cli",
      model: provider && model ? `${provider}/${model}` : undefined,
    });
  });

  pi.registerTool({
    name: "hyphae_memory_recall",
    label: "Recall Hyphae Memory",
    description: "Recall local Agent Memory for one project.",
    parameters: Type.Object({ project: Type.Optional(Type.String()), query: Type.String(), limit: Type.Optional(Type.Number()), kind: Type.Optional(Type.String()), layer: Type.Optional(Type.String()), mode: Type.Optional(Type.String()), prove: Type.Optional(Type.Boolean()) }),
    async execute(_id, params, signal) { return result(await call("hyphae_memory_recall", params, signal)); },
  });
  pi.registerTool({
    name: "hyphae_memory_status",
    label: "Hyphae Memory Status",
    description: "Return redacted local Agent Memory status.",
    parameters: Type.Object({}),
    async execute(_id, params, signal) { return result(await call("hyphae_memory_status", params, signal)); },
  });
  if (config.allow_write) {
    pi.registerTool({
      name: "hyphae_memory_store",
      label: "Store Hyphae Memory",
      description: "Store one local project or global memory.",
      parameters: Type.Object({ project: Type.Optional(Type.String()), text: Type.String(), kind: Type.Optional(Type.String()), scope: Type.Optional(Type.String()), agent: Type.Optional(Type.String()), ttl: Type.Optional(Type.Number()) }),
      async execute(_id, params, signal) { return result(await call("hyphae_memory_store", params, signal)); },
    });
    pi.registerTool({
      name: "hyphae_memory_journal",
      label: "Journal Hyphae Reflection",
      description: "Write one first-person model reflection with harness and model provenance, separate from work memory.",
      parameters: Type.Object({ project: Type.Optional(Type.String()), text: Type.String(), harness: Type.String(), model: Type.String(), ttl: Type.Optional(Type.Number()) }),
      async execute(_id, params, signal) { return result(await call("hyphae_memory_journal", params, signal)); },
    });
    pi.registerTool({
      name: "hyphae_memory_forget",
      label: "Forget Hyphae Memory",
      description: "Remove one project memory from live recall; local backups remain preserved.",
      parameters: Type.Object({ project: Type.Optional(Type.String()), id: Type.String() }),
      async execute(_id, params, signal) { return result(await call("hyphae_memory_forget", params, signal)); },
    });
  }
}
