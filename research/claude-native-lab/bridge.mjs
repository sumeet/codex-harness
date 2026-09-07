import net from "node:net";
import { chmodSync, copyFileSync, statSync, constants, mkdirSync, lstatSync, realpathSync,
  openSync, closeSync, fstatSync, readFileSync, writeFileSync, fsyncSync } from "node:fs";
import { dirname, join } from "node:path";
import { homedir } from "node:os";
import { pathToFileURL } from "node:url";
import { randomUUID, createHash } from "node:crypto";
import { isDeepStrictEqual } from "node:util";

let current;
let server;
let sequence = 0;
const epoch = randomUUID();
const clients = new Set();
const subscriptions = [];
const submissions = new Map();
let modelCatalog = [];
let settingsChanging = false;

function sessionSettings() {
  const state = current.scope.store.getState();
  return {
    model: state.mainLoopModelForSession ?? state.mainLoopModel ?? "default",
    effort: state.sessionEffort ?? null,
    permissionMode: state.toolPermissionContext?.mode ?? null,
    models: modelCatalog,
    permissions: current.settings ? ["default", "acceptEdits", "plan", "auto", "bypassPermissions"].map(value => {
      const verdict = current.settings.permission(value);
      return { value, available: verdict.ok, reason: verdict.error ?? null };
    }) : [],
  };
}

async function refreshSettings() {
  const handles = current;
  if (typeof handles.engine?.supportedModels !== "function") return sessionSettings();
  const models = await handles.engine.supportedModels();
  if (current !== handles) throw new Error("Claude session changed while loading models");
  modelCatalog = models;
  return sessionSettings();
}

async function changeSetting(request) {
  if (!current.settings) throw new Error("This running adapter does not support Claude settings");
  if (settingsChanging) throw new Error("A Claude setting change is already in progress");
  if (current.turn.guard.isActive || current.turn._host.dialogStore.getState().open.length)
    throw new Error("Wait for Claude to finish or resolve its pending question before changing settings");
  const { key, value, expected } = request;
  if (!["model", "effort", "permissionMode"].includes(key) || typeof value !== "string")
    throw new Error("Invalid Claude setting");
  if (!Object.hasOwn(request, "expected") || !isDeepStrictEqual(sessionSettings()[key], expected))
    throw new Error("Claude settings changed in another window; reopen the menu and try again");
  settingsChanging = true;
  try {
    const message = await current.settings.change(key, value);
    const settings = await refreshSettings();
    publish("settings", settings);
    const applied = key === "effort"
      ? (value === "auto" ? settings.effort?.kind === "default" : settings.effort?.value === value)
      : settings[key] === value;
    if (!applied) throw new Error(message || "Claude did not apply the requested setting");
    return { settings, message };
  } finally { settingsChanging = false; }
}

function privateDirectory(path) {
  let created = false;
  try { mkdirSync(path, { mode: 0o700 }); created = true; }
  catch (error) { if (error.code !== "EEXIST") throw error; }
  const metadata = lstatSync(path);
  if (!metadata.isDirectory() || metadata.uid !== process.getuid() || (metadata.mode & 0o077))
    throw new Error("Submission directory must be private and owned by the current user");
  if (created) {
    const parent = openSync(dirname(path), constants.O_RDONLY | constants.O_DIRECTORY | constants.O_NOFOLLOW);
    try { fsyncSync(parent); } finally { closeSync(parent); }
  }
}

function durableRecord(path, value) {
  const descriptor = openSync(path, constants.O_WRONLY | constants.O_CREAT | constants.O_EXCL | constants.O_NOFOLLOW, 0o600);
  try { writeFileSync(descriptor, JSON.stringify(value)); fsyncSync(descriptor); }
  finally { closeSync(descriptor); }
  const directory = openSync(dirname(path), constants.O_RDONLY | constants.O_DIRECTORY | constants.O_NOFOLLOW);
  try { fsyncSync(directory); } finally { closeSync(directory); }
}

