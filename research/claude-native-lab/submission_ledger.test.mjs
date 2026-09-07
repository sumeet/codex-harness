import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { randomUUID } from "node:crypto";
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, statSync, symlinkSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import test from "node:test";
import { SubmissionLedger } from "./bridge.mjs";

if (process.argv[2] === "--claim") {
  const [, , , directory, session, uuid, behavior] = process.argv;
  try {
    const ledger = new SubmissionLedger(directory, session);
    const previous = ledger.claim(uuid, "one logical prompt");
    if (previous) console.log(JSON.stringify({ claimed: false, previous }));
    else {
      if (behavior === "crash-before-admission") process.kill(process.pid, "SIGKILL");
      writeFileSync(join(directory, `admission-${process.pid}`), uuid, { flag: "wx", mode: 0o600 });
      if (behavior === "crash-after-admission") process.kill(process.pid, "SIGKILL");
      ledger.finish(uuid, { state: "accepted" });
      if (behavior === "crash-after-receipt") process.kill(process.pid, "SIGKILL");
      console.log(JSON.stringify({ claimed: true }));
    }
  } catch (error) {
    console.log(JSON.stringify({ claimed: false, error: error.message }));
  }
} else {
  const script = fileURLToPath(import.meta.url);
  const fixture = () => ({ directory: mkdtempSync("/tmp/harness-claude-ledger-"), session: randomUUID(), uuid: randomUUID() });

  test("durable file operations work in the installed native Claude runtime before its CLI starts", {skip: !process.env.HARNESS_NATIVE_TEST_BINARY}, () => {
    const { directory, session, uuid } = fixture();
    const preload = join(directory,"runtime-probe.mjs");
    const bridge = pathToFileURL(fileURLToPath(new URL("bridge.mjs",import.meta.url))).href;
    writeFileSync(preload, `import { SubmissionLedger } from ${JSON.stringify(bridge)};
      const ledger = new SubmissionLedger(${JSON.stringify(directory)}, ${JSON.stringify(session)});
      if (ledger.claim(${JSON.stringify(uuid)}, "offline runtime fixture") !== null) throw new Error("unexpected existing claim");
      ledger.finish(${JSON.stringify(uuid)}, {state:"accepted"});
      const recovered = new SubmissionLedger(${JSON.stringify(directory)}, ${JSON.stringify(session)}).status(${JSON.stringify(uuid)});
      console.log(JSON.stringify({runtime:typeof Bun, state:recovered.state, modelRequestsSent:0}));
      process.exit(0);
    `,{mode:0o600});
    const child = spawnSync(process.env.HARNESS_NATIVE_TEST_BINARY,["--version"],{encoding:"utf8",timeout:10000,
      env:{PATH:process.env.PATH,LANG:"C.UTF-8",BUN_OPTIONS:`--preload ${preload}`,CLAUDE_CONFIG_DIR:directory,DISABLE_AUTOUPDATER:"1",BROWSER:"/bin/false"}});
    assert.equal(child.status,0,child.stderr);
    assert.deepEqual(JSON.parse(child.stdout),{runtime:"object",state:"accepted",modelRequestsSent:0});
  });

  test("a completed receipt survives process death and rejects conflicting text", () => {
    const { directory, session, uuid } = fixture();
    const child = spawnSync(process.execPath, [script, "--claim", directory, session, uuid, "crash-after-receipt"]);
    assert.equal(child.signal, "SIGKILL");
    const ledger = new SubmissionLedger(directory, session);
    assert.equal(ledger.claim(uuid, "one logical prompt").state, "accepted");
    assert.throws(() => ledger.claim(uuid, "different text"), /different text/);
    assert.equal(statSync(join(ledger.path(uuid), "request.json")).mode & 0o777, 0o600);
    assert.equal(JSON.parse(readFileSync(join(ledger.path(uuid), "request.json"))).text, "one logical prompt");
  });

  for (const boundary of ["crash-before-admission", "crash-after-admission"]) {
    test(`${boundary}: an incomplete operation must never be claimed again`, () => {
      const { directory, session, uuid } = fixture();
      const child = spawnSync(process.execPath, [script, "--claim", directory, session, uuid, boundary]);
      assert.equal(child.signal, "SIGKILL");
      const ledger = new SubmissionLedger(directory, session);
      assert.equal(ledger.status(uuid).state, "uncertain");
      assert.equal(ledger.claim(uuid, "one logical prompt").state, "uncertain");
    });
  }

  test("eight processes competing for one UUID get at most one admission", async () => {
    const { directory, session, uuid } = fixture();
    const results = await Promise.all(Array.from({ length: 8 }, () => new Promise((resolve, reject) => {
      const child = spawn(process.execPath, [script, "--claim", directory, session, uuid, "complete"], { stdio: ["ignore", "pipe", "pipe"] });
      let output = "";
      child.stdout.on("data", bytes => { output += bytes; });
      child.on("error", reject);
      child.on("close", code => {
        try { assert.equal(code, 0); resolve(JSON.parse(output)); } catch (error) { reject(error); }
      });
    })));
    assert.equal(results.filter(result => result.claimed).length, 1);
    assert.equal(new SubmissionLedger(directory, session).status(uuid).state, "accepted");
  });

  test("missing, partial, and unsafe records fail closed", () => {
    const { directory, session } = fixture();
    const ledger = new SubmissionLedger(directory, session);
    const missing = randomUUID();
    assert.equal(ledger.status(missing).state, "unknown");
    mkdirSync(ledger.path(missing), { mode: 0o700 });
    assert.equal(ledger.claim(missing, "one logical prompt").state, "uncertain");
    writeFileSync(join(ledger.path(missing), "request.json"), "{partial", { mode: 0o600 });
    assert.throws(() => ledger.claim(missing, "one logical prompt"));
    const substituted = randomUUID();
    symlinkSync(ledger.path(missing), ledger.path(substituted));
    assert.throws(() => ledger.claim(substituted, "one logical prompt"), /Unsafe/);
    assert.throws(() => ledger.claim("../escape", "one logical prompt"), /UUID/);
  });

  test("different IDs allow intentionally repeated text; profiles and sessions remain separate", () => {
    const { directory, session, uuid } = fixture();
    const ledger = new SubmissionLedger(directory, session);
    assert.equal(ledger.claim(uuid, "repeat"), null);
    ledger.finish(uuid, { state: "accepted" });
    assert.equal(ledger.claim(randomUUID(), "repeat"), null);
    assert.equal(new SubmissionLedger(directory, randomUUID()).claim(uuid, "repeat"), null);
    assert.equal(new SubmissionLedger(fixture().directory, session).claim(uuid, "repeat"), null);
  });
}
