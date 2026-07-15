import { formatQuotaAmount, formatQuotaPair } from './quotaAmount.mjs';

export function selectPrimarySnapshot(snapshots) {
  const codex = snapshots.find((snapshot) => snapshot.providerType === 'codex');
  const newApi = snapshots.find((snapshot) => snapshot.providerType === 'new-api');
  const accountType = codex?.account?.accountType;

  if (accountType === 'api') {
    return newApi ?? codex ?? null;
  }

  if (accountType === 'official_login') {
    return codex ?? newApi ?? null;
  }

  return codex?.status === 'ok' ? codex : newApi ?? codex ?? null;
}

export function getCapsuleDisplay(snapshot) {
  if (!snapshot) {
    return { title: 'Codex', subtitle: '暂无数据', meta: '' };
  }

  if (snapshot.providerType === 'new-api') {
    const amountMode = snapshot.amountDisplayMode ?? 'cny';
    const amountOptions = { cnyPerUsd: snapshot.exchangeRateCnyPerUsd };
    const tokenUsage = preferTodayUsage(snapshot);
    const todaySpendRaw = snapshot.localLogs?.today?.rawUsedAmount;
    return {
      title:
        `${snapshot.providerName ?? 'New API'} · ` +
        `余额 ${formatQuotaPair(snapshot.balance, amountMode, amountOptions)} · ` +
        `今日花费 ${formatQuotaAmount(todaySpendRaw, amountMode, amountOptions)}`,
      subtitle:
        `输入 ${formatTokenCount(tokenUsage.inputTokens)} · ` +
        `缓存 ${formatTokenCount(tokenUsage.cachedInputTokens)} · ` +
        `输出 ${formatTokenCount(tokenUsage.outputTokens)} · ` +
        `命中 ${formatPercent(tokenUsage.cacheHitRate)}`,
      meta: ''
    };
  }

  const window5h = snapshot.quota?.window5h?.remainingPercent;
  const weekly = snapshot.quota?.weekly?.remainingPercent;
  const hasWindow5h = Number.isFinite(window5h);
  const hasWeekly = Number.isFinite(weekly);
  const tokenUsage = preferUsage(snapshot);
  const tokenLine = `输入 ${formatTokenCount(tokenUsage.inputTokens)} · 缓存 ${formatTokenCount(tokenUsage.cachedInputTokens)} · 输出 ${formatTokenCount(tokenUsage.outputTokens)} · 命中 ${formatPercent(tokenUsage.cacheHitRate)}`;
  const hasTokenUsage =
    Number.isFinite(Number(tokenUsage.inputTokens)) ||
    Number.isFinite(Number(tokenUsage.cachedInputTokens)) ||
    Number.isFinite(Number(tokenUsage.outputTokens));
  const subtitle = hasWindow5h
    ? `5h 剩余 ${formatPercent(window5h)} · 刷新 ${formatTimeOnly(snapshot.quota?.window5h?.resetAt)}`
    : hasWeekly
      ? `7d 剩余 ${formatPercent(weekly)} · 刷新 ${formatMonthDayTime(snapshot.quota?.weekly?.resetAt)}`
      : hasTokenUsage
        ? tokenLine
        : '官方余量源未发现';
  return {
    title: snapshot.providerName ?? 'Codex',
    subtitle,
    meta: hasWindow5h && hasWeekly
      ? `7d 剩余 ${formatPercent(weekly)} · 刷新 ${formatMonthDayTime(snapshot.quota?.weekly?.resetAt)}`
      : ''
  };
}

export function getPricingFormulaLines(profile) {
  const input = numberOrDefault(profile?.inputPricePerMillion, 0);
  const cachedInput = numberOrDefault(profile?.cachedInputPricePerMillion, 0);
  const output = numberOrDefault(profile?.outputPricePerMillion, 0);
  const modelRatio = numberOrDefault(profile?.modelRatio, 1);
  const completionRatio = numberOrDefault(profile?.completionRatio, 1);
  const groupRatio = numberOrDefault(profile?.groupRatio, 1);
  const safetyMultiplier = numberOrDefault(profile?.safetyMultiplier, 1);
  const quotaUnitPerUsd = numberOrDefault(profile?.quotaUnitPerUsd, 500000);
  const cnyPerUsd = numberOrDefault(profile?.cnyPerUsd, 7.2);

  return {
    money:
      `费用 = ((输入Token - 缓存输入Token)/1M * ${input} + 缓存输入Token/1M * ${cachedInput} + 输出Token/1M * ${output}) ` +
      `* 模型倍率 ${modelRatio} * 分组倍率 ${groupRatio} * 安全倍率 ${safetyMultiplier}；` +
      `汇率 1 USD = ${cnyPerUsd} CNY`,
    quota:
      `配额 = ((输入Token - 缓存输入Token) + 缓存输入Token + 输出Token * 补全倍率 ${completionRatio}) ` +
      `* 模型倍率 ${modelRatio} * 分组倍率 ${groupRatio} * 安全倍率 ${safetyMultiplier}；1 元 = ${quotaUnitPerUsd} 配额`
  };
}

function formatPercent(value) {
  return Number.isFinite(value) ? `${Math.round(value)}%` : '-';
}

function formatTokenCount(value) {
  if (!Number.isFinite(value)) return '-';
  const amount = Number(value);
  if (amount >= 1_000_000_000) return `${trim(amount / 1_000_000_000)}B`;
  if (amount >= 1_000_000) return `${trim(amount / 1_000_000)}M`;
  if (amount >= 1_000) return `${trim(amount / 1_000)}K`;
  return String(amount);
}

function trim(value) {
  return value.toFixed(2).replace(/\.?0+$/, '');
}

function formatResetTime(value) {
  if (!value) return '-';
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return '-';
  return date.toLocaleString('zh-CN', {
    hour12: false,
    month: '2-digit',
    day: '2-digit',
    hour: '2-digit',
    minute: '2-digit'
  });
}

function formatTimeOnly(value) {
  if (!value) return '-';
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return '-';
  return date.toLocaleTimeString('zh-CN', {
    hour12: false,
    hour: '2-digit',
    minute: '2-digit'
  });
}

function formatMonthDayTime(value) {
  if (!value) return '-';
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return '-';
  return date.toLocaleString('zh-CN', {
    hour12: false,
    month: '2-digit',
    day: '2-digit',
    hour: '2-digit',
    minute: '2-digit'
  });
}

function numberOrDefault(value, fallback) {
  const number = Number(value);
  return Number.isFinite(number) ? number : fallback;
}

function preferUsage(snapshot) {
  if (snapshot.localLogs?.today && Number(snapshot.localLogs.today.requestCount) > 0) {
    return snapshot.localLogs.today;
  }
  if (snapshot.usage?.log && Number(snapshot.usage.log.requestCount) > 0) {
    return snapshot.usage.log;
  }
  if (snapshot.localLogs?.all && Number(snapshot.localLogs.all.requestCount) > 0) {
    return snapshot.localLogs.all;
  }
  return snapshot.usage ?? {};
}

function preferTodayUsage(snapshot) {
  return snapshot.localLogs?.today ?? {};
}
