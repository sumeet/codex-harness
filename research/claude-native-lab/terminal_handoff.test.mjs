import assert from "node:assert/strict";
import { execFile, spawn } from "node:child_process";
import { createHash, randomUUID } from "node:crypto";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, realpathSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { promisify } from "node:util";
import test from "node:test";
import { NativeClient } from "./client.mjs";
import { discoverBinary } from "./discover.mjs";
import { isolatedGui } from "./gui_probe.mjs";

const nativeArgument = process.env.HARNESS_NATIVE_TEST_BINARY;
const harnessArgument = process.env.HARNESS_SETUP_BINARY;
const execute = promisify(execFile);

test("stock terminal backgrounds into the instrumented supervisor with history and launch settings", {
  skip: !nativeArgument || !harnessArgument, timeout: 120000,
}, async () => {
  const native = realpathSync(nativeArgument);
  assert.ok(discoverBinary(native).verified);
  const directory = mkdtempSync("/tmp/ht-");
  const configuration = join(directory, "p");
  const runtime = join(directory, "r");
  const workspace = join(directory, "workspace");
  const additional = join(directory, "additional");
  const project = join(configuration, "projects", workspace.replaceAll("/", "-"));
  for (const path of [runtime, workspace, additional, project]) mkdirSync(path, { recursive: true, mode: 0o700 });
  const environment = {
    PATH: process.env.PATH, LANG: "C.UTF-8", TERM: "xterm-256color",
    CLAUDE_CONFIG_DIR: configuration, XDG_RUNTIME_DIR: runtime,
    XDG_CONFIG_HOME: join(directory, "config"), XDG_DATA_HOME: join(directory, "data"), XDG_STATE_HOME: join(directory, "state"),
    HARNESS_CLAUDE_BINARY: native, DISABLE_AUTOUPDATER: "1", BROWSER: "/bin/false",
  };
  const settingsPath = join(directory, "launch-settings.json");
  const mcpPath = join(directory, "mcp.json");
  writeFileSync(settingsPath, JSON.stringify({ env: { HARNESS_HANDOFF_SENTINEL: "preserved-launch-settings" } }), { mode: 0o600 });
  writeFileSync(mcpPath, JSON.stringify({ mcpServers: {} }), { mode: 0o600 });
  writeFileSync(join(configuration, "settings.json"), JSON.stringify({
    remoteControlAtStartup: false, env: { DISABLE_AUTOUPDATER: "1", CLAUDE_CODE_ARTIFACT_AUTO_OPEN: "0" },
  }), { mode: 0o600 });
  writeFileSync(join(configuration, ".claude.json"), JSON.stringify({
    hasCompletedOnboarding: true, theme: "dark", projects: { [workspace]: { hasTrustDialogAccepted: true } },
  }), { mode: 0o600 });
  const conversation = randomUUID();
  const user = randomUUID();
  const assistant = randomUUID();
  const common = { sessionId: conversation, cwd: workspace, version: "2.1.263", isSidechain: false, timestamp: new Date().toISOString() };
  const messages = [
    { ...common, type: "user", uuid: user, parentUuid: null, message: { role: "user", content: "Synthetic saved history for terminal handoff." } },
    { ...common, type: "assistant", uuid: assistant, parentUuid: user, message: {
      id: "msg_handoff_fixture", type: "message", role: "assistant", model: "claude-sonnet-4-6",
      content: [{ type: "text", text: "Synthetic acknowledgment; no model was called." }], stop_reason: "end_turn", usage: { input_tokens: 10, output_tokens: 10 },
    } },
  ];
  writeFileSync(join(project, `${conversation}.jsonl`), messages.map(value => JSON.stringify(value) + "\n").join(""), { mode: 0o600 });
  const run = async (binary, arguments_) => execute(binary, arguments_, {
    env: environment, cwd: workspace, timeout: 15000, maxBuffer: 4 * 1024 * 1024,
  });
  const rows = async () => JSON.parse((await run(native, ["agents", "--json", "--all"])).stdout);
  const poll = async callback => {
    const deadline = Date.now() + 30000;
    while (Date.now() < deadline) {
      const result = await callback();
      if (result) return result;
      await delay(150);
    }
    throw new Error(`Timed out; inspect ${directory}/terminal.cast`);
  };
  const lateSetup = process.env.HARNESS_HANDOFF_LATE_SETUP === "1";
  const refreshService = process.env.HARNESS_HANDOFF_REFRESH_SERVICE === "1";
  const activeBystander = process.env.HARNESS_HANDOFF_ACTIVE_BYSTANDER === "1";
  const existingWorkers = [];
  const quote = value => `'${value.replaceAll("'", "'\\''")}'`;
  const report = { directory, conversation, modelRequestsSent: 0, lateSetup, refreshService, activeBystander, existingWorkers };
  let client, graphical, terminal, service, bystanderTerminal;
  const shellStarted = join(directory, "bystander-started");
  const shellRelease = join(directory, "bystander-release");
  const shellFinished = join(directory, "bystander-finished");
  let bystanderClosed = Promise.resolve();
  let output = "", terminalError;
  let closed = Promise.resolve();
  try {
    if (process.env.HARNESS_HANDOFF_EXISTING_WORKER === "1") {
      await run(native,["--bg","--name","Uninstrumented handoff bystander"]);
      const worker = await poll(async () => (await rows()).find(row => row.kind === "background" && row.pid));
      existingWorkers.push(worker);
      assert.ok(!readFileSync(`/proc/${worker.pid}/environ`).toString().split("\0").some(value => value.startsWith("BUN_OPTIONS=")));
      if (activeBystander) {
        bystanderTerminal = spawn("/usr/bin/script", ["-qefc", `stty rows 40 cols 120 && exec ${[native, "attach", worker.id].map(quote).join(" ")}`, join(directory, "bystander.cast")],
          { env: environment, cwd: workspace, stdio: ["pipe", "ignore", "ignore"] });
        let failure;
        bystanderTerminal.on("error", error => { failure = error; });
        bystanderTerminal.stdin.on("error", error => { failure = error; });
        bystanderClosed = new Promise(resolve => bystanderTerminal.on("close", (code, signal) => resolve({ code, signal })));
        await delay(1500);
        const shellScript = join(directory, "bystander-shell.sh");
        writeFileSync(shellScript, `printf '%s' "$$" > ${quote(shellStarted)}\nwhile [ ! -e ${quote(shellRelease)} ]; do sleep 0.1; done\nprintf '%s' "$$" > ${quote(shellFinished)}\n`, { mode: 0o600 });
        bystanderTerminal.stdin.write(`!sh ${quote(shellScript)}`);
        await delay(300);
        bystanderTerminal.stdin.write("\r");
        await poll(async () => {
          if (failure) throw failure;
          return existsSync(shellStarted);
        });
        report.bystanderShellPid = Number(readFileSync(shellStarted, "utf8"));
        assert.ok(report.bystanderShellPid > 1);
      }
    }
    if (!lateSetup) await run(resolve(harnessArgument), ["--claude-setup", "enable"]);
    const arguments_ = [native, "--resume", conversation, "--settings", settingsPath, "--mcp-config", mcpPath,
      "--strict-mcp-config", "--add-dir", additional, "--model", "haiku"];
    terminal = spawn("/usr/bin/script", ["-qefc", `stty rows 40 cols 120 && exec ${arguments_.map(quote).join(" ")}`, join(directory, "terminal.cast")], {
      env: environment, cwd: workspace, stdio: ["pipe", "pipe", "pipe"],
    });
    terminal.stdout.on("data", bytes => { output = (output + bytes.toString()).slice(-1024 * 1024); });
    terminal.stderr.on("data", bytes => { output = (output + bytes.toString()).slice(-1024 * 1024); });
    terminal.on("error", error => { terminalError = error; });
    terminal.stdin.on("error", error => { terminalError = error; });
    closed = new Promise(resolveClosed => terminal.on("close", (code, signal) => resolveClosed({ code, signal })));
    const before = await poll(async () => {
      if (terminalError) throw terminalError;
      return (await rows()).find(row => row.kind === "interactive" && row.sessionId === conversation);
    });
    assert.equal(before.cwd, workspace);
    const beforeEnvironment = readFileSync(`/proc/${before.pid}/environ`).toString().split("\0");
    assert.ok(!beforeEnvironment.some(value => value.startsWith("BUN_OPTIONS=")), "The ordinary terminal must not be preloaded");
    report.originalPid = before.pid;
    const signals = /^SigCgt:\s+(\w+)/m.exec(readFileSync(`/proc/${before.pid}/status`,"utf8"));
    assert.ok(signals);
    report.sigusr1Handled = Boolean(BigInt(`0x${signals[1]}`) & (1n << 9n));
    if (lateSetup) await run(resolve(harnessArgument),["--claude-setup","enable"]);
    if (refreshService) {
      const originalServiceStatus = await run(native, ["daemon", "status"]);
      report.originalServicePid = Number(/^pid:\s+(\d+)/m.exec(originalServiceStatus.stdout)?.[1]);
      assert.ok(report.originalServicePid > 1);
      await run(native, ["daemon", "stop", "--any", "--keep-workers"]);
      service = spawn(native, ["daemon", "run", "--origin", "transient", "--log-file", join(directory, "refreshed-service.log")],
        { env: environment, cwd: workspace, stdio: "ignore" });
      let serviceError;
      service.on("error", error => { serviceError = error; });
      const expectedWrapper = JSON.parse(JSON.parse(readFileSync(join(configuration, "settings.json"))).processWrapper).join(" ");
      await poll(async () => {
        if (serviceError) throw serviceError;
        let status;
        try { status = await run(native, ["daemon", "status"]); }
        catch (error) {
          if (error.code === 1 && error.stdout.startsWith("not running\n")) return undefined;
          throw error;
        }
        report.serviceStatus = status.stdout;
        const servicePid = /^pid:\s+(\d+)/m.exec(status.stdout)?.[1];
        if (!servicePid || !status.stdout.includes(`launcher: ${expectedWrapper}\n`) || !status.stdout.includes("control.sock: reachable")) return undefined;
        report.refreshedServicePid = Number(servicePid);
        assert.notEqual(report.refreshedServicePid, report.originalServicePid);
        report.existingClientRestartedService = Number(servicePid) !== service.pid;
        return true;
      });
      for (const worker of existingWorkers)
        assert.equal((await rows()).find(row=>row.id===worker.id)?.pid,worker.pid);
      assert.deepEqual((await rows()).filter(row=>row.kind==="background").map(row=>row.id).sort(),existingWorkers.map(row=>row.id).sort(),"Service refresh must not create a conversation");
      assert.equal((await rows()).find(row=>row.sessionId===conversation)?.pid,before.pid,"Service refresh must not replace the ordinary terminal");
      report.serviceRefreshPreservedWorkers = true;
      if (activeBystander) {
        assert.equal(bystanderTerminal.exitCode, null, "Service refresh must preserve terminal attachment");
        process.kill(report.bystanderShellPid, 0);
        writeFileSync(shellRelease, "release\n", { mode: 0o600 });
        await poll(async () => existsSync(shellFinished));
        assert.equal(Number(readFileSync(shellFinished, "utf8")), report.bystanderShellPid);
        assert.equal(bystanderTerminal.exitCode, null);
        report.serviceRefreshPreservedActiveShellAndTerminal = true;
      }
    }
    await delay(1500);
    await assert.rejects(run(resolve(harnessArgument), ["--claude-continue", conversation]), /Run \/bg/);
    assert.equal((await rows()).find(row => row.sessionId === conversation).pid, before.pid);
    terminal.stdin.write("/bg\r");
    const after = await poll(async () => {
      const records = readFileSync(join(project, `${conversation}.jsonl`), "utf8").split("\n").slice(0, -1).map(line => JSON.parse(line));
      const target = records.find(record=>record.type==="continued-in"&&record.sessionId===conversation)?.continuedInSessionId;
      return (await rows()).find(row => row.kind === "background" && row.pid && row.sessionId === target);
    });
    assert.notEqual(after.pid, before.pid);
    assert.equal(after.cwd, workspace);
    report.workerPid = after.pid;
    report.backgroundConversation = after.sessionId;
    const profileHash = createHash("sha256").update(configuration).digest("hex").slice(0, 16);
    const entryPath = join(runtime, `harness-claude-${profileHash}`, "jobs", after.id, "current.json");
    const snapshot = await poll(async () => {
      try {
        const entry = JSON.parse(readFileSync(entryPath));
        client?.close();
        client = new NativeClient(entry.socketPath);
        return await client.request("snapshot", {}, 1000);
      } catch (error) {
        if (!/ENOENT|connect |Disconnected|Timed out/.test(error.message)) throw error;
        return undefined;
      }
    });
    assert.equal(snapshot.pid, after.pid);
    assert.equal(snapshot.sessionId, after.sessionId);
    writeFileSync(join(directory, "snapshot.json"), JSON.stringify(snapshot, null, 2), { mode: 0o600 });
    const restored = snapshot.messages.filter(message => ["user", "assistant"].includes(message.type));
    assert.deepEqual(restored.slice(0, 2).map(message => message.uuid), [user, assistant]);
    const normalize = content => typeof content === "string" ? [{ type: "text", text: content }] : content;
    for (const [index, message] of messages.entries()) {
      assert.deepEqual(normalize(restored[index].message.content), normalize(message.message.content));
    }
    assert.equal(snapshot.turn.isLoading, false);
    for (const worker of existingWorkers) {
      assert.equal((await rows()).find(row => row.id === worker.id)?.pid,worker.pid,"Handover must leave other workers running");
    }
    const state = JSON.parse(readFileSync(join(configuration, "jobs", after.id, "state.json")));
    const flags = state.respawnFlags;
    for (const value of ["--settings", settingsPath, "--mcp-config", mcpPath, "--strict-mcp-config", "--add-dir", additional]) assert.ok(flags.includes(value), `Missing carried flag ${value}`);
    assert.equal(flags[flags.indexOf("--model") + 1], "haiku");
    assert.equal(flags[flags.indexOf("--permission-mode") + 1], "default");
    assert.equal(state.bgIsolation, "none");
    assert.equal(state.interactiveLineage, true);
    const workerEnvironment = readFileSync(`/proc/${after.pid}/environ`).toString().split("\0");
    report.initialEnvironmentHadSentinel = workerEnvironment.includes("HARNESS_HANDOFF_SENTINEL=preserved-launch-settings");
    if (process.env.HARNESS_HANDOFF_SHELL_PROBE === "1") {
      const probe = spawn("/usr/bin/script",["-qefc",`stty rows 40 cols 120 && exec ${[native,"attach",after.id].map(quote).join(" ")}`,join(directory,"environment-probe.cast")],
        {env:environment,cwd:workspace,stdio:["pipe","pipe","pipe"]});
      let probeOutput="";
      probe.stdout.on("data",part=>probeOutput=(probeOutput+part.toString()).slice(-1024*1024));
      probe.stderr.on("data",part=>probeOutput=(probeOutput+part.toString()).slice(-1024*1024));
      const probeClosed=new Promise(resolve=>probe.on("close",(code,signal)=>resolve({code,signal})));
      try {
        await delay(1500);
        probe.stdin.write("!printf 'ENV_PROBE_%s_END\\n' \"$HARNESS_HANDOFF_SENTINEL\"\r");
        await poll(async()=>probeOutput.includes("ENV_PROBE_preserved-launch-settings_END"));
        report.liveShellEnvironmentPreserved=true;
        probe.stdin.write("\x1a");
        const result=await Promise.race([probeClosed,delay(5000).then(()=>{throw new Error("Probe terminal did not detach");})]);
        assert.equal(result.code,0,probeOutput);
      } finally {
        if(probe.exitCode===null&&probe.signalCode===null){probe.kill("SIGTERM");await probeClosed;}
      }
    }
    Object.assign(report, { sameConversationId: after.sessionId === conversation, sameHistory: true, bgIsolation: state.bgIsolation, interactiveLineage: state.interactiveLineage,
      flags, model: state.model, permissionMode: state.permissionMode });
    await delay(1000);
    const baselineSubmitCount = (await client.request("snapshot")).turn.submitCount;
    if (process.env.HARNESS_RESUME_GUI === "1") {
      graphical = await isolatedGui(harnessArgument, environment, directory);
      const catalog = JSON.parse((await run(resolve(harnessArgument), ["--claude-list"])).stdout);
      const original = catalog.sessions.find(session => session.source.conversation_id === conversation);
      const index = catalog.conversations.findIndex(entry => entry.aliases.includes(original.id));
      assert.ok(index >= 0);
      assert.equal(catalog.conversations.length, 1+existingWorkers.length, "Native handoff IDs should form one logical sidebar conversation");
      const logicalId = catalog.conversations[index].id;
      const draftKey = `claude:${logicalId}`;
      const draft = "Unsent draft belongs to the original terminal conversation.";
      const replacement = catalog.sessions.find(session => session.source.conversation_id === after.sessionId);
      const replacementKey = `claude:${replacement.id}`;
      const replacementDraft = "Another saved draft, independently written against the replacement ID.";
      const window = await graphical.open({ [draftKey]: draft, [replacementKey]: replacementDraft });
      await graphical.click(window, 145, 48);
      await delay(1500);
      await graphical.capture("handoff-list");
      await graphical.click(window, 130, 180 + index * 52);
      await delay(2000);
      await graphical.capture("handoff-opened");
      assert.equal(JSON.parse(readFileSync(join(graphical.configuration(window), "harness", "session.json"))).selected_claude_id, logicalId);
      assert.equal(JSON.parse(readFileSync(join(graphical.configuration(window), "harness", "drafts.json"))).drafts[draftKey], draft);
      assert.equal(JSON.parse(readFileSync(join(graphical.configuration(window), "harness", "drafts.json"))).drafts[replacementKey], replacementDraft);
      await graphical.click(window, 45, 758);
      await delay(500);
      await graphical.capture("replacement-draft-selected");
      assert.equal((await client.request("snapshot")).turn.submitCount, baselineSubmitCount, "Switching drafts must not send them");
      await graphical.close();
      graphical = undefined;
      report.guiRetainsOriginalSelectionAndDraft = true;
      report.oneSidebarConversationWithBothDraftsPreserved = true;
    }
    const opened = JSON.parse((await run(resolve(harnessArgument), ["--claude-continue", conversation])).stdout);
    assert.equal(opened.source.conversation_id, after.sessionId);
    assert.equal(opened.source.job.pid, after.pid);
    assert.equal((await rows()).filter(row => row.kind === "background" && row.pid).length, 1+existingWorkers.length);
    report.harnessFollowsNativeHandoff = true;
    const proofPath = join(configuration, "harness-adapter", "continuations", `${after.sessionId}.json`);
    const proof = readFileSync(proofPath);
    try {
      writeFileSync(proofPath, JSON.stringify({ ...JSON.parse(proof), verified: null, history_digest: "conflicting-prior-history" }), { mode: 0o600 });
      await assert.rejects(run(resolve(harnessArgument), ["--claude-continue", conversation]), /different message identities, order, or contents/);
      assert.equal(JSON.parse(readFileSync(proofPath)).history_digest, "conflicting-prior-history");
      assert.equal((await rows()).find(row => row.sessionId === after.sessionId).pid, after.pid);
      report.handoffDoesNotBypassPriorVerification = true;
    } finally { writeFileSync(proofPath, proof, { mode: 0o600 }); }
    if (process.env.HARNESS_HANDOFF_MIGRATE_BYSTANDER === "1") {
      assert.ok(activeBystander && refreshService && report.serviceRefreshPreservedActiveShellAndTerminal);
      const worker = existingWorkers[0];
      await assert.rejects(run(resolve(harnessArgument), ["--claude-continue", worker.sessionId]), /already running without a verified Harness connection/);
      const catalog = JSON.parse((await run(resolve(harnessArgument), ["--claude-list"])).stdout);
      const source = catalog.sessions.find(session => session.source.conversation_id === worker.sessionId);
      const saved = readFileSync(source.source.transcript, "utf8").trimEnd().split("\n").map(line => JSON.parse(line))
        .filter(record => ["user", "assistant"].includes(record.type) && !record.isSidechain);
      assert.ok(saved.length > 0, "Migration probe requires actual persisted conversation history");
      const oldState = JSON.parse(readFileSync(join(configuration, "jobs", worker.id, "state.json")));
      await run(native, ["respawn", worker.id]);
      const replacement = await poll(async () => (await rows()).find(row => row.id === worker.id && row.pid && row.pid !== worker.pid));
      assert.equal(replacement.sessionId, worker.sessionId);
      let migratedClient;
      try {
        const migrated = await poll(async () => {
          try {
            const entry = JSON.parse(readFileSync(join(runtime, `harness-claude-${profileHash}`, "jobs", worker.id, "current.json")));
            migratedClient?.close();
            migratedClient = new NativeClient(entry.socketPath);
            const value = await migratedClient.request("snapshot", {}, 1000);
            return value.pid === replacement.pid ? value : undefined;
          } catch (error) {
            if (!/ENOENT|connect |Disconnected|Timed out/.test(error.message)) throw error;
          }
        });
        const semantic = values => values.filter(message => ["user", "assistant"].includes(message.type)).map(message => ({
          uuid: message.uuid, role: message.message.role, content: normalize(message.message.content),
        }));
        assert.deepEqual(semantic(migrated.messages).slice(0, saved.length), semantic(saved));
        const nextState = JSON.parse(readFileSync(join(configuration, "jobs", worker.id, "state.json")));
        assert.deepEqual(nextState.respawnFlags, oldState.respawnFlags);
        assert.equal((await client.request("snapshot")).pid, after.pid, "Selective migration must leave the other connected worker alone");
        const openedBystander = JSON.parse((await run(resolve(harnessArgument), ["--claude-continue", worker.sessionId])).stdout);
        assert.equal(openedBystander.source.job.pid, replacement.pid);
        assert.equal((await rows()).filter(row => row.kind === "background" && row.pid).length, 1 + existingWorkers.length);
        await delay(1000);
        report.selectiveMigration = { oldPid: worker.pid, newPid: replacement.pid, sessionId: worker.sessionId,
          savedMessagesVerified: saved.length, launchFlagsPreserved: true, otherWorkerUnchanged: true,
          terminalClientStillRunning: bystanderTerminal.exitCode === null && bystanderTerminal.signalCode === null,
          connectedInHarness: true };
        writeFileSync(join(directory, "migrated-bystander.json"), JSON.stringify(migrated, null, 2), { mode: 0o600 });
      } finally { migratedClient?.close(); }
    }
    console.log(JSON.stringify(report));
  } finally {
    writeFileSync(shellRelease, "release\n", { mode: 0o600 });
    if (graphical) {
      try { await graphical.capture("failure"); } finally { await graphical.close(); }
    }
    client?.close();
    if (bystanderTerminal && bystanderTerminal.exitCode === null && bystanderTerminal.signalCode === null) bystanderTerminal.kill("SIGTERM");
    await Promise.race([bystanderClosed, delay(3000)]);
    if (terminal && terminal.exitCode === null && terminal.signalCode === null) terminal.kill("SIGTERM");
    await Promise.race([closed, delay(3000)]);
    for (const row of await rows()) {
      assert.equal(row.cwd, workspace);
      if (row.kind === "background" && row.pid) await run(native, ["stop", row.id]);
    }
    await run(native, ["daemon", "stop", "--any"]);
    if (service && service.exitCode === null && service.signalCode === null) {
      await poll(async()=>service.exitCode!==null||service.signalCode!==null);
    }
    writeFileSync(join(directory, "report.json"), JSON.stringify({ ...report, terminalError: terminalError?.message, terminalTail: output.slice(-4000) }, null, 2), { mode: 0o600 });
    console.log(`Private terminal-handoff evidence: ${directory}`);
  }
});
