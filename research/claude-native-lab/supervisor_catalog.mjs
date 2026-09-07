import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { lstatSync, readFileSync, realpathSync } from "node:fs";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
import { promisify } from "node:util";
import { NativeClient } from "./client.mjs";
import { verifiedBuilds } from "./discover.mjs";
import { processIdentity, readExperiment } from "./supervisor_wrapper.mjs";

const execute = promisify(execFile);
const uuid = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

export function privateEntry(path, kind) {
  const metadata = lstatSync(path);
  assert.equal(metadata.uid, process.getuid(), "Foreign-owned endpoint metadata");
  assert.equal(metadata.mode & 0o077, 0, "Endpoint metadata must be private");
  const expectedType = kind === "directory" ? metadata.isDirectory() : kind === "socket" ? metadata.isSocket() : metadata.isFile();
  assert.ok(expectedType, `Expected a real ${kind}, not a symlink or another file type`);
  return metadata;
}

function privateJson(path) {
  assert.ok(privateEntry(path, "file").size <= 65536, "Endpoint metadata exceeds its size limit");
  return JSON.parse(readFileSync(path, "utf8"));
}

export function validateBinding(row, entry, identity, snapshot) {
  assert.equal(entry.endpointVersion, 1, "Unsupported endpoint metadata version");
  assert.equal(entry.jobId, row.id, "Native job identity changed");
  assert.equal(entry.pid, row.pid, "Native worker changed; refresh discovery");
  assert.deepEqual(entry.identity, identity, "Worker process identity changed; stale endpoint refused");
  if (snapshot) {
    assert.equal(snapshot.pid, row.pid, "Connected socket belongs to another process");
    assert.equal(snapshot.sessionId, row.sessionId, "Native conversation identity changed; refresh discovery");
    assert.match(snapshot.epoch ?? "", uuid, "Missing adapter epoch");
    assert.ok(Number.isSafeInteger(snapshot.sequence) && snapshot.sequence >= 0, "Missing event cursor");
  }
}

export async function nativeRows(configuration) {
  const environment = { ...process.env };
  for (const name of ["LD_PRELOAD", "BUN_OPTIONS", "CLAUDE_CODE_PROCESS_WRAPPER", "CLAUDECODE", "CLAUDE_CODE_ENTRYPOINT"])
    delete environment[name];
  environment.DISABLE_AUTOUPDATER = "1";
  const { stdout } = await execute(configuration.original, ["agents", "--json", "--all", "--cwd", configuration.workspace],
    { env: environment, cwd: configuration.workspace, timeout: 5000, maxBuffer: 4 * 1024 * 1024 });
  const rows = JSON.parse(stdout);
  assert.ok(Array.isArray(rows) && rows.length <= 1000, "Invalid native session listing");
  return rows;
}

export async function connectNativeRow(configuration, row) {
  assert.equal(row.kind, "background", "Only native-supervised sessions have a job endpoint");
  assert.match(row.id ?? "", /^[0-9a-f]{8,64}$/, "Invalid native job ID");
  assert.match(row.sessionId ?? "", uuid, "Native conversation identity is not known yet");
  assert.equal(realpathSync(row.cwd), configuration.workspace, "Native workspace mismatch");
  assert.ok(Number.isSafeInteger(row.pid) && row.pid > 0, "Native worker is not running");
  const job = join(configuration.directory, "jobs", row.id);
  privateEntry(join(configuration.directory, "jobs"), "directory");
  privateEntry(job, "directory");
  const entryPath = join(job, "current.json");
  const entry = privateJson(entryPath);
  const runtime = join(job, String(row.pid));
  assert.equal(entry.runtime, runtime, "Endpoint runtime escaped its native job");
  assert.equal(entry.socketPath, join(runtime, "native.sock"), "Unexpected worker socket path");
  assert.ok(verifiedBuilds.has(entry.sha256), "Unverified adapter build");
  const identity = processIdentity(row.pid);
  assert.equal(identity.uid, process.getuid(), "Native worker belongs to another user");
  assert.equal(identity.executable, configuration.original, "Native executable changed");
  assert.equal(identity.cwd, configuration.workspace, "Native worker left the expected workspace");
  validateBinding(row, entry, identity);
  privateEntry(runtime, "directory");
  privateEntry(entry.socketPath, "socket");
  const client = new NativeClient(entry.socketPath);
  try {
    const snapshot = await client.request("snapshot", {}, 2000);
    const hello = client.events.find((event) => event.event === "hello");
    assert.equal(hello?.protocol, "harness-native-lab/0", "Unsupported native adapter protocol");
    assert.equal(hello.pid, row.pid, "Native handshake process mismatch");
    assert.equal(hello.sessionId, row.sessionId, "Native handshake conversation mismatch");
    assert.equal(hello.epoch, snapshot.epoch, "Adapter changed during connection");
    validateBinding(row, entry, processIdentity(row.pid), snapshot);
    assert.deepEqual(privateJson(entryPath), entry, "Worker endpoint changed during connection");
    return { client, snapshot };
  } catch (error) {
    client.close();
    throw error;
  }
}

export async function observeRow(configuration, row) {
  if (!row || typeof row !== "object" || Array.isArray(row))
    return { availability: "unavailable", canSend: false, canReconnect: false, detail: "Malformed native catalog row" };
  const observation = { id: row.id, sessionId: row.sessionId, cwd: row.cwd, name: row.name,
    nativeState: row.state, nativeStatus: row.status, pid: row.pid, canSend: false, canReconnect: false };
  if (!row.pid) return { ...observation, availability: "dormant", detail: "No worker. Explicit native wake is separate from reconnect." };
  if (row.kind === "interactive") return { ...observation, availability: "native-terminal-only", detail: "Interactive owner; no hot attach is assumed." };
  if (row.kind !== "background") return { ...observation, availability: "unavailable", detail: "Unknown native session kind" };
  try {
    assert.match(row.id ?? "", /^[0-9a-f]{8,64}$/);
    try { lstatSync(join(configuration.directory, "jobs", row.id, "current.json")); }
    catch (error) {
      if (error.code !== "ENOENT") throw error;
      return { ...observation, availability: "needs-adapter", detail: "Native worker is live but has no Harness adapter endpoint; left untouched." };
    }
    const { client, snapshot } = await connectNativeRow(configuration, row);
    try {
      return { ...observation, availability: "connected", canReconnect: true, canSend: true,
        historyContinuity: "not-checked",
        epoch: snapshot.epoch, sequence: snapshot.sequence, messages: snapshot.messages.length, dialogs: snapshot.dialogs.length };
    } finally { client.close(); }
  } catch (error) {
    return { ...observation, availability: "unavailable", detail: error.message };
  }
}

export async function observeCatalog(configuration) {
  const rows = await nativeRows(configuration);
  const observations = [];
  for (let index = 0; index < rows.length; index += 4)
    observations.push(...await Promise.all(rows.slice(index, index + 4).map((row) => observeRow(configuration, row))));
  return observations;
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const configuration = readExperiment(process.argv[2]);
  console.log(JSON.stringify(await observeCatalog(configuration), null, 2));
}
