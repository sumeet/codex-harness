import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { DatabaseSync } from 'node:sqlite';
import test from 'node:test';
import { repairHistory } from './repair-codex-history.mjs';

function fixture(t) {
  const directory = mkdtempSync(join(tmpdir(), 'harness-index-repair-test-'));
  t.after(() => rmSync(directory, { recursive: true, force: true }));
  const threadId = '00000000-0000-4000-8000-000000000001';
  const path = join(directory, 'rollout.jsonl');
  const prefix = JSON.stringify({ type: 'session_meta', payload: { id: threadId } }) + '\n';
  const source = prefix + '{"type":"event_msg","payload":{"type":"task_started"}}\n';
  writeFileSync(path, source);
  const metadata = new DatabaseSync(join(directory, 'state_5.sqlite'));
  metadata.exec('CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT, history_mode TEXT, cli_version TEXT)');
  metadata.prepare('INSERT INTO threads VALUES (?, ?, ?, ?)').run(threadId, path, 'paginated', '0.153.4');
  metadata.close();
  const database = new DatabaseSync(join(directory, 'thread_history_1.sqlite'));
  database.exec('CREATE TABLE thread_history_projection_state (thread_id TEXT PRIMARY KEY, next_rollout_byte_offset INTEGER, next_rollout_ordinal INTEGER); CREATE TABLE thread_items (thread_id TEXT, rollout_ordinal INTEGER, updated_at_ordinal INTEGER); CREATE TABLE thread_turns (thread_id TEXT, rollout_ordinal INTEGER); CREATE TABLE thread_realtime_items (thread_id TEXT, rollout_ordinal INTEGER)');
  database.prepare('INSERT INTO thread_history_projection_state VALUES (?, ?, ?)').run(threadId, Buffer.byteLength(prefix), 12);
  database.prepare('INSERT INTO thread_history_projection_state VALUES (?, ?, ?)').run('unrelated', 999, 40);
  database.close();
  const plan = { version: 1, cliVersion: '0.153.4', threadId, byteOffset: Buffer.byteLength(prefix), expectedOrdinal: 12, observedOrdinal: 11, prefixSha256: createHash('sha256').update(prefix).digest('hex') };
  return { directory, source, path, plan };
}

test('dry run is read-only; apply changes exactly one checkpoint and backs it up', async t => {
  const f = fixture(t);
  assert.equal((await repairHistory(f.directory, f.plan)).applied, false);
  const result = await repairHistory(f.directory, f.plan, true);
  assert.equal(result.applied, true);
  assert.equal(JSON.parse(readFileSync(result.backupPath)).checkpoint.next_rollout_ordinal, 12);
  assert.equal(readFileSync(f.path, 'utf8'), f.source);
  const database = new DatabaseSync(join(f.directory, 'thread_history_1.sqlite'), { readOnly: true });
  assert.equal(database.prepare('SELECT next_rollout_ordinal AS n FROM thread_history_projection_state WHERE thread_id=?').get(f.plan.threadId).n, 11);
  assert.equal(database.prepare('SELECT next_rollout_ordinal AS n FROM thread_history_projection_state WHERE thread_id=?').get('unrelated').n, 40);
  database.close();
  await assert.rejects(repairHistory(f.directory, f.plan, true), /checkpoint has changed/);
});

test('wrong prefix and overlapping materialized items fail closed', async t => {
  const f = fixture(t);
  await assert.rejects(repairHistory(f.directory, { ...f.plan, prefixSha256: '0'.repeat(64) }, true), /prefix differs/);
  const database = new DatabaseSync(join(f.directory, 'thread_history_1.sqlite'));
  database.prepare('INSERT INTO thread_items VALUES (?, ?, ?)').run(f.plan.threadId, f.plan.observedOrdinal, f.plan.observedOrdinal);
  database.close();
  await assert.rejects(repairHistory(f.directory, f.plan, true), /overlaps materialized/);
});

test('unsupported plans and missing history stores are never created or repaired', async t => {
  const f = fixture(t);
  await assert.rejects(repairHistory(f.directory, { ...f.plan, observedOrdinal: 5 }, true), /unsupported repair plan/);
  rmSync(join(f.directory, 'thread_history_1.sqlite'));
  await assert.rejects(repairHistory(f.directory, f.plan, true), /History database is missing/);
});

test('backup failure rolls back and leaves the checkpoint untouched', async t => {
  const f = fixture(t);
  writeFileSync(join(f.directory, 'harness-history-recovery'), 'not a directory');
  await assert.rejects(repairHistory(f.directory, f.plan, true));
  const database = new DatabaseSync(join(f.directory, 'thread_history_1.sqlite'), { readOnly: true });
  assert.equal(database.prepare('SELECT next_rollout_ordinal AS n FROM thread_history_projection_state WHERE thread_id=?').get(f.plan.threadId).n, 12);
  database.close();
});
