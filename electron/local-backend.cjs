const { spawn } = require('node:child_process');
const crypto = require('node:crypto');
const fs = require('node:fs');
const fsp = require('node:fs/promises');
const http = require('node:http');
const os = require('node:os');
const path = require('node:path');
const { pathToFileURL } = require('node:url');

const { DatabaseSync } = require('node:sqlite');
const {
  applyCodexRuntimeObservation,
  confirmQuotaReminder,
  isQuotaReminderPending
} = require('./codex-runtime-state.cjs');

const HOST = '127.0.0.1';
const PORT = 1420;
const QUOTA_UNITS_PER_CNY = 500000;
const INITIAL_SYNC_START = 1780243200;
const SYNC_OVERLAP_SECONDS = 300;
const BACKFILL_WINDOW_SECONDS = 6 * 60 * 60;
const LOG_WINDOW_CAP = 1000;
const ACCOUNT_CACHE_TTL_SECONDS = 5 * 60;
const TOPUP_CACHE_TTL_SECONDS = 10 * 60;
const CODEX_SESSION_DISCOVERY_TTL_SECONDS = 5;
const CODEX_RATE_LIMIT_CACHE_TTL_SECONDS = 60;
const CODEX_SESSION_WATCH_DEBOUNCE_MS = 100;
const CODEX_ACTIVITY_STALE_MS = 15 * 1000;
const GITHUB_LATEST_RELEASE_URL = 'https://github.com/akitten-cn/codex-quota-glance/releases/latest';
const GITHUB_LATEST_RELEASE_API_URL = 'https://api.github.com/repos/akitten-cn/codex-quota-glance/releases/latest';

let server = null;
let database = null;
let paths = null;
let codexSessionWatcher = null;
let codexSessionWatchTimer = null;
let codexSessionWatchNeedsDiscovery = false;
let onCodexSessionChanged = null;
const codexSessionCache = { checkedAt: 0, path: null };
const codexTokenFileCaches = new Map();
const codexActivityCache = { path: null, size: -1, mtimeMs: -1, activity: null };
const codexRateLimitCache = { checkedAt: 0, quota: null, source: null, message: null };

async function startLocalBackend(options = {}) {
  if (server) {
    if (typeof options.onCodexSessionChanged === 'function') {
      onCodexSessionChanged = options.onCodexSessionChanged;
    }
    return server;
  }
  paths = resolvePaths(options);
  onCodexSessionChanged = typeof options.onCodexSessionChanged === 'function'
    ? options.onCodexSessionChanged
    : null;
  Object.assign(codexSessionCache, { checkedAt: 0, path: null });
  codexTokenFileCaches.clear();
  Object.assign(codexActivityCache, { path: null, size: -1, mtimeMs: -1, activity: null });
  fs.mkdirSync(paths.dataDir, { recursive: true });
  initDb();
  syncTodayCodexTokenEvents();

  server = http.createServer((request, response) => {
    handleRequest(request, response).catch((error) => {
      sendJson(response, 500, { ok: false, message: errorMessage(error) });
    });
  });

  await new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(PORT, HOST, () => {
      server.off('error', reject);
      resolve();
    });
  });
  startCodexSessionWatcher();
  return server;
}

async function stopLocalBackend() {
  stopCodexSessionWatcher();
  onCodexSessionChanged = null;
  const current = server;
  server = null;
  if (database) {
    database.close();
    database = null;
  }
  if (!current) return;
  await new Promise((resolve) => current.close(resolve));
}

function startCodexSessionWatcher() {
  stopCodexSessionWatcher();
  if (!paths?.codexSessions || !fs.existsSync(paths.codexSessions)) return;
  try {
    codexSessionWatcher = fs.watch(
      paths.codexSessions,
      { recursive: process.platform === 'win32' },
      (_eventType, filename) => {
        if (filename && !String(filename).toLowerCase().endsWith('.jsonl')) return;
        const changedPath = filename ? path.resolve(paths.codexSessions, String(filename)) : null;
        if (!isSameFilePath(changedPath, codexSessionCache.path)) {
          codexSessionWatchNeedsDiscovery = true;
        }
        if (codexSessionWatchTimer) clearTimeout(codexSessionWatchTimer);
        codexSessionWatchTimer = setTimeout(() => {
          codexSessionWatchTimer = null;
          if (codexSessionWatchNeedsDiscovery) {
            Object.assign(codexSessionCache, { checkedAt: 0, path: null });
            codexSessionWatchNeedsDiscovery = false;
          }
          Object.assign(codexActivityCache, { path: null, size: -1, mtimeMs: -1, activity: null });
          onCodexSessionChanged?.();
        }, CODEX_SESSION_WATCH_DEBOUNCE_MS);
        codexSessionWatchTimer.unref?.();
      }
    );
    codexSessionWatcher.on('error', stopCodexSessionWatcher);
  } catch {
    stopCodexSessionWatcher();
  }
}

function stopCodexSessionWatcher() {
  codexSessionWatchNeedsDiscovery = false;
  if (codexSessionWatchTimer) {
    clearTimeout(codexSessionWatchTimer);
    codexSessionWatchTimer = null;
  }
  if (codexSessionWatcher) {
    codexSessionWatcher.close();
    codexSessionWatcher = null;
  }
}

function isSameFilePath(first, second) {
  if (!first || !second) return false;
  const left = path.normalize(path.resolve(first));
  const right = path.normalize(path.resolve(second));
  return process.platform === 'win32' ? left.toLowerCase() === right.toLowerCase() : left === right;
}

async function handleRequest(request, response) {
  const requestUrl = new URL(request.url || '/', `http://${HOST}:${PORT}`);
  if (requestUrl.pathname === '/newapi-proxy') {
    await proxyNewApi(request, response);
    return;
  }
  if (request.method === 'GET' && requestUrl.pathname === '/local-api/health') {
    sendJson(response, 200, {
      ok: true,
      app: 'codex-quota-glance',
      backend: 'electron-local-backend'
    });
    return;
  }
  if (request.method === 'GET' && requestUrl.pathname === '/local-api/newapi/logs/summary') {
    sendJson(response, 200, getLogSummary(readSummaryContext(request, requestUrl)));
    return;
  }
  if (request.method === 'POST' && requestUrl.pathname === '/local-api/newapi/logs/sync') {
    sendJson(response, 200, await syncLogs(await readJson(request)));
    return;
  }
  if (request.method === 'POST' && requestUrl.pathname === '/local-api/newapi/diagnose') {
    sendJson(response, 200, await diagnoseUserSelf(await readJson(request)));
    return;
  }
  if (request.method === 'GET' && requestUrl.pathname === '/local-api/codex/token/latest') {
    sendJson(response, 200, getLatestCodexTokenUsage());
    return;
  }
  if (request.method === 'GET' && requestUrl.pathname === '/local-api/codex/overview') {
    sendJson(response, 200, await getCodexOverview());
    return;
  }
  if (request.method === 'POST' && requestUrl.pathname === '/local-api/codex/quota-reminder/confirm') {
    const payload = await readJson(request);
    const next = confirmQuotaReminder(readCodexRuntimeState(), payload.resetAt);
    writeCodexRuntimeState(next);
    sendJson(response, 200, { ok: true, quotaReminder: buildQuotaReminder(next) });
    return;
  }
  if (request.method === 'GET' && requestUrl.pathname === '/local-api/codex/token/summary') {
    sendJson(response, 200, getCodexTokenSummary());
    return;
  }
  if (request.method === 'GET' && requestUrl.pathname === '/local-api/codex/status') {
    sendJson(response, 200, await getCodexStatus());
    return;
  }
  if (request.method === 'GET' && requestUrl.pathname === '/local-api/update/latest') {
    sendJson(response, 200, await getLatestRelease());
    return;
  }
  await serveStatic(requestUrl, response);
}

async function getCodexOverview() {
  let status = await getCodexStatus().catch((error) => ({ ok: false, message: errorMessage(error) }));
  let runtime = readCodexRuntimeState();
  let latestToken;
  try {
    latestToken = getLatestCodexTokenUsage();
  } catch (error) {
    latestToken = { ok: false, available: false, message: errorMessage(error) };
  }

  if (status?.accountType) {
    status = {
      ...status,
      quota: reconcileCodexRateLimits(status.quota, latestToken?.quota)
    };
    runtime = applyCodexRuntimeObservation(runtime, {
      accountType: status.accountType,
      now: unixNow(),
      quota: status.quota?.window5h,
      fiveHourQuotaUnavailable:
        hasNormalizedRateLimitWindow(status.quota?.weekly) &&
        !hasNormalizedRateLimitWindow(status.quota?.window5h)
    });
    writeCodexRuntimeState(runtime);
  }

  const tokenSummary = await Promise.resolve()
    .then(() => getCodexTokenSummary())
    .catch((error) => ({ ok: false, message: errorMessage(error) }));
  return { ok: true, status, latestToken, tokenSummary, quotaReminder: buildQuotaReminder(runtime) };
}

function buildQuotaReminder(runtime = readCodexRuntimeState()) {
  return {
    pending: isQuotaReminderPending(runtime, unixNow()),
    resetAt: runtime.waitingResetAt
  };
}