function readRecord(path) {
  let descriptor;
  try { descriptor = openSync(path, constants.O_RDONLY | constants.O_NOFOLLOW | constants.O_NONBLOCK); }
  catch (error) { if (error.code === "ENOENT") return null; throw error; }
  try {
    const metadata = fstatSync(descriptor);
    if (!metadata.isFile() || metadata.uid !== process.getuid() || (metadata.mode & 0o077) || metadata.size > 1024 * 1024)
      throw new Error("Unsafe or oversized submission record");
    return JSON.parse(readFileSync(descriptor, "utf8"));
  } finally { closeSync(descriptor); }
}

export class SubmissionLedger {
  constructor(configuration, sessionId) {
    const profile = realpathSync(configuration);
    const metadata = statSync(profile);
    if (!metadata.isDirectory() || metadata.uid !== process.getuid() || (metadata.mode & 0o022))
      throw new Error("Unsafe Claude profile for submission records");
    this.sessionId = sessionId;
    this.directory = profile;
    for (const component of ["harness-adapter", "submissions", createHash("sha256").update(sessionId).digest("hex")]) {
      this.directory = join(this.directory, component);
      privateDirectory(this.directory);
    }
  }

  path(uuid) {
    if (typeof uuid !== "string" || !/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/.test(uuid))
      throw new Error("submissionId must be a lowercase UUID");
    return join(this.directory, uuid);
  }

  status(uuid, text) {
    const directory = this.path(uuid);
    try {
      const metadata = lstatSync(directory);
      if (!metadata.isDirectory() || metadata.uid !== process.getuid() || (metadata.mode & 0o077))
        throw new Error("Unsafe submission record directory");
    } catch (error) { if (error.code === "ENOENT") return { state: "unknown", uuid }; throw error; }
    const request = readRecord(join(directory, "request.json"));
    if (request && (request.version !== 1 || request.sessionId !== this.sessionId || request.uuid !== uuid || typeof request.text !== "string"))
      throw new Error("Invalid submission identity");
    if (request && text !== undefined && request.text !== text)
      throw new Error("submissionId was already used for different text");
    const outcome = readRecord(join(directory, "reconciled.json")) ?? readRecord(join(directory, "outcome.json"));
    if (outcome) {
      if (!request || outcome.version !== 1 || outcome.uuid !== uuid || !["accepted", "rejected", "uncertain"].includes(outcome.state))
        throw new Error("Invalid submission outcome");
      return outcome;
    }
    // A crash can occur on either side of native admission. Missing completion
    // is never evidence that it is safe to enqueue this operation again.
    return { state: "uncertain", uuid, message: "Claude's acceptance could not be confirmed. This prompt has not been resent." };
  }

  claim(uuid, text) {
    const directory = this.path(uuid);
    try { mkdirSync(directory, { mode: 0o700 }); }
    catch (error) { if (error.code === "EEXIST") return this.status(uuid, text); throw error; }
    durableRecord(join(directory, "request.json"), { version: 1, sessionId: this.sessionId, uuid, text, at: Date.now() });
    const parent = openSync(this.directory, constants.O_RDONLY | constants.O_DIRECTORY | constants.O_NOFOLLOW);
    try { fsyncSync(parent); } finally { closeSync(parent); }
    return null;
  }

  finish(uuid, outcome) {
    durableRecord(join(this.path(uuid), "outcome.json"), { ...outcome, version: 1, uuid });
  }

  confirm(uuid, text) {
    this.status(uuid, text);
    const request = readRecord(join(this.path(uuid), "request.json"));
    if (!request || request.text !== text) throw new Error("No matching saved request to reconcile");
    try {
      durableRecord(join(this.path(uuid), "reconciled.json"), { version: 1, uuid, state: "accepted", evidence: "native_transcript" });
    } catch (error) {
      if (error.code !== "EEXIST") throw error;
      if (this.status(uuid, text).state !== "accepted") throw new Error("Conflicting submission reconciliation");
    }
  }
}

function ledger() {
  return new SubmissionLedger(process.env.CLAUDE_CONFIG_DIR || join(homedir(), ".claude"), current.sessionId);
}

function submissionResult(outcome, duplicate = false) {
  return { ...outcome, accepted: outcome.state === "accepted", duplicate };
}

function reconcileSubmission(records, uuid, text, outcome) {
  if (outcome.state !== "uncertain" || typeof text !== "string") return outcome;
  const present = current.transcript.getSnapshot().some(message => {
    if (message.type !== "user" || message.uuid !== uuid || message.isMeta) return false;
    const content = message.message?.content;
    return content === text || (Array.isArray(content) && content.length === 1 && content[0]?.type === "text" && content[0].text === text);
  });
  if (!present) return outcome;
  records.confirm(uuid, text);
  return records.status(uuid, text);
}

