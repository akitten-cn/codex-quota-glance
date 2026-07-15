import React from 'react';
import { formatPercent, formatTokenCount } from '../lib/format';
import { formatQuotaAmount, formatQuotaPair } from '../lib/quotaAmount.mjs';
import type { ProviderSnapshot } from '../types/provider';

interface Props {
  snapshot: ProviderSnapshot | null;
}

export default function DetailPanel({ snapshot }: Props) {
  if (!snapshot) {
    return (
      <section className="detail-panel">
        <div className="section-band">
          <h2>暂无可用数据</h2>
        </div>
      </section>
    );
  }

  return (
    <section className="detail-panel">
      <div className="section-band">
        <div className="section-title-row">
          <h2>{snapshot.providerName}</h2>
          <span className="mode-pill">{snapshot.providerType === 'new-api' ? 'API 模式' : '官方登录'}</span>
        </div>
        {snapshot.providerType === 'new-api' ? <NewApiDetails snapshot={snapshot} /> : <CodexDetails snapshot={snapshot} />}
      </div>
    </section>
  );
}

function CodexDetails({ snapshot }: { snapshot: ProviderSnapshot }) {
  const tokenUsage = snapshot.localLogs?.today ?? snapshot.usage;
  const tokenUpdatedAt = snapshot.localLogs?.today?.latestLogAt ?? snapshot.usage?.log?.updatedAt;
  const hasWindow5hQuota = Number.isFinite(Number(snapshot.quota?.window5h?.remainingPercent));
  const hasWeeklyQuota = Number.isFinite(Number(snapshot.quota?.weekly?.remainingPercent));
  const hasCodexQuotaData = hasWindow5hQuota || hasWeeklyQuota;
  const hasCodexTokenData =
    Number.isFinite(Number(tokenUsage?.inputTokens)) ||
    Number.isFinite(Number(tokenUsage?.cachedInputTokens)) ||
    Number.isFinite(Number(tokenUsage?.outputTokens));

  return (
    <dl>
      <dt>状态</dt>
      <dd>{statusText(snapshot.status)}</dd>
      {hasCodexQuotaData ? (
        <>
          {hasWindow5hQuota && (
            <>
              <dt>5h 剩余</dt>
              <dd>{formatPercent(snapshot.quota?.window5h?.remainingPercent)}</dd>
              <dt>5h 刷新</dt>
              <dd>{formatDateTime(snapshot.quota?.window5h?.resetAt)}</dd>
            </>
          )}
          {hasWeeklyQuota && (
            <>
              <dt>Weekly 剩余</dt>
              <dd>{formatPercent(snapshot.quota?.weekly?.remainingPercent)}</dd>
              <dt>Weekly 刷新</dt>
              <dd>{formatDateTime(snapshot.quota?.weekly?.resetAt)}</dd>
            </>
          )}
        </>
      ) : (
        <>
          <dt>余量</dt>
          <dd>{snapshot.quota?.message || '官方余量源未发现'}</dd>
        </>
      )}
      {snapshot.quota?.source && (
        <>
          <dt>余量来源</dt>
          <dd>{formatQuotaSource(snapshot.quota.source)}</dd>
        </>
      )}
      {hasCodexTokenData && (
        <>
          <dt>Token</dt>
          <dd>{formatTokenCount(tokenUsage?.totalTokens)}</dd>
          <dt>输入</dt>
          <dd>{formatTokenCount(tokenUsage?.inputTokens)}</dd>
          <dt>缓存输入</dt>
          <dd>{formatTokenCount(tokenUsage?.cachedInputTokens)}</dd>
          <dt>输出</dt>
          <dd>{formatTokenCount(tokenUsage?.outputTokens)}</dd>
          <dt>缓存命中率</dt>
          <dd>{formatPercent(tokenUsage?.cacheHitRate)}</dd>
          <dt>数据来源</dt>
          <dd>Codex 本地会话</dd>
          <dt>数据更新</dt>
          <dd>{formatDateTime(tokenUpdatedAt)}</dd>
          <dt>数据状态</dt>
          <dd>{formatDataFreshness(tokenUpdatedAt)}</dd>
        </>
      )}
    </dl>
  );
}

