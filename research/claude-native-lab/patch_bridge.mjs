import assert from "node:assert/strict";
import { readFileSync, writeFileSync } from "node:fs";
import { createHash } from "node:crypto";
import { inspectContainer } from "./bun_container.mjs";

const [input, output] = process.argv.slice(2);
assert.ok(input && output, "Usage: node patch_bridge.mjs ORIGINAL NEW_BINARY");
const buffer = readFileSync(input);
const expected = "b9c407e36847bcb24b953b1390f240c840ae6c99e10a76475d2fadc5d5c4adca";
assert.equal(createHash("sha256").update(buffer).digest("hex"), expected, "Unverified Claude build; refusing to patch");
const container = inspectContainer(buffer);
const module = container.modules.find((entry) => entry.name.endsWith("/chunk-s5gs13rj.js"));
assert.ok(module, "Missing native REPL module");
const source = buffer
  .subarray(container.dataStart + module.sourceOffset, container.dataStart + module.sourceOffset + module.sourceLength)
  .toString();
const anchor = "Zt=hLt(x.guard,()=>x.isExternalLoading);return NLt(Xe,H,at,Qe,it,";
assert.equal(source.split(anchor).length, 2, "REPL bridge anchor is not unique");
const injection = `Zt=hLt(x.guard,()=>x.isExternalLoading);
if(process.env.HARNESS_CLAUDE_BRIDGE_MODULE){
const handles={turn:x,transcript:H,scope:ne,permissionRelays:me,engine:fe,transport:Se,session:Ke,sessionId:X(),agentId:Ve()};
globalThis.__harnessClaudeHandles=handles;
if(globalThis.__harnessClaudeBridge)globalThis.__harnessClaudeBridge.attach(handles);
else if(!globalThis.__harnessClaudeBridgeLoading){
globalThis.__harnessClaudeBridgeLoading=true;
import(process.env.HARNESS_CLAUDE_BRIDGE_MODULE).then(module=>{
globalThis.__harnessClaudeBridge=module;
module.attach(globalThis.__harnessClaudeHandles);
}).catch(error=>process.stderr.write("Harness bridge failed: "+String(error)+"\\n"));
}}
return NLt(Xe,H,at,Qe,it,`;
const replacement = Buffer.from(source.replace(anchor, injection).replace("// @bun @bytecode", "// @bun") + "\0");
assert.ok(replacement.length < module.bytecodeLength, "Replacement exceeds module's reserved bytecode space");
// Reuse only this module's bytecode allocation; all ELF offsets and other modules remain unchanged.
replacement.copy(buffer, container.dataStart + module.bytecodeOffset);
buffer.writeUInt32LE(module.bytecodeOffset, module.record + 8);
buffer.writeUInt32LE(replacement.length - 1, module.record + 12);
buffer.fill(0, module.record + 24, module.record + 40);
writeFileSync(output, buffer, { flag: "wx", mode: 0o700 });
console.log(
  JSON.stringify({
    original: expected,
    patched: createHash("sha256").update(buffer).digest("hex"),
    module: module.name,
    injectedBytes: replacement.length - module.sourceLength,
  }),
);
