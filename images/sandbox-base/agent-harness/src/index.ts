#!/usr/bin/env node

/**
 * Agent Harness — persistent Claude Code subprocess with HTTP interface
 *
 * Bypasses the Claude Agent SDK's per-message process spawning by keeping
 * a single cli.js process alive.  Uses stdin/stdout stream-json protocol.
 *
 * Endpoints:
 *   POST /message  — JSON body: { "prompt": "...", "session_id": "...", ... }
 *                    Response: SSE stream of Claude Code events
 *   GET  /health   — 200 OK with JSON status
 *
 * Environment:
 *   ANTHROPIC_API_KEY — required
 *   HARNESS_PORT      — optional (default 3000)
 */

import { spawn, type ChildProcess } from "node:child_process";
import { createServer, type IncomingMessage, type ServerResponse } from "node:http";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createInterface, type Interface } from "node:readline";
import { mkdirSync, writeFileSync, existsSync, readFileSync } from "node:fs";

// ── Constants ─────────────────────────────────────────────────────────

const CWD = "/workspace";
const DEFAULT_MODEL = "claude-sonnet-4-6";
const DEFAULT_MAX_TURNS = 10;
const PORT = parseInt(process.env.HARNESS_PORT ?? "3000", 10);

const __filename = fileURLToPath(import.meta.url);
const __dirname = dirname(__filename);
const CLI_JS_PATH = join(__dirname, "sdk", "cli.js");
const SESSION_FILE = join(CWD, ".claude", ".harness-session-id");

// ── Types ──────────────────────────────────────────────────────────────

interface McpHttpServerConfig {
  type: "http";
  url: string;
  headers?: Record<string, string>;
}

interface HarnessInput {
  prompt: string;
  session_id?: string;
  model?: string;
  max_turns?: number;
  disallowed_tools?: string[];
  /** MCP server configurations keyed by server name. */
  mcp_servers?: Record<string, McpHttpServerConfig>;
}

// ── Helpers ────────────────────────────────────────────────────────────

function sendSSE(
  res: ServerResponse,
  event: Record<string, unknown>,
): void {
  const eventType = (event.type as string) ?? "message";
  res.write(`event: ${eventType}\ndata: ${JSON.stringify(event)}\n\n`);
}

function readBody(req: IncomingMessage): Promise<string> {
  return new Promise((resolve, reject) => {
    const chunks: Buffer[] = [];
    req.on("data", (chunk: Buffer) => chunks.push(chunk));
    req.on("end", () => resolve(Buffer.concat(chunks).toString()));
    req.on("error", reject);
  });
}

// ── Session persistence ──────────────────────────────────────────────

function loadSessionId(): string | undefined {
  try {
    if (existsSync(SESSION_FILE)) {
      return readFileSync(SESSION_FILE, "utf-8").trim() || undefined;
    }
  } catch {
    /* ignore */
  }
  return undefined;
}

function saveSessionId(id: string): void {
  try {
    mkdirSync(dirname(SESSION_FILE), { recursive: true });
    writeFileSync(SESSION_FILE, id);
  } catch (err) {
    console.error("Failed to save session ID:", err);
  }
}

// ── MCP settings ─────────────────────────────────────────────────────

function writeMcpSettings(
  mcpServers: Record<string, McpHttpServerConfig>,
): void {
  const settingsDir = join(CWD, ".claude");
  mkdirSync(settingsDir, { recursive: true });
  const settingsPath = join(settingsDir, "settings.json");

  let existing: Record<string, unknown> = {};
  try {
    if (existsSync(settingsPath)) {
      existing = JSON.parse(readFileSync(settingsPath, "utf-8"));
    }
  } catch {
    /* start fresh */
  }
  existing.mcpServers = mcpServers;
  writeFileSync(settingsPath, JSON.stringify(existing, null, 2));
}

// ── Persistent CLI subprocess ────────────────────────────────────────

let cliProcess: ChildProcess | null = null;
let stdoutRL: Interface | null = null;
let activeSessionId: string | undefined;
let busy = false;
let previousCostUsd = 0;

/** Config used for the last spawn — used to detect config changes. */
let lastSpawnModel: string | undefined;
let lastSpawnMcpHash: string | undefined;

