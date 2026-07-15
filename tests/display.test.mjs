import assert from 'node:assert/strict';
import {
  getCapsuleDisplay,
  getPricingFormulaLines,
  selectPrimarySnapshot
} from '../src/lib/display.mjs';

const codexOfficial = {
  providerId: 'codex-local',
  providerName: 'Codex',
  providerType: 'codex',
  account: { accountType: 'official_login' },
  quota: {
    window5h: { remainingPercent: 68, resetAt: '2026-06-06T06:40:56+00:00' },
    weekly: { remainingPercent: 42, resetAt: '2026-06-11T05:34:26+00:00' }
  },
  status: 'ok'
};

const codexApi = {
  ...codexOfficial,
  account: { accountType: 'api' }
};

const newApi = {
  providerId: 'new-api-main',
  providerName: '工作中转站',
  providerType: 'new-api',
  amountDisplayMode: 'cny',
  exchangeRateCnyPerUsd: 7.2,
  usage: {
    inputTokens: 18200,
    cachedInputTokens: 6800,
    outputTokens: 7400,
    totalTokens: 32000,
    cacheHitRate: 37.3
  },
  balance: {
    balance: 12.3,
    totalRecharged: 20,
    usedAmount: 7.7,
    rawBalance: 6150000,
    rawTotalRecharged: 10000000,
    rawUsedAmount: 3850000,
    currency: 'CNY',
    estimated: false
  },
  balanceKey: {
    rawUsedAmount: 3850000,
    rawTotalGranted: 0,
    rawTotalAvailable: 0,
    usedAmount: 7.7,
    unlimitedQuota: true
  },
  status: 'ok'
};

assert.equal(selectPrimarySnapshot([codexOfficial, newApi]).providerType, 'codex');
assert.equal(selectPrimarySnapshot([codexApi, newApi]).providerType, 'new-api');
assert.equal(selectPrimarySnapshot([codexApi]).providerType, 'codex');

const newApiWithAccountError = {
  ...newApi,
  status: 'error',
  error: 'Unauthorized, invalid access token'
};
assert.equal(selectPrimarySnapshot([codexApi, newApiWithAccountError]).providerType, 'new-api');

const apiCapsule = getCapsuleDisplay(newApi);
assert.equal(apiCapsule.title, '工作中转站 · 余额 ¥12.30/约¥20.00 · 今日花费 -');
assert.equal(apiCapsule.subtitle, '输入 - · 缓存 - · 输出 - · 命中 -');

const usdCapsule = getCapsuleDisplay({ ...newApi, amountDisplayMode: 'usd' });
assert.equal(usdCapsule.title, '工作中转站 · 余额 $1.71/约$2.78 · 今日花费 -');
assert.equal(usdCapsule.subtitle, '输入 - · 缓存 - · 输出 - · 命中 -');

const todaySpendCapsule = getCapsuleDisplay({
  ...newApi,
  localLogs: {
    today: {
      inputTokens: 150_000_000,
      cachedInputTokens: 146_000_000,
      outputTokens: 400_000,
      cacheHitRate: 97.33,
      rawUsedAmount: 15090000,
      usedAmount: 30.18,
      requestCount: 1029
    },
    all: {
      inputTokens: 134_163_319,
      cachedInputTokens: 127_557_376,
      outputTokens: 464_357,
      cacheHitRate: 95.07,
      requestCount: 100
    }
  }
});
assert.equal(todaySpendCapsule.title, '工作中转站 · 余额 ¥12.30/约¥20.00 · 今日花费 ¥30.18');
assert.equal(todaySpendCapsule.subtitle, '输入 150M · 缓存 146M · 输出 400K · 命中 97%');

const realTopupCapsule = getCapsuleDisplay({
  ...newApi,
  balance: {
    ...newApi.balance,
    rawTotalRecharged: 10000000,
    totalRechargedEstimated: false
  }
});
assert.equal(realTopupCapsule.title, '工作中转站 · 余额 ¥12.30/¥20.00 · 今日花费 -');

