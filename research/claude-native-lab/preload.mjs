import { writeFileSync, renameSync, unlinkSync } from "node:fs";
import { randomUUID } from "node:crypto";

export function writeStatus(path, state) {
  const temporary = `${path}.${randomUUID()}.pending`;
  try {
    writeFileSync(temporary, JSON.stringify(state), { mode: 0o600, flag: "wx" });
    renameSync(temporary, path);
  } finally {
    try { unlinkSync(temporary); }
    catch (error) { if (error.code !== "ENOENT") process.stderr.write(`Could not remove temporary Claude status: ${error}\n`); }
  }
}

export function findController(root) {
  const matches = new Set();
  function inspect(value) {
    if (!value || typeof value !== "object") return;
    if (value.turn && value.transcript && value.scope && value.session && value.dialogStore && value.messageQueue)
      matches.add(value);
  }
  const pending = [root];
  const seen = new Set();
  while (pending.length) {
    const fiber = pending.pop();
    if (!fiber || seen.has(fiber)) continue;
    seen.add(fiber);
    if (seen.size > 10000) throw new Error("Native UI tree exceeds adapter discovery limit");
    inspect(fiber.memoizedProps?.value);
    let hook = fiber.memoizedState;
    for (let index = 0; hook && index < 512; index++, hook = hook.next) {
      const value = hook.memoizedState;
      inspect(value);
      inspect(value?.current);
      if (Array.isArray(value)) inspect(value[0]);
    }
    pending.push(fiber.sibling, fiber.child);
  }
  if (matches.size > 1) throw new Error("Multiple native session controllers; refusing ambiguous attachment");
  return [...matches][0];
}

export function controllerHandles(controller) {
  const { turn, transcript, scope, session, engine, dialogStore, messageQueue } = controller;
  const functions = [
    [turn, "getSnapshot"],
    [turn, "subscribe"],
    [turn, "applyEvent"],
    [turn, "cancel"],
    [turn, "markSubmit"],
    [turn, "resetTiming"],
    [turn.stream, "getSnapshot"],
    [turn.stream, "subscribe"],
    [turn.stream, "setUserInputOnProcessing"],
    [transcript, "getSnapshot"],
    [transcript, "subscribe"],
    [scope, "computeTools"],
    [scope.store, "getState"],
    [scope.dialogTransport, "subscribe"],
    [scope.dialogTransport, "onUpdate"],
    [scope.dialogTransport, "onCancel"],
    [dialogStore, "getState"],
    [dialogStore, "subscribe"],
    [dialogStore, "onClosed"],
    [dialogStore, "answer"],
    [dialogStore, "dismiss"],
    [messageQueue, "enqueueReportingAdmission"],
    [session.identity, "mainAgentId"],
  ];
  for (const [object, name] of functions)
    if (typeof object?.[name] !== "function") throw new Error(`Native adapter contract missing ${name}`);
  if (turn._host?.dialogStore !== dialogStore || turn._host?.messageQueue !== messageQueue)
    throw new Error("Native session controller does not own the turn's queue and dialogs");
  if (typeof session.id !== "string" || !session.id || typeof turn.guard?.isActive !== "boolean")
    throw new Error("Invalid native session identity or turn guard");
  return {
    controller,
    turn,
    transcript,
    scope,
    session,
    engine,
    sessionId: session.id,
    agentId: session.identity.mainAgentId(session.id),
  };
}