function describe(value) {
  if (value === null || value === undefined) return null;
  return {
    own: Object.keys(value),
    methods: Object.getOwnPropertyNames(Object.getPrototypeOf(value) ?? {}).filter((name) => name !== "constructor"),
  };
}

function send(client, message) {
  if (client.writableLength > 8 * 1024 * 1024) {
    client.destroy();
    return;
  }
  if (!client.destroyed)
    client.write(
      JSON.stringify(message, (_key, value) => {
        if (value instanceof Map) return { $map: [...value] };
        if (value instanceof Set) return { $set: [...value] };
        if (typeof value === "bigint") return String(value);
        return value;
      }) + "\n",
    );
}

function publish(event, data) {
  sequence++;
  for (const client of clients) {
    try {
      send(client, { event, epoch, sequence, data });
    } catch (error) {
      process.stderr.write(`Harness bridge publication failed: ${error}\n`);
      client.destroy();
    }
  }
}

function snapshot() {
  return {
    pid: process.pid,
    sessionId: current.sessionId,
    agentId: current.agentId,
    epoch,
    sequence,
    capabilities: { durableSubmissionDeduplication: 1, checkedDialogReplies: 1,
      sessionSettings: current.settings ? 1 : 0 },
    settings: sessionSettings(),
    messages: current.transcript.getSnapshot(),
    turn: current.turn.getSnapshot(),
    stream: current.turn.stream?.getSnapshot(),
    dialogs: current.turn._host.dialogStore.getState().open,
    remoteControl: {
      enabled: current.scope.store.getState().replBridgeEnabled,
      connected: current.scope.store.getState().replBridgeConnected,
    },
  };
}

