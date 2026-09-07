import assert from "node:assert/strict";
import { test } from "node:test";
import { mkdtempSync, statSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { EventEmitter, once } from "node:events";
import { randomUUID } from "node:crypto";
import { setTimeout as delay } from "node:timers/promises";
import { attach, dispose } from "./bridge.mjs";
import { NativeClient } from "./client.mjs";
import net from "node:net";

function store(initial) {
  let value = initial;
  const events = new EventEmitter();
  return {
    getSnapshot: () => value,
    getState: () => value,
    subscribe: (callback) => {
      events.on("change", callback);
      return () => events.off("change", callback);
    },
    set: (next) => {
      value = next;
      events.emit("change");
    },
  };
}

test("native socket adapter contracts without starting Claude or using credentials", async (context) => {
  const directory = mkdtempSync(join(tmpdir(), "harness-claude-bridge-test-"));
  const socketPath = join(directory, "native.sock");
  process.env.HARNESS_CLAUDE_BRIDGE_SOCKET = socketPath;
  const priorConfiguration = process.env.CLAUDE_CONFIG_DIR;
  process.env.CLAUDE_CONFIG_DIR = directory;
  const queued = [];
  const resolved = [];
  const applied = [];
  let admission = { admitted: true };
  let cancellations = 0;
  let submissions = 0;
  const transcript = store([]);
  const turn = {
    ...store({ isLoading: false, lastQueryCompletionTime: 0 }),
    guard: { isActive: false },
    stream: { ...store({ streamingToolUses: [] }), setUserInputOnProcessing() {} },
    _host: {
      messageQueue: {
        enqueueReportingAdmission: (command) => {
          if (admission.admitted) queued.push(command);
          return admission;
        },
      },
    },
    applyEvent: (event) => applied.push(event),
    cancel: () => {
      cancellations++;
    },
    markSubmit: () => {
      submissions++;
    },
    resetTiming() {},
  };
  const dialogEvents = new EventEmitter();
  const dialogTransport = {
    subscribe: (callback) => {
      dialogEvents.on("request", callback);
      return () => dialogEvents.off("request", callback);
    },
    onUpdate: (callback) => {
      dialogEvents.on("update", callback);
      return () => dialogEvents.off("update", callback);
    },
    onCancel: (callback) => {
      dialogEvents.on("cancel", callback);
      return () => dialogEvents.off("cancel", callback);
    },
    reply: (value) => resolved.push(value),
  };
  const nativeDialogs = store({ open: [] });
  const closed = new EventEmitter();
  const dialogStore = {
    getState: nativeDialogs.getState,
    subscribe: nativeDialogs.subscribe,
    onClosed: (callback) => {
      closed.on("closed", callback);
      return () => closed.off("closed", callback);
    },
    answer(id, result) {
      nativeDialogs.set({ open: nativeDialogs.getState().open.filter((dialog) => dialog.id !== id) });
      closed.emit("closed", { id, type: "answered", result });
    },
    dismiss(id) {
      nativeDialogs.set({ open: nativeDialogs.getState().open.filter((dialog) => dialog.id !== id) });
      closed.emit("closed", { id, type: "dismissed" });
    },
  };
  // This is Claude's native dialog-host wiring: answering the store closes the UI, then resolves the transport.
  dialogEvents.on("request", (dialog) => nativeDialogs.set({ open: [...nativeDialogs.getState().open, dialog] }));
  dialogEvents.on("update", (update) =>
    nativeDialogs.set({
      open: nativeDialogs
        .getState()
        .open.map((dialog) => (dialog.id === update.id ? { ...dialog, payload: update.payload } : dialog)),
    }),
  );
  closed.on("closed", (event) =>
    dialogTransport.reply(
      event.type === "answered" ? { id: event.id, result: event.result } : { id: event.id, cancelled: true },
    ),
  );
  turn._host.dialogStore = dialogStore;
  const originalApply = turn.applyEvent;
  const originalReply = dialogTransport.reply;
  const handles = {
    sessionId: "session-test",
    agentId: "agent-test",
    transcript,
    turn,
    scope: {
      store: store({ replBridgeEnabled: false, replBridgeConnected: false }),
      computeTools: () => [{ name: "Artifact", inputJSONSchema: { type: "object" } }],
      dialogTransport,
    },
  };
  attach(handles);
  await delay(5);
  const first = new NativeClient(socketPath);
  const second = new NativeClient(socketPath);
  try {
    await context.test("stale native session and adapter actions fail before admission", async () => {
      await assert.rejects(first.request("prompt", { text: "must not run", sessionId: "old-session" }), /session changed/);
      await assert.rejects(first.request("interrupt", { epoch: "old-epoch" }), /adapter changed/);
      assert.equal(queued.length, 0);
      assert.equal(cancellations, 0);
    });
    await context.test("private socket and snapshot", async () => {
      assert.equal(statSync(socketPath).mode & 0o777, 0o600);
      const snapshot = await first.request("snapshot");
      assert.equal(snapshot.pid, process.pid);
      assert.equal(snapshot.sessionId, "session-test");
      assert.equal(snapshot.remoteControl.enabled, false);
      assert.deepEqual(await first.request("tool_schema", { name: "Artifact" }), { type: "object" });
      await assert.rejects(first.request("run_arbitrary_command"), /Unknown method/);
    });

    await context.test("settings use native state, refuse stale changes, and keep both clients synchronized", async () => {
      const state = handles.scope.store;
      state.set({ mainLoopModel: "sonnet", sessionEffort: { kind: "inherit" },
        toolPermissionContext: { mode: "default" } });
      let changes = 0;
      handles.engine = { supportedModels: async () => [{ value: "sonnet", displayName: "Sonnet" }] };
      handles.settings = {
        permission: value => ({ ok: value !== "bypassPermissions", error: value === "bypassPermissions" ? "Disabled by session" : null }),
        async change(key, value) {
          changes++;
          if (key === "model") state.set({ ...state.getState(), mainLoopModel: value });
          return "Changed";
        },
      };
      turn.guard.isActive = true;
      await assert.rejects(first.request("set_setting", { key:"model", value:"haiku", expected:"sonnet" }), /finish/);
      turn.guard.isActive = false;
      await assert.rejects(first.request("set_setting", { key:"model", value:"haiku", expected:"old" }), /another window/);
      assert.equal(changes, 0);
      const event = second.waitFor(event => event.event === "settings" && event.data.model === "haiku");
      const changed = await first.request("set_setting", { key:"model", value:"haiku", expected:"sonnet" });
      assert.equal(changed.settings.model, "haiku");
      await event;
      assert.equal((await second.request("snapshot")).settings.model, "haiku");
      await assert.rejects(second.request("set_setting", { key:"model", value:"sonnet", expected:"sonnet" }), /another window/);
      assert.equal(changes, 1);
      delete handles.settings;
    });

    await context.test("prompt admission, stable ID, duplicate and conflicting retries", async () => {
      const submissionId = randomUUID();
      const accepted = await first.request("prompt", { text: "hello", submissionId });
      assert.equal(accepted.uuid, submissionId);
      assert.equal(queued.length, 1);
      assert.equal(queued[0].agentId, "agent-test");
      assert.equal(queued[0].skipSlashCommands, true);
      assert.equal(submissions, 1);
      assert.equal((await second.request("prompt", { text: "hello", submissionId })).duplicate, true);
      assert.equal(queued.length, 1);
      await assert.rejects(first.request("prompt", { text: "other", submissionId }), /different text/);
      await assert.rejects(first.request("prompt", { text: " " }), /nonempty/);
      await assert.rejects(first.request("prompt", { text: "hello", submissionId: "invalid" }), /UUID/);
      await assert.rejects(first.request("prompt", { text: "hello", submissionId:undefined }), /UUID/);
      admission = { admitted: false, reason: "test refusal" };
      const refused = await first.request("prompt", { text: "refused", submissionId:randomUUID() });
      assert.equal(refused.accepted, false);
      assert.equal(refused.state, "rejected");
      assert.match(refused.message, /Native queue rejected/);
      assert.equal(submissions, 1, "Refusal must not mark a submission in the native UI");
      admission = { admitted: true };
    });

    await context.test("post-admission bookkeeping failure cannot cause a duplicate retry", async () => {
      const submissionId = randomUUID();
      const before = queued.length;
      const markSubmit = turn.markSubmit;
      try {
        turn.markSubmit = () => { throw new Error("injected bookkeeping failure"); };
        const accepted = await first.request("prompt", { text: "already queued", submissionId });
        assert.equal(accepted.accepted, true);
        assert.match(accepted.warning, /accepted.*bookkeeping failure/);
        const repeated = await second.request("prompt", { text: "already queued", submissionId });
        assert.equal(repeated.duplicate, true);
        assert.equal(repeated.warning, accepted.warning);
        assert.equal(queued.length, before + 1);
      } finally { turn.markSubmit = markSubmit; }
    });

    await context.test("an exception inside admission cannot cause a second admission", async () => {
      const submissionId = randomUUID();
      const enqueue = turn._host.messageQueue.enqueueReportingAdmission;
      const before = queued.length;
      try {
        turn._host.messageQueue.enqueueReportingAdmission = command => {
          queued.push(command);
          throw new Error("injected exception after queue mutation");
        };
        const result = await first.request("prompt", { text: "ambiguous admission", submissionId });
        assert.equal(result.accepted, false);
        assert.equal(result.state, "uncertain");
        const retry = await second.request("prompt", { text: "ambiguous admission", submissionId });
        assert.equal(retry.state, "uncertain");
        assert.equal(retry.duplicate, true);
        assert.equal(queued.length, before + 1);
        assert.equal((await first.request("submission_status", { submissionId })).state, "uncertain");
        transcript.set([{type:"user",uuid:submissionId,message:{content:"different text"}}]);
        assert.equal((await first.request("submission_status", { submissionId,text:"ambiguous admission" })).state,"uncertain");
        transcript.set([{type:"user",uuid:submissionId,message:{content:[{type:"text",text:"ambiguous admission"}]}}]);
        const reconciled = await first.request("submission_status", { submissionId,text:"ambiguous admission" });
        assert.equal(reconciled.accepted,true);
        assert.equal(reconciled.evidence,"native_transcript");
        transcript.set([]);
        assert.equal((await second.request("prompt", { text:"ambiguous admission",submissionId })).accepted,true);
        assert.equal(queued.length,before+1);
      } finally { turn._host.messageQueue.enqueueReportingAdmission = enqueue; }
    });

    await context.test("native event tap preserves behavior and broadcasts once", async () => {
      attach(handles);
      const event = { type: "stream_event", event: { delta: { type: "text_delta", text: "test" } } };
      turn.applyEvent(event);
      const received = await first.waitFor((message) => message.event === "engine_event");
      await second.waitFor((message) => message.event === "engine_event");
      assert.deepEqual(received.data, event);
      assert.equal(applied.length, 1);
      transcript.set([{ uuid: "message-1", type: "progress", data: new Map([["nested", "value"]]) }]);
      const changed = await first.waitFor((message) => message.event === "transcript" && message.data[0]?.uuid === "message-1");
      assert.deepEqual(changed.data[0].data, { $map: [["nested", "value"]] });
      assert.ok(changed.sequence > received.sequence);
    });

    await context.test("dialog reconnect snapshot, updates, resolution, stale reply rejection", async () => {
      dialogEvents.emit("request", {
        id: "dialog-1",
        kind: "permission_file",
        payload: { input: { content: "original" } },
      });
      await first.waitFor((event) => event.event === "dialog");
      assert.equal((await second.request("snapshot")).dialogs.length, 1);
      dialogEvents.emit("update", { id: "dialog-1", payload: { input: { content: "new" } } });
      assert.equal((await second.request("snapshot")).dialogs[0].payload.input.content, "new");
      await assert.rejects(first.request("reload"), /idle/);
      await second.request("dialog_reply", {
        dialogId: "dialog-1",
        reply: { id: "unrelated", result: { behavior: "allow", updatedInput: { content: "edited" } } },
      });
      assert.equal(resolved[0].id, "dialog-1", "Client cannot substitute an unrelated dialog ID");
      assert.equal(resolved[0].result.updatedInput.content, "edited");
      await assert.rejects(first.request("dialog_reply", { dialogId: "dialog-1", reply: {} }), /resolved dialog/);
      assert.equal((await first.request("snapshot")).dialogs.length, 0);
      dialogEvents.emit("request", { id: "dialog-2", kind: "permission_prompt", payload: {} });
      dialogStore.answer("dialog-2", { behavior: "deny" });
      assert.equal(
        (await first.request("snapshot")).dialogs.length,
        0,
        "A native TUI answer must resolve the adapter's pending dialog too",
      );
    });

    await context.test("question replies compare the exact request and resolve once across clients", async () => {
      const question = JSON.parse(readFileSync(new URL("./fixtures/question-dialog.json", import.meta.url)));
      dialogEvents.emit("request", question);
      const original = (await first.request("snapshot")).dialogs[0];
      const changed = structuredClone(question);
      changed.payload.questions[0].header = "Changed marker";
      changed.payload.input.questions = changed.payload.questions;
      dialogEvents.emit("update", { id:question.id, payload:changed.payload });
      const reply = {result:{behavior:"allow", updatedInput:{...changed.payload.input, answers:{
        "Which marker do you prefer?":"Amber", "Which checks should run?":"Unit, Integration"
      }}}};
      await assert.rejects(first.request("dialog_reply", {dialogId:question.id,expectedDialog:original,reply}), /changed this request/);
      assert.equal((await second.request("snapshot")).dialogs.length, 1);
      // Rust's JSON object ordering need not match the native object's ordering.
      const expectedDialog = Object.fromEntries(Object.entries(changed).reverse());
      await second.request("dialog_reply", {dialogId:question.id,expectedDialog,reply});
      assert.deepEqual(resolved.at(-1), {id:question.id,result:reply.result});
      await assert.rejects(first.request("dialog_reply", {dialogId:question.id,expectedDialog,reply}), /resolved dialog/);
      assert.equal((await first.request("snapshot")).dialogs.length, 0);
    });

    await context.test("disconnect is not cancellation and reconnect keeps native identity", async () => {
      const waiting = assert.rejects(
        first.waitFor((event) => event.event === "never-arrives"),
        /Disconnected/,
      );
      first.close();
      await once(first.socket, "close");
      await waiting;
      await assert.rejects(first.request("snapshot"), /Disconnected/);
      assert.equal(cancellations, 0);
      const reconnected = new NativeClient(socketPath);
      try {
        assert.equal((await reconnected.request("snapshot")).sessionId, "session-test");
        await reconnected.request("interrupt");
        assert.equal(cancellations, 1);
      } finally {
        reconnected.close();
      }
    });
  } finally {
    first.close();
    second.close();
    await dispose();
    if (priorConfiguration === undefined) delete process.env.CLAUDE_CONFIG_DIR;
    else process.env.CLAUDE_CONFIG_DIR = priorConfiguration;
  }
  assert.equal(turn.applyEvent, originalApply);
  assert.equal(dialogTransport.reply, originalReply);
  assert.equal(dialogEvents.listenerCount("request"), 1);
});

test("malformed native response rejects callers without crashing the client", async () => {
  const directory = mkdtempSync(join(tmpdir(), "harness-claude-client-test-"));
  const socketPath = join(directory, "native.sock");
  const server = net.createServer((socket) => {
    socket.once("data", () => socket.end("not-json\n"));
  });
  server.listen(socketPath);
  await once(server, "listening");
  const client = new NativeClient(socketPath);
  try {
    const waiter = assert.rejects(
      client.waitFor((event) => event.event === "hello"),
      /Malformed native response/,
    );
    await assert.rejects(client.request("snapshot"), /Malformed native response/);
    await waiter;
  } finally {
    client.close();
    await new Promise((resolve, reject) => server.close((error) => (error ? reject(error) : resolve())));
  }
});