function NewApiDetails({ snapshot }: { snapshot: ProviderSnapshot }) {
  const amountMode = snapshot.amountDisplayMode ?? 'cny';
  const amountOptions = { cnyPerUsd: snapshot.exchangeRateCnyPerUsd };
  const log = snapshot.usage?.log;
  const tokenUsage =
    log && Number(log.requestCount) > 0
      ? log
      : snapshot.localLogs?.all && Number(snapshot.localLogs.all.requestCount) > 0
        ? snapshot.localLogs.all
        : snapshot.usage;
  const today = snapshot.localLogs?.today;
  const keyQuota = formatKeyQuota(snapshot, amountMode);
  const accountSpendRaw =
    snapshot.balance?.rawUsedAmount ??
    snapshot.balanceKey?.rawUsedAmount ??
    quotaFromCny(snapshot.balance?.usedAmount);

  return (
    <div className="detail-metrics-grid">
      <dl>
        {snapshot.account?.username && (
          <>
            <dt>用户名</dt>
            <dd>{snapshot.account.username}</dd>
          </>
        )}
        {Number.isFinite(snapshot.usage?.requestCount) && (
          <>
            <dt>请求次数</dt>
            <dd>{snapshot.usage?.requestCount}</dd>
          </>
        )}
        <dt>Token</dt>
        <dd>{formatTokenCount(tokenUsage?.totalTokens)}</dd>
        <dt>输入</dt>
        <dd>{formatTokenCount(tokenUsage?.inputTokens)}</dd>
        <dt>缓存输入</dt>
        <dd>{formatTokenCount(tokenUsage?.cachedInputTokens)}</dd>
        <dt>缓存命中率</dt>
        <dd>{formatPercent(tokenUsage?.cacheHitRate)}</dd>
        <dt>输出</dt>
        <dd>{formatTokenCount(tokenUsage?.outputTokens)}</dd>
        <dt>余额</dt>
        <dd>{formatQuotaPair(snapshot.balance, amountMode, amountOptions)}</dd>
        {snapshot.account?.cachedAt && (
          <>
            <dt>余额校准时间</dt>
            <dd>{formatDateTime(snapshot.account.cachedAt)}</dd>
          </>
        )}
        {snapshot.localLogs?.topup && (
          <>
            <dt>充值账单</dt>
            <dd>{formatTopup(snapshot.localLogs.topup, amountMode, snapshot.exchangeRateCnyPerUsd)}</dd>
          </>
        )}
        <dt>平台累计花费</dt>
        <dd>{formatQuotaAmount(accountSpendRaw, amountMode, amountOptions)}</dd>
        {keyQuota !== '-' && (
          <>
            <dt>Key 额度</dt>
            <dd>{keyQuota}</dd>
          </>
        )}
        <dt>本地同步</dt>
        <dd>{formatSyncStatus(snapshot.localLogs?.sync)}</dd>
      </dl>
      <dl className="today-metrics">
        <dt>今日 Token</dt>
        <dd>
          {today ? `${formatTokenCount(today.totalTokens)} / ${today.requestCount ?? 0} 次` : '-'}
        </dd>
        <dt>今日输入</dt>
        <dd>{formatTokenCount(today?.inputTokens)}</dd>
        <dt>今日缓存输入</dt>
        <dd>{formatTokenCount(today?.cachedInputTokens)}</dd>
        <dt>今日输出</dt>
        <dd>{formatTokenCount(today?.outputTokens)}</dd>
        <dt>今日花费</dt>
        <dd>{formatQuotaAmount(today?.rawUsedAmount, amountMode, amountOptions)}</dd>
        <dt>今日缓存命中率</dt>
        <dd>{formatPercent(today?.cacheHitRate)}</dd>
        {snapshot.localLogs?.topup?.updatedAt && (
          <>
            <dt>充值校准时间</dt>
            <dd>{formatDateTime(snapshot.localLogs.topup.updatedAt)}</dd>
          </>
        )}
        {snapshot.accountSyncError && (
          <>
            <dt>余额校准</dt>
            <dd>{snapshot.accountSyncError}</dd>
          </>
        )}
        {log && Number(log.requestCount) > 0 && (
          <>
            <dt>最近日志消耗</dt>
            <dd>{formatQuotaAmount(log.rawUsedAmount, amountMode, amountOptions)}</dd>
            <dt>日志 Token</dt>
            <dd>{`${formatTokenCount(log.totalTokens)} / ${log.requestCount ?? 0} 次`}</dd>
            <dt>日志更新</dt>
            <dd>{log.latestLogAt ? formatDateTime(log.latestLogAt) : log.updatedAt ? '暂无记录' : '-'}</dd>
          </>
        )}
        {snapshot.error && (
          <>
            <dt>接口状态</dt>
            <dd>{snapshot.error}</dd>
          </>
        )}
      </dl>
    </div>
  );
}

function quotaFromCny(value?: number) {
  const number = Number(value);
  return Number.isFinite(number) ? number * 500000 : undefined;
}

function formatSyncStatus(sync?: NonNullable<ProviderSnapshot['localLogs']>['sync']) {
  if (!sync) return '-';
  if (sync.mode === 'rate_limited' || sync.mode === 'backoff') {
    const seconds = Number(sync.retryAfterSeconds);
    return `平台限流，${Number.isFinite(seconds) ? `${Math.ceil(seconds)} 秒后重试` : '等待退避重试'}`;
  }
  if (sync.mode === 'error') {
    return sync.message || '平台同步失败';
  }
  const updatedAt = sync.updatedAt ?? timestampToIso(sync.lastSyncedAt);
  if (!updatedAt) return '-';
  if (sync.inserted === undefined && sync.fetched === undefined) {
    return `${formatDateTime(updatedAt)}，${sync.mode === 'backfill' ? '历史补齐中' : '已校准'}`;
  }
  return `${formatDateTime(updatedAt)}，已校准`;
}

