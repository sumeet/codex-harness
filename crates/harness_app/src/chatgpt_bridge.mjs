// Private ChatGPT conversation compatibility worker.
//
// One process exists only for the lifetime of one send. Chromium is used for
// the official Sentinel browser challenge; authenticated network traffic and
// SSE draining stay here so secrets never appear in process arguments or logs.

import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import crypto from "node:crypto";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";

const require = createRequire(import.meta.url);
const playwrightRoot = process.env.HARNESS_PLAYWRIGHT_ROOT
  ?? "/opt/codex-desktop/resources/cua_node/lib/node_modules/playwright";
const { chromium } = require(playwrightRoot);
const sourceDirectory = path.dirname(fileURLToPath(import.meta.url));
const sentinelSource = fs.readFileSync(path.join(sourceDirectory, "chatgpt_sentinel.js"), "utf8");
const input = JSON.parse(fs.readFileSync(0, "utf8"));

function emit(value) {
  process.stdout.write(`${JSON.stringify(value)}\n`);
}

function authTokens() {
  const codexRoot = process.env.CODEX_HOME || path.join(os.homedir(), ".codex");
  const auth = JSON.parse(fs.readFileSync(path.join(codexRoot, "auth.json"), "utf8"));
  const tokens = auth?.tokens;
  if (!tokens?.access_token || !tokens?.account_id) throw new Error("Codex is not signed in with ChatGPT");
  return tokens;
}

const browserUserAgent =
  "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) "
  + "Chrome/136.0.0.0 Safari/537.36";

function baseHeaders(tokens, deviceId) {
  return {
    Authorization: `Bearer ${tokens.access_token}`,
    "ChatGPT-Account-Id": tokens.account_id,
    "OAI-Language": "en",
    "User-Agent": browserUserAgent,
    "oai-did": deviceId,
    originator: "Codex Browser",
    "sec-ch-ua": '"Chromium";v="136", "Google Chrome";v="136", "Not=A?Brand";v="24"',
    "sec-ch-ua-mobile": "?0",
    "sec-ch-ua-platform": '"Linux"',
    "X-OpenAI-Attach-Auth": "1",
    "X-OpenAI-Attach-Integrity-State": "1",
  };
}

async function checkedJson(response, operation) {
  if (!response.ok) {
    const body = await response.text();
    throw new Error(`${operation} returned HTTP ${response.status}: ${body.slice(0, 300)}`);
  }
  return response.json();
}

async function runSentinel(page, sentinelInput) {
  const payload = Buffer.from(JSON.stringify(sentinelInput)).toString("base64");
  const html = `<!doctype html><html><body></body><script type="module">${sentinelSource.replace(
    "__HARNESS_SENTINEL_INPUT__",
    payload,
  )}</script></html>`;
  await page.route(
    "http://harness.local/**",
    (route) => route.fulfill({ contentType: "text/html", body: html }),
    { times: 1 },
  );
  await page.goto("http://harness.local/sentinel");
  await page.waitForFunction(
    () => document.body.textContent.trim().startsWith("{"),
    null,
    { timeout: 5000 },
  );
  const result = JSON.parse(await page.textContent("body"));
  if (result.error) throw new Error(`ChatGPT integrity worker failed: ${result.error}`);
  return result;
}

async function drainSse(response) {
  const reader = response.body?.getReader();
  if (!reader) return;
  const decoder = new TextDecoder();
  let buffered = "";
  let dataLines = [];

  const dispatch = () => {
    if (dataLines.length === 0) return;
    const data = dataLines.join("\n");
    dataLines = [];
    if (data === "[DONE]") return;
    try {
      emit({ type: "event", data: JSON.parse(data) });
    } catch {
      // Comments, keep-alives, and future non-JSON event types are not
      // transcript content. The final canonical conversation remains the
      // source of truth after the stream ends.
    }
  };

  while (true) {
    const { done, value } = await reader.read();
    buffered += decoder.decode(value ?? new Uint8Array(), { stream: !done });
    const lines = buffered.split(/\r?\n/);
    buffered = done ? "" : lines.pop() ?? "";
    for (const line of lines) {
      if (line === "") dispatch();
      else if (line.startsWith("data:")) dataLines.push(line.slice(5).trimStart());
    }
    if (done) {
      if (buffered.startsWith("data:")) dataLines.push(buffered.slice(5).trimStart());
      dispatch();
      return;
    }
  }
}

async function main() {
  if (input?.request == null || typeof input.request !== "object") {
    throw new Error("ChatGPT bridge input is missing request");
  }
  const tokens = authTokens();
  const deviceId = typeof input.device_id === "string" && input.device_id.length > 0
    ? input.device_id
    : crypto.randomUUID();
  const headers = baseHeaders(tokens, deviceId);
  const browser = await chromium.launch({
    executablePath: process.env.HARNESS_CHROMIUM ?? "/usr/bin/chromium",
    headless: true,
    args: ["--disable-dev-shm-usage"],
  });
  try {
    const page = await browser.newPage();
    const { requirements_key: requirementsKey } = await runSentinel(page, {
      mode: "requirements-key",
    });
    const requirements = await checkedJson(
      await fetch("https://chatgpt.com/backend-api/sentinel/chat-requirements/prepare", {
        method: "POST",
        headers: { ...headers, "Content-Type": "application/json" },
        body: JSON.stringify({ p: requirementsKey }),
      }),
      "ChatGPT integrity preparation",
    );
    const integrity = await runSentinel(page, {
      mode: "solve",
      requirements_key: requirementsKey,
      requirements,
    });

    let conduitToken = null;
    try {
      const prepared = await fetch("https://chatgpt.com/backend-api/f/conversation/prepare", {
        method: "POST",
        headers: {
          ...headers,
          "Content-Type": "application/json",
          "x-conduit-token": input.conduit_token ?? "no-token",
        },
        body: JSON.stringify(input.request),
      });
      if (prepared.ok) conduitToken = (await prepared.json())?.conduit_token ?? null;
    } catch {
      // The official client treats conduit preparation as an optional latency
      // optimization and continues with the integrity headers when it fails.
    }

    const request = { ...input.request };
    request.client_prepare_state = conduitToken ? "success" : "failure";
    const streamHeaders = {
      ...headers,
      Accept: "text/event-stream",
      "Content-Type": "application/json",
    };
    if (requirements.token) {
      streamHeaders["OpenAI-Sentinel-Chat-Requirements-Token"] = requirements.token;
    } else if (requirements.prepare_token) {
      streamHeaders["OpenAI-Sentinel-Chat-Requirements-Prepare-Token"] = requirements.prepare_token;
    }
    if (integrity.proof) streamHeaders["OpenAI-Sentinel-Proof-Token"] = integrity.proof;
    if (integrity.turnstile) streamHeaders["OpenAI-Sentinel-Turnstile-Token"] = integrity.turnstile;
    if (conduitToken) streamHeaders["x-conduit-token"] = conduitToken;

    const response = await fetch("https://chatgpt.com/backend-api/f/conversation", {
      method: "POST",
      headers: streamHeaders,
      body: JSON.stringify(request),
    });
    if (!response.ok) {
      const body = await response.text();
      throw new Error(`ChatGPT send returned HTTP ${response.status}: ${body.slice(0, 300)}`);
    }
    emit({ type: "accepted" });
    await drainSse(response);
    emit({ type: "complete" });
  } finally {
    await browser.close();
  }
}

main().catch((error) => {
  emit({ type: "error", message: error instanceof Error ? error.message : String(error) });
  process.exitCode = 1;
});
