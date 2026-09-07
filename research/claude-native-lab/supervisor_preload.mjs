import assert from "node:assert/strict";
import { mkdirSync, realpathSync, appendFileSync, lstatSync, symlinkSync, renameSync, unlinkSync } from "node:fs";
import { randomUUID } from "node:crypto";
import { basename, join } from "node:path";
import { homedir } from "node:os";
import { fileURLToPath } from "node:url";
import { start, writeStatus } from "./preload.mjs";
import { processIdentity } from "./supervisor_wrapper.mjs";

export function publishSocketAddress(path, target) {
  try { assert.ok(lstatSync(path).isSymbolicLink(), "Refusing to replace a non-symlink socket address"); }
  catch (error) { if (error.code !== "ENOENT") throw error; }
  const temporary = `${path}.${randomUUID()}.pending`;
  try {
    symlinkSync(target, temporary);
    renameSync(temporary, path);
  } finally {
    try { unlinkSync(temporary); }
    catch (error) { if (error.code !== "ENOENT") process.stderr.write(`Could not remove temporary socket address: ${error}\n`); }
  }
}

function awaitAssignment(bootstrap) {
  try {
    // A standby is preloaded before it has a workspace or a conversation owner.
    const job = process.env.CLAUDE_JOB_DIR;
    if (process.env.CLAUDE_CODE_SESSION_KIND !== "bg" || !job) {
      setTimeout(() => awaitAssignment(bootstrap), 100).unref();
      return;
    }
    if (bootstrap.workspace && realpathSync(process.cwd()) !== realpathSync(bootstrap.workspace)) return;
    if (bootstrap.configuration && realpathSync(process.env.CLAUDE_CONFIG_DIR || join(homedir(), ".claude")) !== bootstrap.configuration) return;
    const jobId = basename(job);
    assert.match(jobId, /^[a-f0-9]{8,64}$/);
    const jobDirectory = join(bootstrap.directory, "jobs", jobId);
    mkdirSync(jobDirectory, { recursive: true, mode: 0o700 });
    const runtime = join(jobDirectory, String(process.pid));
    mkdirSync(runtime, { mode: 0o700 });
    const socketPath = join(runtime, "native.sock");
    assert.ok(Buffer.byteLength(socketPath) < 104);
    const statusPath = join(runtime, "status.json");
    Object.assign(process.env, {
      HARNESS_CLAUDE_PRELOAD_SPEC: JSON.stringify(bootstrap.discovery),
      HARNESS_CLAUDE_PRELOAD_STATUS: statusPath,
      HARNESS_CLAUDE_BRIDGE_MODULE: fileURLToPath(new URL("bridge.mjs", import.meta.url)),
      HARNESS_CLAUDE_BRIDGE_SOCKET: socketPath,
    });
    const entry = { endpointVersion: 1, pid: process.pid, jobId, runtime, socketPath,
      identity: processIdentity(process.pid), sha256: bootstrap.discovery.sha256, at: Date.now() };
    writeStatus(statusPath, { pid: process.pid, state: "starting" });
    writeStatus(join(jobDirectory, "current.json"), entry);
    publishSocketAddress(join(jobDirectory, "native.sock"), socketPath);
    appendFileSync(join(bootstrap.directory, "assignments.jsonl"), JSON.stringify(entry) + "\n", { mode: 0o600 });
    start().catch((error) => {
      writeStatus(statusPath, { pid: process.pid, state: "error", error: String(error) });
      process.stderr.write(`Harness supervisor attachment failed: ${error}\n`);
    });
  } catch (error) {
    process.stderr.write(`Harness supervisor bootstrap refused attachment: ${error}\n`);
  }
}

if (process.env.HARNESS_CLAUDE_SUPERVISOR_BOOTSTRAP) {
  const bootstrap = JSON.parse(process.env.HARNESS_CLAUDE_SUPERVISOR_BOOTSTRAP);
  delete process.env.HARNESS_CLAUDE_SUPERVISOR_BOOTSTRAP;
  delete process.env.BUN_OPTIONS;
  awaitAssignment(bootstrap);
}
