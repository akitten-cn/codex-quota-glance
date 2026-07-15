import assert from 'node:assert/strict';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
const { _internals } = require('../electron/local-backend.cjs');

const weeklyOnly = _internals.normalizeCodexRateLimits({
  primary: {
    used_percent: 11,
    window_minutes: 10080,
    resets_at: 1784682594
  },
  secondary: null,
  plan_type: 'plus'
});

assert.equal(weeklyOnly.window5h.remainingPercent, undefined);
assert.equal(weeklyOnly.weekly.remainingPercent, 89);
assert.equal(weeklyOnly.weekly.windowMinutes, 10080);
assert.equal(weeklyOnly.weekly.resetAt, '2026-07-22T01:09:54.000Z');

const standardWindows = _internals.normalizeCodexRateLimits({
  primary: { used_percent: 20, window_minutes: 300, resets_at: 1784000000 },
  secondary: { used_percent: 40, window_minutes: 10080, resets_at: 1784600000 }
});

assert.equal(standardWindows.window5h.remainingPercent, 80);
assert.equal(standardWindows.window5h.windowMinutes, 300);
assert.equal(standardWindows.weekly.remainingPercent, 60);
assert.equal(standardWindows.weekly.windowMinutes, 10080);

const weeklyOnlyCamel = _internals.normalizeCodexRateLimitsCamel({
  primary: { usedPercent: 13, windowMinutes: 10080, resetsAt: 1784682594 },
  secondary: null
});

assert.equal(weeklyOnlyCamel.window5h.remainingPercent, undefined);
assert.equal(weeklyOnlyCamel.weekly.remainingPercent, 87);

console.log('codex rate limit window tests passed');