const officialCapsule = getCapsuleDisplay(codexOfficial);
assert.equal(officialCapsule.title, 'Codex');
assert.match(officialCapsule.subtitle, /5h/);
assert.match(officialCapsule.subtitle, /68%/);
assert.doesNotMatch(officialCapsule.subtitle, /7d/);
assert.match(officialCapsule.meta, /7d/);
assert.match(officialCapsule.meta, /42%/);
assert.match(officialCapsule.subtitle, /刷新/);
assert.doesNotMatch(officialCapsule.subtitle, /06\/06/);

const weeklyOnlyCapsule = getCapsuleDisplay({
  ...codexOfficial,
  quota: {
    window5h: {},
    weekly: { remainingPercent: 89, resetAt: '2026-07-20T06:23:14+00:00' }
  }
});
assert.match(weeklyOnlyCapsule.subtitle, /7d 剩余 89%/);
assert.match(weeklyOnlyCapsule.subtitle, /刷新/);
assert.doesNotMatch(weeklyOnlyCapsule.subtitle, /5h/);
assert.equal(weeklyOnlyCapsule.meta, '');

const restoredQuotaCapsule = getCapsuleDisplay(codexOfficial);
assert.match(restoredQuotaCapsule.subtitle, /5h/);
assert.match(restoredQuotaCapsule.meta, /7d/);

const officialPendingCapsule = getCapsuleDisplay({
  ...codexOfficial,
  quota: { window5h: {}, weekly: {} }
});
assert.equal(officialPendingCapsule.subtitle, '官方余量源未发现');

const officialTokenCapsule = getCapsuleDisplay({
  ...codexOfficial,
  quota: { window5h: {}, weekly: {} },
  usage: {
    inputTokens: 23212,
    cachedInputTokens: 7040,
    outputTokens: 68,
    cacheHitRate: 30.329
  }
});
assert.match(officialTokenCapsule.subtitle, /23\.21K/);
assert.match(officialTokenCapsule.subtitle, /7\.04K/);
assert.match(officialTokenCapsule.subtitle, /68/);
assert.match(officialTokenCapsule.subtitle, /30%/);

const officialDailyTokenCapsule = getCapsuleDisplay({
  ...codexOfficial,
  quota: { window5h: {}, weekly: {} },
  usage: {
    log: {
      requestCount: 1,
      inputTokens: 100,
      cachedInputTokens: 20,
      outputTokens: 10,
      cacheHitRate: 20
    }
  },
  localLogs: {
    today: {
      requestCount: 8,
      inputTokens: 9000,
      cachedInputTokens: 3000,
      outputTokens: 1200,
      cacheHitRate: 33.333
    }
  }
});
assert.match(officialDailyTokenCapsule.subtitle, /9K/);
assert.match(officialDailyTokenCapsule.subtitle, /3K/);
assert.match(officialDailyTokenCapsule.subtitle, /1\.2K/);
assert.doesNotMatch(officialDailyTokenCapsule.subtitle, /100/);

const officialQuotaWithTokenCapsule = getCapsuleDisplay({
  ...codexOfficial,
  usage: {
    inputTokens: 23212,
    cachedInputTokens: 7040,
    outputTokens: 68,
    cacheHitRate: 30.329
  }
});
assert.match(officialQuotaWithTokenCapsule.subtitle, /5h 剩余 68%/);
assert.doesNotMatch(officialQuotaWithTokenCapsule.meta, /输入 23\.21K/);
assert.match(officialQuotaWithTokenCapsule.meta, /7d 剩余 42%/);

const formulaLines = getPricingFormulaLines({
  currency: 'CNY',
  quotaUnitPerUsd: 500000,
  inputPricePerMillion: 2,
  cachedInputPricePerMillion: 0.5,
  outputPricePerMillion: 8,
  modelRatio: 1.2,
  completionRatio: 4,
  groupRatio: 1.5,
  safetyMultiplier: 1.1,
  cnyPerUsd: 7.2
});

assert.ok(formulaLines.money.includes('(输入Token - 缓存输入Token)/1M * 2'));
assert.ok(formulaLines.money.includes('缓存输入Token/1M * 0.5'));
assert.ok(formulaLines.money.includes('模型倍率 1.2'));
assert.ok(formulaLines.money.includes('汇率 1 USD = 7.2 CNY'));
assert.ok(formulaLines.quota.includes('输出Token * 补全倍率 4'));
assert.ok(formulaLines.quota.includes('1 元 = 500000 配额'));

console.log('display tests passed');
