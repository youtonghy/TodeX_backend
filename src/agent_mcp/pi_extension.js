// Managed by TodeX (`crate::agent_mcp`). Loaded into `pi` via `-e` when one or
// more TodeX MCP servers (`todex_ssh`, `todex_desktop`) are enabled for the
// conversation. Registers tools directly with `pi.registerTool` so they remain
// exposed as first-class model tools even when third-party MCP adapters
// (e.g. `pi-mcp-adapter`) disable `builtin:mcp` or strip `exposure: "direct"`.

import { spawn } from "node:child_process";
import * as readline from "node:readline";

const SERVERS_ENV = "TODEX_AGENT_MCP_PI_SERVERS";
const PROTOCOL_VERSION = "2024-11-05";
const HANDSHAKE_TIMEOUT_MS = 10_000;
const DEFAULT_CALL_TIMEOUT_MS = 120_000;

function sanitizeSegment(value) {
  const cleaned = String(value ?? "")
    .replace(/[^A-Za-z0-9_]/g, "_")
    .replace(/_+/g, "_")
    .replace(/^_+|_+$/g, "");
  return cleaned || "unnamed";
}

function formatContentItem(item) {
  if (!item || typeof item !== "object") {
    return [];
  }
  if (item.type === "text" && typeof item.text === "string") {
    return [{ type: "text", text: item.text }];
  }
  if (
    item.type === "image" &&
    typeof item.data === "string" &&
    typeof item.mimeType === "string"
  ) {
    return [{ type: "image", data: item.data, mimeType: item.mimeType }];
  }
  if (item.type === "resource" && item.resource && typeof item.resource === "object") {
    const resource = item.resource;
    const header = resource.uri ? `[Resource: ${resource.uri}]` : "[Resource]";
    if (typeof resource.text === "string") {
      return [{ type: "text", text: `${header}\n${resource.text}` }];
    }
    if (typeof resource.blob === "string") {
      return [
        {
          type: "text",
          text: `${header}\n[Binary resource (${resource.mimeType ?? "application/octet-stream"})]`,
        },
      ];
    }
    return [{ type: "text", text: header }];
  }
  if (item.type === "resource_link") {
    const label = item.title ?? item.name ?? item.uri ?? "resource";
    return [{ type: "text", text: `[Resource link: ${label}] ${item.uri ?? ""}`.trim() }];
  }
  return [{ type: "text", text: JSON.stringify(item) }];
}

function formatToolResult(serverName, toolName, result) {
  const content = [];
  if (Array.isArray(result?.content)) {
    for (const item of result.content) {
      content.push(...formatContentItem(item));
    }
  }
  if (content.length === 0 && result?.structuredContent !== undefined) {
    content.push({
      type: "text",
      text: JSON.stringify(result.structuredContent, null, 2),
    });
  }
  if (content.length === 0) {
    content.push({ type: "text", text: "(no output)" });
  }
  return {
    content,
    details: {
      server: serverName,
      tool: toolName,
      structuredContent: result?.structuredContent,
    },
  };
}

class StdioMcpBridge {
  constructor(config) {
    this.config = config;
    this.child = null;
    this.rl = null;
    this.nextId = 1;
    this.pending = new Map();
    this.connecting = null;
    this.instructions = undefined;
    this.registeredTools = new Map();
  }

  async ensureConnected() {
    if (this.child && !this.child.killed && this.child.exitCode === null) {
      return;
    }
    if (this.connecting) {
      return this.connecting;
    }
    this.connecting = this._connect().finally(() => {
      this.connecting = null;
    });
    return this.connecting;
  }

