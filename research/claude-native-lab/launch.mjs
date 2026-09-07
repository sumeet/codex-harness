import assert from "node:assert/strict";
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, readdirSync, realpathSync, statSync } from "node:fs";
import { join, resolve } from "node:path";
import { homedir } from "node:os";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";
import { parseArgs } from "node:util";
import { discoverBinary } from "./discover.mjs";

const { values, positionals } = parseArgs({
  allowPositionals: true,
  options: {
    "prepare-only": { type: "boolean" },
    patch: { type: "boolean" },
    "probe-unverified": { type: "boolean" },
    "resume-lab": { type: "string" },
  },
});
assert.equal(
  positionals.length,
  1,
  "Usage: node launch.mjs [--prepare-only] [--patch | --probe-unverified] [--resume-lab PREVIOUS_LAB_DIRECTORY] EXACT_NATIVE_CLAUDE_BINARY",
);
assert.ok(!(values.patch && values["probe-unverified"]), "The binary patch is always strictly hash-pinned");
assert.ok(
  !(values["resume-lab"] && values["probe-unverified"]),
  "Probe unverified builds only in a fresh disposable conversation",
);
assert.ok(!process.env.BUN_OPTIONS, "Unset BUN_OPTIONS explicitly before using this isolated lab launcher");
if (!values["prepare-only"])
  assert.ok(
    process.stdin.isTTY && process.stdout.isTTY,
    "Launch from a terminal; this is native interactive Claude, not print/SDK mode",
  );

function alive(pid) {
  assert.ok(Number.isSafeInteger(pid) && pid > 0, "Invalid recorded session PID");
  try {
    process.kill(pid, 0);
    return true;
  } catch (error) {
    if (error.code === "ESRCH") return false;
    throw error;
  }
}

let previous;
if (values["resume-lab"]) {
  const path = realpathSync(values["resume-lab"]);
  assert.match(
    path,
    /^\/tmp\/harness-claude-lab\.[^/]+$/,
    "Only a previous disposable lab can be resumed by this launcher",
  );
  const metadata = statSync(path);
  assert.ok(
    metadata.isDirectory() && metadata.uid === process.getuid() && (metadata.mode & 0o077) === 0,
    "Previous lab directory must be private and owned by this user",
  );
  const preparation = JSON.parse(readFileSync(join(path, "preparation.json"), "utf8"));
  const status = JSON.parse(readFileSync(join(path, "preload-status.json"), "utf8"));
  assert.equal(status.state, "attached", "Previous lab did not attach successfully");
  assert.match(status.sessionId, /^[\da-f]{8}-[\da-f]{4}-[\da-f]{4}-[\da-f]{4}-[\da-f]{12}$/i);
  assert.match(
    realpathSync(preparation.workspace),
    /^\/tmp\/harness-claude-lab\.[^/]+\/workspace$/,
    "Only a disposable lab workspace may be resumed",
  );
  assert.ok(!alive(status.pid), "Stop the previous native Claude process before resuming its conversation");
  const sessions = join(homedir(), ".claude", "sessions");
  for (const name of readdirSync(sessions).filter((name) => /^\d+\.json$/.test(name))) {
    let record;
    try {
      record = JSON.parse(readFileSync(join(sessions, name), "utf8"));
    } catch (error) {
      if (error.code === "ENOENT") continue;
      throw error;
    }
    if (record.sessionId === status.sessionId)
      assert.ok(!alive(record.pid), "Another native process currently owns this conversation");
  }
  previous = { workspace: preparation.workspace, sessionId: status.sessionId };
}

const original = resolve(positionals[0]);
const discovery = discoverBinary(original);
if (!values.patch && !values["probe-unverified"])
  assert.ok(
    discovery.verified,
    "Unverified native build. Use --probe-unverified for an isolated compatibility experiment, not a real session",
  );
const directory = mkdtempSync("/tmp/harness-claude-lab.");
const workspace = previous?.workspace ?? join(directory, "workspace");
if (!previous) mkdirSync(workspace, { mode: 0o700 });
const socketPath = join(directory, "native.sock");
const bridge = fileURLToPath(new URL("bridge.mjs", import.meta.url));
const preload = fileURLToPath(new URL("preload.mjs", import.meta.url));
assert.ok(!/\s/.test(preload), "Bun preload option parsing for paths containing whitespace has not been validated");
let binary = original;
let patchHashes;
if (values.patch) {
  binary = join(directory, "claude-patched");
  const patcher = fileURLToPath(new URL("patch_bridge.mjs", import.meta.url));
  const patch = spawnSync(process.execPath, ["--max-old-space-size=512", patcher, original, binary], {
    encoding: "utf8",
  });
  if (patch.error) throw patch.error;
  if (patch.status !== 0) throw new Error(`Patch failed: ${patch.stderr}`);
  patchHashes = JSON.parse(patch.stdout);
}
const nativeArguments = [
  "--setting-sources",
  "",
  "--settings",
  '{"remoteControlAtStartup":false}',
  "--strict-mcp-config",
  "--mcp-config",
  '{"mcpServers":{}}',
  "--permission-mode",
  "default",
  "--no-chrome",
  "--model",
  "sonnet",
  "--effort",
  "low",
  "--name",
  "harness-native-lab",
  ...(previous ? ["--resume", previous.sessionId] : []),
];
const environment = { ...process.env };
for (const name of ["LD_PRELOAD", "CLAUDECODE", "CLAUDE_CODE_ENTRYPOINT", "DISPLAY", "WAYLAND_DISPLAY"])
  delete environment[name];
Object.assign(environment, {
  DISABLE_AUTOUPDATER: "1",
  HARNESS_CLAUDE_BRIDGE_MODULE: bridge,
  HARNESS_CLAUDE_BRIDGE_SOCKET: socketPath,
  CLAUDE_CODE_ARTIFACT_AUTO_OPEN: "0",
  BROWSER: "/bin/false",
});
if (!values.patch)
  Object.assign(environment, {
    BUN_OPTIONS: `--preload ${preload}`,
    HARNESS_CLAUDE_PRELOAD_SPEC: JSON.stringify(discovery),
    HARNESS_CLAUDE_PRELOAD_STATUS: join(directory, "preload-status.json"),
  });
const preparation = {
  directory,
  workspace,
  binary,
  original,
  mode: values.patch ? "binary-patch" : "stock-preload",
  socketPath,
  bridge,
  discovery,
  patchHashes,
  arguments: nativeArguments,
};
writeFileSync(join(directory, "preparation.json"), JSON.stringify(preparation, null, 2), { flag: "wx", mode: 0o600 });
console.log(JSON.stringify(preparation, null, 2));
if (!values["prepare-only"]) {
  console.log(
    "Only trust this disposable workspace. No prompt is sent automatically. Disconnecting a socket client does not stop Claude; exiting this terminal does.",
  );
  const result = spawnSync(binary, nativeArguments, { cwd: workspace, env: environment, stdio: "inherit" });
  if (result.error) throw result.error;
  if (result.signal) console.error(`Native test process ended with ${result.signal}`);
  process.exitCode = result.status ?? 1;
}
