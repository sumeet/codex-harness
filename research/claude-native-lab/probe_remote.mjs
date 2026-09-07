import assert from "node:assert/strict";
import {
  readFileSync,
  writeFileSync,
  openSync,
  writeSync,
  closeSync,
  mkdtempSync,
  statSync,
  existsSync,
} from "node:fs";
import { homedir } from "node:os";
import { join, resolve, dirname } from "node:path";
import { pathToFileURL } from "node:url";
import { randomUUID } from "node:crypto";
import { setTimeout as delay } from "node:timers/promises";

const [confirmation, sessionId, workspaceArgument, webSocketModule] = process.argv.slice(2);
assert.ok(
  confirmation === "--live" && sessionId && workspaceArgument && webSocketModule,
  "Usage: node probe_remote.mjs --live OWN_DISPOSABLE_RC_SESSION_ID DISPOSABLE_WORKSPACE WS_MODULE_PATH (sends two model prompts)",
);
assert.match(sessionId, /^session_[A-Za-z\d]+$/);
const workspace = resolve(workspaceArgument);
assert.match(workspace, /^\/tmp\/harness-claude-(?:lab|compare)\.[^/]+\/workspace$/);
const parent = statSync(dirname(workspace));
assert.ok(parent.uid === process.getuid() && (parent.mode & 0o077) === 0, "Lab parent must be private");
const directory = mkdtempSync(join(dirname(workspace), "remote-check-"));
const filePath = join(workspace, `remote-denied-${randomUUID()}.txt`);
const { default: WebSocket } = await import(pathToFileURL(resolve(webSocketModule)).href);
const oauth = JSON.parse(readFileSync(join(homedir(), ".claude", ".credentials.json"), "utf8")).claudeAiOauth;
const account = JSON.parse(readFileSync(join(homedir(), ".claude.json"), "utf8")).oauthAccount;
assert.ok(oauth?.accessToken && account?.organizationUuid, "Requires existing normal CLI OAuth authentication");
const headers = { Authorization: `Bearer ${oauth.accessToken}`, "anthropic-version": "2023-06-01" };
const descriptor = openSync(join(directory, "frames.jsonl"), "wx", 0o600);
const received = [];
let bytes = 0;
let failure;
let denialSent = false;
let expectedDenial = false;
let completed = false;
function capture(direction, data) {
  const line = JSON.stringify({ time: Date.now(), direction, data }) + "\n";
  bytes += Buffer.byteLength(line);
  if (bytes > 8 * 1024 * 1024) throw new Error("Remote capture limit exceeded");
  writeSync(descriptor, line);
}
const socket = new WebSocket(
  `wss://api.anthropic.com/v1/sessions/ws/${sessionId}/subscribe?organization_uuid=${encodeURIComponent(account.organizationUuid)}`,
  { headers, handshakeTimeout: 10000, maxPayload: 8 * 1024 * 1024 },
);
function send(message) {
  capture("sent", message);
  socket.send(JSON.stringify(message));
}
socket.on("error", (error) => {
  failure = error;
});
socket.on("close", (code) => {
  if (!completed) failure = new Error(`Remote socket closed during checks (${code})`);
});
socket.on("message", (frame) => {
  try {
    const message = JSON.parse(String(frame));
    capture("received", message);
    if (received.length >= 1024) throw new Error("Too many remote events for a disposable check");
    received.push(message);
    if (message.type === "control_request" && message.request?.subtype === "initialize")
      send({
        type: "control_response",
        session_id: sessionId,
        response: {
          subtype: "success",
          request_id: message.request_id,
          response: {
            commands: [],
            output_style: "normal",
            available_output_styles: ["normal"],
            models: [],
            account: {},
            pid: process.pid,
          },
        },
      });
    if (
      expectedDenial &&
      message.type === "control_request" &&
      message.request?.subtype === "can_use_tool" &&
      message.request.tool_name === "Write" &&
      message.request.input?.file_path === filePath
    ) {
      send({
        type: "control_response",
        response: {
          subtype: "success",
          request_id: message.request_id,
          response: {
            behavior: "deny",
            message: "Intentional permission-denial test. Do not retry or work around this rejection.",
          },
        },
      });
      denialSent = true;
    }
  } catch (error) {
    failure = error;
    socket.close();
  }
});
async function wait(predicate) {
  const deadline = Date.now() + 45000;
  while (!predicate()) {
    if (failure) throw failure;
    if (Date.now() > deadline)
      throw new Error("Timed out; inspect the native terminal. No automatic approval or retry was attempted");
    await delay(100);
  }
}
async function prompt(text) {
  const first = received.length;
  const response = await fetch(`https://api.anthropic.com/v1/sessions/${sessionId}/events`, {
    method: "POST",
    headers: { ...headers, "Content-Type": "application/json", "anthropic-beta": "ccr-byoc-2025-07-29" },
    redirect: "error",
    signal: AbortSignal.timeout(12000),
    body: JSON.stringify({
      events: [
        {
          uuid: randomUUID(),
          session_id: sessionId,
          type: "user",
          parent_tool_use_id: null,
          message: { role: "user", content: text },
        },
      ],
    }),
  });
  if (!response.ok) throw new Error(`Remote prompt submission returned HTTP ${response.status}`);
  return first;
}
try {
  await wait(() => socket.readyState === WebSocket.OPEN);
  const marker = `REMOTE_NATIVE_${randomUUID()}`;
  let first = await prompt(`Reply exactly ${marker}. Do not use tools.`);
  // RC can emit an empty result before the assistant message containing the final text.
  await wait(() => {
    const events = received.slice(first);
    return (
      events.some((event) => event.type === "result") &&
      events.some(
        (event) =>
          event.type === "assistant" &&
          Array.isArray(event.message?.content) &&
          event.message.content.some((block) => block.type === "text" && block.text.includes(marker)),
      )
    );
  });
  console.log(JSON.stringify({ check: "native_reply_over_remote_control", passed: true }));
  expectedDenial = true;
  first = await prompt(
    `Use Write to create ${filePath} containing TEST. Use no other tool. This is a permission denial test: if rejected do not retry or work around the rejection.`,
  );
  await wait(() => denialSent && received.slice(first).some((event) => event.type === "result"));
  assert.ok(!existsSync(filePath), "Denied fixture was unexpectedly created");
  const report = {
    sessionId,
    directory,
    checksPassed: 2,
    denialSent,
    fileCreated: false,
    types: [...new Set(received.map((event) => event.type))],
    caveat:
      "This OAuth HTTP ingress was observed as peer-origin input in Claude 2.1.263; it is not established as equivalent to first-party user input.",
  };
  writeFileSync(join(directory, "report.json"), JSON.stringify(report, null, 2), { flag: "wx", mode: 0o600 });
  console.log(JSON.stringify(report));
  completed = true;
} finally {
  socket.terminate();
  closeSync(descriptor);
}
