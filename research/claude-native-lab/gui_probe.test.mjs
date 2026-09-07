import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { lstatSync, mkdirSync, mkdtempSync, readlinkSync, statSync } from "node:fs";
import { join } from "node:path";
import { promisify } from "node:util";
import test from "node:test";
import { guiSandbox, isolatedGui } from "./gui_probe.mjs";

const execute = promisify(execFile);
const enabled = process.env.HARNESS_RESUME_GUI === "1";
const socketIdentity = path => {
  try {
    const metadata = lstatSync(path);
    return { inode: metadata.ino, device: metadata.dev, mode: metadata.mode,
      link: metadata.isSymbolicLink() ? readlinkSync(path) : null };
  } catch (error) {
    if (error.code === "ENOENT") return null;
    throw error;
  }
};
const desktopIdentity = () => ["/tmp/.X11-unix/X0", "/tmp/.X11-unix/X0_", "/tmp/.X0-lock"].map(socketIdentity);

test("missing sandbox never falls back to a host X server", async () => {
  const before = desktopIdentity();
  const directory = mkdtempSync("/tmp/hgi-");
  await assert.rejects(isolatedGui("/bin/false", { PATH: "/no-gui-tools-here" }, directory), /spawn bwrap ENOENT/);
  assert.deepEqual(desktopIdentity(), before);
});

test("GUI sandbox isolates both socket namespaces before starting an X server", { skip: !enabled }, async () => {
  const directory = mkdtempSync("/tmp/hgi-");
  const sockets = join(directory, "sockets");
  mkdirSync(sockets, { mode: 0o700 });
  const before = desktopIdentity();
  const { stdout } = await execute("bwrap", guiSandbox(directory, sockets, process.execPath, ["--input-type=module", "-e", `
    import { statSync, readlinkSync, readdirSync } from "node:fs";
    const metadata = statSync("/tmp/.X11-unix");
    console.log(JSON.stringify({ network:readlinkSync("/proc/self/ns/net"), mounts:readlinkSync("/proc/self/ns/mnt"),
      device:metadata.dev, inode:metadata.ino, sockets:readdirSync("/tmp/.X11-unix") }));
  `]), { timeout: 5000 });
  const result = JSON.parse(stdout);
  assert.notEqual(result.network, readlinkSync("/proc/self/ns/net"));
  assert.notEqual(result.mounts, readlinkSync("/proc/self/ns/mnt"));
  assert.equal(result.device, statSync(sockets).dev);
  assert.equal(result.inode, statSync(sockets).ino);
  assert.deepEqual(result.sockets, []);
  assert.deepEqual(desktopIdentity(), before);
});

test("simultaneous test displays and their cleanup preserve desktop sockets", { skip: !enabled, timeout: 20000 }, async () => {
  const before = desktopIdentity();
  const displays = [];
  try {
    for (let index = 0; index < 2; index++) {
      const directory = mkdtempSync("/tmp/hgi-");
      const display = await isolatedGui("/bin/false", { PATH: process.env.PATH, LANG: "C.UTF-8" }, directory);
      displays.push(display);
      assert.ok(statSync(join(display.sockets, "X0")).isSocket());
      const information = await display.run("xdpyinfo", []);
      assert.match(information.stdout, /dimensions:\s+1280x900/);
      assert.deepEqual(desktopIdentity(), before);
    }
    assert.notEqual(statSync(join(displays[0].sockets, "X0")).ino, statSync(join(displays[1].sockets, "X0")).ino);
  } finally {
    for (const display of displays) await display.close();
  }
  for (const display of displays) {
    await assert.rejects(display.run("xdpyinfo", []), /unable to open display/);
  }
  assert.deepEqual(desktopIdentity(), before);
});
