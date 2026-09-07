import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { createHash, randomUUID } from "node:crypto";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, realpathSync, watch, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { promisify } from "node:util";
import { NativeClient } from "./client.mjs";
import { discoverBinary } from "./discover.mjs";

const [nativeArgument, harnessArgument] = process.argv.slice(2);
assert.ok(nativeArgument && harnessArgument, "Usage: node lifecycle_probe.mjs EXACT_NATIVE_BINARY HARNESS_BINARY");
const native = realpathSync(nativeArgument);
const harness = resolve(harnessArgument);
const discovery = discoverBinary(native);
assert.ok(discovery.verified, "Use only an already verified native executable");
const execute = promisify(execFile);
const directory = mkdtempSync("/tmp/hl-");
const configuration = join(directory, "p");
const runtime = join(directory, "r");
const workspace = join(directory, "workspace");
for (const path of [configuration, runtime, workspace]) mkdirSync(path, { mode: 0o700 });
const environment = {
  PATH: process.env.PATH,
  LANG: "C.UTF-8",
  TERM: "xterm-256color",
  CLAUDE_CONFIG_DIR: configuration,
  XDG_RUNTIME_DIR: runtime,
  XDG_CONFIG_HOME: join(directory, "config"),
  XDG_DATA_HOME: join(directory, "data"),
  XDG_STATE_HOME: join(directory, "state"),
  HARNESS_CLAUDE_BINARY: native,
  DISABLE_AUTOUPDATER: "1",
  BROWSER: "/bin/false",
};
writeFileSync(join(configuration, "settings.json"), JSON.stringify({
  remoteControlAtStartup: false,
  env: { DISABLE_AUTOUPDATER: "1", CLAUDE_CODE_ARTIFACT_AUTO_OPEN: "0" },
}), { flag: "wx", mode: 0o600 });
writeFileSync(join(configuration, ".claude.json"), JSON.stringify({
  hasCompletedOnboarding: true, theme: "dark", projects: { [workspace]: { hasTrustDialogAccepted: true } },
}), { flag: "wx", mode: 0o600 });
const report = { directory, nativeHash: discovery.sha256, modelRequestsSent: 0, observations: [] };
const record = (step, value) => {
  report.observations.push({ step, ...value });
  console.log(JSON.stringify({ step, ...value }));
  writeFileSync(join(directory, "report.json"), JSON.stringify(report, null, 2), { mode: 0o600 });
};
const run = async (binary, arguments_) => {
  try {
    const result = await execute(binary, arguments_, { env: environment, cwd: workspace, timeout: 45000, maxBuffer: 4 * 1024 * 1024 });
    return { status: 0, ...result };
  } catch (error) {
    if (error.code === "ENOENT") throw error;
    return { status: error.code, signal: error.signal, stdout: error.stdout, stderr: error.stderr, error: error.message };
  }
};
const rows = async () => {
  const result = await run(native, ["agents", "--json", "--all"]);
  assert.equal(result.status, 0, result.stderr || result.error);
  const catalog = JSON.parse(result.stdout);
  assert.ok(Array.isArray(catalog));
  for (const row of catalog) assert.equal(row.cwd, workspace, "Refuse operations on another workspace");
  return catalog;
};
const endpointRoot = join(runtime, `harness-claude-${createHash("sha256").update(configuration).digest("hex").slice(0, 16)}`);
const connected = async (sessionId) => {
  const deadline = Date.now() + 20000;
  let lastError;
  while (Date.now() < deadline) {
    const row = (await rows()).find(value => value.sessionId === sessionId && value.pid);
    let client;
    try {
      assert.ok(row, "No live native row");
      const entry = JSON.parse(readFileSync(join(endpointRoot, "jobs", row.id, "current.json")));
      assert.equal(entry.pid, row.pid);
      client = new NativeClient(entry.socketPath);
      const snapshot = await client.request("snapshot", {}, 1500);
      assert.equal(snapshot.pid, row.pid);
      assert.equal(snapshot.sessionId, sessionId);
      assert.equal(snapshot.turn.submitCount, 0);
      assert.equal(snapshot.turn.isLoading, false);
      return { row, client, snapshot, socketPath: entry.socketPath };
    } catch (error) {
      client?.close();
      lastError = error.message;
      await delay(100);
    }
  }
  throw new Error(`Native snapshot unavailable: ${lastError}`);
};
const stop = async row => {
  assert.equal(row.cwd, workspace);
  assert.match(row.id, /^[a-f0-9]{8,64}$/);
  const result = await run(native, ["stop", row.id]);
  assert.equal(result.status, 0, result.stderr || result.error);
  const deadline = Date.now() + 10000;
  while ((await rows()).some(value => value.id === row.id && value.pid)) {
    assert.ok(Date.now() < deadline, `Test worker ${row.id} did not stop`);
    await delay(100);
  }
};

