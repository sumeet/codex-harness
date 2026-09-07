import assert from "node:assert/strict";
import { execFile, spawn } from "node:child_process";
import { createHash, randomUUID } from "node:crypto";
import { mkdtempSync, mkdirSync, readFileSync, writeFileSync, readdirSync, existsSync } from "node:fs";
import { join, resolve } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { promisify } from "node:util";
import test from "node:test";
import { NativeClient } from "./client.mjs";
import { isolatedGui } from "./gui_probe.mjs";

const harness = process.env.HARNESS_SETUP_BINARY && resolve(process.env.HARNESS_SETUP_BINARY);
const native = process.env.HARNESS_NATIVE_TEST_BINARY && resolve(process.env.HARNESS_NATIVE_TEST_BINARY);
const execute = promisify(execFile);

test("new Claude uses the native supervisor and survives loss of the creating frontend", { skip:!harness || !native, timeout:180000 }, async () => {
  const directory = mkdtempSync("/tmp/hc-");
  const configuration = join(directory,"p"), runtime = join(directory,"r"), workspace = join(directory,"workspace");
  for (const path of [configuration,runtime,workspace]) mkdirSync(path,{mode:0o700});
  const environment = { PATH:process.env.PATH, LANG:"C.UTF-8", TERM:"xterm-256color",
    CLAUDE_CONFIG_DIR:configuration, XDG_RUNTIME_DIR:runtime, XDG_CONFIG_HOME:join(directory,"config"),
    XDG_DATA_HOME:join(directory,"data"), XDG_STATE_HOME:join(directory,"state"), HARNESS_CLAUDE_BINARY:native,
    DISABLE_AUTOUPDATER:"1", BROWSER:"/bin/false" };
  writeFileSync(join(configuration,"settings.json"),JSON.stringify({remoteControlAtStartup:false,
    env:{DISABLE_AUTOUPDATER:"1",CLAUDE_CODE_ARTIFACT_AUTO_OPEN:"0"}}),{mode:0o600});
  writeFileSync(join(configuration,".claude.json"),JSON.stringify({hasCompletedOnboarding:true,theme:"dark",
    projects:{[workspace]:{hasTrustDialogAccepted:true}}}),{mode:0o600});
  const run = async (binary, args) => execute(binary,args,{env:environment,cwd:workspace,timeout:60000,maxBuffer:4*1024*1024});
  const json = async (binary,args) => JSON.parse((await run(binary,args)).stdout);
  const rows = () => json(native,["agents","--json","--all"]);
  const catalog = () => json(harness,["--claude-list"]);
  const requests = join(configuration,"harness-adapter/creations");
  const endpointRoot = join(runtime,`harness-claude-${createHash("sha256").update(configuration).digest("hex").slice(0,16)}`);
  const snapshot = async session => {
    const entry = JSON.parse(readFileSync(join(endpointRoot,"jobs",session.source.job.id,"current.json")));
    const client = new NativeClient(entry.socketPath);
    try { return await client.request("snapshot"); } finally { client.close(); }
  };
  const report = {directory,modelRequestsSent:0};
  let graphical, caller, terminal;
  const attach = async session => {
    const quote = value => `'${value.replaceAll("'", "'\\''")}'`;
    terminal = spawn("/usr/bin/script",["-qefc",`stty rows 40 cols 120 && exec ${[harness,"--claude-job-terminal",session.id].map(quote).join(" ")}`,join(directory,"native-terminal.cast")],
      {env:environment,cwd:workspace,stdio:["pipe","pipe","pipe"]});
    let output = "";
    terminal.stdout.on("data",bytes => { output=(output+bytes.toString()).slice(-1024*1024); });
    terminal.stderr.on("data",bytes => { output=(output+bytes.toString()).slice(-1024*1024); });
    const closed = new Promise((resolveClosed,reject) => {
      terminal.on("error",reject);
      terminal.on("close",(code,signal) => resolveClosed({code,signal}));
    });
    await delay(2500);
    assert.equal(terminal.exitCode,null,output);
    assert.ok(output.includes("Attaching"),output);
    terminal.stdin.write("\x1a");
    const result = await Promise.race([closed,delay(5000).then(() => { throw new Error("Native terminal did not detach"); })]);
    assert.equal(result.code,0,output);
    terminal = undefined;
  };
  try {
    await run(harness,["--claude-setup","enable"]);
    const settingsBefore = readFileSync(join(configuration,"settings.json"),"utf8");
    const first = await json(harness,["--claude-new",workspace]);
    assert.equal(first.source.kind,"native");
    const state = await snapshot(first);
    assert.equal(state.sessionId,first.source.conversation_id);
    assert.equal(state.turn.submitCount,0);
    assert.equal(state.messages.length,0);
    assert.equal((await rows()).length,1);
    const firstRequest = join(requests,readdirSync(requests)[0]);
    const firstState = JSON.parse(readFileSync(join(firstRequest,"state.json")));
    assert.equal(firstState.phase,"connected");
    assert.equal(firstState.session_id,first.source.conversation_id);
    const competing = await Promise.all([json(harness,["--claude-create-recover",firstRequest]),json(harness,["--claude-create-recover",firstRequest])]);
    assert.ok(competing.every(session => session.source.job.pid === first.source.job.pid));
    assert.equal((await rows()).length,1);
    writeFileSync(join(firstRequest,"state.json"),JSON.stringify({phase:"uncertain",detail:"Injected lost startup acknowledgement"}),{mode:0o600});
    const interruptedAcknowledgement = await catalog();
    assert.equal(interruptedAcknowledgement.conversations.length,1,"A startup request and its native worker must share one row");
    const pendingConversation = interruptedAcknowledgement.conversations[0];
    assert.equal(pendingConversation.id,first.id);
    assert.equal(pendingConversation.target.kind,"creation");
    assert.equal(pendingConversation.target.current.id,first.id);
    const creationAlias = pendingConversation.aliases.find(id=>id.startsWith("claude-creation:"));
    assert.ok(creationAlias,"The original startup selection must remain an alias");
    const reconciled = await json(harness,["--claude-create-recover",firstRequest]);
    assert.equal(reconciled.source.job.pid,first.source.job.pid);
    const reconciledCatalog = await catalog();
    assert.equal(reconciledCatalog.conversations.length,1);
    assert.equal(reconciledCatalog.conversations[0].target.kind,"session");
    assert.ok(reconciledCatalog.conversations[0].aliases.includes(creationAlias));
    assert.equal((await rows()).length,1);
    report.unfinishedCreationSharesNativeConversationRow = true;
    report.normalCreation = {conversation:first.source.conversation_id,pid:first.source.job.pid};
    await attach(first);
    assert.equal((await snapshot(first)).pid,first.source.job.pid,"Native terminal must reuse the live worker");
    report.nativeTerminalSameWorker = true;

    const existingRequests = new Set(readdirSync(requests));
    caller = spawn(harness,["--claude-new",workspace],{env:environment,cwd:workspace,stdio:["ignore","pipe","pipe"]});
    const closed = new Promise(resolveClosed => caller.on("close",(code,signal) => resolveClosed({code,signal})));
    let request;
    const deadline = Date.now()+15000;
    while (!request && Date.now()<deadline) {
      for (const id of readdirSync(requests).filter(id => !existingRequests.has(id))) {
        const candidate = join(requests,id);
        if (!existsSync(join(candidate,"operation.lock"))) continue;
        request = candidate;
      }
      if (!request) await delay(5);
    }
    assert.ok(request,"Detached coordinator never took ownership");
    assert.ok(caller.kill("SIGKILL"),"Could not inject frontend death");
    const callerExit = await closed;
    assert.equal(callerExit.signal,"SIGKILL");
    caller = undefined;
    const recovered = await Promise.all([json(harness,["--claude-create-recover",request]),json(harness,["--claude-create-recover",request])]);
    assert.equal(recovered[0].id,recovered[1].id);
    assert.equal(recovered[0].source.job.pid,recovered[1].source.job.pid);
    assert.equal((await rows()).length,2,"Recovery must not duplicate the interrupted creation");
    const recoveredState = await snapshot(recovered[0]);
    assert.equal(recoveredState.turn.submitCount,0);
    assert.equal(recoveredState.messages.length,0);
    assert.equal((await catalog()).creations.length,0);
    assert.equal(readFileSync(join(configuration,"settings.json"),"utf8"),settingsBefore);
    report.interruptedCreation = {conversation:recovered[0].source.conversation_id,pid:recovered[0].source.job.pid,
      frontendKilled:true,concurrentRecoveryWithoutDuplicate:true};

    if (process.env.HARNESS_RESUME_GUI === "1") {
      graphical = await isolatedGui(harness,environment,directory);
      const currentCatalog = await catalog();
      const index = currentCatalog.conversations.findIndex(conversation => conversation.id === recovered[0].id);
      assert.ok(index>=0);
      const window = await graphical.open();
      await graphical.click(window,145,48);
      await delay(1500);
      await graphical.click(window,130,180+52*index);
      await delay(1500);
      await graphical.capture("created-conversation");
      assert.equal(JSON.parse(readFileSync(join(graphical.configuration(window),"harness/session.json"))).selected_claude_id,recovered[0].id);
      const second = await graphical.open();
      await graphical.click(second,145,48);
      await delay(1500);
      await graphical.click(second,130,180+52*index);
      await delay(1500);
      await graphical.capture("second-frontend");
      assert.equal(JSON.parse(readFileSync(join(graphical.configuration(second),"harness/session.json"))).selected_claude_id,recovered[0].id);
      await graphical.close();
      graphical = undefined;
      assert.equal((await snapshot(recovered[0])).pid,recoveredState.pid);
      report.twoFrontendWindows = true;
    }
    const uncertainId = randomUUID();
    const uncertain = join(requests,uncertainId);
    mkdirSync(uncertain,{mode:0o700});
    const recordedRequest = JSON.parse(readFileSync(join(firstRequest,"request.json")));
    writeFileSync(join(uncertain,"request.json"),JSON.stringify({...recordedRequest,id:uncertainId}),{mode:0o600});
    writeFileSync(join(uncertain,"settings.json"),"{}",{mode:0o600});
    writeFileSync(join(uncertain,"state.json"),JSON.stringify({phase:"dispatching"}),{mode:0o600});
    await assert.rejects(run(harness,["--claude-create-recover",uncertain]),/will not be repeated/);
    assert.equal((await rows()).length,2,"An ambiguous crash must not create a replacement");
    assert.equal((await catalog()).creations.length,1,"Unresolved startup must stay discoverable");
    report.uncertainCreationRetainedWithoutRetry = true;
    if (process.env.HARNESS_RESUME_GUI === "1") {
      graphical = await isolatedGui(harness,environment,directory);
      const window = await graphical.open();
      await graphical.click(window,145,48);
      await delay(1500);
      await graphical.capture("uncertain-startup-visible");
      const pendingCatalog = await catalog();
      const pendingIndex = pendingCatalog.conversations.findIndex(conversation=>conversation.target.kind==="creation");
      assert.ok(pendingIndex>=0,"The pending conversation must be in the ordinary conversation list");
      assert.equal(pendingCatalog.conversations[pendingIndex].title,"workspace");
      await graphical.click(window,130,180+52*pendingIndex);
      await delay(1200);
      await graphical.capture("failed-conversation-opened");
      assert.equal(JSON.parse(readFileSync(join(graphical.configuration(window),"harness/session.json"))).selected_claude_id,pendingCatalog.conversations[pendingIndex].id);
      assert.equal((await rows()).length,2,"Selecting a failed conversation must reconcile, not create another");
      await graphical.click(window,615,428);
      await delay(300);
      await graphical.capture("failed-conversation-details");
      await graphical.close();
      graphical = undefined;
    }
    await run(native,["stop",first.source.job.id]);
    for (let attempt=0;attempt<50 && (await rows()).some(row=>row.id===first.source.job.id && row.pid);attempt++) await delay(100);
    await assert.rejects(run(harness,["--claude-continue",first.source.conversation_id]),/saved history/);
    await attach(first);
    const explicitlyResumed = (await catalog()).sessions.find(session=>session.id===first.id);
    assert.ok(explicitlyResumed.source.job.pid);
    assert.notEqual(explicitlyResumed.source.job.pid,first.source.job.pid);
    assert.equal((await snapshot(explicitlyResumed)).turn.submitCount,0);
    report.explicitNativeTerminalMayWakeStoppedConversation = true;
    console.log(JSON.stringify(report));
  } finally {
    if (terminal?.exitCode === null && terminal.signalCode === null) terminal.kill("SIGTERM");
    if (caller?.exitCode === null && caller.signalCode === null) caller.kill("SIGKILL");
    if (graphical) { try { await graphical.capture("failure"); } finally { await graphical.close(); } }
    for (const row of (await rows()).filter(row => row.pid)) {
      assert.equal(row.cwd,workspace);
      assert.match(row.id,/^[a-f0-9]{8,64}$/);
      await run(native,["stop",row.id]);
    }
    for (let attempt=0;attempt<50 && (await rows()).some(row => row.pid);attempt++) await delay(100);
    assert.ok((await rows()).every(row => !row.pid));
    writeFileSync(join(directory,"report.json"),JSON.stringify(report,null,2),{mode:0o600});
    console.log(`Private creation evidence: ${directory}`);
  }
});