  async _connect() {
    this.close();
    const child = spawn(this.config.command, this.config.args ?? [], {
      env: { ...process.env, ...(this.config.env ?? {}) },
      stdio: ["pipe", "pipe", "pipe"],
    });
    this.child = child;
    child.stderr?.on("data", () => {});

    const rl = readline.createInterface({
      input: child.stdout,
      crlfDelay: Infinity,
    });
    this.rl = rl;

    rl.on("line", (line) => {
      const trimmed = line.trim();
      if (!trimmed) {
        return;
      }
      let message;
      try {
        message = JSON.parse(trimmed);
      } catch {
        return;
      }
      if (message && (typeof message.id === "number" || typeof message.id === "string")) {
        const entry = this.pending.get(message.id);
        if (entry) {
          this.pending.delete(message.id);
          entry.cleanup();
          if (message.error) {
            const errText =
              typeof message.error.message === "string"
                ? message.error.message
                : JSON.stringify(message.error);
            entry.reject(new Error(errText));
          } else {
            entry.resolve(message.result);
          }
        }
      }
    });

    const onDisconnect = (err) => {
      if (this.child === child) {
        this.child = null;
        this.rl = null;
      }
      const reason = err instanceof Error ? err : new Error("TodeX MCP bridge closed");
      for (const [, entry] of this.pending) {
        entry.cleanup();
        entry.reject(reason);
      }
      this.pending.clear();
    };

    child.once("error", onDisconnect);
    child.once("exit", () => onDisconnect(new Error("TodeX MCP bridge exited")));

    const initResult = await this.sendRequest(
      "initialize",
      {
        protocolVersion: PROTOCOL_VERSION,
        capabilities: {},
        clientInfo: { name: "todex-pi-mcp", version: "1.0.0" },
      },
      { timeoutMs: HANDSHAKE_TIMEOUT_MS },
    );
    if (typeof initResult?.instructions === "string" && initResult.instructions.trim()) {
      this.instructions = initResult.instructions.trim();
    }
    this.sendNotification("notifications/initialized");
  }

  sendNotification(method, params) {
    if (!this.child?.stdin || this.child.stdin.destroyed) {
      return;
    }
    const payload = params !== undefined ? { jsonrpc: "2.0", method, params } : { jsonrpc: "2.0", method };
    try {
      this.child.stdin.write(`${JSON.stringify(payload)}\n`);
    } catch {
      // Ignore write errors on closing stream.
    }
  }

  sendRequest(method, params, options = {}) {
    if (!this.child?.stdin || this.child.stdin.destroyed) {
      return Promise.reject(new Error(`TodeX MCP server "${this.config.name}" is not connected`));
    }
    if (options.signal?.aborted) {
      return Promise.reject(new Error("Operation aborted"));
    }
    const id = this.nextId++;
    const timeoutMs = options.timeoutMs ?? this.config.timeoutMs ?? DEFAULT_CALL_TIMEOUT_MS;

    return new Promise((resolve, reject) => {
      let timer = null;
      const onAbort = () => {
        this.pending.delete(id);
        cleanup();
        this.sendNotification("notifications/cancelled", {
          requestId: id,
          reason: "Cancelled by user",
        });
        reject(new Error("Operation aborted"));
      };
      const cleanup = () => {
        if (timer !== null) {
          clearTimeout(timer);
          timer = null;
        }
        options.signal?.removeEventListener("abort", onAbort);
      };

      if (timeoutMs > 0) {
        timer = setTimeout(() => {
          this.pending.delete(id);
          cleanup();
          this.sendNotification("notifications/cancelled", {
            requestId: id,
            reason: "Request timed out",
          });
          reject(new Error(`MCP request "${method}" timed out after ${timeoutMs}ms`));
        }, timeoutMs);
      }
      options.signal?.addEventListener("abort", onAbort, { once: true });
      this.pending.set(id, { resolve, reject, cleanup });

      const request = { jsonrpc: "2.0", id, method, params };
      try {
        this.child.stdin.write(`${JSON.stringify(request)}\n`);
      } catch (err) {
        this.pending.delete(id);
        cleanup();
        reject(err instanceof Error ? err : new Error(String(err)));
      }
    });
  }

  async listTools() {
    await this.ensureConnected();
    const result = await this.sendRequest("tools/list", {}, { timeoutMs: HANDSHAKE_TIMEOUT_MS });
    return Array.isArray(result?.tools) ? result.tools : [];
  }

