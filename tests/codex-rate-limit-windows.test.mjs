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

const restoredWindows = _internals.normalizeCodexRateLimits({
  primary: { used_percent: 6, window_minutes: 300, resets_at: 1784690000 },
  secondary: { used_percent: 12, window_minutes: 10080, resets_at: 1785200000 }
});

assert.equal(restoredWindows.window5h.remainingPercent, 94);
assert.equal(restoredWindows.weekly.remainingPercent, 88);

const removedAgain = _internals.normalizeCodexRateLimits({
  primary: { used_percent: 15, window_minutes: 10080, resets_at: 1785280000 },
  secondary: null
});

assert.equal(removedAgain.window5h.remainingPercent, undefined);
assert.equal(removedAgain.weekly.remainingPercent, 85);

const rpcWithoutWindowMinutes = _internals.normalizeCodexRateLimitsCamel({
  primary: {
    usedPercent: 19,
    windowMinutes: null,
    resetsAt: 1784682595
  },
  secondary: null,
  planType: 'plus'
});
const sessionWithWeeklyWindow = _internals.normalizeCodexRateLimits({
  primary: {
    used_percent: 19,
    window_minutes: 10080,
    resets_at: 1784682594
  },
  secondary: null,
  plan_type: 'plus'
});
assert.equal(typeof _internals.reconcileCodexRateLimits, 'function');
const reconciledWeeklyWindow = _internals.reconcileCodexRateLimits(
  rpcWithoutWindowMinutes,
  sessionWithWeeklyWindow
);

assert.equal(reconciledWeeklyWindow.window5h.remainingPercent, undefined);
assert.equal(reconciledWeeklyWindow.weekly.remainingPercent, 81);
assert.equal(reconciledWeeklyWindow.weekly.windowMinutes, 10080);
assert.equal(reconciledWeeklyWindow.weekly.resetAt, '2026-07-22T01:09:55.000Z');

console.log('codex rate limit window tests passed');