function handle(request) {
  if (request.sessionId !== undefined && request.sessionId !== current.sessionId)
    throw new Error("Native session changed; refresh before acting");
  if (request.epoch !== undefined && request.epoch !== epoch)
    throw new Error("Native adapter changed; refresh before acting");
  switch (request.method) {
    case "describe":
      return Object.fromEntries(
        Object.entries({
          ...current,
          host: current.turn._host,
          queue: current.turn._host?.messageQueue,
          dialogs: current.scope.dialogTransport,
          store: current.scope.store,
        }).map(([name, value]) => [name, typeof value === "object" ? describe(value) : value]),
      );
    case "snapshot":
      return snapshot();
    case "settings":
      return refreshSettings();
    case "set_setting":
      return changeSetting(request);
    case "submission_status": {
      const records = ledger();
      return submissionResult(reconcileSubmission(records, request.submissionId, request.text, records.status(request.submissionId, request.text)));
    }
    case "tools":
      return current.scope.computeTools().map((tool) => ({ name: tool.name, keys: Object.keys(tool) }));
    case "tool_schema": {
      const tool = current.scope.computeTools().find((tool) => tool.name === request.name);
      if (!tool) throw new Error("Unknown tool");
      return (
        tool.inputJSONSchema ?? tool.inputSchema?.toJSONSchema?.() ?? { keys: Object.keys(tool.inputSchema ?? {}) }
      );
    }
    case "prepare_test":
      process.env.CLAUDE_CODE_ARTIFACT_AUTO_OPEN = "0";
      process.env.BROWSER = "/bin/false";
      delete process.env.DISPLAY;
      delete process.env.WAYLAND_DISPLAY;
      return { automaticBrowserOpen: false };
    case "prompt": {
      if (typeof request.text !== "string" || !request.text.trim() || request.text.length > 100000)
        throw new Error("Expected a nonempty text prompt under 100000 characters");
      const uuid = request.submissionId;
      if (typeof uuid !== "string" || !/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/.test(uuid))
        throw new Error("submissionId must be a UUID");
      if (submissions.has(uuid)) {
        const previous = submissions.get(uuid);
        if (previous.text !== request.text) throw new Error("submissionId was already used for different text");
        return { ...previous.result, duplicate: true };
      }
      const records = ledger();
      const previous = records.claim(uuid, request.text);
      if (previous) return submissionResult(reconcileSubmission(records, uuid, request.text, previous), true);
      const wasIdle = !current.turn.guard.isActive;
      let admission;
      try {
        admission = current.turn._host.messageQueue.enqueueReportingAdmission({
          value: request.text, mode: "prompt", agentId: current.agentId, uuid, skipSlashCommands: true,
        });
      } catch (error) {
        const outcome = { state: "uncertain", message: `Native admission did not complete normally: ${error.message ?? error}. This prompt will not be resent.` };
        records.finish(uuid, outcome);
        return submissionResult({ ...outcome, uuid });
      }
      if (!admission.admitted) {
        const outcome = { state: "rejected", message: `Native queue rejected prompt: ${JSON.stringify(admission)}` };
        records.finish(uuid, outcome);
        return submissionResult({ ...outcome, uuid });
      }
      const result = { state: "accepted", accepted: true, uuid };
      // Native admission cannot be rolled back by a later UI bookkeeping error.
      submissions.set(uuid, { text: request.text, result });
      try {
        if (wasIdle) {
          current.turn.markSubmit();
          current.turn.resetTiming();
          current.turn.stream.setUserInputOnProcessing(request.text, { kind: "human" });
        }
      } catch (error) {
        result.warning = `Claude accepted this prompt, but its submission display did not update: ${error.message ?? error}. Do not resend it.`;
        process.stderr.write(`Harness bridge: ${result.warning}\n`);
      }
      try { records.finish(uuid, result); }
      catch (error) {
        result.warning = `${result.warning ?? ""} Claude accepted the prompt, but its receipt could not be saved: ${error.message ?? error}. Do not resend it.`.trim();
        process.stderr.write(`Harness bridge: ${result.warning}\n`);
      }
      // Durable records, not a bounded cache, preserve deduplication after reload.
      if (submissions.size > 4096) submissions.delete(submissions.keys().next().value);
      return result;
    }
    case "interrupt":
      current.turn.cancel("remote");
      return { requested: true };
    case "dialog_reply": {
      const store = current.turn._host.dialogStore;
      const dialog = store.getState().open.find((dialog) => dialog.id === request.dialogId);
      if (!dialog)
        throw new Error("Unknown or already resolved dialog");
      if (Object.hasOwn(request, "expectedDialog") && !isDeepStrictEqual(request.expectedDialog, JSON.parse(JSON.stringify(dialog))))
        throw new Error("Claude changed this request. Review it again before answering");
      if (request.reply?.cancelled === true) store.dismiss(request.dialogId);
      else if (request.reply && Object.hasOwn(request.reply, "result"))
        store.answer(request.dialogId, request.reply.result);
      else throw new Error("A dialog result or explicit cancellation is required");
      return { submitted: true };
    }
    case "reload":
      if (current.turn._host.dialogStore.getState().open.length || current.turn.getSnapshot().isLoading)
        throw new Error("Only reload the adapter while the session is idle");
      setImmediate(() => {
        for (const unsubscribe of subscriptions.splice(0)) unsubscribe();
        for (const client of clients) client.end();
        server.close(async () => {
          try {
            const modulePath = join(
              dirname(process.env.HARNESS_CLAUDE_BRIDGE_SOCKET),
              `bridge-reload-${randomUUID()}.mjs`,
            );
            copyFileSync(process.env.HARNESS_CLAUDE_BRIDGE_MODULE, modulePath, constants.COPYFILE_EXCL);
            chmodSync(modulePath, 0o600);
            const url = pathToFileURL(modulePath);
            const module = await import(url.href);
            globalThis.__harnessClaudeBridge = module;
            module.attach(current);
          } catch (error) {
            process.stderr.write(`Harness bridge reload: ${error}\n`);
          }
        });
      });
      return { reloading: true };
    default:
      throw new Error(`Unknown method: ${request.method}`);
  }
}