function formatCoverage(coverage?: NonNullable<ProviderSnapshot['localLogs']>['coverage']) {
  if (!coverage) return '-';
  const warning = coverage.warning ? '，日志可能被截断' : '';
  if (coverage.complete) return `已扫描 6 月 1 日以来日志${warning}`;
  const remaining = Number.isFinite(Number(coverage.missingBeforeSeconds))
    ? `，剩余约 ${formatDuration(coverage.missingBeforeSeconds)}`
    : '';
  const scanned = Number.isFinite(Number(coverage.scannedThroughAt))
    ? `，已扫到 ${formatTimestamp(coverage.scannedThroughAt)}`
    : '';
  const first = Number.isFinite(Number(coverage.firstCreatedAt))
    ? formatTimestamp(coverage.firstCreatedAt)
    : '未知';
  return `未补齐，最早 ${first}${scanned}${remaining}${warning}`;
}

function formatDuration(seconds?: number) {
  const value = Number(seconds);
  if (!Number.isFinite(value) || value <= 0) return '0 分钟';
  const hours = Math.floor(value / 3600);
  const minutes = Math.round((value % 3600) / 60);
  if (hours <= 0) return `${Math.max(1, minutes)} 分钟`;
  if (minutes <= 0) return `${hours} 小时`;
  return `${hours} 小时 ${minutes} 分钟`;
}

function formatLogRange(localLogs?: ProviderSnapshot['localLogs']) {
  const first = localLogs?.coverage?.firstCreatedAt;
  const latest = localLogs?.latestCreatedAt;
  if (Number.isFinite(Number(first)) && Number.isFinite(Number(latest))) {
    return `${formatTimestamp(first)} - ${formatTimestamp(latest)}`;
  }
  if (Number.isFinite(Number(latest))) {
    return `最新 ${formatTimestamp(latest)}`;
  }
  return '-';
}

function formatTopup(topup: NonNullable<ProviderSnapshot['localLogs']>['topup'], amountMode: 'cny' | 'usd', cnyPerUsd?: number) {
  if (!topup || topup.ok === false) return topup?.message || '-';
  const raw = Number.isFinite(Number(topup.rawTotalQuota))
    ? Number(topup.rawTotalQuota)
    : Number.isFinite(Number(topup.totalAmount))
      ? Number(topup.totalAmount) * 500000
      : undefined;
  const amount = formatQuotaAmount(raw, amountMode, { cnyPerUsd });
  const count = Number.isFinite(Number(topup.count)) ? `${topup.count} 笔` : '账单';
  return `${amount} / ${count}`;
}

function formatTimestamp(value?: number) {
  const number = Number(value);
  if (!Number.isFinite(number)) return '-';
  return formatDateTime(new Date(number * 1000).toISOString());
}

function timestampToIso(value?: number) {
  const number = Number(value);
  return Number.isFinite(number) ? new Date(number * 1000).toISOString() : undefined;
}

function formatKeyQuota(snapshot: ProviderSnapshot, amountMode: 'cny' | 'usd') {
  if (snapshot.balanceKey?.unlimitedQuota) return '无限';
  const available = snapshot.balanceKey?.rawTotalAvailable;
  const granted = snapshot.balanceKey?.rawTotalGranted;
  if (!Number.isFinite(Number(available))) return '-';
  const formattedAvailable = formatQuotaAmount(available, amountMode, {
    cnyPerUsd: snapshot.exchangeRateCnyPerUsd
  });
  if (!Number.isFinite(Number(granted))) return formattedAvailable;
  return `${formattedAvailable}/${formatQuotaAmount(granted, amountMode, {
    cnyPerUsd: snapshot.exchangeRateCnyPerUsd
  })}`;
}

function formatDateTime(value?: string) {
  if (!value) return '-';
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return value;
  return date.toLocaleString('zh-CN', {
    hour12: false,
    month: '2-digit',
    day: '2-digit',
    hour: '2-digit',
    minute: '2-digit',
    second: '2-digit'
  });
}

function formatDataFreshness(value?: string) {
  if (!value) return '-';
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return '-';
  const ageSeconds = Math.max(0, Math.floor((Date.now() - date.getTime()) / 1000));
  if (ageSeconds < 60) return '刚刚更新';
  const minutes = Math.floor(ageSeconds / 60);
  if (minutes < 60) return `${minutes} 分钟前${minutes >= 10 ? '，可能偏旧' : ''}`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours} 小时前，可能偏旧`;
  const days = Math.floor(hours / 24);
  return `${days} 天前，可能偏旧`;
}

function statusText(status?: string) {
  if (status === 'ok') return '正常';
  if (status === 'loading') return '刷新中';
  if (status === 'not_logged_in') return '未登录';
  if (status === 'not_installed') return '未安装';
  if (status === 'error') return '错误';
  return '不可用';
}

function formatQuotaSource(source?: string) {
  if (source === 'codex-rpc') return 'Codex app-server';
  if (source === 'codex-session') return '本地会话';
  if (source === 'cache') return '缓存';
  return source || '-';
}
