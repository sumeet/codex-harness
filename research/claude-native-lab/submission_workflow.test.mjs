import assert from "node:assert/strict";
import net from "node:net";
import { randomUUID } from "node:crypto";
import { once } from "node:events";
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, chmodSync } from "node:fs";
import { join, resolve } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import test from "node:test";
import { SubmissionLedger } from "./bridge.mjs";
import { isolatedGui } from "./gui_probe.mjs";

const harness = process.env.HARNESS_SETUP_BINARY && resolve(process.env.HARNESS_SETUP_BINARY);

test("real Harness retries a lost receipt after restart without submitting twice", { skip: !harness || process.env.HARNESS_RESUME_GUI !== "1", timeout: 90000 }, async () => {
  const directory = mkdtempSync("/tmp/hq-");
  const configuration = join(directory,"profile"), runtime = join(directory,"runtime"), data = join(directory,"data"), workspace = join(directory,"workspace");
  for (const path of [configuration,runtime,data,workspace,join(data,"harness"),join(data,"harness/claude")]) mkdirSync(path,{mode:0o700});
  const identifier = randomUUID(), sessionId = randomUUID(), draftKey = `claude:${identifier}`;
  const host = join(runtime,`harness-claude-${identifier}`);
  mkdirSync(host,{mode:0o700});
  writeFileSync(join(data,`harness/claude/${identifier}.json`),JSON.stringify({id:identifier,directory:host,cwd:workspace,title:"Send recovery fixture",lifecycle_version:0,created_at_ms:Date.now(),source:{kind:"managed"}}),{mode:0o600});
  const nativeCatalog = join(directory,"empty-native-catalog");
  writeFileSync(nativeCatalog,"#!/bin/sh\nprintf '[]\\n'\n",{mode:0o700});
  const environment = {PATH:process.env.PATH,LANG:"C.UTF-8",CLAUDE_CONFIG_DIR:configuration,XDG_RUNTIME_DIR:runtime,
    XDG_CONFIG_HOME:join(directory,"config"),XDG_DATA_HOME:data,XDG_STATE_HOME:join(directory,"state"),HARNESS_CLAUDE_BINARY:nativeCatalog,BROWSER:"/bin/false"};
  const records = new SubmissionLedger(configuration,sessionId);
  let epoch = randomUUID(), mode = "drop-receipt";
  let probeDisconnects = 0;
  const admitted = [], requests = [], clients = new Set();
  const server = net.createServer(client => {
    clients.add(client);
    client.on("close",()=>clients.delete(client));
    client.on("error",error=>{
      if (error.code === "EPIPE") probeDisconnects++;
      else process.stderr.write(`Submission fixture socket: ${error.message}\n`);
    });
    client.write(JSON.stringify({event:"hello",sessionId,epoch,sequence:0,pid:process.pid})+"\n");
    client.setEncoding("utf8");
    let pending = "";
    client.on("data",bytes => {
      pending += bytes;
      let newline;
      while ((newline=pending.indexOf("\n"))>=0) {
        const request = JSON.parse(pending.slice(0,newline)); pending = pending.slice(newline+1);
        try {
          let result;
          if (request.method === "snapshot") result = {sessionId,epoch,sequence:0,pid:process.pid,messages:[],dialogs:[],turn:{isLoading:false},capabilities:{durableSubmissionDeduplication:1}};
          else {
            assert.equal(request.sessionId,sessionId);
            assert.equal(request.epoch,epoch);
            requests.push({method:request.method,id:request.submissionId,text:request.text});
            let outcome;
            if (request.method === "submission_status") outcome = records.status(request.submissionId,request.text);
            else {
              assert.equal(request.method,"prompt");
              outcome = records.claim(request.submissionId,request.text);
              if (!outcome) {
                admitted.push(request.submissionId);
                outcome = {state:mode === "uncertain" ? "uncertain" : "accepted",uuid:request.submissionId};
                records.finish(request.submissionId,outcome);
              }
            }
            result = {...outcome,accepted:outcome.state==="accepted"};
            if (mode === "drop-receipt") { client.destroy(); continue; }
          }
          client.write(JSON.stringify({id:request.id,result})+"\n");
        } catch (error) { client.write(JSON.stringify({id:request.id,error:error.message})+"\n"); }
      }
    });
  });
  const socketPath = join(host,"native.sock");
  server.listen(socketPath); await once(server,"listening"); chmodSync(socketPath,0o600);
  let graphical;
  const readStore = window => JSON.parse(readFileSync(join(graphical.configuration(window),"harness/drafts.json")));
  const select = async window => {
    await graphical.click(window,145,48); await delay(700);
    await graphical.click(window,120,180); await delay(700);
  };
  const send = async window => { await graphical.click(window,1250,880); await delay(700); };
  const text = "Fixture prompt: perform exactly one action.";
  try {
    graphical = await isolatedGui(harness,environment,directory);
    let window = await graphical.open({[draftKey]:text}); await select(window); await send(window);
    assert.equal(admitted.length,1);
    const originalId = admitted[0];
    assert.equal(readStore(window).claude_pending_sends[draftKey].id,originalId);
    assert.equal(readStore(window).drafts[draftKey],text);
    await graphical.capture("lost-receipt-draft-kept");
    await graphical.close(); graphical = undefined;
    epoch = randomUUID(); mode = "accepted";
    graphical = await isolatedGui(harness,environment,directory);
    window = await graphical.open({}, {restore:true}); await select(window); await send(window);
    assert.equal(admitted.length,1,"Retry after frontend and adapter epoch changes must not enqueue again");
    assert.equal(requests.at(-1).id,originalId);
    assert.equal(readStore(window).claude_pending_sends[draftKey],undefined);
    assert.equal(readStore(window).drafts[draftKey],undefined);
    await graphical.capture("receipt-recovered");
    await graphical.click(window,360,810); await graphical.key(window,"i"); await graphical.type(window,text); await delay(450); await send(window);
    assert.equal(admitted.length,2,"Intentionally typing the same text again is a new send");
    assert.notEqual(admitted[1],originalId);
    mode = "uncertain";
    await graphical.click(window,360,810); await graphical.type(window,"Uncertain fixture prompt"); await delay(450); await send(window);
    assert.equal(admitted.length,3);
    const uncertainId = admitted[2];
    assert.equal(readStore(window).claude_pending_sends[draftKey].id,uncertainId);
    await send(window);
    assert.equal(admitted.length,3);
    assert.equal(requests.at(-1).id,uncertainId);
    assert.equal(readStore(window).drafts[draftKey],"Uncertain fixture prompt");
    await graphical.capture("uncertain-retry-not-duplicated");
    const storePath = join(graphical.configuration(window),"harness/drafts.json");
    await graphical.close(); graphical = undefined;
    const edited = JSON.parse(readFileSync(storePath));
    edited.drafts[draftKey] = "A different draft prepared while the previous send is uncertain";
    writeFileSync(storePath,JSON.stringify(edited),{mode:0o600});
    graphical = await isolatedGui(harness,environment,directory);
    window = await graphical.open({}, {restore:true}); await select(window); await send(window);
    assert.equal(requests.at(-1).method,"submission_status");
    assert.equal(requests.at(-1).id,uncertainId);
    assert.equal(admitted.length,3,"An edited draft must not silently replace an unresolved send");
    assert.equal(readStore(window).drafts[draftKey],edited.drafts[draftKey]);
    await graphical.capture("edited-draft-kept-during-reconciliation");
    await graphical.close(); graphical = undefined;
    const cleared = JSON.parse(readFileSync(storePath));
    delete cleared.drafts[draftKey];
    writeFileSync(storePath,JSON.stringify(cleared),{mode:0o600});
    graphical = await isolatedGui(harness,environment,directory);
    window = await graphical.open({}, {restore:true}); await select(window);
    const beforeCheck = requests.length;
    await send(window);
    assert.equal(requests.length,beforeCheck+1,"Clearing the composer must not disable checking a saved send");
    assert.equal(requests.at(-1).method,"submission_status");
    assert.equal(admitted.length,3);
    await graphical.capture("empty-draft-can-check-pending-send");
    console.log(JSON.stringify({directory,kind:"real Harness UI with a synthetic managed transport and production durable ledger",modelRequestsSent:0,admissions:admitted.length,frontendRestart:true,changedAdapterEpoch:true,sameIdRetry:true,intentionalRepeatDistinct:true,uncertainDraftRetained:true,emptyDraftCanCheck:true,probeDisconnects}));
  } finally {
    if (graphical) { try { await graphical.capture("final"); } finally { await graphical.close(); } }
    for (const client of clients) client.destroy();
    await new Promise((resolveClosed,reject)=>server.close(error=>error?reject(error):resolveClosed()));
    console.log(`Private submission GUI evidence: ${directory}`);
  }
});