export function attach(handles) {
  const changed =
    current?.transcript !== handles.transcript ||
    current?.turn !== handles.turn ||
    current?.scope.dialogTransport !== handles.scope.dialogTransport ||
    current?.turn._host.dialogStore !== handles.turn._host.dialogStore;
  const sessionChanged = current && current.sessionId !== handles.sessionId;
  if (current?.sessionId !== handles.sessionId) submissions.clear();
  current = handles;
  if (changed) {
    for (const unsubscribe of subscriptions.splice(0)) unsubscribe();
    modelCatalog = [];
    if (current.settings) {
      let previous = sessionSettings();
      subscriptions.push(current.scope.store.subscribe(() => {
        const settings = sessionSettings();
        if (!isDeepStrictEqual(previous, settings)) {
          previous = settings;
          publish("settings", settings);
        }
      }));
      refreshSettings().then(settings => publish("settings", settings))
        .catch(error => publish("settings_error", { message: `Could not load Claude settings: ${error}` }));
    }
    for (const [name, store] of [
      ["transcript", current.transcript],
      ["turn", current.turn],
      ["stream", current.turn.stream],
    ]) {
      if (typeof store?.subscribe !== "function" || typeof store?.getSnapshot !== "function") continue;
      subscriptions.push(
        store.subscribe(() => {
          try {
            publish(name, store.getSnapshot());
          } catch (error) {
            publish("adapter_error", { message: String(error) });
          }
        }),
      );
    }
    const turn = current.turn;
    const originalApplyEvent = turn.applyEvent;
    turn.applyEvent = (...arguments_) => {
      try {
        publish("engine_event", arguments_[0]);
      } catch (error) {
        process.stderr.write(`Harness bridge event: ${error}\n`);
      }
      return originalApplyEvent.apply(turn, arguments_);
    };
    subscriptions.push(() => {
      turn.applyEvent = originalApplyEvent;
    });
    const transport = current.scope.dialogTransport;
    subscriptions.push(
      transport.subscribe((dialog) => {
        publish("dialog", dialog);
      }),
    );
    subscriptions.push(
      transport.onUpdate((update) => {
        publish("dialog_update", update);
      }),
    );
    subscriptions.push(
      transport.onCancel((id) => {
        publish("dialog_cancel", { id });
      }),
    );
    const dialogStore = current.turn._host.dialogStore;
    subscriptions.push(dialogStore.subscribe(() => publish("dialogs", dialogStore.getState().open)));
    subscriptions.push(dialogStore.onClosed((event) => publish("dialog_resolved", event)));
  }
  if (sessionChanged) publish("session_changed", { sessionId: current.sessionId, agentId: current.agentId });
  if (server) return;
  const path = process.env.HARNESS_CLAUDE_BRIDGE_SOCKET;
  if (!path || !path.startsWith("/")) throw new Error("An absolute HARNESS_CLAUDE_BRIDGE_SOCKET is required");
  const directory = statSync(dirname(path));
  if (directory.uid !== process.getuid() || (directory.mode & 0o077) !== 0)
    throw new Error("Socket directory must be private to the current user (0700)");
  server = net.createServer((client) => {
    clients.add(client);
    let pending = "";
    send(client, {
      event: "hello",
      pid: process.pid,
      sessionId: current.sessionId,
      epoch,
      sequence,
      protocol: "harness-native-lab/0",
    });
    client.setEncoding("utf8");
    client.on("data", (chunk) => {
      pending += chunk;
      if (pending.length > 1048576) {
        client.destroy(new Error("Request too large"));
        return;
      }
      let newline;
      while ((newline = pending.indexOf("\n")) >= 0) {
        const line = pending.slice(0, newline);
        pending = pending.slice(newline + 1);
        let request;
        try {
          request = JSON.parse(line);
          Promise.resolve(handle(request))
            .then((result) => send(client, { id: request.id, result }))
            .catch((error) => send(client, { id: request.id, error: String(error) }));
        } catch (error) {
          send(client, { id: request?.id, error: String(error) });
        }
      }
    });
    client.on("close", () => clients.delete(client));
    client.on("error", (error) => process.stderr.write(`Harness bridge client: ${error.message}\n`));
  });
  server.on("error", (error) => process.stderr.write(`Harness bridge socket: ${error.message}\n`));
  server.listen(path, () => chmodSync(path, 0o600));
}

export async function dispose() {
  for (const unsubscribe of subscriptions.splice(0)) unsubscribe();
  for (const client of clients) client.destroy();
  if (server) await new Promise((resolve, reject) => server.close((error) => (error ? reject(error) : resolve())));
  server = undefined;
  current = undefined;
  submissions.clear();
}
