import { NativeClient } from "./client.mjs";
import { randomUUID } from "node:crypto";

const [socketPath, recordPath, prompt] = process.argv.slice(2);
if (!socketPath || !recordPath || !prompt) throw new Error("Usage: node run_prompt.mjs SOCKET NEW_RECORD_FILE PROMPT");
const client = new NativeClient(socketPath, recordPath);
let active = false;
let completionBefore = 0;
const counts = {};
client.on("event", (event) => {
  const kind = event.event === "engine_event" ? `engine:${event.data.type}` : event.event;
  counts[kind] = (counts[kind] ?? 0) + 1;
  if (event.event === "turn" && event.data.isLoading) active = true;
  if (event.event === "engine_event" && event.data.type === "assistant") {
    for (const block of event.data.message.content ?? []) {
      if (block.type === "tool_use") console.log(JSON.stringify({ tool: block.name, input: block.input }));
      else if (block.type === "text") console.log(JSON.stringify({ text: block.text }));
    }
  }
});
try {
  const before = await client.request("snapshot");
  completionBefore = before.turn.lastQueryCompletionTime;
  if (before.turn.isLoading || before.dialogs.length)
    throw new Error("Session is not idle; finish or interrupt the current test first");
  console.log(JSON.stringify({ pid: before.pid, sessionId: before.sessionId, remoteControl: before.remoteControl }));
  console.log(JSON.stringify(await client.request("prompt", { text: prompt, submissionId:randomUUID() })));
  const result = await client.waitFor(
    (event) =>
      event.event === "dialog" ||
      (active &&
        event.event === "turn" &&
        !event.data.isLoading &&
        event.data.lastQueryCompletionTime > completionBefore),
    90000,
  );
  if (result.event === "dialog") console.log(JSON.stringify({ pendingDialog: result.data }));
  else console.log(JSON.stringify({ completed: true }));
  console.log(JSON.stringify({ counts }));
} finally {
  client.close();
}