try {
  const setup = await run(harness, ["--claude-setup", "enable"]);
  assert.equal(setup.status, 0, setup.stderr || setup.error);
  const requestedId = randomUUID();
  const launchSettings = join(directory, `${requestedId}.json`);
  writeFileSync(launchSettings, "{}", { flag: "wx", mode: 0o600 });
  const arguments_ = ["--bg", "--name", "harness-empty-lifecycle-probe", "--session-id", requestedId, "--settings", launchSettings];
  const creation = await run(native, arguments_);
  record("create-without-prompt", { requestedId, ...creation });
  assert.equal(creation.status, 0, creation.stderr || creation.error);
  const initialRows = await rows();
  assert.equal(initialRows.length, 1);
  const initial = await connected(initialRows[0].sessionId);
  const nativeState = JSON.parse(readFileSync(join(configuration, "jobs", initial.row.id, "state.json")));
  assert.ok(nativeState.respawnFlags.includes(launchSettings));
  record("durable-creation-marker", { jobId: initial.row.id, conversation: initial.row.sessionId,
    settingsPath: launchSettings, respawnFlags: nativeState.respawnFlags });
  try {
    const secondClient = new NativeClient(initial.socketPath);
    try {
      const secondSnapshot = await secondClient.request("snapshot");
      assert.equal(secondSnapshot.pid, initial.snapshot.pid);
      assert.equal(secondSnapshot.epoch, initial.snapshot.epoch);
      record("two-clients-one-empty-worker", {
        sessionId: initial.snapshot.sessionId, requestedIdHonored: initial.snapshot.sessionId === requestedId,
        pid: initial.snapshot.pid, messages: initial.snapshot.messages.length,
      });
    } finally { secondClient.close(); }
  } finally { initial.client.close(); }

  // Repeat only in this disposable profile: the question is whether native creation is idempotent.
  const repeated = await run(native, arguments_);
  const repeatedRows = await rows();
  record("repeat-identical-create", { ...repeated, sessions: repeatedRows.map(row => ({ id: row.id, sessionId: row.sessionId, pid: row.pid })) });
  for (const row of repeatedRows.filter(row => row.id !== initial.row.id && row.pid)) await stop(row);
  await stop(initial.row);
  const transcript = join(configuration, "projects", workspace.replaceAll("/", "-"), `${initial.row.sessionId}.jsonl`);
  record("harness-stopped-empty-conversation", await run(harness, ["--claude-continue", initial.row.sessionId]));
  const wake = await run(native, ["--bg", "--resume", initial.row.sessionId]);
  record("resume-empty-stopped-worker", { historyFileExists: existsSync(transcript), ...wake });
  if (wake.status === 0) {
    const resumed = await connected(initial.row.sessionId);
    try { record("empty-history-restored", { pid: resumed.snapshot.pid, sessionId: resumed.snapshot.sessionId, messages: resumed.snapshot.messages.length }); }
    finally { resumed.client.close(); }
  }
  const throughHarness = await run(harness, ["--claude-continue", initial.row.sessionId]);
  record("harness-empty-conversation-continuation", throughHarness);

  const knownIds = new Set((await rows()).map(row => row.id));
  let published, watchError, killed = false, launcher;
  const jobs = join(configuration, "jobs");
  // Fail the caller after native state exists, before it can acknowledge creation.
  const watcher = watch(jobs, { recursive: true }, (_event, filename) => {
    if (published || !filename || !launcher?.pid) return;
    const match = /^([a-f0-9]{8})\/state\.json$/.exec(String(filename));
    if (!match || knownIds.has(match[1])) return;
    try {
      const state = JSON.parse(readFileSync(join(jobs, String(filename))));
      assert.equal(state.cwd, workspace);
      assert.equal(realpathSync(`/proc/${launcher.pid}/exe`), native);
      published = { jobId: match[1], sessionId: state.sessionId };
      killed = launcher.kill("SIGKILL");
    } catch (error) {
      if (error.code !== "ENOENT" && !(error instanceof SyntaxError)) watchError = error.message;
    }
  });
  watcher.on("error", error => { watchError = error.message; });
  let interrupted;
  try {
    interrupted = await new Promise(resolveResult => {
      launcher = execFile(native, ["--bg", "--name", "harness-interrupted-create-probe"],
        { env: environment, cwd: workspace, timeout: 20000, maxBuffer: 4 * 1024 * 1024 },
        (error, stdout, stderr) => resolveResult({ status: error ? error.code ?? null : 0, signal: error?.signal, stdout, stderr }));
    });
  } finally { watcher.close(); }
  assert.equal(watchError, undefined, watchError);
  assert.ok(killed && published, "Creation fault was not injected; no crash-recovery claim is valid");
  assert.equal(interrupted.signal, "SIGKILL");
  await delay(1000);
  let interruptedRow = (await rows()).find(row => row.sessionId === published.sessionId);
  record("caller-died-after-native-state-publication", { published, ...interrupted,
    discovered: interruptedRow && { id: interruptedRow.id, sessionId: interruptedRow.sessionId, pid: interruptedRow.pid, state: interruptedRow.state } });
  if (interruptedRow && !interruptedRow.pid) {
    record("recover-known-empty-job-after-caller-death", await run(native, ["--bg", "--resume", published.sessionId]));
    interruptedRow = (await rows()).find(row => row.sessionId === published.sessionId);
  }
  if (interruptedRow?.pid) {
    const recovered = await connected(published.sessionId);
    try { record("reconciled-without-repeating-create", { pid: recovered.snapshot.pid, sessionId: recovered.snapshot.sessionId, messages: recovered.snapshot.messages.length }); }
    finally { recovered.client.close(); }
  }
} finally {
  for (const row of (await rows()).filter(row => row.pid)) await stop(row);
  record("cleanup", { remainingLiveWorkers: (await rows()).filter(row => row.pid).length });
  console.log(`Private lifecycle evidence: ${directory}/report.json`);
}
