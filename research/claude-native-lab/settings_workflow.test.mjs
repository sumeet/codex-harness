import assert from "node:assert/strict";
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, existsSync } from "node:fs";
import { join, resolve } from "node:path";
import { spawn } from "node:child_process";
import { setTimeout as delay } from "node:timers/promises";
import test from "node:test";
import { discoverBinary } from "./discover.mjs";
import { NativeClient } from "./client.mjs";

const native = process.env.HARNESS_NATIVE_TEST_BINARY;

test("native session settings use the terminal's own control handlers", { skip: !native, timeout: 40000 }, async () => {
  const directory = mkdtempSync("/tmp/hsettings-");
  const configuration = join(directory, "profile");
  const workspace = join(directory, "workspace");
  const runtime = join(directory, "runtime");
  for (const path of [configuration, workspace, runtime]) mkdirSync(path, { mode: 0o700 });
  const discovery = discoverBinary(resolve(native));
  assert.ok(discovery.verified);
  const statusPath = join(directory, "preload.json");
  const socketPath = join(directory, "native.sock");
  writeFileSync(join(configuration, "settings.json"), JSON.stringify({ remoteControlAtStartup: false }), { mode: 0o600 });
  writeFileSync(join(configuration, ".claude.json"), JSON.stringify({ hasCompletedOnboarding: true,
    projects: { [workspace]: { hasTrustDialogAccepted: true } } }), { mode: 0o600 });
  const environment = { ...process.env, CLAUDE_CONFIG_DIR: configuration, XDG_RUNTIME_DIR: runtime,
    DISABLE_AUTOUPDATER: "1", BROWSER: "/bin/false", CLAUDE_CODE_ARTIFACT_AUTO_OPEN: "0",
    HARNESS_CLAUDE_BRIDGE_MODULE: resolve("research/claude-native-lab/bridge.mjs"),
    HARNESS_CLAUDE_BRIDGE_SOCKET: socketPath, HARNESS_CLAUDE_PRELOAD_STATUS: statusPath,
    HARNESS_CLAUDE_PRELOAD_SPEC: JSON.stringify(discovery),
    BUN_OPTIONS: `--preload ${resolve("research/claude-native-lab/preload.mjs")}` };
  for (const name of ["LD_PRELOAD", "CLAUDE_CODE_PROCESS_WRAPPER", "CLAUDECODE", "CLAUDE_CODE_ENTRYPOINT", "DISPLAY", "WAYLAND_DISPLAY"])
    delete environment[name];
  const quote = value => `'${value.replaceAll("'", "'\\''")}'`;
  const command = [resolve(native), "--model", "sonnet", "--permission-mode", "default", "--strict-mcp-config", "--mcp-config", '{"mcpServers":{}}', "--no-chrome"].map(quote).join(" ");
  const terminal = spawn("/usr/bin/script", ["-q", "-e", "-c", command, "/dev/null"],
    { cwd: workspace, env: environment, detached: true, stdio: ["pipe", "pipe", "pipe"] });
  const closed = new Promise(resolveClosed => terminal.on("close", resolveClosed));
  let output = "";
  for (const stream of [terminal.stdout, terminal.stderr]) stream.on("data", part => output += part);
  let client;
  try {
    const deadline = Date.now() + 20000;
    while (!existsSync(socketPath) && Date.now() < deadline) await delay(100);
    assert.ok(existsSync(socketPath), `No adapter socket; inspect ${directory}`);
    client = new NativeClient(socketPath);
    const description = await client.request("describe");
    writeFileSync(join(directory, "description.json"), JSON.stringify(description, null, 2), { mode: 0o600 });
    const settings = await client.request("settings");
    writeFileSync(join(directory, "settings.json"), JSON.stringify(settings, null, 2), { mode: 0o600 });
    assert.equal(settings.model, "sonnet");
    assert.equal(settings.permissionMode, "default");
    const changed = await client.request("set_setting", { key: "model", value: "haiku", expected: "sonnet" });
    assert.equal(changed.settings.model, "haiku");
    await assert.rejects(client.request("set_setting", { key: "model", value: "sonnet", expected: "sonnet" }), /changed in another window/);
    const planned = await client.request("set_setting", { key: "permissionMode", value: "plan", expected: "default" });
    assert.equal(planned.settings.permissionMode, "plan");
    const edits = await client.request("set_setting", { key: "permissionMode", value: "acceptEdits", expected: "plan" });
    assert.equal(edits.settings.permissionMode, "acceptEdits");
    await assert.rejects(client.request("set_setting", { key: "permissionMode", value: "bypassPermissions", expected: "acceptEdits" }), /dangerously-skip-permissions/);
    const sonnet = await client.request("set_setting", { key: "model", value: "sonnet", expected: "haiku" });
    assert.equal(sonnet.settings.model, "sonnet");
    const effort = await client.request("set_setting", { key: "effort", value: "low", expected: sonnet.settings.effort });
    assert.deepEqual(effort.settings.effort, { kind: "level", value: "low" });
    const automatic = await client.request("set_setting", { key: "effort", value: "auto", expected: effort.settings.effort });
    assert.deepEqual(automatic.settings.effort, { kind: "default" });
    assert.deepEqual(JSON.parse(readFileSync(join(configuration, "settings.json"), "utf8")), { remoteControlAtStartup: false });
    const snapshot = await client.request("snapshot");
    assert.equal(snapshot.turn.submitCount, 0);
    console.log(`Native settings evidence: ${directory}`);
  } finally {
    client?.close();
    writeFileSync(join(directory, "terminal.log"), output, { mode: 0o600 });
    if (terminal.exitCode === null && terminal.signalCode === null) {
      try { process.kill(-terminal.pid, "SIGTERM"); } catch (error) { if (error.code !== "ESRCH") throw error; }
      await Promise.race([closed, delay(3000)]);
      if (terminal.exitCode === null && terminal.signalCode === null) {
        try { process.kill(-terminal.pid, "SIGKILL"); } catch (error) { if (error.code !== "ESRCH") throw error; }
        await closed;
      }
    }
  }
});
