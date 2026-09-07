import assert from "node:assert/strict";
import { existsSync, mkdtempSync, readFileSync, writeFileSync, realpathSync, statSync } from "node:fs";
import { dirname, join } from "node:path";
import { randomUUID } from "node:crypto";
import { setTimeout as delay } from "node:timers/promises";
import { NativeClient } from "./client.mjs";

const [confirmation, socketPath, workspacePath] = process.argv.slice(2);
assert.equal(
  confirmation,
  "--live",
  "This uses the authenticated model. Pass --live SOCKET DISPOSABLE_WORKSPACE explicitly.",
);
assert.ok(socketPath && workspacePath);
const workspace = realpathSync(workspacePath);
assert.match(workspace, /^\/tmp\/harness-claude-(?:lab|supervisor)\.[^/]+\/workspace$/,
  "Live checks must stay in a disposable lab workspace");
const metadata = statSync(dirname(workspace));
assert.ok(metadata.uid === process.getuid() && (metadata.mode & 0o077) === 0,
  "The disposable lab must be private and owned by this user");
const fixtures = mkdtempSync(join(workspace, "checks-"));
const captures = mkdtempSync(join(dirname(socketPath), "checks-"));
let client = new NativeClient(socketPath, join(captures, "events.jsonl"));
const initial = await client.request("snapshot");
const checks = [];

async function begin(text, submissionId = randomUUID()) {
  const before = await client.request("snapshot");
  assert.equal(before.pid, initial.pid);
  assert.equal(before.turn.isLoading, false, "Previous turn is not finished");
  assert.equal(before.dialogs.length, 0, "A native dialog is already pending");
  const acknowledgment = await client.request("prompt", { text, submissionId });
  return { before, acknowledgment };
}

async function complete(before) {
  await client.waitFor(
    (event) =>
      event.sequence > before.sequence &&
      event.event === "turn" &&
      !event.data.isLoading &&
      event.data.lastQueryCompletionTime > before.turn.lastQueryCompletionTime,
    45000,
  );
  return client.request("snapshot");
}

async function fileDialog(before, filePath) {
  const event = await client.waitFor((event) => event.sequence > before.sequence && event.event === "dialog", 45000);
  assert.equal(event.data.kind, "permission_file");
  assert.equal(event.data.payload.toolName, "Write");
  assert.equal(event.data.payload.input.file_path, filePath);
  return event.data;
}

function passed(name, details = {}) {
  const check = { name, passed: true, ...details };
  checks.push(check);
  console.log(JSON.stringify(check));
}

try {
  assert.equal(initial.remoteControl.enabled, false);
  assert.equal(initial.remoteControl.connected, false);

  const deniedPath = join(fixtures, "must-not-exist.txt");
  {
    const { before } = await begin(
      `Use Write to create ${deniedPath} containing DENIAL_TEST. Do not use any other tool. This is a permission-denial test: if the host rejects it, do not retry or work around the rejection.`,
    );
    const dialog = await fileDialog(before, deniedPath);
    await client.request("dialog_reply", {
      dialogId: dialog.id,
      reply: { result: { behavior: "deny", feedback: "Intentional integration test. Do not retry this write." } },
    });
    await complete(before);
    assert.equal(existsSync(deniedPath), false);
    await assert.rejects(
      client.request("dialog_reply", { dialogId: dialog.id, reply: { result: { behavior: "allow" } } }),
      /resolved dialog/,
    );
    passed("deny_write_and_reject_stale_approval");
  }

  const modifiedPath = join(fixtures, "modified-by-approval.txt");
  {
    const { before } = await begin(
      `Use Write to create ${modifiedPath} containing ORIGINAL_INPUT followed by a newline. Do not use any other tool. Then reply DONE.`,
    );
    const dialog = await fileDialog(before, modifiedPath);
    await client.request("dialog_reply", {
      dialogId: dialog.id,
      reply: {
        result: { behavior: "allow", updatedInput: { ...dialog.payload.input, content: "HOST_MODIFIED_INPUT\n" } },
      },
    });
    await complete(before);
    assert.equal(readFileSync(modifiedPath, "utf8"), "HOST_MODIFIED_INPUT\n");
    passed("approve_with_modified_tool_input");
  }

  {
    const text =
      "Reply with the numbers 1 through 30 separated by spaces, then STREAM_TEST_COMPLETE. Do not use tools.";
    const submissionId = randomUUID();
    const { before } = await begin(text, submissionId);
    const duplicate = await client.request("prompt", { text, submissionId });
    assert.equal(duplicate.duplicate, true);
    await assert.rejects(client.request("prompt", { text: "Different text", submissionId }), /different text/);
    const after = await complete(before);
    assert.equal(
      after.messages.filter((message) => message.type === "user" && message.uuid === submissionId).length,
      1,
    );
    const deltas = client.events.filter(
      (event) =>
        event.sequence > before.sequence &&
        event.event === "engine_event" &&
        event.data.type === "stream_event" &&
        event.data.event?.delta?.type === "text_delta",
    );
    assert.ok(deltas.length > 0, "No live text deltas captured");
    passed("streaming_and_retry_deduplication", { deltaCount: deltas.length });
  }

  {
    const interruptedPath = join(fixtures, "interrupted-before-approval.txt");
    const { before } = await begin(
      `Use Write to create ${interruptedPath} containing INTERRUPT_TEST. Do not use any other tool. This is an interruption test: if interrupted or denied, do not retry.`,
    );
    const dialog = await fileDialog(before, interruptedPath);
    const pending = await client.request("snapshot");
    assert.equal(pending.turn.isLoading, true, "Native turn must still be waiting for permission before disconnect");
    assert.ok(pending.dialogs.some((candidate) => candidate.id === dialog.id));
    client.close();
    await delay(500);
    client = new NativeClient(socketPath, join(captures, "reconnected.jsonl"));
    const resumed = await client.request("snapshot");
    assert.equal(resumed.pid, initial.pid);
    assert.equal(resumed.sessionId, initial.sessionId);
    assert.equal(resumed.turn.isLoading, true, "Disconnect unexpectedly ended the native turn");
    assert.ok(
      resumed.dialogs.some((candidate) => candidate.id === dialog.id),
      "Pending permission was lost during reconnect",
    );
    const started = Date.now();
    await client.request("interrupt");
    await client.waitFor((event) => event.event === "turn" && !event.data.isLoading, 15000);
    const after = await client.request("snapshot");
    assert.equal(after.pid, initial.pid);
    assert.equal(after.dialogs.length, 0);
    assert.equal(existsSync(interruptedPath), false);
    passed("disconnect_reconnect_pending_permission_and_interrupt_same_process", {
      pid: after.pid,
      interruptMilliseconds: Date.now() - started,
    });
  }
} catch (error) {
  checks.push({ passed: false, error: String(error) });
  try {
    await client.request("interrupt");
  } catch (interruptError) {
    console.error(`Could not interrupt failed check: ${interruptError}`);
  }
  process.exitCode = 1;
} finally {
  client.close();
  const report = {
    pid: initial.pid,
    sessionId: initial.sessionId,
    remoteControl: initial.remoteControl,
    captures,
    fixtures,
    checks,
  };
  writeFileSync(join(captures, "report.json"), JSON.stringify(report, null, 2), { mode: 0o600, flag: "wx" });
  console.log(JSON.stringify({ report: join(captures, "report.json"), checks }));
}
