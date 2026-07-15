import assert from 'node:assert/strict';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
const { _internals } = require('../electron/local-backend.cjs');

assert.equal(typeof _internals.settleCodexActivity, 'function');
assert.equal(typeof _internals.codexActivityUpdate, 'function');

const nowMs = Date.parse('2026-07-01T12:00:00.000Z');

const staleThinking = _internals.settleCodexActivity(
  { status: 'thinking', label: 'thinking', timestamp: '2026-07-01T11:57:00.000Z', needsHumanAttention: false },
  { mtimeMs: nowMs - 180_000 },
  nowMs
);

assert.equal(staleThinking.status, 'finished');
assert.equal(staleThinking.needsHumanAttention, false);
assert.equal(staleThinking.completedTask, true);

const staleExecuting = _internals.settleCodexActivity(
  { status: 'executing', label: 'executing', timestamp: '2026-07-01T11:57:00.000Z', needsHumanAttention: false },
  { mtimeMs: nowMs - 180_000 },
  nowMs
);

assert.equal(staleExecuting.status, 'finished');

const activeEditingUpdate = _internals.codexActivityUpdate(
  {
    timestamp: '2026-07-01T11:57:00.000Z',
    type: 'response_item',
    payload: { type: 'custom_tool_call', name: 'apply_patch', status: 'completed' }
  },
  { isInsideTurn: true, waitingForPlanChoice: false, lastFinalAnswerAt: null }
);
assert.equal(activeEditingUpdate.status, 'executing');
assert.equal(activeEditingUpdate.isInsideTurn, true);

const activeEditing = _internals.settleCodexActivity(
  {
    status: activeEditingUpdate.status,
    label: 'executing',
    timestamp: '2026-07-01T11:57:00.000Z',
    needsHumanAttention: false,
    activeTurn: activeEditingUpdate.isInsideTurn
  },
  { mtimeMs: nowMs - 180_000 },
  nowMs
);
assert.equal(activeEditing.status, 'executing');

const activeLongThinking = _internals.settleCodexActivity(
  {
    status: 'thinking',
    label: 'thinking',
    timestamp: '2026-07-01T11:00:00.000Z',
    needsHumanAttention: false,
    activeTurn: true
  },
  { mtimeMs: nowMs - 3_600_000 },
  nowMs
);
assert.equal(activeLongThinking.status, 'thinking');

const waitingForUser = _internals.settleCodexActivity(
  { status: 'waiting_for_user', label: 'waiting', timestamp: '2026-07-01T11:57:00.000Z', needsHumanAttention: true },
  { mtimeMs: nowMs - 180_000 },
  nowMs
);

assert.equal(waitingForUser.status, 'waiting_for_user');
assert.equal(waitingForUser.needsHumanAttention, true);

const freshThinking = _internals.settleCodexActivity(
  { status: 'thinking', label: 'thinking', timestamp: '2026-07-01T11:59:46.000Z', needsHumanAttention: false },
  { mtimeMs: nowMs - 14_000 },
  nowMs
);

assert.equal(freshThinking.status, 'thinking');

const completed = _internals.codexActivityUpdate(
  { timestamp: '2026-07-03T08:00:00.200Z', type: 'event_msg', payload: { type: 'task_complete' } },
  {
    isInsideTurn: true,
    waitingForPlanChoice: false,
    lastFinalAnswerAt: Date.parse('2026-07-03T08:00:00.000Z') / 1000
  }
);
assert.equal(completed.status, 'finished');

const staleAfter15Seconds = _internals.settleCodexActivity(
  { status: 'thinking', timestamp: '2026-07-03T08:00:00.000Z', needsHumanAttention: false },
  { mtimeMs: Date.parse('2026-07-03T08:00:00.000Z') },
  Date.parse('2026-07-03T08:00:15.001Z')
);
assert.equal(staleAfter15Seconds.status, 'finished');

console.log('codex activity behavior tests passed');