  async callTool(name, args, signal) {
    await this.ensureConnected();
    return this.sendRequest(
      "tools/call",
      { name, arguments: args ?? {} },
      { timeoutMs: this.config.timeoutMs ?? DEFAULT_CALL_TIMEOUT_MS, signal },
    );
  }

  async syncTools(pi) {
    const tools = await this.listTools();
    const serverSegment = sanitizeSegment(this.config.name);
    const namespaceName = `mcp__${serverSegment}`;
    const baseDescription = `Tools from MCP server "${this.config.name}".`;
    const namespaceDescription = this.instructions
      ? `${baseDescription}\n\n${this.instructions}`
      : baseDescription;
    const seen = new Set();

    for (const tool of tools) {
      if (!tool || typeof tool.name !== "string") {
        continue;
      }
      const fullName = `mcp__${serverSegment}__${sanitizeSegment(tool.name)}`;
      seen.add(fullName);
      const definition = {
        name: fullName,
        label: `${this.config.name}: ${tool.title ?? tool.name}`,
        description: tool.description ?? `Tool from MCP server ${this.config.name}.`,
        parameters: tool.inputSchema ?? { type: "object", properties: {} },
        namespace: {
          name: namespaceName,
          description: namespaceDescription,
        },
        exposure: "direct",
        execute: async (_toolCallId, params, signal) => {
          const result = await this.callTool(tool.name, params ?? {}, signal);
          const formatted = formatToolResult(this.config.name, tool.name, result);
          if (result?.isError) {
            const text = formatted.content
              .filter((item) => item.type === "text")
              .map((item) => item.text)
              .join("\n")
              .trim();
            throw new Error(text || `MCP tool "${tool.name}" returned an error.`);
          }
          return formatted;
        },
      };
      this.registeredTools.set(fullName, definition);
      pi.registerTool(definition);
    }

    for (const [fullName, previous] of this.registeredTools.entries()) {
      if (!seen.has(fullName) && previous.exposure !== "hidden") {
        const hidden = { ...previous, exposure: "hidden" };
        this.registeredTools.set(fullName, hidden);
        pi.registerTool(hidden);
      }
    }
  }

  close() {
    if (this.rl) {
      try {
        this.rl.close();
      } catch {}
      this.rl = null;
    }
    if (this.child) {
      const child = this.child;
      this.child = null;
      try {
        child.stdin?.end();
      } catch {}
      setTimeout(() => {
        if (child.exitCode === null && !child.killed) {
          try {
            child.kill();
          } catch {}
        }
      }, 250).unref?.();
    }
    for (const [, entry] of this.pending) {
      entry.cleanup();
      entry.reject(new Error("TodeX MCP bridge closed"));
    }
    this.pending.clear();
  }
}

export default function todexAgentMcpExtension(pi) {
  const raw = process.env[SERVERS_ENV];
  delete process.env[SERVERS_ENV];
  if (!raw) {
    return;
  }

  let serverConfigs;
  try {
    serverConfigs = JSON.parse(raw);
  } catch {
    return;
  }
  if (!Array.isArray(serverConfigs) || serverConfigs.length === 0) {
    return;
  }

  const bridges = serverConfigs
    .filter((entry) => entry && typeof entry.name === "string" && typeof entry.command === "string")
    .map((entry) => new StdioMcpBridge(entry));

  if (bridges.length === 0) {
    return;
  }

  const syncAll = async () => {
    await Promise.all(
      bridges.map(async (bridge) => {
        try {
          await bridge.syncTools(pi);
        } catch {
          // Keep the Pi session alive even if a bridge is unreachable.
        }
      }),
    );
  };

  const closeAll = () => {
    for (const bridge of bridges) {
      bridge.close();
    }
  };

  pi.on("session_start", async () => {
    await syncAll();
  });

  pi.on("before_agent_start", async () => {
    await syncAll();
  });

  pi.on("session_shutdown", async () => {
    closeAll();
  });

  process.once("exit", closeAll);
}
