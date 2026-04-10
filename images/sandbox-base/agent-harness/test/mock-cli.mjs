#!/usr/bin/env node

/**
 * Mock cli.js — speaks the Claude Code stream-json NDJSON protocol.
 *
 * For each "user" message on stdin it emits:
 *   1. system init event (first message only)
 *   2. assistant text event
 *   3. result event (terminal)
 *
 * Stays alive between messages (persistent subprocess, same as real cli.js).
 * Exits cleanly on stdin close / EOF.
 */

import { createInterface } from "node:readline";

const rl = createInterface({ input: process.stdin, crlfDelay: Infinity });
let messageCount = 0;
const SESSION_ID = "mock-session-001";

rl.on("line", (line) => {
  let msg;
  try {
    msg = JSON.parse(line);
  } catch {
    return; // skip non-JSON
  }
  if (msg.type !== "user") return;

  messageCount++;
  const prompt = msg.message?.content?.[0]?.text ?? "";

  // Emit system init on first message
  if (messageCount === 1) {
    console.log(
      JSON.stringify({
        type: "system",
        subtype: "init",
        session_id: SESSION_ID,
        model: "mock-model",
        tools: [],
        mcp_servers: [],
      }),
    );
  }

  // Emit assistant response
  console.log(
    JSON.stringify({
      type: "assistant",
      session_id: SESSION_ID,
      message: {
        role: "assistant",
        content: [{ type: "text", text: `Mock response to: ${prompt}` }],
      },
    }),
  );

  // Emit terminal result
  console.log(
    JSON.stringify({
      type: "result",
      subtype: "success",
      session_id: SESSION_ID,
      is_error: false,
      duration_ms: 100,
      total_cost_usd: messageCount * 0.001,
      result: `Mock response to: ${prompt}`,
    }),
  );
});

rl.on("close", () => process.exit(0));