function mcpHash(servers?: Record<string, McpHttpServerConfig>): string {
  if (!servers || Object.keys(servers).length === 0) return "";
  return JSON.stringify(servers);
}

/** Callback for the in-flight message — receives each event. */
let onEvent: ((event: Record<string, unknown>) => void) | null = null;
/** Called when a terminal event (result/error) arrives. */
let onComplete: (() => void) | null = null;

interface SpawnConfig {
  model: string;
  maxTurns: number;
  mcpServers?: Record<string, McpHttpServerConfig>;
  resume?: string;
}

function spawnCli(config: SpawnConfig): ChildProcess {
  // Write MCP settings before spawning so cli.js picks them up
  if (config.mcpServers && Object.keys(config.mcpServers).length > 0) {
    writeMcpSettings(config.mcpServers);
  }

  const args = [
    CLI_JS_PATH,
    "--output-format",
    "stream-json",
    "--input-format",
    "stream-json",
    "--verbose",
    "--model",
    config.model,
    "--max-turns",
    String(config.maxTurns),
    "--permission-mode", "bypassPermissions",
    "--allow-dangerously-skip-permissions",
  ];

  if (config.resume) {
    args.push("--resume", config.resume);
  }

  // Track spawn config for change detection
  lastSpawnModel = config.model;
  lastSpawnMcpHash = mcpHash(config.mcpServers);

  console.log(`Spawning cli.js (resume=${config.resume ?? "none"}, model=${config.model})`);

  const proc = spawn(process.execPath, args, {
    cwd: CWD,
    stdio: ["pipe", "pipe", "pipe"],
    env: {
      ...process.env,
      DISABLE_AUTOUPDATER: "1",
    },
  });

  proc.stderr?.on("data", (data: Buffer) => {
    process.stderr.write(`[cli] ${data.toString()}`);
  });

  proc.on("exit", (code, signal) => {
    console.error(`cli.js exited: code=${code} signal=${signal}`);
    cliProcess = null;
    stdoutRL?.close();
    stdoutRL = null;

    // If a message was in flight, signal failure
    if (onComplete) {
      onEvent?.({
        type: "error",
        message: "cli_crashed",
        details: `Process exited: code=${code} signal=${signal}`,
      });
      onComplete();
    }
  });

  // Parse stdout as newline-delimited JSON (stream-json output)
  if (proc.stdout) {
    const rl = createInterface({ input: proc.stdout, crlfDelay: Infinity });
    stdoutRL = rl;

    rl.on("line", (line) => {
      if (!line.trim()) return;

      let event: Record<string, unknown>;
      try {
        event = JSON.parse(line);
      } catch {
        return; // skip non-JSON lines (e.g. startup banners)
      }

      // Capture session_id and convert cumulative cost to per-message delta
      if (event.type === "result") {
        if (typeof event.session_id === "string") {
          activeSessionId = event.session_id;
          saveSessionId(event.session_id);
        }
        if (typeof event.total_cost_usd === "number") {
          const cumulativeCost = event.total_cost_usd;
          event.total_cost_usd = Math.max(0, cumulativeCost - previousCostUsd);
          previousCostUsd = cumulativeCost;
        }
      }

      // Forward to the in-flight message handler
      onEvent?.(event);

      // Terminal events mark end of a message cycle
      if (event.type === "result" || event.type === "error") {
        const completeFn = onComplete;
        onEvent = null;
        onComplete = null;

        // Claude Code stops reading stdin after a result — kill and
        // respawn with --resume on the next message.
        if (proc.exitCode === null) {
          proc.kill("SIGTERM");
        }
        cliProcess = null;
        stdoutRL?.close();
        stdoutRL = null;

        completeFn?.();
      }
    });
  }

  return proc;
}

