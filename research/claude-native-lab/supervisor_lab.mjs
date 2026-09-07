import assert from "node:assert/strict";
import { mkdtempSync, mkdirSync, readFileSync, realpathSync, readdirSync, writeFileSync, symlinkSync, lstatSync, readlinkSync, unlinkSync, chmodSync, renameSync, existsSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { spawn, spawnSync } from "node:child_process";
import { setTimeout as delay } from "node:timers/promises";
import { discoverBinary } from "./discover.mjs";
import { NativeClient } from "./client.mjs";
import { randomUUID, createHash } from "node:crypto";
import { publishSocketAddress } from "./supervisor_preload.mjs";
import net from "node:net";
import { readExperiment } from "./supervisor_wrapper.mjs";
import { homedir } from "node:os";
import { nativeRows, connectNativeRow } from "./supervisor_catalog.mjs";

const [operation, input, ...arguments_] = process.argv.slice(2);
assert.ok(input, "Usage: supervisor_lab.mjs prepare EXACT_BINARY | OPERATION PRIVATE_DIRECTORY [ARGS...] (see SUPERVISOR-AUDIT.md)");
if (operation === "prepare") {
  const original = realpathSync(input);
  const discovery = discoverBinary(original);
  assert.ok(discovery.verified, "Prepare a supervisor experiment only with a verified native build");
  const directory = mkdtempSync("/tmp/harness-claude-supervisor.");
  const workspace = join(directory, "workspace");
  mkdirSync(workspace, { mode: 0o700 });
  const wrapper = [process.execPath, "--no-warnings", fileURLToPath(new URL("supervisor_wrapper.mjs", import.meta.url)), directory];
  const settings = {
    processWrapper: JSON.stringify(wrapper),
    remoteControlAtStartup: false,
    env: { DISABLE_AUTOUPDATER: "1", CLAUDE_CODE_ARTIFACT_AUTO_OPEN: "0" },
  };
  const settingsPath = join(directory, "settings.json");
  const configuration = { directory, workspace, original, settingsPath, wrapper, discovery };
  writeFileSync(settingsPath, JSON.stringify(settings, null, 2), { flag: "wx", mode: 0o600 });
  writeFileSync(join(directory, "experiment.json"), JSON.stringify(configuration, null, 2), { flag: "wx", mode: 0o600 });
  console.log(JSON.stringify(configuration, null, 2));
} else {
  const configuration = readExperiment(input);
  const directory = configuration.directory;
  if (operation === "missing-history-attach") {
    const [jobId, transcriptPath] = arguments_;
    assert.match(jobId, /^[a-f0-9]{8}$/);
    const row = (await nativeRows(configuration)).find((row) => row.id === jobId);
    assert.ok(row && !row.pid, "Only test a stopped disposable native job");
    assert.match(row.sessionId, /^[a-f0-9-]{36}$/);
    const path = realpathSync(transcriptPath);
    assert.equal(path, transcriptPath, "Do not move a symlinked transcript");
    assert.ok(path.startsWith(join(homedir(), ".claude", "projects") + "/"));
    assert.ok(path.endsWith(`/${row.sessionId}.jsonl`));
    assert.equal(lstatSync(path).uid, process.getuid());
    const contents = readFileSync(path);
    const records = contents.toString().trim().split("\n").map(JSON.parse);
    assert.ok(records.some((record) => record.cwd === configuration.workspace));
    assert.ok(records.every((record) => !record.cwd || record.cwd === configuration.workspace));
    const backup = join(dirname(path), `.harness-withheld-${row.sessionId}-${randomUUID()}`);
    assert.ok(!existsSync(backup));
    const environment = { ...process.env, DISABLE_AUTOUPDATER: "1", BROWSER: "/bin/false" };
    for (const name of ["LD_PRELOAD", "BUN_OPTIONS", "CLAUDE_CODE_PROCESS_WRAPPER", "CLAUDECODE", "CLAUDE_CODE_ENTRYPOINT"])
      delete environment[name];
    // script supplies a disposable PTY; quote the executable as a shell argument.
    const quotedBinary = `'${configuration.original.replaceAll("'", "'\\''")}'`;
    let result, unexpectedWorker = false, additionalTranscript, workerSnapshot;
    renameSync(path, backup);
    try {
      result = spawnSync("/usr/bin/script", ["-q", "-e", "-c", `${quotedBinary} attach ${jobId}`, "/dev/null"],
        { cwd: configuration.workspace, env: environment, encoding: "utf8", timeout: 15000, maxBuffer: 1024 * 1024 });
    } finally {
      const after = (await nativeRows(configuration)).find((row) => row.id === jobId);
      if (after?.pid) {
        unexpectedWorker = true;
        try {
          const { client, snapshot } = await connectNativeRow(configuration, after);
          try { workerSnapshot = { pid: snapshot.pid, sessionId: snapshot.sessionId, messages: snapshot.messages.length, active: snapshot.turn.isLoading }; }
          finally { client.close(); }
        } catch (error) { workerSnapshot = { error: error.message }; }
        const stopped = spawnSync(configuration.original, ["stop", jobId],
          { cwd: configuration.workspace, env: environment, encoding: "utf8", timeout: 10000 });
        assert.equal(stopped.status, 0, `Could not stop unexpected test worker. Original history remains in ${backup}`);
      }
      if (existsSync(path)) {
        additionalTranscript = join(dirname(path), `.harness-unexpected-history-${randomUUID()}`);
        renameSync(path, additionalTranscript);
      }
      renameSync(backup, path);
      assert.equal(createHash("sha256").update(readFileSync(path)).digest("hex"), createHash("sha256").update(contents).digest("hex"));
    }
    const report = { jobId, sessionId: row.sessionId, exitCode: result?.status, signal: result?.signal,
      error: result?.error?.message, output: result?.stdout, stderr: result?.stderr,
      unexpectedWorker, workerSnapshot, additionalTranscript, originalHistoryRestored: true };
    const reportPath = join(directory, `missing-history-${Date.now()}.json`);
    writeFileSync(reportPath, JSON.stringify(report, null, 2), { flag: "wx", mode: 0o600 });
    console.log(JSON.stringify({ reportPath, ...report }));
  } else if (operation === "race-respawn") {
    const [jobId] = arguments_;
    assert.match(jobId, /^[a-f0-9]{8}$/);
    const entryPath = join(directory, "jobs", jobId, "current.json");
    const entry = JSON.parse(readFileSync(entryPath, "utf8"));
    const client = new NativeClient(entry.socketPath);
    let before;
    try { before = await client.request("snapshot"); } finally { client.close(); }
    assert.equal(before.pid, entry.pid);
    assert.equal(before.turn.isLoading, false);
    assert.equal(before.dialogs.length, 0);
    assert.equal(realpathSync(`/proc/${entry.pid}/cwd`), configuration.workspace);
    assert.equal(realpathSync(`/proc/${entry.pid}/exe`), configuration.original);
    const samples = [];
    const sampleOwners = () => {
      const owners = [];
      for (const name of readdirSync(join(homedir(), ".claude", "sessions")).filter((name) => /^\d+\.json$/.test(name))) {
        try {
          const record = JSON.parse(readFileSync(join(homedir(), ".claude", "sessions", name), "utf8"));
          if (record.sessionId !== before.sessionId) continue;
          const pid = Number(name.slice(0, -5));
          if (realpathSync(`/proc/${pid}/exe`) === configuration.original && realpathSync(`/proc/${pid}/cwd`) === configuration.workspace)
            owners.push(pid);
        } catch (error) { if (!["ENOENT", "ESRCH"].includes(error.code)) throw error; }
      }
      samples.push({ at: Date.now(), owners });
    };
    const started = Date.now();
    const requests = Array.from({ length: 2 }, () => new Promise((resolve, reject) => {
      const child = spawn(process.execPath, [fileURLToPath(import.meta.url), "native", directory, "respawn", jobId],
        { stdio: ["ignore", "pipe", "pipe"] });
      let output = "", errors = "";
      child.stdout.on("data", (value) => { output += value; });
      child.stderr.on("data", (value) => { errors += value; });
      child.on("error", reject);
      child.on("close", (status, signal) => resolve({ status, signal, output, errors }));
    }));
    let completed = false, requestError;
    const results = Promise.all(requests).catch((error) => { requestError = error; return []; })
      .finally(() => { completed = true; });
    let after;
    while (Date.now() - started < 30000) {
      sampleOwners();
      await delay(100);
      if (requestError) throw requestError;
      const next = JSON.parse(readFileSync(entryPath, "utf8"));
      if (!completed || next.pid === entry.pid) continue;
      const connection = new NativeClient(next.socketPath);
      try { after = await connection.request("snapshot", {}, 500); break; }
      catch (error) { if (!/connect |Disconnected|Timed out/.test(error.message)) throw error; }
      finally { connection.close(); }
    }
    assert.ok(after, "No instrumented owner appeared after the respawn race");
    sampleOwners();
    const report = { before: { pid: before.pid, sessionId: before.sessionId, epoch: before.epoch }, requests: await results,
      after: { pid: after.pid, sessionId: after.sessionId, epoch: after.epoch, messages: after.messages.length },
      maximumSampledOwners: Math.max(...samples.map((sample) => sample.owners.length)), samples,
      preservedMessageUuids: before.messages.filter((message) => ["user", "assistant"].includes(message.type) && message.uuid)
        .every((message) => after.messages.some((next) => next.uuid === message.uuid)),
      qualification: "Two concurrent respawns of one idle disposable job; 100ms sampling is not proof of a global atomic ownership lock" };
    const reportPath = join(directory, `race-${started}.json`);
    writeFileSync(reportPath, JSON.stringify(report, null, 2), { flag: "wx", mode: 0o600 });
    console.log(JSON.stringify({ reportPath, ...report, samples: report.samples.length }));
    assert.equal(after.sessionId, before.sessionId);
    assert.equal(report.preservedMessageUuids, true);
    assert.ok(report.maximumSampledOwners <= 1, "Concurrent native owners were observed");
  } else if (operation === "crash-idle-worker") {
    const [jobId] = arguments_;
    assert.match(jobId, /^[a-f0-9]{8}$/);
    const entryPath = join(directory, "jobs", jobId, "current.json");
    const entry = JSON.parse(readFileSync(entryPath, "utf8"));
    const client = new NativeClient(entry.socketPath);
    let before;
    try { before = await client.request("snapshot"); } finally { client.close(); }
    assert.equal(before.pid, entry.pid);
    assert.equal(before.turn.isLoading, false, "Only crash an idle disposable worker");
    assert.equal(before.dialogs.length, 0);
    assert.equal(realpathSync(`/proc/${entry.pid}/cwd`), configuration.workspace);
    assert.equal(realpathSync(`/proc/${entry.pid}/exe`), configuration.original);
    const started = Date.now();
    process.kill(entry.pid, "SIGKILL");
    let after, lastError;
    while (Date.now() - started < 30000) {
      await delay(200);
      const next = JSON.parse(readFileSync(entryPath, "utf8"));
      if (next.pid === entry.pid) continue;
      const connection = new NativeClient(next.socketPath);
      try { after = await connection.request("snapshot", {}, 1000); break; }
      catch (error) { lastError = error.message; }
      finally { connection.close(); }
    }
    const persisted = before.messages.filter((message) => ["user", "assistant"].includes(message.type) && message.uuid);
    const report = { before: { pid: before.pid, sessionId: before.sessionId, epoch: before.epoch, messages: before.messages.length },
      recoveredAutomatically: Boolean(after), elapsedMs: Date.now() - started, lastError,
      after: after && { pid: after.pid, sessionId: after.sessionId, epoch: after.epoch, messages: after.messages.length },
      preservedMessageUuids: after && persisted.every((message) => after.messages.some((next) => next.uuid === message.uuid)) };
    const reportPath = join(directory, `crash-${started}.json`);
    writeFileSync(reportPath, JSON.stringify(report, null, 2), { flag: "wx", mode: 0o600 });
    console.log(JSON.stringify({ reportPath, ...report }));
    if (after) { assert.equal(after.sessionId, before.sessionId); assert.notEqual(after.epoch, before.epoch); }
  } else if (operation === "relay-fixtures") {
    const [jobId] = arguments_;
    assert.match(jobId, /^[a-f0-9]{8}$/);
    const servers = [], connections = new Set();
    for (const frontend of readdirSync(directory).filter((name) => /^ui-[1-8]$/.test(name))) {
      const fixture = JSON.parse(readFileSync(join(directory, frontend, "manifest.json"), "utf8"));
      if (fixture.jobId !== jobId) continue;
      const path = join(fixture.compatibilityDirectory, "native.sock");
      assert.ok(lstatSync(path).isSymbolicLink(), "Replace only this experiment's fixture symlink");
      assert.equal(readlinkSync(path), join(directory, "jobs", jobId, "native.sock"));
      unlinkSync(path);
      const server = net.createServer((client) => {
        const entry = JSON.parse(readFileSync(join(directory, "jobs", jobId, "current.json"), "utf8"));
        const backend = net.connect(entry.socketPath);
        connections.add(client); connections.add(backend);
        const close = () => { client.destroy(); backend.destroy(); connections.delete(client); connections.delete(backend); };
        client.on("error", close); backend.on("error", close);
        client.on("close", close); backend.on("close", close);
        client.pipe(backend).pipe(client);
      });
      server.listen(path, () => chmodSync(path, 0o600));
      servers.push(server);
    }
    const close = () => { for (const connection of connections) connection.destroy(); for (const server of servers) server.close(); };
    process.on("SIGINT", close); process.on("SIGTERM", close);
    console.log(JSON.stringify({ fixtureRelayPid: process.pid, frontends: servers.length,
      qualification: "Raw-byte fixture compatibility only; not production supervisor discovery" }));
  } else if (operation === "refresh-fixtures") {
    const [jobId] = arguments_;
    assert.match(jobId, /^[a-f0-9]{8}$/);
    const job = join(directory, "jobs", jobId);
    const entry = JSON.parse(readFileSync(join(job, "current.json"), "utf8"));
    publishSocketAddress(join(job, "native.sock"), entry.socketPath);
    for (const frontend of readdirSync(directory).filter((name) => /^ui-[1-8]$/.test(name))) {
      const fixture = JSON.parse(readFileSync(join(directory, frontend, "manifest.json"), "utf8"));
      if (fixture.jobId !== jobId) continue;
      publishSocketAddress(join(fixture.compatibilityDirectory, "native.sock"), join(job, "native.sock"));
    }
  } else if (operation === "harness-fixture") {
    const [jobId, index] = arguments_;
    assert.match(jobId, /^[a-f0-9]{8}$/);
    assert.match(index, /^[1-8]$/);
    const entry = JSON.parse(readFileSync(join(directory, "jobs", jobId, "current.json"), "utf8"));
    const frontend = join(directory, `ui-${index}`);
    mkdirSync(frontend, { mode: 0o700 });
    const config = join(frontend, "config");
    const data = join(frontend, "data");
    const runtime = join(frontend, "runtime");
    for (const path of [join(config, "harness"), join(data, "harness", "claude"), runtime])
      mkdirSync(path, { recursive: true, mode: 0o700 });
    const id = randomUUID();
    const compatibilityDirectory = join("/tmp", `harness-claude-${id}`);
    mkdirSync(compatibilityDirectory, { mode: 0o700 });
    const address = join(directory, "jobs", jobId, "native.sock");
    publishSocketAddress(address, entry.socketPath);
    symlinkSync(address, join(compatibilityDirectory, "native.sock"));
    const session = { id, directory: compatibilityDirectory, cwd: configuration.workspace,
      title: "Native supervisor experiment", lifecycle_version: 0, created_at_ms: Date.now() };
    const fixtures = [
      [join(data, "harness", "claude", `${id}.json`), session],
      [join(config, "harness", "session.json"), { workspace_mode: "claude", selected_claude_id: id, sidebar_open: true }],
      [join(frontend, "fixture.json"), { user: { markdown: "Isolated supervisor frontend experiment" }, events: [] }],
      [join(frontend, "manifest.json"), { frontend, config, data, runtime, compatibilityDirectory, jobId, nativePid: entry.pid }],
    ];
    for (const [path, value] of fixtures) writeFileSync(path, JSON.stringify(value, null, 2), { flag: "wx", mode: 0o600 });
    console.log(JSON.stringify({ frontend, config, data, runtime, compatibilityDirectory, nativePid: entry.pid }));
  } else if (operation === "watch-client") {
    const [socketPath, milliseconds] = arguments_;
    const client = new NativeClient(socketPath);
    const sequences = [];
    const eventKinds = {};
    client.on("event", (event) => {
      sequences.push(event.sequence);
      eventKinds[event.event] = (eventKinds[event.event] ?? 0) + 1;
    });
    try {
      const before = await client.request("snapshot");
      await delay(Number(milliseconds));
      const after = await client.request("snapshot");
      assert.equal(before.pid, after.pid);
      assert.equal(before.epoch, after.epoch);
      console.log(JSON.stringify({ clientPid: process.pid, nativePid: after.pid, epoch: after.epoch,
        sessionId: after.sessionId, messagesBefore: before.messages.length, messagesAfter: after.messages.length,
        eventKinds, sequences }));
    } finally { client.close(); }
  } else if (operation === "watch") {
    const [jobId, milliseconds = "45000"] = arguments_;
    assert.match(jobId, /^[a-f0-9]{8}$/);
    assert.ok(Number(milliseconds) > 0 && Number(milliseconds) <= 120000);
    const entry = JSON.parse(readFileSync(join(directory, "jobs", jobId, "current.json"), "utf8"));
    const results = await Promise.all(Array.from({ length: 8 }, () => new Promise((resolve, reject) => {
      const child = spawn(process.execPath, [fileURLToPath(import.meta.url), "watch-client", directory, entry.socketPath, milliseconds],
        { stdio: ["ignore", "pipe", "pipe"] });
      let output = "", errors = "";
      child.stdout.on("data", (data) => { output += data; });
      child.stderr.on("data", (data) => { errors += data; });
      child.on("error", reject);
      child.on("close", (code) => {
        if (code !== 0) reject(new Error(`Watcher exited ${code}: ${errors}`));
        else { try { resolve(JSON.parse(output)); } catch (error) { reject(error); } }
      });
    })));
    for (const result of results) {
      assert.equal(result.nativePid, entry.pid);
      assert.equal(new Set(result.sequences).size, result.sequences.length);
      assert.ok(result.sequences.every((sequence, index) => index === 0 || sequence === result.sequences[index - 1] + 1));
    }
    const commonStart = Math.max(...results.map((result) => result.sequences[0] ?? 0));
    const commonEnd = Math.min(...results.map((result) => result.sequences.at(-1) ?? 0));
    for (const result of results) assert.deepEqual(result.sequences.filter((sequence) => sequence >= commonStart && sequence <= commonEnd),
      results[0].sequences.filter((sequence) => sequence >= commonStart && sequence <= commonEnd));
    const reportPath = join(directory, `watch-${Date.now()}.json`);
    writeFileSync(reportPath, JSON.stringify({ commonStart, commonEnd, results }, null, 2), { flag: "wx", mode: 0o600 });
    console.log(JSON.stringify({ reportPath, clients: results.length, nativePid: entry.pid,
      eventsPerClient: results.map((result) => result.sequences.length), commonStart, commonEnd }));
  } else if (operation === "snapshot") {
    const jobs = join(directory, "jobs");
    const results = [];
    for (const job of readdirSync(jobs)) {
      const entry = JSON.parse(readFileSync(join(jobs, job, "current.json"), "utf8"));
      const client = new NativeClient(entry.socketPath);
      try {
        const snapshot = await client.request("snapshot");
        results.push({ job, pid: snapshot.pid, sessionId: snapshot.sessionId, epoch: snapshot.epoch,
          messages: snapshot.messages.length, dialogs: snapshot.dialogs.length, loading: snapshot.turn.isLoading,
          socketPath: entry.socketPath });
      } catch (error) { results.push({ job, error: error.message }); }
      finally { client.close(); }
    }
    console.log(JSON.stringify(results, null, 2));
  } else if (operation === "native") {
    const environment = { ...process.env };
    for (const name of ["BUN_OPTIONS", "LD_PRELOAD", "CLAUDECODE", "CLAUDE_CODE_ENTRYPOINT", "DISPLAY", "WAYLAND_DISPLAY"])
      delete environment[name];
    Object.assign(environment, { DISABLE_AUTOUPDATER: "1", BROWSER: "/bin/false", CLAUDE_CODE_ARTIFACT_AUTO_OPEN: "0",
      CLAUDE_CODE_PROCESS_WRAPPER: JSON.stringify(configuration.wrapper) });
    const result = spawnSync(configuration.original, arguments_, { cwd: configuration.workspace, env: environment, stdio: "inherit" });
    if (result.error) throw result.error;
    process.exitCode = result.status ?? 1;
  } else throw new Error(`Unknown operation ${operation}; see SUPERVISOR-AUDIT.md for available operations`);
}
