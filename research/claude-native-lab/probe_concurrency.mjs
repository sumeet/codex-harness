import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
import { EventEmitter } from "node:events";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { setTimeout as delay } from "node:timers/promises";
import { performance } from "node:perf_hooks";
import { NativeClient } from "./client.mjs";
import { attach, dispose } from "./bridge.mjs";

const script = fileURLToPath(import.meta.url);

if (process.argv[2] === "--client") {
  const client = new NativeClient(process.argv[3]);
  try {
    const parameters = JSON.parse(process.argv[5]);
    const result = await client.request(process.argv[4], parameters);
    console.log(JSON.stringify({ ok: true, result }));
  } catch (error) {
    console.log(JSON.stringify({ ok: false, error: error.message }));
  } finally {
    client.close();
  }
} else {
  await probe();
}

function store(initial) {
  let value = initial;
  const events = new EventEmitter();
  return {
    getSnapshot: () => value,
    getState: () => value,
    subscribe(callback) {
      events.on("change", callback);
      return () => events.off("change", callback);
    },
    set(next) {
      value = next;
      events.emit("change");
    },
  };
}

function clientProcess(socket, method, parameters) {
  return new Promise((resolve, reject) => {
    const child = spawn(process.execPath, [script, "--client", socket, method, JSON.stringify(parameters)], {
      stdio: ["ignore", "pipe", "pipe"],
    });
    let output = "";
    let errors = "";
    const timeout = setTimeout(() => child.kill("SIGKILL"), 20000);
    child.stdout.on("data", (data) => { output += data; });
    child.stderr.on("data", (data) => { errors += data; });
    child.on("error", (error) => { clearTimeout(timeout); reject(error); });
    child.on("close", (code, signal) => {
      clearTimeout(timeout);
      if (code !== 0) { reject(new Error(`Client exited ${code}/${signal}: ${errors}`)); return; }
      try { resolve(JSON.parse(output)); } catch (error) { reject(error); }
    });
  });
}