export async function start() {
  const specification = JSON.parse(process.env.HARNESS_CLAUDE_PRELOAD_SPEC);
  // The preload belongs to this Claude process, not Bun/Claude subprocesses launched by its tools.
  delete process.env.BUN_OPTIONS;
  delete process.env.HARNESS_CLAUDE_PRELOAD_SPEC;
  const module = await import(specification.registryModule);
  if (specification.registryKind !== "map" && typeof module[specification.registryExport] !== "function")
    throw new Error("Missing native Ink registry export");
  const bridge = await import(process.env.HARNESS_CLAUDE_BRIDGE_MODULE);
  const controls = specification.settingsControls;
  const settingsModules = controls ? {
    model: await import(controls.modelModule),
    effort: await import(controls.effortModule),
    permissions: await import(controls.permissionModule),
  } : null;
  const deadline = Date.now() + 30000;
  let unsubscribe;
  let failed = false;
  let current;
  globalThis.__harnessClaudeBridge = bridge;
  function report(state) {
    if (process.env.HARNESS_CLAUDE_PRELOAD_STATUS)
      writeStatus(process.env.HARNESS_CLAUDE_PRELOAD_STATUS, { pid: process.pid, ...state });
  }
  function inspect(ink) {
    if (failed) return;
    try {
      const controller = findController(ink.container?.current);
      if (!controller) return;
      const handles = controllerHandles(controller);
      if (settingsModules) {
        const validate = settingsModules.permissions[controls.validatePermissionExport];
        const change = settingsModules.permissions[controls.setPermissionExport];
        if (typeof validate !== "function" || typeof change !== "function" ||
            typeof controller._buildIdleToolUseContext !== "function" ||
            typeof settingsModules.model.call !== "function" || typeof settingsModules.effort.call !== "function")
          throw new Error("Native settings control contract changed");
        handles.settings = {
          permission: mode => validate(mode, controller.store.getState().toolPermissionContext),
          async change(key, value) {
            if (key === "model" || key === "effort") {
              const context = controller._buildIdleToolUseContext();
              // Composer controls edit this session, not defaults for other
              // sessions. Native command handlers already support that policy.
              context.options = { ...context.options, isNonInteractiveSession: true };
              const result = await settingsModules[key].call(value, context);
              return result.value;
            }
            if (key !== "permissionMode") throw new Error("Unknown Claude setting");
            const result = change(value, controller.store.getState().toolPermissionContext,
              update => controller._setAppState(state => ({ ...state,
                toolPermissionContext: update(state.toolPermissionContext) })), "harness");
            if (!result.ok) throw new Error(result.error);
            return null;
          },
        };
      }
      if (
        current?.turn === handles.turn &&
        current?.transcript === handles.transcript &&
        current?.scope === handles.scope &&
        current?.sessionId === handles.sessionId &&
        current?.agentId === handles.agentId
      )
        return;
      globalThis.__harnessClaudeHandles = handles;
      globalThis.__harnessClaudeBridge.attach(handles);
      current = handles;
      report({ state: "attached", sessionId: handles.sessionId, agentId: handles.agentId });
    } catch (error) {
      failed = true;
      unsubscribe?.();
      globalThis.__harnessClaudeBridge
        .dispose()
        .catch((disposeError) => process.stderr.write(`Harness preload cleanup failed: ${disposeError}\n`));
      report({ state: "unsupported", error: String(error) });
      process.stderr.write(`Harness preload adapter stopped: ${error}\n`);
    }
  }
  function poll() {
    try {
      const maps =
        specification.registryKind === "map"
          ? (specification.registryExports ?? [specification.registryExport])
              .map((name) => module[name])
              .filter((value) => value instanceof Map)
          : [];
      if (maps.length > 1) throw new Error("Multiple exported native Ink registries");
      const registry = specification.registryKind === "map" ? maps[0] : module[specification.registryExport]();
      const ink = registry?.get(process.stdout);
      if (ink?.container) {
        if (typeof ink.subscribeLayout !== "function") {
          report({
            state: "unsupported",
            error: "Native Ink has no subscribeLayout callback",
            inkKeys: Object.keys(ink),
          });
          return;
        }
        unsubscribe = ink.subscribeLayout(() => inspect(ink));
        inspect(ink);
        return;
      }
      if (Date.now() > deadline) {
        report({ state: "not-mounted" });
        return;
      }
      setTimeout(poll, 100).unref();
    } catch (error) {
      report({ state: "unsupported", error: String(error) });
      process.stderr.write(`Harness preload discovery stopped: ${error}\n`);
    }
  }
  poll();
}

if (process.env.HARNESS_CLAUDE_PRELOAD_SPEC) {
  start().catch((error) => {
    process.stderr.write(`Harness preload failed: ${error}\n`);
    if (process.env.HARNESS_CLAUDE_PRELOAD_STATUS) {
      try { writeStatus(process.env.HARNESS_CLAUDE_PRELOAD_STATUS, { pid: process.pid, state: "error", error: String(error) }); }
      catch (statusError) { process.stderr.write(`Harness preload status failed: ${statusError}\n`); }
    }
  });
}
