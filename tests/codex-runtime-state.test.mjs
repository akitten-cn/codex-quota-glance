import assert from 'node:assert/strict';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
const {
  applyCodexRuntimeObservation,
  confirmQuotaReminder,
  isQuotaReminderPending
} = require('../electron/codex-runtime-state.cjs');

const official = applyCodexRuntimeObservation({}, {
  accountType: 'official_login',
  now: 1000,
  quota: { remainingPercent: 0, resetAt: '1970-01-01T00:20:00.000Z' }
});
assert.equal(official.lastAccountType, 'official_login');
assert.equal(official.waitingResetAt, 1200);

const api = applyCodexRuntimeObservation(official, {
  accountType: 'api',
  now: 1050
});
assert.equal(api.apiSpendStartedAt, 1050);
assert.equal(api.waitingResetAt, 1200);
assert.equal(isQuotaReminderPending(api, 1199), false);
assert.equal(isQuotaReminderPending(api, 1200), true);

const restartedApi = applyCodexRuntimeObservation(api, {
  accountType: 'api',
  now: 1100
});
assert.equal(restartedApi.apiSpendStartedAt, 1050);

const officialNotRecovered = applyCodexRuntimeObservation(official, {
  accountType: 'official_login',
  now: 1200,
  quota: { remainingPercent: 0, resetAt: '1970-01-01T00:20:00.000Z' }
});
assert.equal(isQuotaReminderPending(officialNotRecovered, 1200), false);

const officialRecovered = applyCodexRuntimeObservation(officialNotRecovered, {
  accountType: 'official_login',
  now: 1201,
  quota: { remainingPercent: 100, resetAt: '1970-01-01T00:40:00.000Z' }
});
assert.equal(isQuotaReminderPending(officialRecovered, 1201), true);

const confirmed = confirmQuotaReminder(api, 1200);
assert.equal(confirmed.confirmedResetAt, 1200);
assert.equal(isQuotaReminderPending(confirmed, 1300), false);

const nextCycle = applyCodexRuntimeObservation(confirmed, {
  accountType: 'official_login',
  now: 2000,
  quota: { remainingPercent: 0, resetAt: '1970-01-01T00:40:00.000Z' }
});
assert.equal(nextCycle.waitingResetAt, 2400);
assert.equal(nextCycle.confirmedResetAt, undefined);

const fiveHourRemoved = applyCodexRuntimeObservation(nextCycle, {
  accountType: 'official_login',
  now: 2100,
  fiveHourQuotaUnavailable: true
});
assert.equal(fiveHourRemoved.waitingResetAt, undefined);
assert.equal(fiveHourRemoved.confirmedResetAt, undefined);
assert.equal(fiveHourRemoved.lastOfficialRemainingPercent, undefined);

console.log('codex runtime state tests passed');
