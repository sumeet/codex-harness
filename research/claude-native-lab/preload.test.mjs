import assert from "node:assert/strict";
import { test } from "node:test";
import { registrySpecification } from "./discover.mjs";
import { findController, controllerHandles, writeStatus } from "./preload.mjs";
import { mkdtempSync, readFileSync, readdirSync, rmSync, statSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

test("status publication atomically replaces complete private snapshots", () => {
  const directory = mkdtempSync(join(tmpdir(), "harness-claude-status-test-"));
  try {
    const path = join(directory, "status.json");
    for (let sequence = 0; sequence < 20; sequence++) {
      writeStatus(path, { state: "attached", sequence });
      assert.deepEqual(JSON.parse(readFileSync(path, "utf8")), { state: "attached", sequence });
      assert.equal(statSync(path).mode & 0o777, 0o600);
    }
    assert.deepEqual(readdirSync(directory), ["status.json"]);
    assert.throws(() => writeStatus(directory, {}));
    assert.deepEqual(readdirSync(directory), ["status.json"]);
  } finally { rmSync(directory, { recursive: true }); }
});

function fixture() {
  const transcript = {
    getSnapshot() {
      return [];
    },
    subscribe() {},
  };
  const dialogStore = { getState() {}, subscribe() {}, onClosed() {}, answer() {}, dismiss() {} };
  const messageQueue = { enqueueReportingAdmission() {} };
  const turn = {
    getSnapshot() {},
    subscribe() {},
    applyEvent() {},
    cancel() {},
    markSubmit() {},
    resetTiming() {},
    guard: { isActive: false },
    stream: { getSnapshot() {}, subscribe() {}, setUserInputOnProcessing() {} },
    _host: { dialogStore, messageQueue },
  };
  const scope = {
    computeTools() {},
    store: { getState() {} },
    dialogTransport: { subscribe() {}, onUpdate() {}, onCancel() {} },
  };
  const session = {
    id: "native-session",
    identity: {
      mainAgentId(id) {
        return id;
      },
    },
  };
  return { turn, transcript, scope, session, dialogStore, messageQueue };
}

test("registry discovery follows structure instead of chunk or minified symbol names", () => {
  for (const [name, getter, alias] of [
    ["chunk-one", "Hs", "Hs"],
    ["renamed-build", "$a", "differentExport"],
  ]) {
    const source = `class Registry extends Map { claimForStandaloneRender(){} get pendingStandaloneRender(){} } function ${getter}(){return pool.of(root().host)} export {${getter} as ${alias}};`;
    assert.deepEqual(registrySpecification([{ name, source }]), {
      registryModule: name,
      registryKind: "host-map-getter",
      registryExport: alias,
    });
  }
});

test("registry discovery rejects missing, duplicated, or ambiguous getter candidates", () => {
  assert.throws(() => registrySpecification([]), /found 0/);
  const candidate = {
    name: "test",
    source:
      "class R extends Map {claimForStandaloneRender(){} get pendingStandaloneRender(){}} function get(){return slot.of(root().host)} export {get};",
  };
  assert.throws(() => registrySpecification([candidate, candidate]), /found 2/);
  assert.throws(
    () =>
      registrySpecification([
        { ...candidate, source: candidate.source + "function other(){return pool.of(root().host)}" },
      ]),
    /Ambiguous/,
  );
});

test("older direct Map export is discovery evidence, not a compatibility assertion", () => {
  assert.deepEqual(
    registrySpecification([
      {
        name: "older",
        source:
          "class R extends Map {claimForStandaloneRender(){} get pendingStandaloneRender(){}} var value; export {value as registry};",
      },
    ]),
    { registryModule: "older", registryKind: "map", registryExports: ["registry"] },
  );
});

test("controller discovery is independent of component names and hook positions", () => {
  const controller = fixture();
  let hook = { memoizedState: { current: controller }, next: null };
  for (let index = 0; index < 400; index++) hook = { memoizedState: [0, []], next: hook };
  const root = { memoizedState: hook, child: { memoizedProps: { value: controller } } };
  root.sibling = root;
  assert.equal(findController(root), controller);
  assert.equal(findController({ memoizedState: { memoizedState: [controller, []] } }), controller);
});

test("ambiguous controllers are rejected instead of selecting an arbitrary session", () => {
  assert.throws(
    () => findController({ memoizedProps: { value: fixture() }, child: { memoizedProps: { value: fixture() } } }),
    /Multiple/,
  );
});

test("runtime capabilities and native ownership must match before enabling control", () => {
  const controller = fixture();
  assert.equal(controllerHandles(controller).agentId, "native-session");
  controller.turn._host.messageQueue = { enqueueReportingAdmission() {} };
  assert.throws(() => controllerHandles(controller), /does not own/);
  controller.turn._host.messageQueue = controller.messageQueue;
  delete controller.scope.dialogTransport.onCancel;
  assert.throws(() => controllerHandles(controller), /missing onCancel/);
});

test("a reused controller's changed native session identity is read afresh", () => {
  const controller = fixture();
  const before = controllerHandles(controller);
  controller.session.id = "new-session";
  const after = controllerHandles(controller);
  assert.equal(before.sessionId, "native-session");
  assert.equal(after.sessionId, "new-session");
});
