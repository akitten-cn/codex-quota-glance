function applyCodexRuntimeObservation(current = {}, observation = {}) {
  const next = { ...current };
  const accountType = stringOrUndefined(observation.accountType);
  const now = finiteTimestamp(observation.now);

  if (accountType === 'api' && current.lastAccountType !== 'api' && now !== undefined) {
    next.apiSpendStartedAt = now;
  }
  if (accountType) next.lastAccountType = accountType;

  if (accountType === 'official_login' && observation.fiveHourQuotaUnavailable) {
    next.lastOfficialRemainingPercent = undefined;
    next.waitingResetAt = undefined;
    next.confirmedResetAt = undefined;
    return next;
  }

  const remaining = Number(observation.quota?.remainingPercent);
  const resetAt = toUnixSeconds(observation.quota?.resetAt);
  if (accountType === 'official_login' && Number.isFinite(remaining) && remaining <= 0 && resetAt !== undefined) {
    if (resetAt !== current.waitingResetAt) next.confirmedResetAt = undefined;
    next.waitingResetAt = resetAt;
    next.lastOfficialRemainingPercent = remaining;
  } else if (accountType === 'official_login' && Number.isFinite(remaining)) {
    next.lastOfficialRemainingPercent = remaining;
  }
  return next;
}

function isQuotaReminderPending(state = {}, now = Date.now() / 1000) {
  const resetAt = finiteTimestamp(state.waitingResetAt);
  if (resetAt === undefined || Number(now) < resetAt || state.confirmedResetAt === resetAt) return false;
  return state.lastAccountType === 'api' || Number(state.lastOfficialRemainingPercent) > 0;
}

function confirmQuotaReminder(state = {}, resetAt) {
  const value = finiteTimestamp(resetAt);
  return value === state.waitingResetAt ? { ...state, confirmedResetAt: value } : state;
}

function finiteTimestamp(value) {
  const number = Number(value);
  return Number.isFinite(number) && number >= 0 ? Math.trunc(number) : undefined;
}

function toUnixSeconds(value) {
  if (value === undefined || value === null || value === '') return undefined;
  if (typeof value === 'number') return finiteTimestamp(value);
  const timestamp = Date.parse(String(value));
  return Number.isFinite(timestamp) ? Math.trunc(timestamp / 1000) : undefined;
}

function stringOrUndefined(value) {
  if (value === undefined || value === null) return undefined;
  const text = String(value).trim();
  return text || undefined;
}

module.exports = {
  applyCodexRuntimeObservation,
  confirmQuotaReminder,
  isQuotaReminderPending
};
