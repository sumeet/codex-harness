import assert from "node:assert/strict";
import { execFile, spawn } from "node:child_process";
import { appendFileSync, mkdirSync, mkdtempSync, realpathSync, statSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { promisify } from "node:util";

const execute = promisify(execFile);

export function guiSandbox(directory, sockets, binary, arguments_, { reportProcess = false } = {}) {
  directory = realpathSync(directory);
  sockets = realpathSync(sockets);
  assert.ok(directory.startsWith("/tmp/") && sockets.startsWith(`${directory}/`));
  const metadata = statSync(directory);
  assert.equal(metadata.uid, process.getuid());
  assert.equal(metadata.mode & 0o077, 0, "GUI fixtures must have a private root");
  // Xvfb -displayfd probes :0 and can unlink a live Xwayland socket. Both
  // filesystem and abstract sockets must be isolated before it can start.
  return ["--unshare-user", "--unshare-net", "--die-with-parent",
    "--ro-bind", "/", "/", "--dev", "/dev", "--tmpfs", "/tmp",
    "--bind", directory, directory, "--bind", sockets, "/tmp/.X11-unix",
    ...(reportProcess ? ["--info-fd", "4"] : []), "--", binary, ...arguments_];
}

export async function isolatedGui(harness, environment, directory) {
  directory = realpathSync(directory);
  const captures = join(directory, "ui");
  mkdirSync(captures, { recursive: true, mode: 0o700 });
  const sockets = mkdtempSync(join(captures, "display-sockets-"));
  const sandbox = (binary, arguments_, options) => guiSandbox(directory, sockets, binary, arguments_, options);
  environment = { ...environment, LIBGL_ALWAYS_SOFTWARE: "1", XDG_CACHE_HOME: join(directory, "cache"),
    __EGL_VENDOR_LIBRARY_FILENAMES: "/usr/share/glvnd/egl_vendor.d/50_mesa.json" };
  for (const name of ["DISPLAY", "WAYLAND_DISPLAY", "XAUTHORITY", "DBUS_SESSION_BUS_ADDRESS"])
    delete environment[name];
  const children = [];
  const launch = async (binary, arguments_, options, name, { nativeVisibility = false } = {}) => {
    if (nativeVisibility) assert.equal(binary, resolve(harness));
    const child = spawn(nativeVisibility ? binary : "bwrap",
      nativeVisibility ? arguments_ : sandbox(binary, arguments_, { reportProcess: true }), {
      ...options, stdio: ["ignore", "pipe", "pipe", "pipe", "pipe"],
    });
    const closed = new Promise(resolveClosed => child.on("close", resolveClosed));
    const record = { child, closed, processId: undefined };
    children.push(record);
    child.on("error", error => appendFileSync(join(captures, `${name}.log`), `${error.stack}\n`, { mode: 0o600 }));
    for (const pipe of [child.stdout, child.stderr]) {
      pipe?.on("data", bytes => appendFileSync(join(captures, `${name}.log`), bytes, { mode: 0o600 }));
    }
    record.processId = nativeVisibility ? child.pid : await new Promise((resolveProcess, reject) => {
      let information = "";
      child.stdio[4].on("data", bytes => information += bytes);
      child.stdio[4].on("end", () => {
        try {
          const processId = JSON.parse(information)["child-pid"];
          assert.ok(Number.isSafeInteger(processId) && processId > 1);
          resolveProcess(processId);
        } catch (error) { reject(new Error(`Could not establish GUI sandbox for ${name}: ${error.message}`)); }
      });
      child.on("error", reject);
    });
    appendFileSync(join(captures, "isolation.jsonl"), JSON.stringify({ name, processId: record.processId,
      sockets, sandboxed: !nativeVisibility, display: options.env.DISPLAY ?? ":0" }) + "\n", { mode: 0o600 });
    return record;
  };
  const close = async () => {
    for (const { child, closed, processId } of children.toReversed()) {
      if (child.exitCode === null && child.signalCode === null) {
        try { processId ? process.kill(processId, "SIGTERM") : child.kill("SIGTERM"); }
        catch (error) { if (error.code !== "ESRCH") throw error; }
      }
      await Promise.race([closed, delay(3000)]);
      if (child.exitCode === null && child.signalCode === null) {
        child.kill("SIGKILL");
        await closed;
      }
    }
  };
  try {
    const { child: display } = await launch("Xvfb", [":0", "-displayfd", "3", "-screen", "0", "1280x900x24", "-nolisten", "tcp"],
      { env: environment }, "display");
    const number = await Promise.race([
      new Promise((resolveDisplay, reject) => {
        let text = "";
        display.stdio[3].on("data", bytes => {
          text += bytes;
          if (text.includes("\n")) resolveDisplay(text.trim());
        });
        display.on("error", reject);
        display.on("exit", code => reject(new Error(`Xvfb exited: ${code}`)));
      }),
      delay(5000).then(() => { throw new Error("Xvfb did not allocate a display"); }),
    ]);
    assert.equal(number, "0");
    const graphical = { ...environment, DISPLAY: `:${number}`, LIBGL_ALWAYS_SOFTWARE: "1" };
    delete graphical.WAYLAND_DISPLAY;
    await launch("xcompmgr", [], { env: graphical }, "compositor");
    const run = (binary, arguments_) => execute("bwrap", sandbox(binary, arguments_), { env: graphical, timeout: 10000 });
    const windows = [];
    const configurations = new Map();
    const open = async (drafts = {}, { restore = false } = {}) => {
      const configuration = join(directory, `frontend-${windows.length}`, "config");
      mkdirSync(join(configuration, "harness"), { recursive: true, mode: 0o700 });
      if (!restore) writeFileSync(join(configuration, "harness", "drafts.json"), JSON.stringify({ drafts }), { mode: 0o600 });
      // Replay skips the Codex connection; selecting Claude still uses the real
      // native catalog and production opening path, without touching Codex state.
      // Native discovery verifies /proc/<pid>/{exe,cwd}; a child user namespace
      // cannot inspect workers outside it. XCB accepts the private socket path
      // directly, without probing the user's filesystem or abstract X0 socket.
      const { child, processId } = await launch(resolve(harness), ["--replay=0"], {
        cwd: directory, env: { ...graphical, DISPLAY: join(sockets, "X0"), XDG_CONFIG_HOME: configuration },
      }, `harness-${windows.length}`, { nativeVisibility: true });
      const deadline = Date.now() + 15000;
      let identifier;
      while (!identifier) {
        assert.ok(Date.now() < deadline, "Harness window did not appear");
        assert.equal(child.exitCode, null, "Harness exited before opening a window");
        try {
          const result = await run("xdotool", ["search", "--all", "--onlyvisible", "--pid", String(processId), "--name", "Harness"]);
          identifier = result.stdout.trim().split("\n").find(Boolean);
        } catch (error) {
          if (error.code !== 1) throw error;
        }
        await delay(100);
      }
      windows.push(identifier);
      configurations.set(identifier, configuration);
      await run("xdotool", ["windowmove", identifier, "0", "0", "windowsize", identifier, "1280", "900", "windowfocus", identifier]);
      await delay(1200);
      return identifier;
    };
    return { close, open, run, sockets, configuration: identifier => configurations.get(identifier),
      capture: async name => { const path = join(captures, `${name}.png`); await run("import", ["-window", "root", path]); return path; },
      click: async (identifier, x, y) => run("xdotool", ["windowraise", identifier, "windowfocus", identifier, "mousemove", "--window", identifier, String(x), String(y), "click", "1"]),
      key: async (identifier, key) => run("xdotool", ["windowfocus", identifier, "key", "--clearmodifiers", key]),
      type: async (identifier, text) => run("xdotool", ["windowfocus", identifier, "type", "--clearmodifiers", text]),
    };
  } catch (error) {
    await close();
    throw error;
  }
}
