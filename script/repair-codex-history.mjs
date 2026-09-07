import { createHash } from 'node:crypto';
import { createReadStream, existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { homedir } from 'node:os';
import { join, resolve } from 'node:path';
import { createInterface } from 'node:readline';
import { DatabaseSync } from 'node:sqlite';
import { pathToFileURL } from 'node:url';

export async function inspectRepair(directory, plan) {
  if (plan.version !== 1 || plan.cliVersion !== '0.153.4' || !/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/.test(plan.threadId)
    || !Number.isSafeInteger(plan.byteOffset) || plan.byteOffset <= 0
    || !Number.isSafeInteger(plan.expectedOrdinal) || plan.expectedOrdinal < 1
    || plan.observedOrdinal !== plan.expectedOrdinal - 1
    || !/^[0-9a-f]{64}$/.test(plan.prefixSha256)) {
    throw new Error('Invalid or unsupported repair plan; refusing to guess an index checkpoint');
  }
  const metadata = new DatabaseSync(join(directory, 'state_5.sqlite'), { readOnly: true });
  let thread;
  try {
    thread = metadata.prepare('SELECT rollout_path, history_mode, cli_version FROM threads WHERE id = ?').get(plan.threadId);
  } finally { metadata.close(); }
  if (thread?.history_mode !== 'paginated') throw new Error('Expected an existing paginated conversation');
  if (thread.cli_version !== plan.cliVersion) throw new Error('The conversation was updated by a different Codex version; re-test before repairing');
  const historyPath = join(directory, 'thread_history_1.sqlite');
  if (!existsSync(historyPath)) throw new Error('History database is missing');
  const database = new DatabaseSync(historyPath, { readOnly: true });
  let checkpoint;
  try {
    checkpoint = database.prepare('SELECT next_rollout_byte_offset, next_rollout_ordinal FROM thread_history_projection_state WHERE thread_id = ?').get(plan.threadId);
    validateCheckpoint(database, plan, checkpoint);
  } finally { database.close(); }

  const hash = createHash('sha256');
  let bytes = 0;
  let lastByte;
  for await (const chunk of createReadStream(thread.rollout_path, { start: 0, end: plan.byteOffset - 1 })) {
    bytes += chunk.length;
    lastByte = chunk.at(-1);
    hash.update(chunk);
  }
  if (bytes !== plan.byteOffset || lastByte !== 10 || hash.digest('hex') !== plan.prefixSha256) {
    throw new Error('The saved transcript prefix differs from the tested repair plan');
  }
  const lines = createInterface({ input: createReadStream(thread.rollout_path, { start: 0, end: Math.min(plan.byteOffset - 1, 1024 * 1024) }) });
  try {
    for await (const line of lines) {
      const header = JSON.parse(line);
      if (header.type !== 'session_meta' || header.payload?.id !== plan.threadId) {
        throw new Error('Saved transcript identity does not match the repair plan');
      }
      break;
    }
  } finally { lines.close(); lines.input.destroy(); }
  return { historyPath, rolloutPath: thread.rollout_path, checkpoint };
}

function validateCheckpoint(database, plan, checkpoint) {
  if (checkpoint?.next_rollout_byte_offset !== plan.byteOffset
    || checkpoint.next_rollout_ordinal !== plan.expectedOrdinal) {
    throw new Error('The checkpoint has changed; it may already be repaired. Nothing was modified');
  }
  // Rewinding over an already materialized item could overwrite its identity.
  for (const table of ['thread_items', 'thread_turns', 'thread_realtime_items']) {
    const row = database.prepare(`SELECT max(rollout_ordinal) AS maximum FROM ${table} WHERE thread_id = ?`).get(plan.threadId);
    if (row.maximum !== null && row.maximum >= plan.observedOrdinal) {
      throw new Error('The proposed ordinal overlaps materialized history; refusing repair');
    }
  }
  const updated = database.prepare('SELECT max(updated_at_ordinal) AS maximum FROM thread_items WHERE thread_id = ?').get(plan.threadId);
  if (updated.maximum !== null && updated.maximum >= plan.observedOrdinal) {
    throw new Error('The proposed ordinal overlaps an item update; refusing repair');
  }
}

export async function repairHistory(directory, plan, apply = false) {
  const inspected = await inspectRepair(directory, plan);
  if (!apply) return { applied: false, ...inspected };
  const database = new DatabaseSync(inspected.historyPath);
  database.exec('PRAGMA busy_timeout = 5000');
  let transaction = false;
  let backupPath;
  try {
    database.exec('BEGIN IMMEDIATE');
    transaction = true;
    const checkpoint = database.prepare('SELECT next_rollout_byte_offset, next_rollout_ordinal FROM thread_history_projection_state WHERE thread_id = ?').get(plan.threadId);
    validateCheckpoint(database, plan, checkpoint);
    const backupDirectory = join(directory, 'harness-history-recovery');
    mkdirSync(backupDirectory, { recursive: true, mode: 0o700 });
    backupPath = join(backupDirectory, `${plan.threadId}-${Date.now()}.json`);
    writeFileSync(backupPath, JSON.stringify({ plan, checkpoint, rolloutPath: inspected.rolloutPath }, null, 2) + '\n', { flag: 'wx', mode: 0o600 });
    const result = database.prepare('UPDATE thread_history_projection_state SET next_rollout_ordinal = ? WHERE thread_id = ? AND next_rollout_byte_offset = ? AND next_rollout_ordinal = ?')
      .run(plan.observedOrdinal, plan.threadId, plan.byteOffset, plan.expectedOrdinal);
    if (result.changes !== 1) throw new Error('Checkpoint changed during repair');
    database.exec('COMMIT');
    transaction = false;
  } finally {
    if (transaction) database.exec('ROLLBACK');
    database.close();
  }
  return { applied: true, backupPath, threadId: plan.threadId };
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  try {
    let planPath;
    let directory = process.env.CODEX_HOME ?? join(homedir(), '.codex');
    let apply = false;
    const arguments_ = process.argv.slice(2);
    for (let index = 0; index < arguments_.length; index++) {
      if (arguments_[index] === '--apply') apply = true;
      else if (arguments_[index] === '--plan') planPath = arguments_[++index];
      else if (arguments_[index] === '--codex-home') directory = arguments_[++index];
      else throw new Error('Usage: node script/repair-codex-history.mjs --plan PLAN.json [--codex-home DIR] [--apply]');
    }
    if (!planPath || !directory) throw new Error('An explicit, tested --plan is required');
    const result = await repairHistory(resolve(directory), JSON.parse(readFileSync(planPath, 'utf8')), apply);
    console.log(JSON.stringify(result, null, 2));
    console.log(apply ? 'Checkpoint corrected. The running Codex writer can now catch up; refresh Harness after it does.' : 'Validated only. Re-run with --apply to correct this one checkpoint; no transcript bytes are changed.');
  } catch (error) {
    console.error(error.message);
    process.exitCode = 1;
  }
}
