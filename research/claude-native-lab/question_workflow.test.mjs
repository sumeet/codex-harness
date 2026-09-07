import assert from "node:assert/strict";
import { randomUUID } from "node:crypto";
import { EventEmitter } from "node:events";
import { mkdtempSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import test from "node:test";
import { attach, dispose } from "./bridge.mjs";
import { isolatedGui } from "./gui_probe.mjs";

const harness = process.env.HARNESS_SETUP_BINARY && resolve(process.env.HARNESS_SETUP_BINARY);

function store(initial) {
  let value = initial;
  const events = new EventEmitter();
  return {
    getSnapshot: () => value, getState: () => value,
    set(next) { value = next; events.emit("change"); },
    subscribe(callback) { events.on("change", callback); return () => events.off("change", callback); },
  };
}

test("Harness answers native questions in the shared surface across two windows", {
  skip: !harness || process.env.HARNESS_RESUME_GUI !== "1", timeout: 90000,
}, async () => {
  const directory = mkdtempSync("/tmp/hquestion-");
  const profile = join(directory, "profile"), runtime = join(directory, "runtime"), data = join(directory, "data"), workspace = join(directory, "workspace");
  for (const path of [profile, runtime, data, workspace, join(data,"harness"), join(data,"harness/claude")]) mkdirSync(path, {mode:0o700});
  const identifier = randomUUID(), sessionId = randomUUID(), host = join(runtime, `harness-claude-${identifier}`);
  mkdirSync(host, {mode:0o700});
  writeFileSync(join(data, `harness/claude/${identifier}.json`), JSON.stringify({id:identifier, directory:host, cwd:workspace,
    title:"Question workflow", lifecycle_version:0, created_at_ms:Date.now(), source:{kind:"managed"}}), {mode:0o600});
  const catalog = join(directory, "empty-catalog");
  writeFileSync(catalog, "#!/bin/sh\nprintf '[]\\n'\n", {mode:0o700});
  const environment = {PATH:process.env.PATH, LANG:"C.UTF-8", CLAUDE_CONFIG_DIR:profile, XDG_RUNTIME_DIR:runtime,
    XDG_CONFIG_HOME:join(directory,"config"), XDG_DATA_HOME:data, XDG_STATE_HOME:join(directory,"state"), HARNESS_CLAUDE_BINARY:catalog, BROWSER:"/bin/false"};
  const previousProfile = process.env.CLAUDE_CONFIG_DIR, previousSocket = process.env.HARNESS_CLAUDE_BRIDGE_SOCKET;
  process.env.CLAUDE_CONFIG_DIR = profile;
  process.env.HARNESS_CLAUDE_BRIDGE_SOCKET = join(host, "native.sock");
  const fixture = JSON.parse(readFileSync(new URL("./fixtures/question-dialog.json", import.meta.url)));
  const dialogs = store({open:[fixture]}), resolved = [], events = new EventEmitter();
  const dialogStore = {...dialogs,
    onClosed(callback) {events.on("closed", callback); return () => events.off("closed", callback);},
    answer(id, result) {
      resolved.push({id,result});
      dialogs.set({open:dialogs.getState().open.filter(dialog => dialog.id !== id)});
      events.emit("closed", {id,type:"answered",result});
    },
    dismiss(id) {
      dialogs.set({open:dialogs.getState().open.filter(dialog => dialog.id !== id)});
      events.emit("closed", {id,type:"dismissed"});
    },
  };
  const transport = {
    subscribe(callback) {events.on("request", callback); return () => events.off("request", callback);},
    onUpdate(callback) {events.on("update", callback); return () => events.off("update", callback);},
    onCancel(callback) {events.on("cancel", callback); return () => events.off("cancel", callback);},
  };
  attach({sessionId, agentId:sessionId, transcript:store([]),
    turn:{...store({isLoading:false}), stream:store({}), applyEvent() {},
      _host:{dialogStore, messageQueue:{enqueueReportingAdmission() {throw new Error("This fixture must never submit a prompt");}}}},
    scope:{dialogTransport:transport, store:store({})}});
  let graphical;
  try {
    graphical = await isolatedGui(harness, environment, directory);
    const first = await graphical.open();
    const select = async window => {
      await graphical.click(window,145,48); await delay(700);
      await graphical.click(window,120,180); await delay(700);
    };
    await select(first);
    await graphical.capture("question-open");
    const second = await graphical.open();
    await select(second);
    await graphical.click(second,470,80);
    await graphical.click(second,470,265);
    await graphical.click(first,470,130);
    await graphical.click(first,470,265);
    await graphical.click(first,470,315);
    await graphical.capture("question-selected");
    await graphical.click(first,1160,396); await delay(700);
    assert.equal(resolved.length, 1, "Submit answers must resolve the native dialog");
    assert.deepEqual(resolved[0].result.updatedInput.answers, {
      "Which marker do you prefer?":"Amber", "Which checks should run?":"Unit, Integration",
    });
    await graphical.capture("question-answered");
    await graphical.click(second,1160,396); await delay(300);
    assert.equal(resolved.length, 1, "An answer in one window must retire the other window's controls");
    await graphical.capture("other-window-retired");

    const custom = {...structuredClone(fixture), id:"custom-question"};
    dialogs.set({open:[custom]}); await delay(500);
    await graphical.click(second,470,80);
    await graphical.click(second,470,175); await graphical.type(second,"Graphite");
    await graphical.click(second,470,265);
    await graphical.click(second,470,315);
    await graphical.click(second,470,362); await graphical.type(second,"Manual review");
    await graphical.capture("custom-and-multiple-answers");
    await graphical.key(second,"ctrl+Return"); await delay(500);
    assert.equal(resolved.length, 2);
    assert.deepEqual(resolved[1].result.updatedInput.answers, {
      "Which marker do you prefer?":"Graphite", "Which checks should run?":"Unit, Integration, Manual review",
    });

    const updated = {...structuredClone(fixture), id:"changing-question"};
    dialogs.set({open:[updated]}); await delay(500);
    await graphical.click(second,470,175); await graphical.type(second,"Discarded custom answer");
    await graphical.click(second,470,130);
    await graphical.click(second,470,265);
    updated.payload.questions[0].question = "Which replacement marker do you prefer?";
    updated.payload.input.questions = updated.payload.questions;
    dialogs.set({open:[updated]}); await delay(500);
    await graphical.click(second,1160,416); await delay(300);
    assert.equal(resolved.length, 2, "A changed question must discard the old answers");
    await graphical.capture("changed-question-requires-review");
    await graphical.click(second,470,175); await graphical.type(second,"Another discarded custom answer");
    await graphical.click(second,470,130);
    await graphical.click(second,470,265);
    await graphical.click(second,1160,396); await delay(500);
    assert.equal(resolved.length, 3);
    assert.deepEqual(resolved[2].result.updatedInput.answers, {
      "Which replacement marker do you prefer?":"Amber", "Which checks should run?":"Unit",
    }, "Picking an option after typing a custom answer must clear the custom answer");
    await graphical.capture("updated-question-answered");
    dialogs.set({open:[{...structuredClone(fixture),id:"reconnected-question"}]});
    await graphical.close(); graphical = undefined;
    graphical = await isolatedGui(harness, environment, directory);
    const reconnected = await graphical.open({}, {restore:true}); await select(reconnected);
    await graphical.click(reconnected,470,44);
    for (const key of ["l", "Return", "j", "Return", "l", "Return", "Return"])
      await graphical.key(reconnected,key);
    await graphical.capture("reconnected-keyboard-choices");
    await graphical.key(reconnected,"ctrl+Return"); await delay(500);
    assert.equal(resolved.length, 4);
    assert.deepEqual(resolved[3].result.updatedInput.answers, {
      "Which marker do you prefer?":"Amber", "Which checks should run?":"Unit",
    }, "Reconnection restores a pending question, and keyboard multi-select can deselect an option");
    console.log(JSON.stringify({directory, kind:"real Harness and production socket bridge with a synthetic native controller", modelRequestsSent:0, answers:resolved}));
  } finally {
    if (graphical) {try {await graphical.capture("final");} finally {await graphical.close();}}
    await dispose();
    if (previousProfile === undefined) delete process.env.CLAUDE_CONFIG_DIR; else process.env.CLAUDE_CONFIG_DIR = previousProfile;
    if (previousSocket === undefined) delete process.env.HARNESS_CLAUDE_BRIDGE_SOCKET; else process.env.HARNESS_CLAUDE_BRIDGE_SOCKET = previousSocket;
    console.log(`Private question UI evidence: ${directory}`);
  }
});