async function probe() {
  const directory = mkdtempSync(join(tmpdir(), "harness-claude-concurrency-"));
  const socket = join(directory, "native.sock");
  process.env.HARNESS_CLAUDE_BRIDGE_SOCKET = socket;
  process.env.CLAUDE_CONFIG_DIR = directory;
  const queued = [];
  const resolved = [];
  const transcript = store([]);
  const dialogs = store({ open: [] });
  const dialogEvents = new EventEmitter();
  const turn = {
    ...store({ isLoading: false }),
    guard: { isActive: false },
    stream: { ...store({}), setUserInputOnProcessing() {} },
    applyEvent() {}, cancel() {}, markSubmit() {}, resetTiming() {},
    _host: {
      messageQueue: { enqueueReportingAdmission(command) { queued.push(command); return { admitted: true }; } },
      dialogStore: {
        getState: dialogs.getState,
        subscribe: dialogs.subscribe,
        onClosed(callback) { dialogEvents.on("closed", callback); return () => dialogEvents.off("closed", callback); },
        answer(id, result) {
          resolved.push({ id, result });
          dialogs.set({ open: dialogs.getState().open.filter((dialog) => dialog.id !== id) });
          dialogEvents.emit("closed", { id, type: "answered", result });
        },
        dismiss(id) { this.answer(id, { cancelled: true }); },
      },
    },
  };
  const handles = {
    sessionId: randomUUID(), agentId: randomUUID(), transcript, turn,
    scope: {
      store: store({ replBridgeEnabled: false, replBridgeConnected: false }),
      dialogTransport: {
        subscribe() { return () => {}; }, onUpdate() { return () => {}; }, onCancel() { return () => {}; },
      },
    },
  };
  const report = { kind: "offline real-socket/process probe with synthetic native stores", clients: 8, checks: {}, findings: {} };
  const clients = [];
  try {
    attach(handles);
    await delay(20);
    const observer = new NativeClient(socket);
    clients.push(observer);
    const snapshot = await observer.request("snapshot");
    const binding = { sessionId: snapshot.sessionId, epoch: snapshot.epoch };
    const submissionId = randomUUID();
    const duplicateResults = await Promise.all(Array.from({ length: 8 }, () =>
      clientProcess(socket, "prompt", { ...binding, text: "same logical submission", submissionId })));
    assert.equal(queued.length, 1);
    assert.equal(duplicateResults.filter((reply) => reply.ok && !reply.result.duplicate).length, 1);
    assert.equal(duplicateResults.filter((reply) => reply.result?.duplicate).length, 7);
    report.checks.crossProcessDuplicateSubmission = "one admission, seven duplicate acknowledgements";

    const distinctResults = await Promise.all(Array.from({ length: 8 }, (_, index) =>
      clientProcess(socket, "prompt", { ...binding, text: `distinct submission ${index}`, submissionId: randomUUID() })));
    assert.equal(queued.length, 9);
    assert.ok(distinctResults.every((reply) => reply.ok));
    report.checks.distinctSubmissions = "all eight admitted once";

    dialogs.set({ open: [{ id: "race-dialog", kind: "permission_file" }] });
    const approvals = await Promise.all(Array.from({ length: 8 }, (_, index) => clientProcess(socket, "dialog_reply", {
      ...binding, dialogId: "race-dialog", reply: { result: { behavior: index % 2 ? "deny" : "allow" } },
    })));
    assert.equal(resolved.length, 1);
    assert.equal(approvals.filter((reply) => reply.ok).length, 1);
    assert.ok(approvals.filter((reply) => !reply.ok).every((reply) => reply.error.includes("already resolved")));
    report.checks.racingApprovals = "one resolution, seven stale replies rejected";

    const subscribers = Array.from({ length: 8 }, () => new NativeClient(socket));
    clients.push(...subscribers);
    await Promise.all(subscribers.map((client) => client.request("snapshot")));
    const sequences = subscribers.map(() => []);
    subscribers.forEach((client, index) => client.on("event", (event) => {
      if (event.event === "engine_event") sequences[index].push(event.sequence);
    }));
    for (let index = 0; index < 250; index++) turn.applyEvent({ type: "test-event", index });
    await Promise.all(subscribers.map((client) => client.waitFor((event) => event.data?.index === 249)));
    for (const received of sequences) {
      assert.equal(received.length, 250);
      assert.deepEqual(received, sequences[0]);
      assert.equal(new Set(received).size, 250);
    }
    report.checks.broadcastOrdering = "eight subscribers received the same 250-event order";
    for (const client of subscribers) client.close();
    await delay(20);

    // Inject failure after native admission, where an RPC error cannot undo queueing.
    const uncertainId = randomUUID();
    const before = queued.length;
    turn.markSubmit = () => { throw new Error("injected post-admission failure"); };
    const admitted = await observer.request("prompt", { ...binding, text: "uncertain", submissionId: uncertainId });
    assert.equal(admitted.accepted, true);
    assert.match(admitted.warning, /post-admission/);
    turn.markSubmit = () => {};
    const retried = await observer.request("prompt", { ...binding, text: "uncertain", submissionId: uncertainId });
    assert.equal(retried.duplicate, true);
    assert.equal(retried.warning, admitted.warning);
    assert.equal(queued.length - before, 1);
    report.findings.postAdmissionFailure = { admissionsForOneId: queued.length - before, expectedForSafeRetry: 1 };

    const payload = Array.from({ length: 1024 }, (_, index) => ({ uuid: `message-${index}`, type: "assistant", message: { content: "x".repeat(1024) } }));
    report.findings.fullSnapshotFanout = [];
    for (const count of [1, 4, 8]) {
      const readers = Array.from({ length: count - 1 }, () => new NativeClient(socket));
      clients.push(...readers);
      await Promise.all(readers.map((client) => client.request("snapshot")));
      const started = performance.now();
      transcript.set(payload);
      const synchronousPublishMs = performance.now() - started;
      await delay(100);
      report.findings.fullSnapshotFanout.push({ clients: count, snapshotBytes: Buffer.byteLength(JSON.stringify(payload)), synchronousPublishMs });
      for (const reader of readers) reader.close();
      await delay(30);
    }

    // A fresh bridge instance has no in-memory ledger; only durable receipts remain.
    observer.close();
    await dispose();
    const replacement = await import(`./bridge.mjs?probe=${randomUUID()}`);
    try {
      replacement.attach(handles);
      await delay(20);
      const reconnected = new NativeClient(socket);
      clients.push(reconnected);
      const resumed = await reconnected.request("snapshot");
      const beforeRetry = queued.length;
      await assert.rejects(reconnected.request("prompt", { ...binding, text: "same logical submission", submissionId }), /adapter changed/);
      const receipt = await reconnected.request("prompt", { sessionId: resumed.sessionId, epoch: resumed.epoch, text: "same logical submission", submissionId });
      assert.equal(receipt.accepted, true);
      assert.equal(receipt.duplicate, true);
      assert.equal(queued.length, beforeRetry);
      report.findings.adapterRestart = { oldEpochRejected: true, retryUnderNewEpochAdmittedAgain: false, durableReceiptRecovered: true };
      reconnected.close();
    } finally { await replacement.dispose(); }
    console.log(JSON.stringify(report, null, 2));
  } finally {
    for (const client of clients) client.close();
    await dispose();
    rmSync(directory, { recursive: true, force: true });
  }
}