async function getLatestRelease() {
  const apiResponse = await fetch(GITHUB_LATEST_RELEASE_API_URL, {
    method: 'GET',
    headers: {
      Accept: 'application/vnd.github+json',
      'User-Agent': 'CodexQuotaGlance/0.1'
    }
  });
  if (apiResponse.ok) {
    const release = await apiResponse.json();
    return {
      ok: true,
      tag_name: release.tag_name,
      html_url: release.html_url,
      assets: Array.isArray(release.assets) ? release.assets.map((asset) => ({
        name: asset.name,
        browser_download_url: asset.browser_download_url,
        size: asset.size
      })) : []
    };
  }

  const response = await fetch(GITHUB_LATEST_RELEASE_URL, {
    method: 'GET',
    redirect: 'manual',
    headers: {
      Accept: 'text/html',
      'User-Agent': 'CodexQuotaGlance/0.1'
    }
  });
  const location = response.headers.get('location');
  const releaseUrl = location ? new URL(location, GITHUB_LATEST_RELEASE_URL).toString() : response.url;
  const tagName = releaseUrl.match(/\/releases\/tag\/([^/?#]+)/)?.[1];
  if (!tagName) {
    return {
      ok: false,
      message: `GitHub Releases 检查失败：HTTP ${response.status || 'unknown'}`
    };
  }
  return {
    ok: true,
    tag_name: decodeURIComponent(tagName),
    html_url: releaseUrl
  };
}

function resolvePaths(options) {
  const appRoot = path.resolve(options.appRoot || path.join(__dirname, '..'));
  const distDir = path.resolve(options.distDir || path.join(appRoot, 'dist'));
  const dataDir = path.resolve(
    process.env.CODEX_QUOTA_DATA_DIR ||
      (process.env.LOCALAPPDATA
        ? path.join(process.env.LOCALAPPDATA, 'CodexQuotaGlance', 'data')
        : path.join(appRoot, 'data'))
  );
  const codexHome = path.resolve(process.env.CODEX_HOME || path.join(os.homedir(), '.codex'));
  return {
    appRoot,
    distDir,
    dataDir,
    database: path.join(dataDir, 'newapi-usage.sqlite3'),
    requestDebugLog: path.join(dataDir, 'request-debug.log'),
    codexHome,
    codexSessions: path.join(codexHome, 'sessions')
  };
}

function initDb() {
  if (database) return database;
  database = new DatabaseSync(paths.database);
  database.exec(`
    create table if not exists newapi_logs (
      unique_id text primary key,
      provider_log_id text,
      request_id text,
      created_at integer not null,
      token_name text,
      model_name text,
      group_name text,
      input_tokens integer not null default 0,
      cached_input_tokens integer not null default 0,
      output_tokens integer not null default 0,
      total_tokens integer not null default 0,
      raw_used_amount integer not null default 0,
      other_json text
    );
    create index if not exists idx_newapi_logs_created_at on newapi_logs(created_at);
    create table if not exists newapi_sync_state (
      sync_key text primary key,
      base_url text not null,
      api_user text,
      key_fingerprint text,
      latest_created_at integer,
      last_synced_at integer,
      fail_count integer default 0,
      blocked_until integer,
      backfill_until integer,
      backfill_complete integer default 0,
      backfill_warning text
    );
    create table if not exists newapi_account_cache (
      account_key text primary key,
      base_url text not null,
      api_user text,
      token_fingerprint text,
      snapshot_json text not null,
      updated_at integer not null
    );
    create table if not exists newapi_topup_cache (
      topup_key text primary key,
      base_url text not null,
      api_user text,
      token_fingerprint text,
      snapshot_json text not null,
      updated_at integer not null
    );
    create table if not exists codex_token_events (
      event_id text primary key,
      account_type text not null,
      session_file text,
      event_timestamp integer not null,
      event_iso text,
      input_tokens integer not null default 0,
      cached_input_tokens integer not null default 0,
      output_tokens integer not null default 0,
      total_tokens integer not null default 0,
      reasoning_output_tokens integer not null default 0,
      raw_json text
    );
    create index if not exists idx_codex_token_events_account_time on codex_token_events(account_type, event_timestamp);
    create table if not exists codex_runtime_state (
      id integer primary key check (id = 1),
      last_account_type text,
      api_spend_started_at integer,
      last_official_remaining_percent real,
      waiting_reset_at integer,
      confirmed_reset_at integer
    );
    insert or ignore into codex_runtime_state (id) values (1);
  `);
  ensureColumn('newapi_sync_state', 'fail_count', 'integer default 0');
  ensureColumn('newapi_sync_state', 'blocked_until', 'integer');
  ensureColumn('newapi_sync_state', 'backfill_until', 'integer');
  ensureColumn('newapi_sync_state', 'backfill_complete', 'integer default 0');
  ensureColumn('newapi_sync_state', 'backfill_warning', 'text');
  return database;
}

function ensureColumn(table, column, definition) {
  const columns = new Set(database.prepare(`pragma table_info(${table})`).all().map((row) => row.name));
  if (!columns.has(column)) {
    database.exec(`alter table ${table} add column ${column} ${definition}`);
  }
}

function readCodexRuntimeState() {
  const row = database.prepare(`
    select last_account_type, api_spend_started_at, last_official_remaining_percent,
      waiting_reset_at, confirmed_reset_at
    from codex_runtime_state where id = 1
  `).get() || {};
  return {
    lastAccountType: stringOrUndefined(row.last_account_type),
    apiSpendStartedAt: numberOrUndefined(row.api_spend_started_at),
    lastOfficialRemainingPercent: numberOrUndefined(row.last_official_remaining_percent),
    waitingResetAt: numberOrUndefined(row.waiting_reset_at),
    confirmedResetAt: numberOrUndefined(row.confirmed_reset_at)
  };
}

function writeCodexRuntimeState(state = {}) {
  database.prepare(`
    update codex_runtime_state set
      last_account_type = ?,
      api_spend_started_at = ?,
      last_official_remaining_percent = ?,
      waiting_reset_at = ?,
      confirmed_reset_at = ?
    where id = 1
  `).run(
    state.lastAccountType ?? null,
    state.apiSpendStartedAt ?? null,
    state.lastOfficialRemainingPercent ?? null,
    state.waitingResetAt ?? null,
    state.confirmedResetAt ?? null
  );
  return state;
}

async function serveStatic(requestUrl, response) {
  const pathname = decodeURIComponent(requestUrl.pathname);
  const requested = pathname === '/' ? 'index.html' : pathname.slice(1);
  const candidate = path.resolve(paths.distDir, requested);
  const root = path.resolve(paths.distDir);
  let filePath = candidate.startsWith(root) && fs.existsSync(candidate) && fs.statSync(candidate).isFile()
    ? candidate
    : path.join(root, 'index.html');
  let content;
  try {
    content = await fsp.readFile(filePath);
  } catch {
    filePath = path.join(paths.appRoot, 'dist', 'index.html');
    content = await fsp.readFile(filePath);
  }
  response.writeHead(200, {
    'Content-Type': contentType(filePath),
    'Cache-Control': path.basename(filePath) === 'index.html' ? 'no-store' : 'public, max-age=31536000',
    'Content-Length': String(content.length)
  });
  response.end(content);
}

async function proxyNewApi(request, response) {
  const target = stringHeader(request.headers['x-newapi-target']);
  if (!target) {
    sendText(response, 400, 'Missing X-NewAPI-Target');
    return;
  }
  let targetUrl;
  try {
    targetUrl = new URL(target);
  } catch {
    sendText(response, 400, 'Unsupported target URL');
    return;
  }
  if (!['http:', 'https:'].includes(targetUrl.protocol)) {
    sendText(response, 400, 'Unsupported target protocol');
    return;
  }
  const body = request.method === 'GET' ? undefined : await readBuffer(request);
  const upstream = await fetch(targetUrl, {
    method: request.method,
    headers: pruneHeaders({
      Authorization: stringHeader(request.headers.authorization),
      'New-Api-User': stringHeader(request.headers['new-api-user']),
      Accept: stringHeader(request.headers.accept) || 'application/json'
    }),
    body
  });
  const buffer = Buffer.from(await upstream.arrayBuffer());
  response.writeHead(upstream.status, {
    'Content-Type': upstream.headers.get('content-type') || 'application/json; charset=utf-8',
    'Cache-Control': 'no-store',
    'Content-Length': String(buffer.length)
  });
  response.end(buffer);
}

async function syncLogs(payload) {
  const baseUrl = trimTrailingSlash(payload.baseUrl);
  const apiKey = String(payload.apiKey || '');
  const accessToken = String(payload.accessToken || '');
  const apiUser = String(payload.newApiUser || '');
  const tokenName = String(payload.tokenName || '');
  const accountCacheTtl = normalizeTtl(payload.accountCacheTtlSeconds, ACCOUNT_CACHE_TTL_SECONDS);
  const topupCacheTtl = normalizeTtl(payload.topupCacheTtlSeconds, TOPUP_CACHE_TTL_SECONDS);
  const forceAccountRefresh = payload.forceAccountRefresh === true;
  if (!baseUrl || !(apiKey.trim() || accessToken.trim())) {
    throw new Error('baseUrl and apiKey/accessToken are required');
  }

  const syncSecret = accessToken.trim() || apiKey;
  const syncKey = makeSyncKey(baseUrl, apiUser, syncSecret);
  const summaryContext = { baseUrl, apiUser, accessToken };
  const blocked = getSyncBlock(syncKey);
  const nowTs = unixNow();
  if (blocked?.blocked_until && Number(blocked.blocked_until) > nowTs) {
    return {
      ok: true,
      mode: 'backoff',
      blockedUntil: Number(blocked.blocked_until),
      retryAfterSeconds: Number(blocked.blocked_until) - nowTs,
      fetched: 0,
      inserted: 0,
      insertedUsage: summarizeLogRows([]),
      summary: getLogSummary(summaryContext)
    };
  }

  const syncLatest = getLatestCreatedAt(syncKey);
  let latest = syncLatest;
  let seededFromGlobalLatest = false;
  if (latest === undefined) {
    latest = getGlobalLatestCreatedAt();
    seededFromGlobalLatest = latest !== undefined;
  }

  const coverage = getLogCoverageForSync(syncKey);
  let backfill = !seededFromGlobalLatest && coverage.complete === false && coverage.firstCreatedAt;
  let start;
  let end;
  if (backfill) {
    start = Math.max(INITIAL_SYNC_START, getBackfillUntil(syncKey) ?? INITIAL_SYNC_START);
    if (start >= Number(coverage.firstCreatedAt) - SYNC_OVERLAP_SECONDS) {
      markBackfillComplete(syncKey);
      backfill = false;
      start = Math.max(0, (latest || INITIAL_SYNC_START) - (latest ? SYNC_OVERLAP_SECONDS : 0));
      end = numberOrUndefined(payload.endTimestamp) ?? unixNow();
    } else {
      end = Math.min(
        start + BACKFILL_WINDOW_SECONDS,
        Number(coverage.firstCreatedAt) + SYNC_OVERLAP_SECONDS,
        numberOrUndefined(payload.endTimestamp) ?? unixNow()
      );
    }
  } else {
    start = Math.max(0, (latest || INITIAL_SYNC_START) - (latest ? SYNC_OVERLAP_SECONDS : 0));
    end = numberOrUndefined(payload.endTimestamp) ?? unixNow();
  }

  let account = null;
  let topup = null;
  if (accessToken.trim()) {
    try {
      ({ account, topup } = await refreshAccountAndTopup({
        baseUrl,
        accessToken,
        apiUser,
        force: forceAccountRefresh,
        accountTtlSeconds: accountCacheTtl,
        topupTtlSeconds: topupCacheTtl
      }));
    } catch (error) {
      if (error instanceof RateLimitedError) {
        const blockedUntil = saveSyncFailure(syncKey, baseUrl, apiUser, syncSecret, latest, error.retryAfter);
        return rateLimitedResult(blockedUntil, 0, [], account, topup, summaryContext);
      }
      account = { ok: false, message: errorMessage(error) };
      topup = getLatestTopupCache();
    }
  } else {
    account = { ok: false, message: '未配置系统访问令牌，余额未校准' };
    topup = getLatestTopupCache();
  }

  let mode = backfill ? 'backfill' : latest ? 'incremental' : 'initial';
  let fetched = 0;
  let page = 1;
  const pageSize = Math.max(1, Math.min(1000, Number(payload.pageSize) || 100));
  const logWindowCap = Number(payload.logWindowCap) || LOG_WINDOW_CAP;
  let pageLimitReached = false;
  let insertedRows = [];

  try {
    if (!accessToken.trim()) throw new Error('missing access token for account log');
    while (true) {
      const data = await fetchSelfLogPage({ baseUrl, accessToken, apiUser, page, pageSize, tokenName, start, end });
      const items = normalizeLogItems(data);
      if (items.length === 0) break;
      fetched += items.length;
      insertedRows.push(...insertLogs(items));
      if (items.length < pageSize) break;
      page += 1;
      if (page > 500) {
        pageLimitReached = true;
        break;
      }
    }
  } catch (error) {
    if (error instanceof RateLimitedError) {
      const blockedUntil = saveSyncFailure(syncKey, baseUrl, apiUser, syncSecret, latest, error.retryAfter);
      return rateLimitedResult(blockedUntil, fetched, insertedRows, account, topup, summaryContext);
    }
    mode = 'fallback-token';
    try {
      if (!apiKey.trim()) throw new Error('missing api key for token log fallback');
      const data = await fetchTokenLogPage({ baseUrl, apiKey, apiUser });
      const items = normalizeLogItems(data);
      fetched = items.length;
      insertedRows = insertLogs(items);
    } catch (fallbackError) {
      if (fallbackError instanceof RateLimitedError) {
        const blockedUntil = saveSyncFailure(syncKey, baseUrl, apiUser, syncSecret, latest, fallbackError.retryAfter);
        return rateLimitedResult(blockedUntil, fetched, insertedRows, account, topup, summaryContext);
      }
      throw fallbackError;
    }
  }

  const newest = getGlobalLatestCreatedAt();
  saveSyncState(syncKey, baseUrl, apiUser, syncSecret, newest);
  const capped = fetched >= logWindowCap || pageLimitReached;
  const backfillWarning = capped ? 'some log windows reached the platform cap; logs may be truncated' : undefined;
  if (mode === 'backfill') {
    const firstAfterSync = getFirstCreatedAtForSync();
    if (capped) {
      markBackfillIncomplete(syncKey, backfillWarning);
    } else if (firstAfterSync !== undefined && end >= firstAfterSync - SYNC_OVERLAP_SECONDS) {
      markBackfillComplete(syncKey);
    } else {
      saveBackfillUntil(syncKey, end > start ? end : undefined);
    }
  }
  return {
    ok: true,
    mode,
    startTimestamp: start,
    endTimestamp: end,
    pages: page,
    fetched,
    inserted: insertedRows.length,
    capped,
    backfillWarning,
    insertedUsage: summarizeLogRows(insertedRows),
    account,
    topup,
    summary: getLogSummary(summaryContext)
  };
}

function rateLimitedResult(blockedUntil, fetched, insertedRows, account, topup, summaryContext) {
  return {
    ok: true,
    mode: 'rate_limited',
    blockedUntil,
    retryAfterSeconds: Math.max(0, blockedUntil - unixNow()),
    fetched,
    inserted: insertedRows.length,
    insertedUsage: summarizeLogRows(insertedRows),
    account,
    topup,
    summary: getLogSummary(summaryContext)
  };
}

async function fetchSelfLogPage({ baseUrl, accessToken, apiUser, page, pageSize, tokenName, start, end }) {
  const url = new URL('/api/log/self', `${baseUrl}/`);
  url.search = new URLSearchParams({
    p: String(page),
    page_size: String(pageSize),
    type: '0',
    token_name: tokenName,
    model_name: '',
    start_timestamp: String(start),
    end_timestamp: String(end),
    group: '',
    request_id: ''
  });
  return fetchJson(url, { headers: newApiAuthHeaders(accessToken, apiUser) }, 'log sync failed');
}

async function fetchTokenLogPage({ baseUrl, apiKey, apiUser }) {
  const url = new URL('/api/log/token', `${baseUrl}/`);
  url.search = new URLSearchParams({ key: apiKey });
  return fetchJson(url, { headers: newApiAuthHeaders(apiKey, apiUser) }, 'token log sync failed');
}

async function fetchUserSelfSnapshot({ baseUrl, accessToken, apiUser }) {
  const url = new URL('/api/user/self', `${baseUrl}/`);
  const payload = await fetchJson(url, { headers: newApiAuthHeaders(accessToken, apiUser) }, 'user self failed');
  if (payload && payload.success === false) {
    return { ok: false, message: stringOrUndefined(payload.message) || 'user self failed' };
  }
  const data = payload?.data && typeof payload.data === 'object' ? payload.data : payload || {};
  const quota = numberOrUndefined(data.quota);
  const usedQuota = numberOrUndefined(data.used_quota ?? data.usedQuota);
  if (quota === undefined) return { ok: false, message: 'missing quota' };
  const rawTotal = usedQuota !== undefined ? quota + usedQuota : undefined;
  return {
    ok: true,
    username: stringOrUndefined(data.username),
    displayName: stringOrUndefined(data.display_name ?? data.displayName),
    email: stringOrUndefined(data.email),
    group: stringOrUndefined(data.group),
    requestCount: numberOrUndefined(data.request_count ?? data.requestCount),
    balance: {
      balance: quota / QUOTA_UNITS_PER_CNY,
      usedAmount: usedQuota !== undefined ? usedQuota / QUOTA_UNITS_PER_CNY : undefined,
      totalRecharged: rawTotal !== undefined ? rawTotal / QUOTA_UNITS_PER_CNY : undefined,
      rawBalance: quota,
      rawUsedAmount: usedQuota,
      rawTotalRecharged: rawTotal,
      totalRechargedEstimated: true,
      currency: 'CNY',
      source: 'provider',
      estimated: false
    }
  };
}

async function fetchTopupSnapshot({ baseUrl, accessToken, apiUser }) {
  const pageSize = 100;
  let page = 1;
  const allItems = [];
  let total;
  while (true) {
    const url = new URL('/api/user/topup/self', `${baseUrl}/`);
    url.search = new URLSearchParams({ p: String(page), page_size: String(pageSize) });
    const payload = await fetchJson(url, { headers: newApiAuthHeaders(accessToken, apiUser) }, 'topup failed');
    if (payload && payload.success === false) {
      return { ok: false, message: stringOrUndefined(payload.message) || 'topup failed' };
    }
    const data = payload?.data ?? payload;
    const items = normalizeTopupItems(data);
    allItems.push(...items);
    total = data && typeof data === 'object' ? numberOrUndefined(data.total) : undefined;
    if (items.length < pageSize) break;
    if (total !== undefined && allItems.length >= total) break;
    page += 1;
    if (page > 100) break;
  }
  return summarizeTopupItems(allItems, page, total);
}

async function refreshAccountAndTopup({ baseUrl, accessToken, apiUser, force, accountTtlSeconds, topupTtlSeconds }) {
  let account = force ? null : getFreshAccountCache(accountTtlSeconds);
  let topup = force ? null : getFreshTopupCache(topupTtlSeconds);
  if (!account) {
    account = await fetchUserSelfSnapshot({ baseUrl, accessToken, apiUser });
    if (account?.ok) saveAccountCache(baseUrl, apiUser, accessToken, account);
  }
  if (!topup) {
    topup = await fetchTopupSnapshot({ baseUrl, accessToken, apiUser });
    if (topup?.ok) {
      saveTopupCache(baseUrl, apiUser, accessToken, topup);
    } else {
      topup = getLatestTopupCache();
    }
  }
  return { account: applyTopupToAccount(account, topup), topup };
}

async function diagnoseUserSelf(payload) {
  const baseUrl = trimTrailingSlash(payload.baseUrl);
  const accessToken = cleanAuthorizationToken(payload.accessToken);
  const apiUser = String(payload.newApiUser || '');
  if (!baseUrl || !accessToken) throw new Error('baseUrl and accessToken are required');
  const url = new URL('/api/user/self', `${baseUrl}/`);
  const rawRequest = [
    'GET /api/user/self HTTP/1.1',
    `Host: ${url.host}`,
    `Authorization: Bearer ${maskSecret(accessToken)}`,
    `New-Api-User: ${apiUser}`,
    'User-Agent: Apifox/1.0.0 (https://apifox.com)',
    'Accept: */*',
    `Host: ${url.host}`,
    'Connection: keep-alive'
  ].join('\r\n');
  let httpStatus = 0;
  let parsedBody = {};
  let message = '';
  try {
    const response = await fetch(url, {
      headers: newApiAuthHeaders(accessToken, apiUser, true)
    });
    httpStatus = response.status;
    const text = await response.text();
    message = text.slice(0, 200);
    try {
      parsedBody = JSON.parse(text);
    } catch {
      parsedBody = {};
    }
  } catch (error) {
    message = errorMessage(error);
  }
  const data = parsedBody?.data && typeof parsedBody.data === 'object' ? parsedBody.data : {};
  const result = {
    ok: true,
    request: {
      url: String(url),
      sentHeaders: maskedNewApiHeaders(accessToken, apiUser, true),
      rawHttpRequest: rawRequest,
      diagnostics: newApiHeaderDiagnostics(accessToken)
    },
    response: {
      httpStatus,
      success: parsedBody?.success,
      message: parsedBody?.message ?? message,
      dataKeys: Object.keys(data).slice(0, 40)
    }
  };
  appendRequestDebug({ source: 'diagnose-user-self', request: result.request, response: result.response });
  return result;
}

async function fetchJson(url, options, label) {
  const response = await fetch(url, options);
  const text = await response.text();
  if (response.status === 429) throw new RateLimitedError(response.headers.get('retry-after'));
  if (response.status >= 400) throw new Error(`${label}: HTTP ${response.status} ${text.slice(0, 200)}`);
  try {
    return JSON.parse(text);
  } catch {
    throw new Error(`${label}: invalid JSON`);
  }
}

function getLogSummary(context = {}) {
  const now = new Date();
  const dayStart = new Date(now);
  dayStart.setHours(0, 0, 0, 0);
  const start = Math.floor(dayStart.getTime() / 1000);
  const end = start + 86400;
  const db = initDb();
  const baseUrl = trimTrailingSlash(context.baseUrl);
  const apiUser = String(context.apiUser || context.newApiUser || '');
  const accessToken = String(context.accessToken || '');
  const sync = getLatestSyncSnapshot();
  const topup = getLatestTopupCache({ baseUrl, apiUser, accessToken });
  const account = applyTopupToAccount(getLatestAccountCache({ baseUrl, apiUser, accessToken }), topup);
  return {
    ok: true,
    today: summarizeRows('where created_at >= ? and created_at < ?', [start, end]),
    all: summarizeRows('', []),
    latestCreatedAt: getGlobalLatestCreatedAt(db),
    coverage: getLogCoverage(sync),
    sync,
    account,
    topup
  };
}

function summarizeRows(whereSql, params) {
  const row = database.prepare(`
    select
      count(*) as request_count,
      coalesce(sum(input_tokens), 0) as input_tokens,
      coalesce(sum(cached_input_tokens), 0) as cached_input_tokens,
      coalesce(sum(output_tokens), 0) as output_tokens,
      coalesce(sum(total_tokens), 0) as total_tokens,
      coalesce(sum(raw_used_amount), 0) as raw_used_amount
    from newapi_logs
    ${whereSql}
  `).get(...params);
  const inputTokens = Number(row.input_tokens || 0);
  const cached = Number(row.cached_input_tokens || 0);
  const rawUsed = Number(row.raw_used_amount || 0);
  return {
    requestCount: Number(row.request_count || 0),
    inputTokens,
    cachedInputTokens: cached,
    outputTokens: Number(row.output_tokens || 0),
    totalTokens: Number(row.total_tokens || 0),
    rawUsedAmount: rawUsed,
    usedAmount: rawUsed / QUOTA_UNITS_PER_CNY,
    cacheHitRate: inputTokens ? (cached / inputTokens) * 100 : undefined
  };
}

function insertLogs(items) {
  const rows = items.map(normalizeLogRow);
  const inserted = [];
  const statement = database.prepare(`
    insert or ignore into newapi_logs (
      unique_id, provider_log_id, request_id, created_at, token_name, model_name,
      group_name, input_tokens, cached_input_tokens, output_tokens, total_tokens,
      raw_used_amount, other_json
    ) values (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
  `);
  database.exec('begin');
  try {
    for (const row of rows) {
      const result = statement.run(
        row.unique_id,
        row.provider_log_id,
        row.request_id,
        row.created_at,
        row.token_name,
        row.model_name,
        row.group_name,
        row.input_tokens,
        row.cached_input_tokens,
        row.output_tokens,
        row.total_tokens,
        row.raw_used_amount,
        row.other_json
      );
      if (result.changes) inserted.push(row);
    }
    database.exec('commit');
  } catch (error) {
    database.exec('rollback');
    throw error;
  }
  return inserted;
}

function normalizeLogItems(payload) {
  if (Array.isArray(payload)) return payload.filter((item) => item && typeof item === 'object');
  const data = payload?.data ?? payload;
  if (Array.isArray(data)) return data.filter((item) => item && typeof item === 'object');
  if (data && typeof data === 'object') {
    for (const key of ['items', 'logs', 'data']) {
      if (Array.isArray(data[key])) return data[key].filter((item) => item && typeof item === 'object');
    }
  }
  return [];
}

function normalizeLogRow(item) {
  const other = parseOther(item.other);
  const requestId = stringOrUndefined(item.request_id);
  const providerLogId = stringOrUndefined(item.id);
  const createdAt = Math.trunc(numberOrZero(item.created_at));
  const inputTokens = Math.trunc(numberOrZero(item.prompt_tokens ?? item.input_tokens));
  const outputTokens = Math.trunc(numberOrZero(item.completion_tokens ?? item.output_tokens));
  const cached = Math.trunc(numberOrZero(other.cache_tokens ?? item.cached_tokens ?? item.cached_input_tokens));
  return {
    unique_id: requestId ? `req:${requestId}` : `id:${providerLogId || `${createdAt}:${inputTokens}:${outputTokens}`}`,
    provider_log_id: providerLogId,
    request_id: requestId,
    created_at: createdAt,
    token_name: stringOrUndefined(item.token_name),
    model_name: stringOrUndefined(item.model_name),
    group_name: stringOrUndefined(item.group),
    input_tokens: inputTokens,
    cached_input_tokens: cached,
    output_tokens: outputTokens,
    total_tokens: Math.trunc(numberOrZero(item.total_tokens) || inputTokens + outputTokens),
    raw_used_amount: Math.trunc(numberOrZero(item.quota ?? item.used_quota)),
    other_json: typeof item.other === 'string' ? item.other : JSON.stringify(other)
  };
}

function summarizeLogRows(rows) {
  const totals = rows.reduce((sum, row) => ({
    requestCount: sum.requestCount + 1,
    inputTokens: sum.inputTokens + Number(row.input_tokens || 0),
    cachedInputTokens: sum.cachedInputTokens + Number(row.cached_input_tokens || 0),
    outputTokens: sum.outputTokens + Number(row.output_tokens || 0),
    totalTokens: sum.totalTokens + Number(row.total_tokens || 0),
    rawUsedAmount: sum.rawUsedAmount + Number(row.raw_used_amount || 0),
    latestCreatedAt: Math.max(sum.latestCreatedAt || 0, Number(row.created_at || 0))
  }), { requestCount: 0, inputTokens: 0, cachedInputTokens: 0, outputTokens: 0, totalTokens: 0, rawUsedAmount: 0, latestCreatedAt: undefined });
  return {
    ...totals,
    usedAmount: totals.rawUsedAmount / QUOTA_UNITS_PER_CNY,
    cacheHitRate: totals.inputTokens ? (totals.cachedInputTokens / totals.inputTokens) * 100 : undefined,
    latestCreatedAt: totals.latestCreatedAt || undefined
  };
}

function getLatestCodexTokenUsage() {
  const latestFile = findLatestCodexSessionFile();
  if (!latestFile) return { ok: true, available: false };
  let tokenEvents;
  try {
    tokenEvents = readNewCodexTokenEvents(latestFile);
  } catch (error) {
    return { ok: false, available: false, message: errorMessage(error) };
  }
  const { latestEvent, newEvents } = tokenEvents;
  if (!latestEvent) return { ok: true, available: false, sessionFile: latestFile };
  const accountType = getCodexAccountType();
  for (const rawEvent of newEvents) {
    saveCodexTokenEvent(buildCodexTokenEvent(rawEvent, latestFile, accountType), rawEvent);
  }
  return buildCodexTokenEvent(latestEvent, latestFile, accountType);
}

function buildCodexTokenEvent(rawEvent, sessionFile, accountType = getCodexAccountType()) {
  const payload = rawEvent.payload || {};
  const info = payload.info || {};
  const usage = info.last_token_usage || {};
  const rateLimits = payload.rate_limits || {};
  const timestamp = String(rawEvent.timestamp || '');
  const eventId = sha256(`${sessionFile}:${timestamp}:${JSON.stringify(usage)}:${JSON.stringify(rateLimits)}`);
  return {
    ok: true,
    available: true,
    source: 'codex',
    eventId,
    timestamp,
    accountType,
    sessionFile,
    usage: {
      inputTokens: Math.trunc(numberOrZero(usage.input_tokens)),
      cachedInputTokens: Math.trunc(numberOrZero(usage.cached_input_tokens)),
      outputTokens: Math.trunc(numberOrZero(usage.output_tokens)),
      totalTokens: Math.trunc(numberOrZero(usage.total_tokens)),
      reasoningOutputTokens: Math.trunc(numberOrZero(usage.reasoning_output_tokens))
    },
    quota: normalizeCodexRateLimits(rateLimits)
  };
}

function getCodexTokenSummary(context = {}) {
  const now = context.now instanceof Date ? context.now : new Date();
  const dayStart = new Date(now);
  dayStart.setHours(0, 0, 0, 0);
  const start = Math.floor(dayStart.getTime() / 1000);
  const dayEnd = new Date(dayStart);
  dayEnd.setDate(dayEnd.getDate() + 1);
  const end = Math.floor(dayEnd.getTime() / 1000);
  const runtime = readCodexRuntimeState();
  const apiSpendStartedAt = numberOrUndefined(runtime.apiSpendStartedAt);
  const apiStart = Math.max(start, apiSpendStartedAt ?? start);
  return {
    ok: true,
    accountType: runtime.lastAccountType || getCodexAccountType(),
    today: summarizeCodexTokenRows('where event_timestamp >= ? and event_timestamp < ?', [start, end]),
    all: summarizeCodexTokenRows('', []),
    apiSpendToday: summarizeCodexTokenRows(
      "where account_type = 'api' and event_timestamp >= ? and event_timestamp < ?",
      [apiStart, end]
    ),
    apiSpendStartedAt,
    latestEventAt: getLatestCodexEventAt()
  };
}

function summarizeCodexTokenRows(whereSql, params) {
  const row = database.prepare(`
    select
      count(*) as request_count,
      coalesce(sum(input_tokens), 0) as input_tokens,
      coalesce(sum(cached_input_tokens), 0) as cached_input_tokens,
      coalesce(sum(output_tokens), 0) as output_tokens,
      coalesce(sum(total_tokens), 0) as total_tokens,
      max(event_timestamp) as latest_event_at
    from codex_token_events
    ${whereSql}
  `).get(...params);
  const inputTokens = Number(row.input_tokens || 0);
  const cached = Number(row.cached_input_tokens || 0);
  return {
    requestCount: Number(row.request_count || 0),
    inputTokens,
    cachedInputTokens: cached,
    outputTokens: Number(row.output_tokens || 0),
    totalTokens: Number(row.total_tokens || 0),
    rawUsedAmount: 0,
    usedAmount: 0,
    cacheHitRate: inputTokens ? (cached / inputTokens) * 100 : undefined,
    latestLogAt: row.latest_event_at ? timestampToIso(row.latest_event_at) : undefined
  };
}

function getLatestCodexEventAt() {
  const row = database.prepare('select max(event_timestamp) as latest from codex_token_events').get();
  const latest = Number(row?.latest);
  return Number.isFinite(latest) && latest > 0 ? latest : undefined;
}

function saveCodexTokenEvent(event, rawEvent) {
  if (!event.eventId || !event.usage) return;
  const timestamp = parseIsoTimestamp(event.timestamp) || Date.now() / 1000;
  const usage = event.usage;
  database.prepare(`
    insert or ignore into codex_token_events (
      event_id, account_type, session_file, event_timestamp, event_iso,
      input_tokens, cached_input_tokens, output_tokens, total_tokens,
      reasoning_output_tokens, raw_json
    ) values (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
  `).run(
    event.eventId,
    event.accountType || 'api',
    event.sessionFile,
    Math.trunc(timestamp),
    event.timestamp,
    Math.trunc(numberOrZero(usage.inputTokens)),
    Math.trunc(numberOrZero(usage.cachedInputTokens)),
    Math.trunc(numberOrZero(usage.outputTokens)),
    Math.trunc(numberOrZero(usage.totalTokens)),
    Math.trunc(numberOrZero(usage.reasoningOutputTokens)),
    JSON.stringify(rawEvent || event)
  );
}

async function getCodexStatus() {
  const config = parseCodexConfig(path.join(paths.codexHome, 'config.toml'));
  const providerId = stringOrUndefined(config.model_provider);
  const provider = providerId && config[`model_providers.${providerId}`] && typeof config[`model_providers.${providerId}`] === 'object'
    ? config[`model_providers.${providerId}`]
    : {};
  const baseUrl = stringOrUndefined(provider.base_url);
  const apiKey = getCodexApiKey(config, provider);
  const providerName = stringOrUndefined(provider.name) || providerId;
  const model = stringOrUndefined(config.model);
  const authExists = fs.existsSync(path.join(paths.codexHome, 'auth.json'));
  const customProvider = Boolean(providerId && !['openai', 'chatgpt'].includes(providerId));
  const accountType = customProvider || baseUrl ? 'api' : authExists ? 'official_login' : 'api';
  const rateLimits = accountType === 'official_login'
    ? await getCodexRateLimits()
    : { quota: {}, source: undefined, message: undefined };
  return {
    ok: true,
    accountType,
    providerName,
    model,
    baseUrl,
    apiKeyFingerprint: apiKey ? codexApiKeyFingerprint(apiKey) : undefined,
    quota: rateLimits.quota || {},
    quotaSource: rateLimits.source,
    quotaMessage: rateLimits.message,
    activity: getLatestCodexActivity(),
    source: path.join(paths.codexHome, 'config.toml'),
    updatedAt: new Date().toISOString()
  };
}

async function getCodexRateLimits() {
  const now = Date.now() / 1000;
  if (codexRateLimitCache.quota && now - codexRateLimitCache.checkedAt < CODEX_RATE_LIMIT_CACHE_TTL_SECONDS) {
    return {
      quota: codexRateLimitCache.quota,
      source: codexRateLimitCache.source || 'cache',
      message: codexRateLimitCache.message
    };
  }
  const rpc = await fetchCodexRateLimitsRpc();
  if (Object.keys(rpc.quota || {}).length > 0) {
    Object.assign(codexRateLimitCache, { checkedAt: now, quota: rpc.quota, source: rpc.source, message: rpc.message });
    return rpc;
  }
  const session = fetchCodexRateLimitsFromSession();
  const quota = session.quota || {};
  Object.assign(codexRateLimitCache, { checkedAt: now, quota, source: session.source, message: rpc.message || session.message });
  return { quota, source: session.source || 'codex-session', message: rpc.message || session.message };
}

async function fetchCodexRateLimitsRpc() {
  const codexPath = findCodexBinary();
  if (!codexPath) return { quota: {}, source: 'codex-rpc', message: 'PATH 中找不到 codex' };
  return new Promise((resolve) => {
    let processRef;
    const stdoutLines = [];
    let requestId = 0;
    let settled = false;
    const finish = (value) => {
      if (settled) return;
      settled = true;
      if (processRef && !processRef.killed) processRef.kill();
      resolve(value);
    };
    try {
      processRef = spawn(codexPath, ['-s', 'read-only', '-a', 'untrusted', 'app-server'], {
        windowsHide: true,
        stdio: ['pipe', 'pipe', 'ignore']
      });
    } catch (error) {
      finish({ quota: {}, source: 'codex-rpc', message: `Codex RPC 出错：${errorMessage(error)}` });
      return;
    }
    processRef.stdout.on('data', (chunk) => {
      for (const line of String(chunk).split(/\r?\n/)) {
        if (line.trim()) stdoutLines.push(line.trim());
      }
    });
    processRef.on('error', (error) => finish({ quota: {}, source: 'codex-rpc', message: `Codex RPC 出错：${errorMessage(error)}` }));
    const sendRequest = async (method, params = {}, timeout = 5000) => {
      requestId += 1;
      const id = requestId;
      processRef.stdin.write(`${JSON.stringify({ id, method, params })}\n`);
      const deadline = Date.now() + timeout;
      while (Date.now() < deadline) {
        for (const line of stdoutLines) {
          try {
            const message = JSON.parse(line);
            if (message.id === id) return message;
          } catch {}
        }
        await delay(50);
      }
      return null;
    };
    const sendNotification = (method, params = {}) => {
      processRef.stdin.write(`${JSON.stringify({ method, params })}\n`);
    };
    (async () => {
      const init = await sendRequest('initialize', { clientInfo: { name: 'codex-quota-glance', version: '0.1.0' } });
      if (!init || init.error) {
        finish({ quota: {}, source: 'codex-rpc', message: rpcErrorMessage(init, '初始化 Codex RPC 失败') });
        return;
      }
      sendNotification('initialized');
      const limits = await sendRequest('account/rateLimits/read');
      if (!limits || limits.error) {
        finish({ quota: {}, source: 'codex-rpc', message: rpcErrorMessage(limits, '读取 Codex 余量失败') });
        return;
      }
      finish({
        quota: normalizeCodexRateLimitsCamel(limits.result?.rateLimits || {}),
        source: 'codex-rpc',
        message: undefined
      });
    })().catch((error) => finish({ quota: {}, source: 'codex-rpc', message: `Codex RPC 出错：${errorMessage(error)}` }));
  });
}

function fetchCodexRateLimitsFromSession() {
  const latestEvent = findLatestCodexRateLimitEvent();
  if (!latestEvent) return { quota: {}, source: 'codex-session', message: '会话中没有可用的官方余量记录' };
  return {
    quota: normalizeCodexRateLimits(latestEvent.payload?.rate_limits || {}),
    source: 'codex-session',
    message: undefined
  };
}

function findLatestCodexRateLimitEvent(limit = 80) {
  let latestEvent = null;
  let latestTimestamp = -1;
  for (const sessionFile of recentCodexSessionFiles(limit)) {
    let content = '';
    try {
      content = fs.readFileSync(sessionFile, 'utf8');
    } catch {
      continue;
    }
    for (const line of content.split(/\r?\n/)) {
      if (!line.includes('"token_count"') || !line.includes('"rate_limits"')) continue;
      let event;
      try {
        event = JSON.parse(line);
      } catch {
        continue;
      }
      if (event.payload?.type !== 'token_count' || !hasUsableRateLimits(event.payload?.rate_limits)) continue;
      const timestamp = parseIsoTimestamp(String(event.timestamp || '')) || 0;
      if (timestamp >= latestTimestamp) {
        latestTimestamp = timestamp;
        latestEvent = event;
      }
    }
  }
  return latestEvent;
}

function getLatestCodexActivity() {
  const latestFile = findLatestCodexSessionFile();
  if (!latestFile) {
    return { status: 'unknown', label: '未读取到 Codex 会话', needsHumanAttention: false, completedTask: false };
  }
  try {
    const stat = fs.statSync(latestFile);
    if (
      codexActivityCache.path === latestFile &&
      codexActivityCache.size === stat.size &&
      codexActivityCache.mtimeMs === stat.mtimeMs &&
      codexActivityCache.activity
    ) {
      const activity = settleCodexActivity(codexActivityCache.activity, stat);
      codexActivityCache.activity = activity;
      return activity;
    }
    const activity = settleCodexActivity({ ...parseCodexActivity(latestFile), sessionFile: latestFile }, stat);
    Object.assign(codexActivityCache, {
      path: latestFile,
      size: stat.size,
      mtimeMs: stat.mtimeMs,
      activity
    });
    return activity;
  } catch (error) {
    return { status: 'unknown', label: errorMessage(error), needsHumanAttention: false, completedTask: false, sessionFile: latestFile };
  }
}

function parseCodexActivity(sessionFile) {
  let activity = null;
  let isInsideTurn = false;
  let waitingForPlanChoice = false;
  let lastFinalAnswerAt = null;
  for (const line of recentSessionLines(sessionFile, 512 * 1024)) {
    let event;
    try {
      event = JSON.parse(line);
    } catch {
      continue;
    }
    const update = codexActivityUpdate(event, { isInsideTurn, waitingForPlanChoice, lastFinalAnswerAt });
    if (!update) continue;
    isInsideTurn = Boolean(update.isInsideTurn);
    waitingForPlanChoice = Boolean(update.waitingForPlanChoice);
    if (update.clearsFinalAnswer) lastFinalAnswerAt = null;
    const eventTs = parseIsoTimestamp(String(event.timestamp || ''));
    if (update.isFinalAnswer) lastFinalAnswerAt = eventTs;
    activity = {
      status: update.status,
      label: codexActivityLabel(update.status, Boolean(update.needsHumanAttention)),
      timestamp: event.timestamp,
      needsHumanAttention: Boolean(update.needsHumanAttention),
      completedTask: Boolean(update.completedTask),
      activeTurn: isInsideTurn
    };
  }
  return activity || { status: 'unknown', label: '状态未知', needsHumanAttention: false, completedTask: false };
}

function settleCodexActivity(activity, stat, nowMs = Date.now()) {
  if (!activity || activity.needsHumanAttention) return activity;
  if (activity.activeTurn) return activity;
  if (!['thinking', 'executing', 'answering'].includes(activity.status)) return activity;
  const eventMs = (parseIsoTimestamp(String(activity.timestamp || '')) || 0) * 1000;
  const fileMs = Number(stat?.mtimeMs || 0);
  const latestMs = Math.max(eventMs, fileMs);
  if (!Number.isFinite(latestMs) || latestMs <= 0 || nowMs - latestMs < CODEX_ACTIVITY_STALE_MS) {
    return activity;
  }
  return {
    ...activity,
    status: 'finished',
    label: codexActivityLabel('finished', false),
    needsHumanAttention: false,
    completedTask: true,
    stale: true
  };
}

function codexActivityUpdate(event, state) {
  const eventType = String(event.type || '');
  const payload = event.payload && typeof event.payload === 'object' ? event.payload : {};
  const payloadType = String(payload.type || '');
  if (payload.phase === 'final_answer') return activityUpdate('thinking', false, state.waitingForPlanChoice, { isFinalAnswer: true });
  if (containsHumanWaitingSignal(payload) || containsHumanReviewSignal(payload)) {
    return activityUpdate('waiting_for_user', true, state.waitingForPlanChoice, { needsHumanAttention: true });
  }
  if (containsAutoReviewSignal(payload)) return activityUpdate('auto_reviewing', true, state.waitingForPlanChoice);
  if (isToolStartEvent(eventType, payloadType, payload)) return activityUpdate('executing', true, false, { clearsFinalAnswer: true });
  if (eventType === 'event_msg') {
    if (payloadType === 'task_started') return activityUpdate('thinking', true, false, { clearsFinalAnswer: true });
    if (payloadType === 'task_complete') {
      if (state.waitingForPlanChoice) return activityUpdate('waiting_for_user', false, true, { needsHumanAttention: true });
      return activityUpdate('finished', false, false, { completedTask: true });
    }
    if (['turn_aborted', 'thread_rolled_back'].includes(payloadType)) return activityUpdate('finished', false, false, { clearsFinalAnswer: true });
    if (payloadType === 'user_message') return activityUpdate('thinking', true, false, { clearsFinalAnswer: true });
    if (payloadType === 'agent_message') {
      if (containsPlanChoiceSignal(payload)) return activityUpdate('waiting_for_user', false, true, { needsHumanAttention: true });
      return state.isInsideTurn ? activityUpdate('thinking', true, state.waitingForPlanChoice) : null;
    }
    if (payloadType === 'patch_apply_begin') return activityUpdate('executing', true, false, { clearsFinalAnswer: true });
    if (payloadType === 'patch_apply_end') return activityUpdate('thinking', true, false);
    if (payloadType === 'agent_message_delta') return state.isInsideTurn ? activityUpdate('thinking', true, state.waitingForPlanChoice) : null;
    if (payloadType === 'token_count') return null;
    return payloadType && state.isInsideTurn ? activityUpdate('thinking', true, state.waitingForPlanChoice) : null;
  }
  if (eventType === 'response_item') {
    if (containsPlanChoiceSignal(payload)) return activityUpdate('waiting_for_user', false, true, { needsHumanAttention: true });
    if (payloadType === 'function_call') {
      const needsUser = functionCallNeedsUser(payload);
      return activityUpdate(needsUser ? 'waiting_for_user' : 'executing', true, needsUser, {
        needsHumanAttention: needsUser,
        clearsFinalAnswer: true
      });
    }
    if (['function_call_output', 'custom_tool_call_output'].includes(payloadType)) {
      return activityUpdate('thinking', true, false);
    }
    if (payloadType === 'reasoning') return state.isInsideTurn ? activityUpdate('thinking', true, state.waitingForPlanChoice) : null;
    if (payloadType === 'message') {
      return state.isInsideTurn ? activityUpdate('thinking', true, state.waitingForPlanChoice) : null;
    }
    return payloadType && state.isInsideTurn ? activityUpdate('thinking', true, state.waitingForPlanChoice) : null;
  }
  return null;
}

function activityUpdate(status, isInsideTurn, waitingForPlanChoice, extra = {}) {
  return { status, isInsideTurn, waitingForPlanChoice, ...extra };
}

function findLatestCodexSessionFile() {
  if (!fs.existsSync(paths.codexSessions)) return null;
  const now = Date.now() / 1000;
  if (codexSessionCache.path && fs.existsSync(codexSessionCache.path) && now - codexSessionCache.checkedAt < CODEX_SESSION_DISCOVERY_TTL_SECONDS) {
    return codexSessionCache.path;
  }
  const files = recentCodexSessionFiles(500);
  const latest = files[0] || null;
  codexSessionCache.checkedAt = now;
  codexSessionCache.path = latest;
  return latest;
}

function recentCodexSessionFiles(limit = 80) {
  const files = [];
  walkFiles(paths.codexSessions, (filePath) => {
    if (!filePath.endsWith('.jsonl')) return;
    try {
      files.push({ path: filePath, mtimeMs: fs.statSync(filePath).mtimeMs });
    } catch {}
  });
  files.sort((a, b) => b.mtimeMs - a.mtimeMs);
  return files.slice(0, Math.max(1, limit)).map((file) => file.path);
}

function syncTodayCodexTokenEvents() {
  if (!paths?.codexSessions || !fs.existsSync(paths.codexSessions)) return;
  const accountType = getCodexAccountType();
  const dayStart = new Date();
  dayStart.setHours(0, 0, 0, 0);
  const dayStartMs = dayStart.getTime();
  for (const sessionFile of recentCodexSessionFiles(500)) {
    let stat;
    try {
      stat = fs.statSync(sessionFile);
    } catch {
      continue;
    }
    if (stat.mtimeMs < dayStartMs) continue;
    let content;
    try {
      content = fs.readFileSync(sessionFile, 'utf8');
    } catch {
      continue;
    }
    const contentLines = content.split(/\r?\n/);
    const remainder = /\r?\n$/.test(content) ? '' : contentLines.at(-1) || '';
    const newEvents = parseCodexTokenEvents(content);
    for (const rawEvent of newEvents) {
      saveCodexTokenEvent(buildCodexTokenEvent(rawEvent, sessionFile, accountType), rawEvent);
    }
    codexTokenFileCaches.set(sessionFile, {
      offset: Buffer.byteLength(content),
      remainder,
      latestEvent: newEvents.at(-1) || null
    });
  }
}

function readNewCodexTokenEvents(sessionFile) {
  const cached = codexTokenFileCaches.get(sessionFile) || {
    offset: 0,
    remainder: '',
    latestEvent: null
  };
  let cachedOffset = Number(cached.offset || 0);
  let latestEvent = cached.latestEvent;
  let remainder = String(cached.remainder || '');
  const size = fs.statSync(sessionFile).size;
  if (size < cachedOffset) {
    cachedOffset = 0;
    latestEvent = null;
    remainder = '';
  }
  const fd = fs.openSync(sessionFile, 'r');
  try {
    const buffer = Buffer.alloc(Math.max(0, size - cachedOffset));
    fs.readSync(fd, buffer, 0, buffer.length, cachedOffset);
    const chunk = remainder + buffer.toString('utf8');
    const endsWithNewline = /\r?\n$/.test(chunk);
    const lines = chunk.split(/\r?\n/);
    remainder = endsWithNewline ? '' : lines.pop() || '';
    const parsedEvents = parseCodexTokenEvents(lines.join('\n'));
    const newEvents = parsedEvents;
    if (newEvents.length > 0) latestEvent = newEvents.at(-1);
    codexTokenFileCaches.set(sessionFile, { offset: size, remainder, latestEvent });
    return { latestEvent, newEvents };
  } finally {
    fs.closeSync(fd);
  }
}

function parseCodexTokenEvents(content) {
  const events = [];
  for (const line of String(content || '').split(/\r?\n/)) {
    if (!line.includes('"token_count"')) continue;
    try {
      const event = JSON.parse(line);
      if (event.payload?.type !== 'token_count') continue;
      events.push(event);
    } catch {}
  }
  return events;
}

function recentSessionLines(sessionFile, maxBytes) {
  const fd = fs.openSync(sessionFile, 'r');
  try {
    const total = fs.fstatSync(fd).size;
    const offset = Math.max(0, total - maxBytes);
    const buffer = Buffer.alloc(total - offset);
    fs.readSync(fd, buffer, 0, buffer.length, offset);
    let data = buffer;
    if (offset > 0) {
      const newline = data.indexOf(10);
      if (newline >= 0) data = data.subarray(newline + 1);
    }
    return data.toString('utf8').split(/\r?\n/);
  } finally {
    fs.closeSync(fd);
  }
}

function walkFiles(root, visitor) {
  if (!fs.existsSync(root)) return;
  const stack = [root];
  while (stack.length) {
    const current = stack.pop();
    let entries = [];
    try {
      entries = fs.readdirSync(current, { withFileTypes: true });
    } catch {
      continue;
    }
    for (const entry of entries) {
      const fullPath = path.join(current, entry.name);
      if (entry.isDirectory()) stack.push(fullPath);
      else if (entry.isFile()) visitor(fullPath);
    }
  }
}

function parseCodexConfig(filePath) {
  if (!fs.existsSync(filePath)) return {};
  const result = {};
  let currentSection = null;
  for (const line of fs.readFileSync(filePath, 'utf8').split(/\r?\n/)) {
    const stripped = line.trim();
    if (!stripped || stripped.startsWith('#')) continue;
    if (stripped.startsWith('[') && stripped.endsWith(']')) {
      currentSection = stripped.slice(1, -1).trim();
      if (!result[currentSection]) result[currentSection] = {};
      continue;
    }
    const index = stripped.indexOf('=');
    if (index < 0) continue;
    const key = stripped.slice(0, index).trim();
    const value = parseTomlScalar(stripped.slice(index + 1).trim());
    if (currentSection) result[currentSection][key] = value;
    else result[key] = value;
  }
  return result;
}

function parseTomlScalar(value) {
  if ((value.startsWith('"') && value.endsWith('"')) || (value.startsWith("'") && value.endsWith("'"))) {
    return value.slice(1, -1);
  }
  if (value.toLowerCase() === 'true') return true;
  if (value.toLowerCase() === 'false') return false;
  return value;
}

function getCodexApiKey(config, provider) {
  const candidates = [
    stringOrUndefined(provider.api_key),
    stringOrUndefined(provider.apiKey)
  ];
  const envKey = stringOrUndefined(provider.env_key || provider.api_key_env);
  if (envKey) candidates.push(stringOrUndefined(process.env[envKey]));
  candidates.push(stringOrUndefined(process.env.OPENAI_API_KEY));
  candidates.push(stringOrUndefined(process.env.CODEX_API_KEY));
  const authKey = readCodexAuthKey();
  if (authKey) candidates.push(authKey);
  return candidates.find(Boolean);
}

function readCodexAuthKey() {
  const authPath = path.join(paths.codexHome, 'auth.json');
  if (!fs.existsSync(authPath)) return undefined;
  try {
    const data = JSON.parse(fs.readFileSync(authPath, 'utf8'));
    return stringOrUndefined(data.OPENAI_API_KEY) || stringOrUndefined(data.CODEX_API_KEY);
  } catch {
    return undefined;
  }
}

function findCodexBinary() {
  const candidates = [];
  const config = parseCodexConfig(path.join(paths.codexHome, 'config.toml'));
  const envSection = config['mcp_servers.node_repl.env'];
  if (envSection && typeof envSection === 'object') candidates.push(stringOrUndefined(envSection.CODEX_CLI_PATH));
  candidates.push(stringOrUndefined(process.env.CODEX_CLI_PATH));
  for (const dir of String(process.env.PATH || '').split(path.delimiter)) {
    candidates.push(path.join(dir, process.platform === 'win32' ? 'codex.cmd' : 'codex'));
    candidates.push(path.join(dir, process.platform === 'win32' ? 'codex.exe' : 'codex'));
  }
  if (process.env.LOCALAPPDATA) {
    const binRoot = path.join(process.env.LOCALAPPDATA, 'OpenAI', 'Codex', 'bin');
    walkFiles(binRoot, (file) => {
      if (path.basename(file).toLowerCase() === 'codex.exe') candidates.push(file);
    });
  }
  return candidates.map((item) => item && String(item).replace(/^['"]|['"]$/g, '')).find((item) => item && fs.existsSync(item));
}

function getCodexAccountType() {
  const config = parseCodexConfig(path.join(paths.codexHome, 'config.toml'));
  const providerId = stringOrUndefined(config.model_provider);
  const provider = providerId && config[`model_providers.${providerId}`] && typeof config[`model_providers.${providerId}`] === 'object'
    ? config[`model_providers.${providerId}`]
    : {};
  const baseUrl = stringOrUndefined(provider.base_url);
  const customProvider = Boolean(providerId && !['openai', 'chatgpt'].includes(providerId));
  const authExists = fs.existsSync(path.join(paths.codexHome, 'auth.json'));
  return customProvider || baseUrl ? 'api' : authExists ? 'official_login' : 'api';
}

function normalizeCodexRateLimits(rateLimits) {
  if (!rateLimits || typeof rateLimits !== 'object') return {};
  const windows = assignCodexRateLimitWindows(
    normalizeCodexRateLimitWindow(rateLimits.primary),
    normalizeCodexRateLimitWindow(rateLimits.secondary)
  );
  return {
    ...windows,
    planType: stringOrUndefined(rateLimits.plan_type),
    rateLimitReachedType: stringOrUndefined(rateLimits.rate_limit_reached_type)
  };
}

function normalizeCodexRateLimitsCamel(rateLimits) {
  if (!rateLimits || typeof rateLimits !== 'object') return {};
  const windows = assignCodexRateLimitWindows(
    normalizeCodexRateLimitWindowCamel(rateLimits.primary),
    normalizeCodexRateLimitWindowCamel(rateLimits.secondary)
  );
  return {
    ...windows,
    planType: stringOrUndefined(rateLimits.planType),
    rateLimitReachedType: stringOrUndefined(rateLimits.rateLimitReachedType)
  };
}

function assignCodexRateLimitWindows(primary, secondary) {
  const entries = [
    { position: 'primary', value: primary, role: codexRateLimitRole(primary) },
    { position: 'secondary', value: secondary, role: codexRateLimitRole(secondary) }
  ].filter((entry) => hasNormalizedRateLimitWindow(entry.value));
  let window5h = entries.find((entry) => entry.role === 'window5h')?.value;
  let weekly = entries.find((entry) => entry.role === 'weekly')?.value;

  for (const entry of entries.filter((candidate) => !candidate.role)) {
    if (entry.position === 'primary' && !window5h) window5h = entry.value;
    else if (entry.position === 'secondary' && !weekly) weekly = entry.value;
    else if (!window5h) window5h = entry.value;
    else if (!weekly) weekly = entry.value;
  }

  return { window5h: window5h || {}, weekly: weekly || {} };
}

function reconcileCodexRateLimits(quota, referenceQuota) {
  if (!quota || typeof quota !== 'object') return quota || {};
  const window5h = quota.window5h || {};
  const weekly = quota.weekly || {};
  const referenceWeekly = referenceQuota?.weekly || {};
  const hasAmbiguousPrimary =
    hasNormalizedRateLimitWindow(window5h) &&
    !hasNormalizedRateLimitWindow(weekly) &&
    numberOrUndefined(window5h.windowMinutes) === undefined;

  if (
    !hasAmbiguousPrimary ||
    !hasNormalizedRateLimitWindow(referenceWeekly) ||
    !codexRateLimitWindowsMatch(window5h, referenceWeekly)
  ) {
    return quota;
  }

  return {
    ...quota,
    window5h: {},
    weekly: {
      ...referenceWeekly,
      ...window5h,
      windowMinutes: referenceWeekly.windowMinutes
    }
  };
}

function codexRateLimitWindowsMatch(first, second) {
  const firstReset = Date.parse(String(first?.resetAt || ''));
  const secondReset = Date.parse(String(second?.resetAt || ''));
  if (!Number.isFinite(firstReset) || !Number.isFinite(secondReset) || Math.abs(firstReset - secondReset) > 5000) {
    return false;
  }
  const firstUsed = numberOrUndefined(first?.usedPercent);
  const secondUsed = numberOrUndefined(second?.usedPercent);
  return firstUsed === undefined || secondUsed === undefined || Math.abs(firstUsed - secondUsed) < 0.01;
}

function codexRateLimitRole(window) {
  const minutes = numberOrUndefined(window?.windowMinutes);
  if (minutes === undefined || minutes <= 0) return undefined;
  return minutes >= 24 * 60 ? 'weekly' : 'window5h';
}

function hasNormalizedRateLimitWindow(window) {
  return Boolean(window && typeof window === 'object' && Object.values(window).some((value) => value !== undefined));
}

function normalizeCodexRateLimitWindow(window) {
  if (!window || typeof window !== 'object') return {};
  const usedPercent = numberOrUndefined(window.used_percent);
  const resetsAt = numberOrUndefined(window.resets_at);
  return {
    usedPercent,
    remainingPercent: usedPercent === undefined ? undefined : Math.max(0, Math.min(100, 100 - usedPercent)),
    windowMinutes: numberOrUndefined(window.window_minutes),
    resetAt: timestampToIso(resetsAt),
    resetInSeconds: resetsAt === undefined ? undefined : Math.max(0, Math.trunc(resetsAt - Date.now() / 1000))
  };
}

function normalizeCodexRateLimitWindowCamel(window) {
  if (!window || typeof window !== 'object') return {};
  const usedPercent = numberOrUndefined(window.usedPercent);
  const resetsAt = numberOrUndefined(window.resetsAt);
  return {
    usedPercent,
    remainingPercent: usedPercent === undefined ? undefined : Math.max(0, Math.min(100, 100 - usedPercent)),
    windowMinutes: numberOrUndefined(window.windowMinutes),
    resetAt: timestampToIso(resetsAt),
    resetInSeconds: resetsAt === undefined ? undefined : Math.max(0, Math.trunc(resetsAt - Date.now() / 1000))
  };
}

function hasUsableRateLimits(rateLimits) {
  return rateLimits && typeof rateLimits === 'object' && (
    hasUsableRateLimitWindow(rateLimits.primary) || hasUsableRateLimitWindow(rateLimits.secondary)
  );
}

function hasUsableRateLimitWindow(window) {
  return window && typeof window === 'object' && (
    numberOrUndefined(window.used_percent) !== undefined ||
    numberOrUndefined(window.resets_at) !== undefined ||
    numberOrUndefined(window.window_minutes) !== undefined
  );
}

function getLatestCreatedAt(syncKey) {
  const row = database.prepare('select latest_created_at from newapi_sync_state where sync_key = ?').get(syncKey);
  return row?.latest_created_at ? Number(row.latest_created_at) : undefined;
}

function getBackfillUntil(syncKey) {
  const row = database.prepare('select backfill_until from newapi_sync_state where sync_key = ?').get(syncKey);
  return row?.backfill_until ? Number(row.backfill_until) : undefined;
}

function saveBackfillUntil(syncKey, value) {
  database.prepare('update newapi_sync_state set backfill_until = ? where sync_key = ?').run(value ?? null, syncKey);
}

function markBackfillComplete(syncKey) {
  database.prepare('update newapi_sync_state set backfill_until = null, backfill_complete = 1, backfill_warning = null where sync_key = ?').run(syncKey);
}

function markBackfillIncomplete(syncKey, warning) {
  database.prepare('update newapi_sync_state set backfill_complete = 0, backfill_warning = ? where sync_key = ?').run(warning ?? null, syncKey);
}

function getGlobalLatestCreatedAt() {
  const row = database.prepare('select max(created_at) as latest from newapi_logs').get();
  return row?.latest ? Number(row.latest) : undefined;
}

function getFirstCreatedAt() {
  const row = database.prepare('select min(created_at) as first from newapi_logs').get();
  return row?.first ? Number(row.first) : undefined;
}

function getFirstCreatedAtForSync() {
  return getFirstCreatedAt();
}

function getLogCoverage(syncSnapshot) {
  const first = getFirstCreatedAt();
  const expected = INITIAL_SYNC_START;
  const scanned = numberOrUndefined(syncSnapshot?.backfillUntil);
  const backfillComplete = syncSnapshot?.backfillComplete === true;
  const warning = stringOrUndefined(syncSnapshot?.backfillWarning);
  const completeBoundary = first ?? getGlobalLatestCreatedAt();
  let complete = false;
  if (completeBoundary !== undefined && !warning) {
    complete = backfillComplete ||
      (first !== undefined && first <= expected + SYNC_OVERLAP_SECONDS) ||
      (scanned !== undefined && scanned >= completeBoundary - SYNC_OVERLAP_SECONDS);
  }
  const scannedFloor = scanned ?? expected;
  const missing = complete ? 0 : Math.max(0, (first ?? completeBoundary ?? expected) - Math.max(expected, scannedFloor));
  return {
    complete,
    firstCreatedAt: first,
    expectedStartAt: expected,
    scannedThroughAt: scanned,
    missingBeforeSeconds: missing,
    warning
  };
}

function getLogCoverageForSync(syncKey) {
  return getLogCoverage(getLatestSyncSnapshot(syncKey));
}

function getLatestSyncSnapshot(syncKey) {
  const row = syncKey
    ? database.prepare(`
        select latest_created_at, last_synced_at, fail_count, blocked_until, backfill_until,
               backfill_complete, backfill_warning
        from newapi_sync_state where sync_key = ? limit 1
      `).get(syncKey)
    : database.prepare(`
        select latest_created_at, last_synced_at, fail_count, blocked_until, backfill_until,
               backfill_complete, backfill_warning
        from newapi_sync_state order by last_synced_at desc limit 1
      `).get();
  if (!row) return null;
  const blockedUntil = row.blocked_until;
  const backfillUntil = row.backfill_until;
  const backfillComplete = row.backfill_complete === 1;
  const first = getFirstCreatedAt();
  const backfillDone = backfillComplete ||
    (backfillUntil !== null && backfillUntil !== undefined && first !== undefined && Number(backfillUntil) >= first - SYNC_OVERLAP_SECONDS);
  return {
    mode: blockedUntil ? 'backoff' : backfillUntil && !backfillDone ? 'backfill' : 'incremental',
    latestCreatedAt: row.latest_created_at ?? undefined,
    lastSyncedAt: row.last_synced_at ?? undefined,
    failCount: row.fail_count ?? undefined,
    blockedUntil: row.blocked_until ?? undefined,
    backfillUntil: row.backfill_until ?? undefined,
    backfillComplete,
    backfillWarning: stringOrUndefined(row.backfill_warning)
  };
}

function saveSyncState(syncKey, baseUrl, apiUser, apiKey, latest) {
  database.prepare(`
    insert into newapi_sync_state (
      sync_key, base_url, api_user, key_fingerprint, latest_created_at, last_synced_at,
      fail_count, blocked_until, backfill_until
    ) values (?, ?, ?, ?, ?, strftime('%s','now'), 0, null, null)
    on conflict(sync_key) do update set
      latest_created_at = excluded.latest_created_at,
      last_synced_at = excluded.last_synced_at,
      fail_count = 0,
      blocked_until = null
  `).run(syncKey, baseUrl, apiUser, keyFingerprint(apiKey), latest ?? null);
}

function getSyncBlock(syncKey) {
  return database.prepare('select fail_count, blocked_until from newapi_sync_state where sync_key = ?').get(syncKey) || null;
}

function saveSyncFailure(syncKey, baseUrl, apiUser, apiKey, latest, retryAfter) {
  const nowTs = unixNow();
  const state = getSyncBlock(syncKey) || {};
  const failCount = Number(state.fail_count || 0) + 1;
  const delaySeconds = retryAfter && retryAfter > 0 ? retryAfter : backoffSeconds(failCount);
  const blockedUntil = nowTs + delaySeconds;
  database.prepare(`
    insert into newapi_sync_state (
      sync_key, base_url, api_user, key_fingerprint, latest_created_at, last_synced_at,
      fail_count, blocked_until
    ) values (?, ?, ?, ?, ?, strftime('%s','now'), ?, ?)
    on conflict(sync_key) do update set
      latest_created_at = coalesce(excluded.latest_created_at, newapi_sync_state.latest_created_at),
      last_synced_at = excluded.last_synced_at,
      fail_count = excluded.fail_count,
      blocked_until = excluded.blocked_until
  `).run(syncKey, baseUrl, apiUser, keyFingerprint(apiKey), latest ?? null, failCount, blockedUntil);
  return blockedUntil;
}

function saveAccountCache(baseUrl, apiUser, accessToken, snapshot) {
  const cached = { ...snapshot, cached: true, cachedAt: new Date().toISOString() };
  database.prepare(`
    insert into newapi_account_cache (account_key, base_url, api_user, token_fingerprint, snapshot_json, updated_at)
    values (?, ?, ?, ?, ?, strftime('%s','now'))
    on conflict(account_key) do update set snapshot_json = excluded.snapshot_json, updated_at = excluded.updated_at
  `).run(makeAccountKey(baseUrl, apiUser, accessToken), baseUrl, apiUser, keyFingerprint(accessToken), JSON.stringify(cached));
}

function saveTopupCache(baseUrl, apiUser, accessToken, snapshot) {
  const cached = { ...snapshot, cached: true, updatedAt: new Date().toISOString() };
  database.prepare(`
    insert into newapi_topup_cache (topup_key, base_url, api_user, token_fingerprint, snapshot_json, updated_at)
    values (?, ?, ?, ?, ?, strftime('%s','now'))
    on conflict(topup_key) do update set snapshot_json = excluded.snapshot_json, updated_at = excluded.updated_at
  `).run(makeAccountKey(baseUrl, apiUser, accessToken), baseUrl, apiUser, keyFingerprint(accessToken), JSON.stringify(cached));
}

function getLatestAccountCache({ baseUrl = '', apiUser = '', accessToken = '' } = {}) {
  const accountKey = baseUrl && accessToken ? makeAccountKey(baseUrl, apiUser, accessToken) : '';
  const row = accountKey
    ? database.prepare('select snapshot_json from newapi_account_cache where account_key = ? limit 1').get(accountKey)
    : database.prepare('select snapshot_json from newapi_account_cache order by updated_at desc limit 1').get();
  return parseJsonObject(row?.snapshot_json);
}

function getFreshAccountCache(ttlSeconds) {
  const cutoff = unixNow() - ttlSeconds;
  const row = database.prepare('select snapshot_json from newapi_account_cache where updated_at >= ? order by updated_at desc limit 1').get(cutoff);
  return parseJsonObject(row?.snapshot_json);
}

function getLatestTopupCache({ baseUrl = '', apiUser = '', accessToken = '' } = {}) {
  const topupKey = baseUrl && accessToken ? makeAccountKey(baseUrl, apiUser, accessToken) : '';
  const row = topupKey
    ? database.prepare('select snapshot_json from newapi_topup_cache where topup_key = ? limit 1').get(topupKey)
    : database.prepare('select snapshot_json from newapi_topup_cache order by updated_at desc limit 1').get();
  return parseJsonObject(row?.snapshot_json);
}

function getFreshTopupCache(ttlSeconds) {
  const cutoff = unixNow() - ttlSeconds;
  const row = database.prepare('select snapshot_json from newapi_topup_cache where updated_at >= ? order by updated_at desc limit 1').get(cutoff);
  return parseJsonObject(row?.snapshot_json);
}

function applyTopupToAccount(account, topup) {
  if (!account || account.ok !== true || !topup || topup.ok !== true) return account;
  let totalAmount = numberOrUndefined(topup.totalAmount);
  let rawTotal = numberOrUndefined(topup.rawTotalQuota);
  if (totalAmount === undefined && rawTotal === undefined) return account;
  if (rawTotal === undefined && totalAmount !== undefined) rawTotal = totalAmount * QUOTA_UNITS_PER_CNY;
  if (totalAmount === undefined && rawTotal !== undefined) totalAmount = rawTotal / QUOTA_UNITS_PER_CNY;
  return {
    ...account,
    balance: {
      ...(account.balance || {}),
      totalRecharged: totalAmount,
      rawTotalRecharged: rawTotal,
      totalRechargedEstimated: false
    }
  };
}

function normalizeTopupItems(data) {
  if (Array.isArray(data)) return data.filter((item) => item && typeof item === 'object');
  if (data && typeof data === 'object') {
    for (const key of ['items', 'data', 'logs']) {
      if (Array.isArray(data[key])) return data[key].filter((item) => item && typeof item === 'object');
    }
  }
  return [];
}

function summarizeTopupItems(items, pages, total) {
  const successful = items.filter((item) => String(item.status || '').toLowerCase() === 'success');
  const totalAmount = successful.reduce((sum, item) => sum + numberOrZero(item.amount), 0);
  const totalMoney = successful.reduce((sum, item) => sum + numberOrZero(item.money), 0);
  const rawTotalQuota = successful.reduce((sum, item) => sum + numberOrZero(item.quota_granted), 0);
  const latestTime = Math.max(0, ...successful.map((item) => Math.trunc(numberOrZero(item.create_time))));
  return {
    ok: true,
    count: successful.length,
    totalAmount,
    totalMoney,
    rawTotalQuota: rawTotalQuota ? Math.trunc(rawTotalQuota) : Math.trunc(totalAmount * QUOTA_UNITS_PER_CNY),
    latestCreatedAt: latestTime || undefined,
    pages,
    providerTotal: total
  };
}

function readSummaryContext(request, requestUrl) {
  return {
    baseUrl: stringHeader(request.headers['x-newapi-baseurl']) || requestUrl.searchParams.get('baseUrl'),
    accessToken: stringHeader(request.headers['x-newapi-accesstoken']) || requestUrl.searchParams.get('accessToken'),
    apiUser: stringHeader(request.headers['x-newapi-user']) || requestUrl.searchParams.get('apiUser') || requestUrl.searchParams.get('newApiUser')
  };
}

function newApiAuthHeaders(token, apiUser, apifoxLike = false) {
  const clean = cleanAuthorizationToken(token);
  return pruneHeaders({
    Authorization: `Bearer ${clean}`,
    'New-Api-User': String(apiUser || '').trim(),
    ...(apifoxLike ? {
      'User-Agent': 'Apifox/1.0.0 (https://apifox.com)',
      Accept: '*/*',
      Connection: 'keep-alive'
    } : { Accept: 'application/json' })
  });
}

function maskedNewApiHeaders(token, apiUser, apifoxLike = false) {
  const clean = cleanAuthorizationToken(token);
  return {
    Authorization: `Bearer ${maskSecret(clean)}`,
    'New-Api-User': String(apiUser || '').trim(),
    ...(apifoxLike ? { 'User-Agent': 'Apifox/1.0.0 (https://apifox.com)', Accept: '*/*' } : {})
  };
}

function newApiHeaderDiagnostics(token) {
  const clean = cleanAuthorizationToken(token);
  return {
    tokenTrimmedLength: clean.length,
    tokenHashPrefix: sha256(clean).slice(0, 12),
    authorizationPrefix: clean ? 'Bearer ' : ''
  };
}

function cleanAuthorizationToken(token) {
  const text = String(token || '').trim();
  const bearerMatch = text.match(/\bBearer\s+([^\s,]+)/i);
  if (bearerMatch?.[1]) return bearerMatch[1].trim();
  const authorizationMatch = text.match(/^Authorization\s*:\s*([^\s,]+)/i);
  if (authorizationMatch?.[1]) return authorizationMatch[1].trim();
  return text.split(/\s+/)[0] || '';
}

function contentType(filePath) {
  const ext = path.extname(filePath).toLowerCase();
  if (ext === '.html') return 'text/html; charset=utf-8';
  if (ext === '.js') return 'text/javascript; charset=utf-8';
  if (ext === '.css') return 'text/css; charset=utf-8';
  if (ext === '.json') return 'application/json; charset=utf-8';
  if (ext === '.svg') return 'image/svg+xml';
  if (ext === '.png') return 'image/png';
  if (ext === '.ico') return 'image/x-icon';
  return 'application/octet-stream';
}

function sendJson(response, status, payload) {
  const body = Buffer.from(JSON.stringify(payload, (_key, value) => value === undefined ? null : value));
  response.writeHead(status, {
    'Content-Type': 'application/json; charset=utf-8',
    'Cache-Control': 'no-store',
    'Content-Length': String(body.length)
  });
  response.end(body);
}

function sendText(response, status, text) {
  const body = Buffer.from(String(text), 'utf8');
  response.writeHead(status, {
    'Content-Type': 'text/plain; charset=utf-8',
    'Content-Length': String(body.length)
  });
  response.end(body);
}

function readBuffer(request) {
  return new Promise((resolve, reject) => {
    const chunks = [];
    request.on('data', (chunk) => chunks.push(chunk));
    request.on('end', () => resolve(Buffer.concat(chunks)));
    request.on('error', reject);
  });
}

async function readJson(request) {
  const buffer = await readBuffer(request);
  if (!buffer.length) return {};
  return JSON.parse(buffer.toString('utf8'));
}

function pruneHeaders(headers) {
  return Object.fromEntries(Object.entries(headers).filter(([, value]) => value !== undefined && value !== null && value !== ''));
}

function stringHeader(value) {
  return Array.isArray(value) ? String(value[0] || '') : String(value || '');
}

function trimTrailingSlash(value) {
  return String(value || '').trim().replace(/\/+$/, '');
}

function parseOther(value) {
  if (value && typeof value === 'object') return value;
  if (typeof value === 'string' && value.trim()) {
    try {
      const parsed = JSON.parse(value);
      return parsed && typeof parsed === 'object' ? parsed : {};
    } catch {
      return {};
    }
  }
  return {};
}

function parseJsonObject(value) {
  if (!value) return null;
  try {
    const parsed = JSON.parse(value);
    return parsed && typeof parsed === 'object' ? parsed : null;
  } catch {
    return null;
  }
}

function numberOrZero(value) {
  const number = Number(value);
  return Number.isFinite(number) ? number : 0;
}

function numberOrUndefined(value) {
  if (value === undefined || value === null || value === '') return undefined;
  const number = Number(value);
  return Number.isFinite(number) ? number : undefined;
}

function stringOrUndefined(value) {
  if (value === undefined || value === null) return undefined;
  const text = String(value).trim();
  return text ? text : undefined;
}

function unixNow() {
  return Math.trunc(Date.now() / 1000);
}

function normalizeTtl(value, fallback) {
  const number = Number(value);
  return Number.isFinite(number) ? Math.min(21600, Math.max(60, Math.trunc(number))) : fallback;
}

function parseRetryAfter(value) {
  const number = Number(String(value || '').trim());
  return Number.isFinite(number) && number > 0 ? Math.trunc(number) : undefined;
}

function backoffSeconds(failCount) {
  if (failCount <= 1) return 60;
  if (failCount === 2) return 180;
  return 300;
}

function makeSyncKey(baseUrl, apiUser, apiKey) {
  return sha256(`${baseUrl}|${apiUser}|${keyFingerprint(apiKey)}`);
}

function makeAccountKey(baseUrl, apiUser, accessToken) {
  return sha256(`${baseUrl}|${apiUser}|${keyFingerprint(accessToken)}`);
}

function keyFingerprint(value) {
  return sha256(String(value || '')).slice(0, 16);
}

function sha256(value) {
  return crypto.createHash('sha256').update(String(value || '')).digest('hex');
}

function codexApiKeyFingerprint(value) {
  let hash = 0x811c9dc5;
  for (const char of String(value || '')) {
    hash ^= char.charCodeAt(0);
    hash = Math.imul(hash, 0x01000193) >>> 0;
  }
  return `fnv1a:${hash.toString(16).padStart(8, '0')}`;
}

function maskSecret(value) {
  const text = String(value || '');
  if (text.length <= 10) return text ? '***' : '';
  return `${text.slice(0, 4)}...${text.slice(-4)}`;
}

function appendRequestDebug(entry) {
  try {
    fs.appendFileSync(paths.requestDebugLog, `${JSON.stringify({ ...entry, at: new Date().toISOString() })}\n`, 'utf8');
  } catch {}
}

function parseIsoTimestamp(value) {
  if (!value) return undefined;
  const timestamp = Date.parse(String(value).replace('Z', '+00:00'));
  return Number.isFinite(timestamp) ? timestamp / 1000 : undefined;
}

function timestampToIso(value) {
  const number = Number(value);
  if (!Number.isFinite(number)) return undefined;
  return new Date(number * 1000).toISOString();
}

function delay(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function rpcErrorMessage(message, fallback) {
  const error = message?.error;
  return error?.message ? `${fallback}：${error.message}` : fallback;
}

function codexActivityLabel(status, needsHumanAttention) {
  if (status === 'thinking') return '思考中';
  if (status === 'executing') return '执行中';
  if (status === 'answering') return '执行中';
  if (status === 'waiting_for_user') return needsHumanAttention ? '等待授权' : '思考中';
  if (status === 'auto_reviewing') return '自动审核中';
  if (status === 'finished') return '空闲';
  return '未知';
}

function functionCallNeedsUser(payload) {
  const name = String(payload.name || '');
  if (['request_user_input', 'request_plugin_install'].includes(name)) return true;
  return String(payload.arguments || '').toLowerCase().includes('require_escalated') ||
    String(payload.arguments || '').toLowerCase().includes('sandbox_permissions');
}

function isToolStartEvent(eventType, payloadType, payload) {
  if (functionCallNeedsUser(payload)) return false;
  if (['function_call', 'custom_tool_call', 'web_search_call', 'patch_apply_begin'].includes(eventType)) return true;
  if (['function_call', 'custom_tool_call', 'web_search_call', 'patch_apply_begin'].includes(payloadType)) return true;
  return ['apply_patch', 'exec_command', 'write_stdin', 'view_image'].includes(String(payload.name || ''));
}

function containsHumanWaitingSignal(payload) {
  const values = [payload.type, payload.name].map((value) => String(value || '').toLowerCase());
  return values.some((value) => value.includes('approval') || value.includes('permission') || value.includes('request_user_input')) ||
    functionCallNeedsUser(payload);
}

function containsAutoReviewSignal(payload) {
  return structuredStringValues(payload).some((value) => value.replace(/-/g, '_').toLowerCase().includes('auto_review'));
}

function containsHumanReviewSignal(payload) {
  if (containsAutoReviewSignal(payload)) return false;
  return structuredStringValues(payload).some((value) => {
    const text = value.replace(/-/g, '_').toLowerCase();
    return text.includes('review_pending') || text.includes('reviewing') || text.includes('reviewer');
  });
}

function containsPlanChoiceSignal(payload) {
  return stringValues(payload).some((value) => value.toLowerCase().includes('<proposed_plan>') || value.includes('实施此计划'));
}

function structuredStringValues(payload) {
  return ['type', 'name', 'status', 'reviewer', 'approval_reviewer', 'approvals_reviewer']
    .map((key) => payload[key])
    .filter((value) => value !== undefined && value !== null)
    .map(String);
}

function stringValues(value) {
  if (typeof value === 'string') return [value];
  if (Array.isArray(value)) return value.flatMap(stringValues);
  if (value && typeof value === 'object') return Object.values(value).flatMap(stringValues);
  return [];
}

function errorMessage(error) {
  return error instanceof Error ? error.message : String(error || 'Unknown error');
}

class RateLimitedError extends Error {
  constructor(retryAfter) {
    super('rate limited');
    this.retryAfter = parseRetryAfter(retryAfter);
  }
}

module.exports = {
  startLocalBackend,
  stopLocalBackend,
  _internals: {
    normalizeLogRow,
    normalizeLogItems,
    summarizeLogRows,
    normalizeCodexRateLimits,
    normalizeCodexRateLimitsCamel,
    reconcileCodexRateLimits,
    parseCodexConfig,
    parseCodexTokenEvents,
    settleCodexActivity,
    codexActivityUpdate,
    cleanAuthorizationToken,
    makeSyncKey,
    keyFingerprint
  }
};
