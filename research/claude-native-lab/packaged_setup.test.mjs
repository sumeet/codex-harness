import assert from "node:assert/strict";
import { mkdtempSync, mkdirSync, readFileSync, writeFileSync, readdirSync, readlinkSync } from "node:fs";
import { join, resolve, dirname } from "node:path";
import { spawn, spawnSync } from "node:child_process";
import { once } from "node:events";
import { setTimeout as delay } from "node:timers/promises";
import { createHash } from "node:crypto";
import test from "node:test";

const executable = process.env.HARNESS_SETUP_BINARY;

test("packaged native setup preserves settings and safely delegates failed hooks", { skip: !executable, timeout:30000 }, async () => {
  const directory = mkdtempSync("/tmp/hs-");
  const configuration = join(directory, "profile");
  const runtime = join(directory, "r");
  for (const path of [configuration, runtime]) mkdirSync(path, { mode: 0o700 });
  const environment = { ...process.env, CLAUDE_CONFIG_DIR: configuration, XDG_RUNTIME_DIR: runtime };
  for (const name of ["LD_PRELOAD", "BUN_OPTIONS", "CLAUDE_CODE_PROCESS_WRAPPER", "CLAUDECODE", "CLAUDE_CODE_ENTRYPOINT"])
    delete environment[name];
  const binary = resolve(executable);
  const settingsPath = join(configuration, "settings.json");
  const original = '{"theme":"dark","env":{"HARNESS_SETUP_SENTINEL":"kept"},"permissions":{"allow":[]}}\n';
  writeFileSync(settingsPath, original, { flag: "wx", mode: 0o600 });
  const setup = (operation, overrides = {}) => spawnSync(binary, ["--claude-setup", operation],
    { env: { ...environment, ...overrides }, encoding: "utf8", timeout: 10000 });
  const success = (result) => {
    assert.equal(result.status, 0, result.stderr || result.error?.message);
    return JSON.parse(result.stdout);
  };
  assert.equal(success(setup("status")).configured, false);
  const prepared = success(setup("prepare"));
  assert.equal(readFileSync(settingsPath, "utf8"), original);
  assert.notEqual(setup("enable", { HARNESS_CLAUDE_BINARY: "/bin/sh" }).status, 0);
  assert.equal(readFileSync(settingsPath, "utf8"), original);
  assert.equal(success(setup("enable")).configured, true);
  const enabled = JSON.parse(readFileSync(settingsPath, "utf8"));
  assert.deepEqual({ ...enabled, processWrapper: undefined }, { ...JSON.parse(original), processWrapper: undefined });
  const [launcher] = JSON.parse(enabled.processWrapper);
  assert.equal(dirname(prepared), dirname(launcher));
  const packageRoot = join(configuration, "harness-adapter");
  const backups = readdirSync(packageRoot).filter((name) => name.startsWith("settings-backup-"));
  assert.equal(backups.length, 1);
  assert.equal(readFileSync(join(packageRoot, backups[0]), "utf8"), original);
  assert.equal(success(setup("enable")).configured, true);

  const endpointRoot = join(runtime, `harness-claude-${createHash("sha256").update(configuration).digest("hex").slice(0, 16)}`);
  const delegated = (arguments_, overrides = {}, launch = launcher) => {
    const result = spawnSync(launch, arguments_, { env: { ...environment, HARNESS_SETUP_SENTINEL: "untouched", ...overrides },
      encoding: "utf8", timeout: 10000 });
    assert.equal(result.status, 0, result.stderr || result.error?.message);
    assert.equal(result.stderr, "");
    assert.equal(result.stdout, `${result.pid}\nuntouched\nfirst with spaces\nsecond'quoted\n`);
  };
  const shell = ["/bin/sh", "-c", 'printf "%s\\n" "$$" "$HARNESS_SETUP_SENTINEL" "$1" "$2"', "fixture", "first with spaces", "second'quoted"];
  delegated(shell);
  delegated(shell, { CLAUDE_CODE_SESSION_KIND: "bg", CLAUDE_JOB_DIR: join(directory, "01234567") });
  assert.equal(JSON.parse(readFileSync(join(endpointRoot, "last-startup.json"), "utf8")).state, "unavailable");
  assert.match(success(setup("status")).detail, /last launch could not connect/);
  delegated(shell, { CLAUDE_CODE_SESSION_KIND: "bg", CLAUDE_JOB_DIR: join(directory, "01234567"), BUN_OPTIONS: "existing-hook" });
  assert.match(JSON.parse(readFileSync(join(endpointRoot, "last-startup.json"), "utf8")).detail, /existing Bun/);
  delegated(["--claude-wrap", join(directory, "missing.json"), ...shell], {}, binary);

  assert.equal(success(setup("disable")).configured, false);
  assert.deepEqual(JSON.parse(readFileSync(settingsPath, "utf8")), JSON.parse(original));
  // A supervisor may still hold the previous launcher after setup is disabled.
  delegated(shell);
  const disabledStatus = readFileSync(join(endpointRoot,"last-startup.json"),"utf8");
  delegated(shell, { CLAUDE_CODE_SESSION_KIND:"bg", CLAUDE_JOB_DIR:join(directory,"01234567") });
  assert.equal(readFileSync(join(endpointRoot,"last-startup.json"),"utf8"),disabledStatus,
    "A cached launcher must respect disabling the integration for future workers");

  success(setup("enable"));
  const lockPath = join(packageRoot,"setup.lock");
  const locker = spawn("flock",["-x",lockPath,process.execPath,"-e",
    'process.stdout.write("locked"); process.stdin.once("data", () => process.exit(0));'],
    {env:environment,stdio:["pipe","pipe","pipe"]});
  const lockerClosed = once(locker,"close");
  let waiting, waitingClosed;
  try {
    await once(locker.stdout,"data");
    waiting = spawn(launcher,shell,{env:{...environment,HARNESS_SETUP_SENTINEL:"untouched",
      CLAUDE_CODE_SESSION_KIND:"bg",CLAUDE_JOB_DIR:join(directory,"01234567")},stdio:["ignore","pipe","pipe"]});
    waitingClosed = once(waiting,"close");
    let stdout = "", stderr = "";
    waiting.stdout.on("data",part => stdout += part);
    waiting.stderr.on("data",part => stderr += part);
    const deadline = Date.now()+1500;
    while (true) {
      const descriptors = readdirSync(`/proc/${waiting.pid}/fd`);
      const reachedLock = descriptors.some(descriptor => {
        try { return readlinkSync(`/proc/${waiting.pid}/fd/${descriptor}`) === lockPath; }
        catch (error) { if (error.code === "ENOENT") return false; throw error; }
      });
      if (reachedLock) break;
      assert.ok(Date.now()<deadline,"The wrapper must reach the held setup lock");
      await delay(5);
    }
    // Complete a settings disable while this fixture holds the setup lock.
    writeFileSync(settingsPath,original);
    locker.stdin.end("release");
    assert.equal((await lockerClosed)[0],0);
    assert.equal((await waitingClosed)[0],0,stderr);
    assert.equal(stderr,"");
    assert.equal(stdout,`${waiting.pid}\nuntouched\nfirst with spaces\nsecond'quoted\n`);
    assert.equal(readFileSync(join(endpointRoot,"last-startup.json"),"utf8"),disabledStatus,
      "A worker waiting behind disable must recheck consent after acquiring the lock");
  } finally {
    if (locker.exitCode === null && locker.signalCode === null) { locker.kill(); await lockerClosed; }
    if (waiting && waiting.exitCode === null && waiting.signalCode === null) { waiting.kill(); await waitingClosed; }
  }
  const corporate = '{"processWrapper":"[\\"/corporate/launcher\\"]","theme":"light"}';
  writeFileSync(settingsPath, corporate);
  assert.notEqual(setup("enable").status, 0);
  assert.notEqual(setup("disable").status, 0);
  assert.equal(readFileSync(settingsPath, "utf8"), corporate);
  console.log(`Isolated packaged setup evidence: ${directory}`);
});
