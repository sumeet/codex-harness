import assert from "node:assert/strict";
import { test } from "node:test";
import { chmodSync, mkdtempSync, mkdirSync, readFileSync, readlinkSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";
import { launchRole, prepareWrappedLaunch, readExperiment, workerEnvironment, processStartTicks, processIdentity } from "./supervisor_wrapper.mjs";
import { publishSocketAddress } from "./supervisor_preload.mjs";
import { observeRow, validateBinding, privateEntry } from "./supervisor_catalog.mjs";
import { randomUUID } from "node:crypto";

function withExperiment(body) {
  const directory = mkdtempSync("/tmp/harness-claude-supervisor.");
  const workspace = join(directory, "workspace");
  mkdirSync(workspace, { mode: 0o700 });
  const configuration = { directory, workspace };
  writeFileSync(join(directory, "experiment.json"), JSON.stringify(configuration), { mode: 0o600, flag: "wx" });
  try { body(configuration); }
  finally { rmSync(directory, { recursive: true }); }
}

test("native launcher roles include unassigned warm standbys", () => {
  const worker = { CLAUDE_CODE_SESSION_KIND: "bg", CLAUDE_JOB_DIR: "/native/jobs/12345678" };
  assert.equal(launchRole([], worker), "worker");
  assert.equal(launchRole(["--bg-spare"], {}), "standby");
  assert.equal(launchRole(["--bg-pty-host"], worker), "terminal-host");
  assert.equal(launchRole(["daemon", "run"], worker), "supervisor");
  assert.equal(launchRole(["attach", "12345678"], {}), "helper");
  assert.equal(launchRole([], { CLAUDE_CODE_SESSION_KIND: "bg" }), "helper");
});

test("worker bootstrap preserves inherited settings and rejects hook conflicts or unsupported builds", () => {
  const inherited = { SESSION_TEST_TOKEN: "private-test-value", CLAUDE_CODE_PROCESS_WRAPPER: "existing-wrapper", PATH: "/bin" };
  const discovery = { verified: true, sha256: "test-only" };
  const result = workerEnvironment("/private/experiment", "/private/workspace", discovery, inherited);
  for (const [name, value] of Object.entries(inherited)) assert.equal(result[name], value);
  assert.equal(inherited.BUN_OPTIONS, undefined);
  assert.match(result.BUN_OPTIONS, /^--preload \/.*\/supervisor_preload\.mjs$/);
  assert.deepEqual(JSON.parse(result.HARNESS_CLAUDE_SUPERVISOR_BOOTSTRAP), {
    directory: "/private/experiment", workspace: "/private/workspace", discovery,
  });
  assert.throws(() => workerEnvironment("", "", discovery, { BUN_OPTIONS: "--preload foreign.mjs" }), /composed explicitly/);
  assert.throws(() => workerEnvironment("", "", { verified: false, sha256: "unknown" }, {}), /Unsupported/);
});

test("helper wrapping retains argv and environment without logging their contents", () => withExperiment(({ directory }) => {
  const command = ["/unused/native", "attach", "test-job", "argument with spaces"];
  const environment = { SESSION_TEST_TOKEN: "private-test-value", BUN_OPTIONS: "foreign-hook" };
  const result = prepareWrappedLaunch(directory, command, environment);
  assert.deepEqual(result.command, command);
  assert.deepEqual(result.environment, environment);
  const log = readFileSync(join(directory, "launches.jsonl"), "utf8");
  assert.ok(!log.includes("private-test-value"));
  assert.ok(!log.includes("argument with spaces"));
  assert.equal(JSON.parse(log).role, "helper");
}));

test("wrapper exec preserves the PID tracked by the native supervisor", () => withExperiment(({ directory }) => {
  const wrapper = fileURLToPath(new URL("supervisor_wrapper.mjs", import.meta.url));
  const result = spawnSync(process.execPath, ["--no-warnings", wrapper, directory, process.execPath, "-e",
    "process.stdout.write(JSON.stringify({pid: process.pid, value: process.env.SESSION_TEST_TOKEN, args: process.argv.slice(1)}))",
    "argument with spaces"], { env: { SESSION_TEST_TOKEN: "private-test-value" }, encoding: "utf8", timeout: 5000 });
  assert.equal(result.error, undefined);
  assert.equal(result.status, 0, result.stderr);
  assert.equal(result.stderr, "");
  const output = JSON.parse(result.stdout);
  assert.equal(output.pid, result.pid);
  assert.equal(JSON.parse(readFileSync(join(directory, "launches.jsonl"), "utf8")).pid, result.pid);
  assert.equal(output.value, "private-test-value");
  assert.deepEqual(output.args, ["argument with spaces"]);
}));

test("lab launch refuses public directories and workspace escapes", () => withExperiment(({ directory }) => {
  chmodSync(directory, 0o755);
  assert.throws(() => readExperiment(directory));
  chmodSync(directory, 0o700);
  writeFileSync(join(directory, "experiment.json"), JSON.stringify({ directory, workspace: "/tmp" }));
  assert.throws(() => readExperiment(directory));
}));

test("stable socket address can change workers without replacing unrelated files", () => withExperiment(({ directory }) => {
  const path = join(directory, "native.sock");
  publishSocketAddress(path, "/test/worker-one.sock");
  assert.equal(readlinkSync(path), "/test/worker-one.sock");
  publishSocketAddress(path, "/test/worker-two.sock");
  assert.equal(readlinkSync(path), "/test/worker-two.sock");
  assert.ok(!readdirSync(directory).some((name) => name.endsWith(".pending")));
  assert.throws(() => publishSocketAddress(join(directory, "experiment.json"), "/test/worker.sock"), /non-symlink/);
  assert.equal(readExperiment(directory).directory, directory);
}));

test("Linux process identity handles spaces and parentheses in command names", () => {
  const fields = ["S", ...Array.from({ length: 18 }, () => "0"), "12345678"];
  assert.equal(processStartTicks(`12 (name with ) spaces) ${fields.join(" ")}`), "12345678");
  assert.throws(() => processStartTicks("12 malformed"));
  const identity = processIdentity(process.pid);
  assert.equal(identity.uid, process.getuid());
  assert.match(identity.bootId, /^[a-f0-9-]+$/);
  assert.match(identity.startTicks, /^\d+$/);
});

test("endpoint binding rejects PID reuse, stale boots, and changed conversations", () => {
  const row = { id: "1234abcd", pid: 42, sessionId: randomUUID() };
  const identity = { bootId: randomUUID(), startTicks: "123", executable: "/native", cwd: "/workspace", uid: 1000 };
  const entry = { endpointVersion: 1, jobId: row.id, pid: row.pid, identity };
  const snapshot = { pid: row.pid, sessionId: row.sessionId, epoch: randomUUID(), sequence: 7 };
  validateBinding(row, entry, identity, snapshot);
  assert.throws(() => validateBinding(row, entry, { ...identity, startTicks: "124" }, snapshot), /stale endpoint/);
  assert.throws(() => validateBinding(row, entry, { ...identity, bootId: randomUUID() }, snapshot), /stale endpoint/);
  assert.throws(() => validateBinding({ ...row, pid: 43 }, entry, identity, snapshot), /worker changed/);
  assert.throws(() => validateBinding(row, entry, identity, { ...snapshot, sessionId: randomUUID() }), /conversation identity/);
  assert.throws(() => validateBinding(row, entry, identity, { ...snapshot, sequence: -1 }), /cursor/);
});

test("native completion and worker availability are independent and never imply wake", async () => {
  for (const nativeState of ["working", "blocked", "done", "failed", "stopped"]) {
    const observation = await observeRow({}, { kind: "background", id: "1234abcd", state: nativeState });
    assert.equal(observation.nativeState, nativeState);
    assert.equal(observation.availability, "dormant");
    assert.equal(observation.canSend, false);
    assert.equal(observation.canReconnect, false);
  }
  const interactive = await observeRow({}, { kind: "interactive", pid: 1 });
  assert.equal(interactive.availability, "native-terminal-only");
  assert.equal(interactive.canSend, false);
  for (const row of [null, [], { kind: "future-kind", pid: 123 }]) {
    const observation = await observeRow({}, row);
    assert.equal(observation.availability, "unavailable");
    assert.equal(observation.canSend, false);
  }
});

test("endpoint validation rejects symlink indirection without changing its target", () => withExperiment(({ directory }) => {
  const path = join(directory, "current.json");
  const original = join(directory, "experiment.json");
  publishSocketAddress(path, original);
  assert.throws(() => privateEntry(path, "file"), /private|symlink/);
  assert.equal(readExperiment(directory).directory, directory);
}));
