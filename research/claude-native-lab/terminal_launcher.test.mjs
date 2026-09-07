import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";
import test from "node:test";

const launcher = process.env.HARNESS_TERMINAL_LAUNCHER_TEST_PATH;
const oldLauncher = process.env.HARNESS_TERMINAL_LAUNCHER_BEFORE_PATH;

test("terminal launcher preserves caller input and argv while selecting an installed executable", {
  skip: !launcher, timeout: 15000,
}, () => {
  const directory = mkdtempSync("/tmp/harness-launcher-test-");
  const data = join(directory, "data with spaces");
  const versions = join(data, "claude", "versions");
  mkdirSync(versions, { recursive: true, mode: 0o700 });
  const standIn = `#!${process.execPath}
let input = "";
const report = () => process.stdout.write(JSON.stringify({ executable: process.argv[1], arguments: process.argv.slice(2), terminal: Boolean(process.stdin.isTTY), input, sentinel: process.env.HARNESS_LAUNCHER_TEST_SENTINEL }) + "\\n");
if (process.stdin.isTTY) report();
else { process.stdin.setEncoding("utf8"); process.stdin.on("data", part => input += part); process.stdin.on("end", report); }
`;
  for (const version of ["2.1.263", "2.1.243-musl", "2.1.243", "2.1.153", "2.1.108", "2.1.87", "9.9.9-musl"])
    writeFileSync(join(versions, version), standIn, { mode: 0o700 });
  writeFileSync(join(versions, "9.9.8"), "", { mode: 0o700 });
  writeFileSync(join(versions, "9.9.7"), standIn, { mode: 0o600 });
  mkdirSync(join(versions, "9.9.6"), { mode: 0o700 });
  const environment = {
    PATH: process.env.PATH, LANG: "C.UTF-8", TERM: "xterm-256color",
    XDG_DATA_HOME: data, HARNESS_LAUNCHER_TEST_SENTINEL: "preserve unrelated environment",
  };
  const arguments_ = ["--model", "stand-in-only", "argument with spaces"];
  const input = "caller-supplied input\nsecond line\n";
  const run = path => spawnSync(resolve(path), arguments_, {
    env: environment, input, encoding: "utf8", timeout: 5000,
  });
  if (oldLauncher) {
    const before = run(oldLauncher);
    assert.ifError(before.error);
    assert.equal(before.status, 0, before.stderr);
    const observed = JSON.parse(before.stdout);
    assert.notEqual(observed.input, input);
    assert.ok(observed.input.includes(join(versions, "2.1.243-musl")));
    assert.ok(observed.input.includes("\0"));
    console.log("Reproduced old launcher feeding installed-version paths to its child");
  }
  const result = run(launcher);
  assert.ifError(result.error);
  assert.equal(result.status, 0, result.stderr);
  const expected = {
    executable: join(versions, "2.1.263"), arguments: arguments_, terminal: false, input,
    sentinel: environment.HARNESS_LAUNCHER_TEST_SENTINEL,
  };
  assert.deepEqual(JSON.parse(result.stdout), expected);
  const quote = value => `'${value.replaceAll("'", "'\\''")}'`;
  const terminal = spawnSync("/usr/bin/script", ["-qefc", `exec ${[resolve(launcher), ...arguments_].map(quote).join(" ")}`, "/dev/null"], {
    env: environment, input: "", encoding: "utf8", timeout: 5000,
  });
  assert.ifError(terminal.error);
  assert.equal(terminal.status, 0, terminal.stderr);
  const output = terminal.stdout.split(/\r?\n/).find(line => line.startsWith("{"));
  assert.ok(output, terminal.stdout);
  assert.deepEqual(JSON.parse(output), { ...expected, terminal: true, input: "" });
  const emptyData = join(directory, "empty-data");
  mkdirSync(join(emptyData, "claude", "versions"), { recursive: true, mode: 0o700 });
  const absent = spawnSync(resolve(launcher), [], {
    env: { ...environment, XDG_DATA_HOME: emptyData }, input: "", encoding: "utf8", timeout: 5000,
  });
  assert.ifError(absent.error);
  assert.equal(absent.status, 127);
  assert.match(absent.stderr, /no installed executable found/);
  console.log(`Launcher evidence: ${directory}; no Claude executable or credentials used`);
});