/** Ensure a cli.js process is running, spawning or reusing as needed. */
function ensureCli(input: HarnessInput): ChildProcess {
  const model = input.model ?? DEFAULT_MODEL;
  const maxTurns = input.max_turns ?? DEFAULT_MAX_TURNS;
  const currentMcpHash = mcpHash(input.mcp_servers);

  // Kill existing process if config changed (model or MCP servers)
  if (cliProcess && cliProcess.exitCode === null) {
    if (model !== lastSpawnModel || currentMcpHash !== lastSpawnMcpHash) {
      console.log(
        `Config changed (model: ${lastSpawnModel}->${model}, mcp: ${lastSpawnMcpHash !== currentMcpHash}), respawning`,
      );
      cliProcess.kill("SIGTERM");
      cliProcess = null;
      stdoutRL?.close();
      stdoutRL = null;
    } else {
      return cliProcess;
    }
  }

  const resume = activeSessionId ?? loadSessionId();

  cliProcess = spawnCli({
    model,
    maxTurns,
    mcpServers: input.mcp_servers,
    resume: resume,
  });

  return cliProcess;
}

// ── Core message handler ──────────────────────────────────────────────

async function handleMessage(
  input: HarnessInput,
  emit: (event: Record<string, unknown>) => void,
): Promise<void> {
  const proc = ensureCli(input);

  if (!proc.stdin || !proc.stdin.writable) {
    emit({
      type: "error",
      message: "cli_error",
      details: "stdin not writable",
    });
    return;
  }

  return new Promise<void>((resolve) => {
    onEvent = emit;
    onComplete = () => {
      onEvent = null;
      onComplete = null;
      resolve();
    };

    // Write user message as JSON line to cli.js stdin
    const msg = JSON.stringify({
      type: "user",
      session_id: "",
      message: {
        role: "user",
        content: [{ type: "text", text: input.prompt }],
      },
      parent_tool_use_id: null,
    });
    proc.stdin!.write(msg + "\n");
  });
}

// ── HTTP request handler ──────────────────────────────────────────────

async function handleRequest(
  req: IncomingMessage,
  res: ServerResponse,
): Promise<void> {
  // Health check
  if (req.method === "GET" && req.url === "/health") {
    res.writeHead(200, { "Content-Type": "application/json" });
    res.end(
      JSON.stringify({
        status: "ok",
        session_id: activeSessionId ?? null,
        busy,
        cli_alive: cliProcess !== null && cliProcess.exitCode === null,
      }),
    );
    return;
  }

  // Message endpoint
  if (req.method === "POST" && req.url === "/message") {
    if (busy) {
      res.writeHead(409, { "Content-Type": "application/json" });
      res.end(JSON.stringify({ error: "already processing a message" }));
      return;
    }

    let body: string;
    try {
      body = await readBody(req);
    } catch {
      res.writeHead(400, { "Content-Type": "application/json" });
      res.end(JSON.stringify({ error: "failed to read request body" }));
      return;
    }

    let input: HarnessInput;
    try {
      input = JSON.parse(body) as HarnessInput;
    } catch {
      res.writeHead(400, { "Content-Type": "application/json" });
      res.end(JSON.stringify({ error: "invalid JSON" }));
      return;
    }

    if (!input.prompt) {
      res.writeHead(400, { "Content-Type": "application/json" });
      res.end(JSON.stringify({ error: "missing prompt" }));
      return;
    }

    // SSE response headers
    res.writeHead(200, {
      "Content-Type": "text/event-stream",
      "Cache-Control": "no-cache",
      Connection: "keep-alive",
    });

    busy = true;
    try {
      await handleMessage(input, (event) => sendSSE(res, event));
    } finally {
      busy = false;
    }
    res.end();
    return;
  }

  res.writeHead(404, { "Content-Type": "application/json" });
  res.end(JSON.stringify({ error: "not found" }));
}

// ── Server bootstrap ──────────────────────────────────────────────────

if (!process.env.ANTHROPIC_API_KEY) {
  console.error("ANTHROPIC_API_KEY environment variable is required");
  process.exit(1);
}

const server = createServer((req, res) => {
  handleRequest(req, res).catch((err) => {
    console.error("Unhandled error in request handler:", err);
    if (!res.headersSent) {
      res.writeHead(500, { "Content-Type": "application/json" });
    }
    res.end(JSON.stringify({ error: String(err) }));
  });
});

server.listen(PORT, "0.0.0.0", () => {
  console.log(`agent-harness listening on 0.0.0.0:${PORT} (SDK bypass mode)`);
});
