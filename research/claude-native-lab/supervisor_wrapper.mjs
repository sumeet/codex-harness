import assert from "node:assert/strict";
import { appendFileSync, readFileSync, realpathSync, statSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { discoverBinary } from "./discover.mjs";

export function processStartTicks(stat) {
  const end = stat.lastIndexOf(") ");
  assert.ok(end > 0, "Invalid Linux process stat");
  const ticks = stat.slice(end + 2).trim().split(/\s+/)[19];
  assert.match(ticks ?? "", /^\d+$/, "Missing process start time");
  return ticks;
}

export function processIdentity(pid) {
  assert.ok(Number.isSafeInteger(pid) && pid > 0, "Invalid worker PID");
  const directory = `/proc/${pid}`;
  const startTicks = processStartTicks(readFileSync(join(directory, "stat"), "utf8"));
  const identity = {
    bootId: readFileSync("/proc/sys/kernel/random/boot_id", "utf8").trim(),
    startTicks,
    executable: realpathSync(join(directory, "exe")),
    cwd: realpathSync(join(directory, "cwd")),
    uid: statSync(directory).uid,
  };
  assert.equal(processStartTicks(readFileSync(join(directory, "stat"), "utf8")), startTicks, "Process changed during inspection");
  return identity;
}

export function launchRole(arguments_, environment) {
  if (arguments_[0] === "--bg-pty-host") return "terminal-host";
  if (arguments_[0] === "--bg-spare") return "standby";
  if (arguments_[0] === "daemon") return "supervisor";
  if (environment.CLAUDE_CODE_SESSION_KIND === "bg" && environment.CLAUDE_JOB_DIR)
    return "worker";
  return "helper";
}

export function readExperiment(directory) {
  directory = realpathSync(directory);
  assert.match(directory, /^\/tmp\/harness-claude-supervisor\.[^/]+$/);
  const metadata = statSync(directory);
  assert.ok(metadata.isDirectory() && metadata.uid === process.getuid() && (metadata.mode & 0o077) === 0);
  const configuration = JSON.parse(readFileSync(join(directory, "experiment.json"), "utf8"));
  assert.equal(configuration.directory, directory);
  assert.equal(realpathSync(configuration.workspace), join(directory, "workspace"));
  return configuration;
}

export function workerEnvironment(directory, workspace, discovery, environment) {
  assert.ok(!environment.BUN_OPTIONS, "An existing Bun startup hook must be composed explicitly");
  assert.equal(discovery.verified, true, `Unsupported native worker hash ${discovery.sha256}`);
  const preload = fileURLToPath(new URL("supervisor_preload.mjs", import.meta.url));
  assert.ok(!/\s/.test(preload), "Preload paths containing whitespace need separate validation");
  return {
    ...environment,
    BUN_OPTIONS: `--preload ${preload}`,
    HARNESS_CLAUDE_SUPERVISOR_BOOTSTRAP: JSON.stringify({ directory, workspace, discovery }),
  };
}

export function prepareWrappedLaunch(directory, command, environment = process.env) {
  assert.ok(command.length > 0, "Expected the original Claude command");
  const configuration = readExperiment(directory);
  directory = configuration.directory;
  const role = launchRole(command.slice(1), environment);
  const scoped = realpathSync(process.cwd()) === realpathSync(configuration.workspace);
  const entry = { pid: process.pid, role, scoped, executable: command[0], at: Date.now() };
  let inherited = { ...environment };
  if ((role === "worker" && scoped) || role === "standby") {
    assert.ok(!environment.BUN_OPTIONS, "An existing Bun startup hook must be composed explicitly");
    const discovery = discoverBinary(command[0]);
    inherited = workerEnvironment(directory, configuration.workspace, discovery, environment);
    Object.assign(entry, { bootstrap: true, sha256: discovery.sha256 });
  }
  appendFileSync(join(directory, "launches.jsonl"), JSON.stringify(entry) + "\n", { mode: 0o600 });
  return { command, environment: inherited, entry };
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  try {
    const [directory, ...command] = process.argv.slice(2);
    const launch = prepareWrappedLaunch(directory, command);
    // exec preserves the PID tracked by Claude's native supervisor.
    process.execve(command[0], command, launch.environment);
  } catch (error) {
    process.stderr.write(`Harness supervisor wrapper refused launch: ${error.message}\n`);
    process.exitCode = 1;
  }
}
