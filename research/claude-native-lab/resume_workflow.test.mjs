import assert from "node:assert/strict";
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, renameSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { spawnSync, spawn } from "node:child_process";
import { createHash, randomUUID } from "node:crypto";
import { setTimeout as delay } from "node:timers/promises";
import test from "node:test";
import { NativeClient } from "./client.mjs";
import { isolatedGui } from "./gui_probe.mjs";

const harness = process.env.HARNESS_SETUP_BINARY;
const native = process.env.HARNESS_NATIVE_TEST_BINARY;

test("continue existing native history through packaged Harness, without a model request", { skip: !harness || !native, timeout: 180000 }, async () => {
  const directory = mkdtempSync("/tmp/hr-");
  const configuration = join(directory, "p");
  const runtime = join(directory, "r");
  const workspace = join(directory, "workspace");
  const project = join(configuration, "projects", workspace.replaceAll("/", "-"));
  for (const path of [project, runtime, workspace]) mkdirSync(path, { recursive: true, mode: 0o700 });
  const environment = { ...process.env, CLAUDE_CONFIG_DIR: configuration, XDG_RUNTIME_DIR: runtime,
    XDG_DATA_HOME: join(directory, "data"), XDG_CONFIG_HOME: join(directory, "config"), XDG_STATE_HOME: join(directory, "state"),
    DISABLE_AUTOUPDATER: "1", BROWSER: "/bin/false", HARNESS_CLAUDE_BINARY: resolve(native) };
  for (const name of ["LD_PRELOAD", "BUN_OPTIONS", "CLAUDE_CODE_PROCESS_WRAPPER", "CLAUDECODE", "CLAUDE_CODE_ENTRYPOINT", "DISPLAY", "WAYLAND_DISPLAY", "HARNESS_CLAUDE_ENDPOINT_ROOT"])
    delete environment[name];
  const conversation = randomUUID();
  const first = randomUUID();
  const second = randomUUID();
  const common = { sessionId: conversation, cwd: workspace, version: "2.1.263", isSidechain: false, timestamp: new Date().toISOString() };
  let messages = [
    { ...common, type: "user", uuid: first, parentUuid: null, message: { role: "user", content: "Synthetic saved fixture: CONTINUATION_MARKER_42" } },
    { ...common, type: "assistant", uuid: second, parentUuid: first, message: { id: "msg_fixture", type: "message", role: "assistant", model: "claude-sonnet-4-6",
      content: [{ type: "text", text: "Fixture acknowledgment: CONTINUATION_MARKER_42" }], stop_reason: "end_turn", stop_sequence: null, usage: { input_tokens: 20, output_tokens: 10 } } },
  ];
  let parallelResult;
  if (process.env.HARNESS_RESUME_PARALLEL_FIXTURE === "1") {
    const request = randomUUID(), firstTool = randomUUID(), secondTool = randomUUID(), secondResult = randomUUID();
    parallelResult = randomUUID();
    const assistant = (uuid, parentUuid, content, id = "msg_parallel") => ({
      ...common, type: "assistant", uuid, parentUuid,
      message: { ...messages[1].message, id, content },
    });
    const result = (uuid, parentUuid, tool) => ({
      ...common, type: "user", uuid, parentUuid, sourceToolAssistantUUID: parentUuid,
      message: { role: "user", content: [{ type: "tool_result", tool_use_id: tool, content: `Saved output from ${tool}` }] },
    });
    messages.push(
      { ...common, type: "user", uuid: request, parentUuid: second, message: { role: "user", content: "Synthetic parallel tool fixture; do not execute anything." } },
      assistant(firstTool, request, [{ type: "tool_use", id: "toolu_fixture_first", name: "Bash", input: { command: "true" } }]),
      assistant(secondTool, firstTool, [{ type: "tool_use", id: "toolu_fixture_second", name: "Bash", input: { command: "true" } }]),
      result(parallelResult, firstTool, "toolu_fixture_first"),
      result(secondResult, secondTool, "toolu_fixture_second"),
      assistant(randomUUID(), secondResult, [{ type: "text", text: "Both saved outputs were received." }], "msg_final"),
    );
  }
  if (process.env.HARNESS_RESUME_HISTORY_FIXTURE) {
    const source = resolve(process.env.HARNESS_RESUME_HISTORY_FIXTURE);
    assert.match(source, /\/projects\/-tmp-harness-claude-(?:lab|supervisor)-[^/]+-workspace\/[a-f0-9-]+\.jsonl$/,
      "Copy only a previously disposable lab transcript");
    const records = readFileSync(source, "utf8").trim().split("\n").map(JSON.parse)
      .filter(record => record.isSidechain !== true && record.uuid && Object.hasOwn(record, "parentUuid"));
    const byId = new Map(records.map(record => [record.uuid, record]));
    const selected = [];
    const visited = new Set();
    let leaf = records.at(-1)?.uuid;
    while (leaf) {
      assert.ok(!visited.has(leaf), "History cycle");
      visited.add(leaf);
      const record = byId.get(leaf);
      assert.ok(record, "Missing history ancestor");
      selected.push(record);
      leaf = record.parentUuid;
    }
    // The original profile and conversation are never resumed or modified.
    messages = selected.reverse().map(record => ({ ...record, sessionId: conversation, cwd: workspace }));
  }
  const expectedIds = messages.filter(message => ["user", "assistant"].includes(message.type)).map(message => message.uuid);
  assert.ok(expectedIds.length > 0);
  const transcript = join(project, `${conversation}.jsonl`);
  writeFileSync(transcript, messages.map(message => JSON.stringify(message) + "\n").join(""), { flag: "wx", mode: 0o600 });
  const originalSourceDigest = createHash("sha256").update(readFileSync(transcript)).digest("hex");
  writeFileSync(join(configuration, "settings.json"), JSON.stringify({ remoteControlAtStartup: false, env: { DISABLE_AUTOUPDATER: "1", CLAUDE_CODE_ARTIFACT_AUTO_OPEN: "0" } }), { flag: "wx", mode: 0o600 });
  writeFileSync(join(configuration, ".claude.json"), JSON.stringify({ hasCompletedOnboarding: true, theme: "dark", projects: { [workspace]: { hasTrustDialogAccepted: true } } }), { flag: "wx", mode: 0o600 });
  const run = (binary, arguments_) => spawnSync(resolve(binary), arguments_, { env: environment, cwd: workspace, encoding: "utf8", timeout: 45000, maxBuffer: 4 * 1024 * 1024 });
  const success = result => { assert.equal(result.status, 0, result.stderr || result.error?.message); return JSON.parse(result.stdout); };
  const catalog = () => success(run(harness, ["--claude-list"]));
  const live = () => catalog().sessions.filter(session => session.source.job?.pid);
  const stop = async () => {
    for (const session of live()) {
      assert.equal(session.cwd, workspace);
      assert.equal(session.source.conversation_id, conversation, "Unexpected fork in isolated profile");
      const result = run(native, ["stop", session.source.job.id]);
      assert.equal(result.status, 0, result.stderr);
    }
    for (let retry = 0; retry < 50 && live().length; retry++) await delay(100);
    assert.equal(live().length, 0);
  };
  const snapshot = async session => {
    const profileHash = (await import("node:crypto")).createHash("sha256").update(configuration).digest("hex").slice(0, 16);
    const entry = JSON.parse(readFileSync(join(runtime, `harness-claude-${profileHash}`, "jobs", session.source.job.id, "current.json")));
    const client = new NativeClient(entry.socketPath);
    try { return await client.request("snapshot"); } finally { client.close(); }
  };
  let graphical;
  try {
    const onboarding = process.env.HARNESS_RESUME_ONBOARDING === "1";
    const settingsPath = join(configuration,"settings.json");
    const originalSettings = readFileSync(settingsPath,"utf8");
    if (!onboarding) success(run(harness, ["--claude-setup", "enable"]));
    if (process.env.HARNESS_CLAUDE_CONTROLS_GUI === "1") {
      const hidden = randomUUID();
      writeFileSync(join(project, `${hidden}.jsonl`), JSON.stringify({ ...common,
        sessionId: hidden, uuid: randomUUID(), parentUuid: null, type: "user",
        message: { role: "user", content: "HIDDEN_TEST_CONVERSATION_99" } }) + "\n", { mode: 0o600 });
      writeFileSync(join(configuration, "harness-adapter", "hidden-conversations.json"),
        JSON.stringify({version:1, conversations:[hidden]}), {mode:0o600});
      assert.ok(catalog().sessions.some(session => session.source.conversation_id === hidden),
        "Hiding must not remove saved history from backend discovery");
    }
    if (process.env.HARNESS_LEGACY_LAUNCHER_FIXTURE === "1") {
      const settings = JSON.parse(readFileSync(settingsPath));
      const [currentLauncher] = JSON.parse(settings.processWrapper);
      const current = JSON.parse(readFileSync(join(dirname(currentLauncher),"manifest.json")));
      const root = join(configuration,"harness-adapter"), legacy = join(root,"v1-legacy-fixture");
      mkdirSync(legacy,{mode:0o700});
      const manifestPath = join(legacy,"manifest.json"), launcher = join(legacy,"launcher.sh");
      writeFileSync(manifestPath,JSON.stringify({...current,package:legacy}),{mode:0o600});
      const quote = value => `'${value.replaceAll("'", "'\\''")}'`;
      writeFileSync(launcher,`#!/bin/sh\nexec ${quote(resolve(harness))} --claude-wrap ${quote(manifestPath)} "$@"\n`,{mode:0o700});
      const registryPath = join(root,"installed.json");
      const packages = JSON.parse(readFileSync(registryPath));
      writeFileSync(registryPath,JSON.stringify([...packages,manifestPath]),{mode:0o600});
      settings.processWrapper = JSON.stringify([launcher]);
      writeFileSync(settingsPath,JSON.stringify(settings),{mode:0o600});
    }
    const configuredSettings = readFileSync(settingsPath,"utf8");
    const originalTitle = catalog().sessions.find(session => session.source.conversation_id === conversation).title;
    if (process.env.HARNESS_RESUME_GUI === "1") {
      graphical = await isolatedGui(harness, environment, directory);
      const logicalId = catalog().sessions.find(session => session.source.conversation_id === conversation).id;
      const draftKey = `claude:${logicalId}`;
      const draft = "Saved draft: opening a conversation must not send this.";
      const window = await graphical.open({ [draftKey]: draft });
      await graphical.capture("initial");
      await graphical.click(window, 145, 48);
      await delay(1500);
      assert.equal(live().length, 0, "Browsing Claude must not wake a worker");
      await graphical.capture("saved-list");
      if (process.env.HARNESS_CLAUDE_CONTROLS_GUI === "1") {
        const text = await graphical.run("tesseract", [join(directory,"ui","saved-list.png"), "stdout"]);
        assert.doesNotMatch(text.stdout, /HIDDEN_TEST/);
        assert.match(text.stdout, /Show 1 hidden/);
      }
      await graphical.click(window, 130, process.env.HARNESS_CLAUDE_CONTROLS_GUI === "1" ? 270 : 240);
      if (onboarding) {
        await delay(1000);
        await graphical.capture("setup-consent");
        assert.equal(readFileSync(settingsPath,"utf8"), originalSettings, "Browsing/opening must not silently change settings");
        assert.equal(live().length,0);
        await graphical.key(window,"Return"); await delay(600);
        await graphical.capture("setup-cancelled");
        assert.equal(readFileSync(settingsPath,"utf8"),originalSettings);
        assert.equal(live().length,0,"Cancelling setup must not start a worker");
        await graphical.click(window,130,240); await delay(700);
        await graphical.key(window,"Escape"); await delay(500);
        assert.equal(readFileSync(settingsPath,"utf8"),originalSettings);
        assert.equal(live().length,0,"Escape must dismiss without launching");
        await graphical.click(window,130,240); await delay(700);
        await graphical.key(window,"Tab");
        await graphical.key(window,"Return");
      }
      const deadline = Date.now() + 15000;
      while (!live().length && Date.now() < deadline) await delay(200);
      await delay(1000);
      await graphical.capture("opened");
      assert.equal(live().length, 1, `One thread click should start the saved conversation; inspect ${directory}/ui`);
      assert.equal(JSON.parse(readFileSync(join(graphical.configuration(window), "harness", "session.json"))).selected_claude_id, logicalId);
      assert.equal(JSON.parse(readFileSync(join(graphical.configuration(window), "harness", "drafts.json"))).drafts[draftKey], draft);
      const owner = live()[0].source.job.pid;
      if (process.env.HARNESS_CLAUDE_CONTROLS_GUI === "1") {
        assert.equal((await snapshot(live()[0])).capabilities.sessionSettings, 1);
        await graphical.click(window, 1070, 875);
        await delay(350);
        await graphical.capture("model-menu");
        await graphical.key(window, "Home");
        for (let index = 0; index < 3; index++) await graphical.key(window, "Down");
        await graphical.key(window, "Return");
        await delay(1000);
        await graphical.capture("model-selected");
        assert.equal((await snapshot(live()[0])).settings.model, "haiku", `Model menu did not apply; inspect ${directory}/ui`);
        assert.equal((await snapshot(live()[0])).turn.submitCount, 0);
        await graphical.click(window, 1110, 880);
        await delay(300);
        await graphical.capture("permissions-menu");
        await graphical.key(window, "Home");
        for (let index = 0; index < 2; index++) await graphical.key(window, "Down");
        await graphical.key(window, "Return");
        await delay(500);
        assert.equal((await snapshot(live()[0])).settings.permissionMode, "plan");
        await graphical.click(window, 1200, 880);
        await graphical.key(window, "Home");
        for (let index = 0; index < 2; index++) await graphical.key(window, "Down");
        await graphical.key(window, "Return");
        await delay(600);
        assert.equal((await snapshot(live()[0])).settings.model, "sonnet");
        await graphical.click(window, 1180, 880);
        await delay(300);
        await graphical.capture("effort-menu");
        await graphical.key(window, "Home");
        await graphical.key(window, "Down");
        await graphical.key(window, "Return");
        await delay(500);
        assert.deepEqual((await snapshot(live()[0])).settings.effort, {kind:"level",value:"low"});
        assert.equal((await snapshot(live()[0])).turn.submitCount, 0);
        await graphical.run("xdotool", ["mousemove", "--window", window, "145", "48", "click", "3"]);
        await delay(300);
        await graphical.capture("provider-context-menu");
        await graphical.key(window, "Return");
        await delay(1500);
        await graphical.capture("provider-new-window");
        const windows = await graphical.run("xdotool", ["search", "--onlyvisible", "--name", "Harness"]);
        assert.ok(windows.stdout.trim().split("\n").length >= 2, "Provider menu must open a new window");
        const created = windows.stdout.trim().split("\n").find(identifier => identifier !== window);
        const openedPath = join(directory,"ui","new-window-conversation.png");
        await graphical.run("import", ["-window", created, openedPath]);
        const openedText = await graphical.run("tesseract", [openedPath,"stdout"]);
        assert.match(openedText.stdout, /CONTINUATION_MARKER_42/);
        assert.match(openedText.stdout, /Plan\s*mode/);
        assert.match(openedText.stdout, /Sonnet/);
        assert.match(openedText.stdout, /Low/);
        assert.doesNotMatch(openedText.stdout, /Ask Codex/);
        assert.equal(live()[0].source.job.pid, owner);
      }
      const second = await graphical.open();
      await graphical.click(second, 145, 48);
      await delay(1500);
      await graphical.capture("second-before-selection");
      await graphical.click(second, 130, process.env.HARNESS_CLAUDE_CONTROLS_GUI === "1" ? 210 : 180);
      await delay(1500);
      await graphical.capture("second-frontend");
      assert.equal(live().length, 1);
      assert.equal(JSON.parse(readFileSync(join(graphical.configuration(second), "harness", "session.json"))).selected_claude_id, logicalId);
      assert.equal(live()[0].source.job.pid, owner, "Second frontend must reuse the worker");
      await graphical.close();
      graphical = undefined;
      assert.equal(live()[0].source.job.pid, owner, "Closing all frontends must leave the native worker running");
      console.log(`Verified single-click open and two frontend windows: ${directory}/ui`);
    }
    let session = success(run(harness, ["--claude-continue", conversation]));
    assert.equal(catalog().sessions.find(entry => entry.id === session.id).title, originalTitle,
      "The supervisor's default ID label must not replace the saved conversation title");
    let state = await snapshot(session);
    assert.equal(state.capabilities.checkedDialogReplies,1, "The current adapter must load through the installed launcher");
    assert.equal(state.sessionId, conversation);
    assert.deepEqual(state.messages.filter(message => ["user", "assistant"].includes(message.type)).map(message => message.uuid), expectedIds);
    assert.equal(state.turn.submitCount, 0);
    assert.equal(state.turn.isLoading, false);
    const firstPid = session.source.job.pid;
    assert.equal(catalog().statuses[session.id].phase, "available");
    if (parallelResult) {
      const proofPath = join(configuration, "harness-adapter", "continuations", `${conversation}.json`);
      const proof = JSON.parse(readFileSync(proofPath));
      const canonical = value => Array.isArray(value) ? value.map(canonical)
        : value && typeof value === "object" ? Object.fromEntries(Object.keys(value).sort().map(key => [key, canonical(value[key])])) : value;
      const legacy = messages.filter(message => ["user", "assistant"].includes(message.type) && message.uuid !== parallelResult)
        .map(message => canonical({ uuid: message.uuid, type: message.type, role: message.message.role,
          content: typeof message.message.content === "string" ? [{ type: "text", text: message.message.content }] : message.message.content }));
      const legacyProof = { ...proof, verified: null, source_digest: originalSourceDigest,
        message_count: legacy.length, history_digest: createHash("sha256").update(JSON.stringify(legacy)).digest("hex") };
      writeFileSync(proofPath, JSON.stringify(legacyProof));
      assert.equal(catalog().statuses[session.id].phase, "unavailable");
      const before = readFileSync(proofPath, "utf8");
      const checked = success(run(harness, ["--claude-check-history", conversation]));
      assert.equal(checked.previousExpectedMessages, expectedIds.length - 1);
      assert.equal(checked.verifiedMessages, expectedIds.length);
      assert.equal(checked.readOnly, true);
      assert.equal(readFileSync(proofPath, "utf8"), before, "Read-only checking must not mark the worker verified");
      if (process.env.HARNESS_RESUME_REPAIR_GUI === "1") {
        graphical = await isolatedGui(harness, environment, directory);
        const draftKey = `claude:${session.id}`;
        const draft = "Keep this draft while repairing parallel history.";
        const window = await graphical.open({ [draftKey]: draft });
        await graphical.click(window, 145, 48);
        await delay(1500);
        await graphical.capture("parallel-before-open");
        await graphical.click(window, 130, 240);
        const deadline = Date.now() + 15000;
        while (!JSON.parse(readFileSync(proofPath)).verified && Date.now() < deadline) await delay(200);
        await delay(1000);
        await graphical.capture("parallel-repaired");
        assert.ok(JSON.parse(readFileSync(proofPath)).verified, "Selecting the failed conversation must reverify its existing worker");
        assert.equal(JSON.parse(readFileSync(join(graphical.configuration(window), "harness", "drafts.json"))).drafts[draftKey], draft);
        assert.equal(live()[0].source.job.pid, firstPid);
        await graphical.close();
        graphical = undefined;
      }
      session = success(run(harness, ["--claude-continue", conversation]));
      assert.equal(session.source.job.pid, firstPid, "Repair must reuse the already running worker");
      const repaired = JSON.parse(readFileSync(proofPath));
      assert.equal(repaired.message_count, expectedIds.length);
      assert.ok(repaired.verified);
      assert.equal(catalog().statuses[session.id].phase, "available");
      assert.equal((await snapshot(session)).turn.submitCount, 0, "Repair must not send any prompt");
    }
    session = success(run(harness, ["--claude-continue", conversation]));
    assert.equal(session.source.job.pid, firstPid, "Continuing a live verified worker must not restart it");
    await stop();
    const request = () => new Promise((resolveResult, reject) => {
      const child = spawn(resolve(harness), ["--claude-continue", conversation], { env: environment, cwd: workspace, stdio: ["ignore", "pipe", "pipe"] });
      let stdout = "", stderr = "";
      child.stdout.on("data", part => stdout += part);
      child.stderr.on("data", part => stderr += part);
      child.on("error", reject);
      child.on("close", status => resolveResult({ status, stdout, stderr }));
    });
    const competing = await Promise.all([request(), request()]);
    assert.ok(competing.every(result => result.status === 0), JSON.stringify(competing));
    assert.equal(JSON.parse(competing[0].stdout).source.job.pid,JSON.parse(competing[1].stdout).source.job.pid,
      "The second frontend should wait for and reuse the first frontend's verified opening");
    assert.equal(live().length, 1);
    session = live()[0];
    assert.notEqual(session.source.job.pid, firstPid);
    state = await snapshot(session);
    assert.equal(state.turn.submitCount, 0);
    assert.deepEqual(state.messages.filter(message => ["user", "assistant"].includes(message.type)).map(message => message.uuid), expectedIds);
    const recordPath = join(configuration, "harness-adapter", "continuations", `${conversation}.json`);
    const record = JSON.parse(readFileSync(recordPath));
    writeFileSync(recordPath, JSON.stringify({ ...record, verified: null, history_digest: "wrong-history" }));
    assert.equal(catalog().statuses[session.id].phase, "unavailable");
    const failed = run(harness, ["--claude-continue", conversation]);
    assert.notEqual(failed.status, 0);
    assert.match(failed.stderr, /different message identities, order, or contents/);
    assert.equal(live()[0].source.job.pid, session.source.job.pid);
    writeFileSync(recordPath, JSON.stringify({ ...record, verified: null }));
    const verified = success(run(harness, ["--claude-continue", conversation]));
    assert.equal(verified.source.job.pid, session.source.job.pid, "Reverification must not relaunch");
    await stop();
    renameSync(transcript, `${transcript}.held`);
    try {
      assert.notEqual(run(harness, ["--claude-continue", conversation]).status, 0);
      assert.equal(live().length, 0, "Missing history must not wake an empty native session");
    } finally { renameSync(`${transcript}.held`, transcript); }
    if (!onboarding) assert.equal(readFileSync(settingsPath,"utf8"),configuredSettings,
      "Opening through an older launcher must update adapter assets without rewriting user settings");
    console.log(`Isolated resume evidence: ${directory}; conversation ${conversation}; ${expectedIds.length} messages; no credentials copied, no model prompt sent`);
  } finally {
    if (graphical) {
      try { await graphical.capture("failure"); } finally { await graphical.close(); }
    }
    await stop();
    const stopped = run(native, ["daemon", "stop", "--any"]);
    assert.equal(stopped.status, 0, stopped.stderr || stopped.error?.message);
  }
});
