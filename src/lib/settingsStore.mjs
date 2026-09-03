export const SETTINGS_STORAGE_KEY = 'codexQuotaGlanceSettings';

export const DEFAULT_APP_SETTINGS = Object.freeze({
  newApi: Object.freeze({
    activeProviderId: 'default-new-api',
    providers: Object.freeze([
      Object.freeze({
        id: 'default-new-api',
        displayName: '工作中转站',
        baseUrl: 'https://your-new-api.example.com',
        apiKey: 'sk-••••••••••••••••',
        accessToken: '',
        newApiUser: '1',
        pricingProfile: Object.freeze({
          id: 'default-new-api',
          name: 'New API 默认估算',
          currency: 'CNY',
          quotaUnitPerUsd: 500000,
          initialBalance: 0,
          totalRecharged: 0,
          resetMode: 'none',
          inputPricePerMillion: 2,
          cachedInputPricePerMillion: 0.5,
          outputPricePerMillion: 8,
          cnyPerUsd: 7.2,
          modelRatio: 1,
          completionRatio: 4,
          groupRatio: 1,
          safetyMultiplier: 1
        })
      })
    ]),
    displayName: '工作中转站',
    baseUrl: 'https://your-new-api.example.com',
    apiKey: 'sk-••••••••••••••••',
    accessToken: '',
    newApiUser: '1',
    amountDisplayMode: 'cny',
    refreshIntervalSeconds: 30,
    codexTokenPollIntervalSeconds: 5,
    platformSyncIntervalSeconds: 600,
    accountRefreshIntervalSeconds: 300,
    topupRefreshIntervalSeconds: 600,
    spendToastSeconds: 5
  }),
  pricingProfile: Object.freeze({
    id: 'default-new-api',
    name: 'New API 默认估算',
    currency: 'CNY',
    quotaUnitPerUsd: 500000,
    initialBalance: 0,
    totalRecharged: 0,
    resetMode: 'none',
    inputPricePerMillion: 2,
    cachedInputPricePerMillion: 0.5,
    outputPricePerMillion: 8,
    cnyPerUsd: 7.2,
    modelRatio: 1,
    completionRatio: 4,
    groupRatio: 1,
    safetyMultiplier: 1
  }),
  window: Object.freeze({
    capsulePosition: Object.freeze({
      x: 28,
      y: 28
    })
  })
});

export function loadAppSettings(storage = browserStorage()) {
  const raw = storage?.getItem?.(SETTINGS_STORAGE_KEY);
  if (!raw) return cloneDefaultSettings();

  try {
    return mergeAppSettings(JSON.parse(raw));
  } catch {
    return cloneDefaultSettings();
  }
}

export function saveAppSettings(storage = browserStorage(), settings) {
  storage?.setItem?.(SETTINGS_STORAGE_KEY, JSON.stringify(toPersistedSettings(settings)));
}

export function mergeAppSettings(settings) {
  const newApi = isObject(settings?.newApi) ? settings.newApi : {};
  const normalizedApiKey = normalizeApiKey(newApi.apiKey ?? DEFAULT_APP_SETTINGS.newApi.apiKey);
  const normalizedAccessToken = normalizeBearerToken(newApi.accessToken ?? DEFAULT_APP_SETTINGS.newApi.accessToken);
  const migratedAccessToken = !normalizedAccessToken && normalizedApiKey && !normalizedApiKey.startsWith('sk-')
    ? normalizedApiKey
    : normalizedAccessToken;
  const migratedApiKey = migratedAccessToken === normalizedApiKey && !normalizedApiKey.startsWith('sk-')
    ? ''
    : normalizedApiKey;
  const pricingProfile = normalizePricingProfile(
    migratePricingProfileDefaults(isObject(settings?.pricingProfile) ? settings.pricingProfile : {})
  );
  const baseNewApi = {
    ...DEFAULT_APP_SETTINGS.newApi,
    ...newApi,
    apiKey: migratedApiKey,
    accessToken: migratedAccessToken,
    amountDisplayMode: normalizeAmountDisplayMode(newApi.amountDisplayMode),
    refreshIntervalSeconds: normalizeRefreshInterval(newApi.refreshIntervalSeconds),
    codexTokenPollIntervalSeconds: normalizeCodexTokenPollInterval(newApi.codexTokenPollIntervalSeconds),
    platformSyncIntervalSeconds: normalizePlatformSyncInterval(newApi.platformSyncIntervalSeconds),
    accountRefreshIntervalSeconds: normalizeCalibrationInterval(newApi.accountRefreshIntervalSeconds, DEFAULT_APP_SETTINGS.newApi.accountRefreshIntervalSeconds),
    topupRefreshIntervalSeconds: normalizeCalibrationInterval(newApi.topupRefreshIntervalSeconds, DEFAULT_APP_SETTINGS.newApi.topupRefreshIntervalSeconds),
    spendToastSeconds: normalizeToastSeconds(newApi.spendToastSeconds)
  };
  const basePricingProfile = {
    ...DEFAULT_APP_SETTINGS.pricingProfile,
    ...pricingProfile
  };
  const normalizedProviders = normalizeProviders(newApi.providers, baseNewApi, basePricingProfile);
  const activeProviderId = normalizeActiveProviderId(newApi.activeProviderId, normalizedProviders);
  const providers = applyTopLevelProviderOverrides(normalizedProviders, activeProviderId, newApi, baseNewApi);
  const activeProvider = providers.find((provider) => provider.id === activeProviderId) ?? providers[0];
  return {
    newApi: {
      ...baseNewApi,
      activeProviderId,
      providers,
      displayName: activeProvider.displayName,
      baseUrl: activeProvider.baseUrl,
      apiKey: activeProvider.apiKey,
      accessToken: activeProvider.accessToken,
      newApiUser: activeProvider.newApiUser
    },
    pricingProfile: activeProvider.pricingProfile,
    window: {
      ...DEFAULT_APP_SETTINGS.window,
      ...(isObject(settings?.window) ? settings.window : {}),
      capsulePosition: normalizeCapsulePosition(settings?.window?.capsulePosition)
    }
  };
}

function migratePricingProfileDefaults(profile) {
  const initial = Number(profile.initialBalance);
  const total = Number(profile.totalRecharged);
  if ((initial === 120 && total === 120) || (initial === 90 && total === 90)) {
    return {
      ...profile,
      initialBalance: DEFAULT_APP_SETTINGS.pricingProfile.initialBalance,
      totalRecharged: DEFAULT_APP_SETTINGS.pricingProfile.totalRecharged
    };
  }
  return profile;
}

function normalizePricingProfile(profile) {
  return profile;
}

function normalizeAmountDisplayMode(value) {
  return value === 'usd' ? 'usd' : 'cny';
}

function normalizeRefreshInterval(value) {
  const number = Number(value);
  if (!Number.isFinite(number)) return DEFAULT_APP_SETTINGS.newApi.refreshIntervalSeconds;
  return Math.min(3600, Math.max(5, Math.round(number)));
}

function normalizeCodexTokenPollInterval(value) {
  const number = Number(value);
  if (!Number.isFinite(number)) return DEFAULT_APP_SETTINGS.newApi.codexTokenPollIntervalSeconds;
  return Math.min(60, Math.max(5, Math.round(number)));
}

function normalizePlatformSyncInterval(value) {
  const number = Number(value);
  if (!Number.isFinite(number)) return DEFAULT_APP_SETTINGS.newApi.platformSyncIntervalSeconds;
  return Math.min(21600, Math.max(60, Math.round(number)));
}

function normalizeCalibrationInterval(value, fallback) {
  const number = Number(value);
  if (!Number.isFinite(number)) return fallback;
  return Math.min(21600, Math.max(60, Math.round(number)));
}

function normalizeToastSeconds(value) {
  const number = Number(value);
  if (!Number.isFinite(number)) return DEFAULT_APP_SETTINGS.newApi.spendToastSeconds;
  return Math.min(30, Math.max(1, Math.round(number)));
}

export function updatePricingProfile(settings, key, value) {
  const pricingProfile = {
    ...settings.pricingProfile,
    [key]: coerceNumber(value, settings.pricingProfile[key])
  };
  const provider = getActiveProvider(settings);
  return {
    ...settings,
    newApi: provider
      ? {
          ...settings.newApi,
          providers: settings.newApi.providers.map((item) =>
            item.id === provider.id ? { ...item, pricingProfile } : item
          )
        }
      : settings.newApi,
    pricingProfile
  };
}

export function upsertNewApiProvider(settings, provider) {
  const normalized = normalizeProvider(provider, settings.newApi, settings.pricingProfile);
  const exists = settings.newApi.providers.some((item) => item.id === normalized.id);
  const providers = exists
    ? settings.newApi.providers.map((item) => item.id === normalized.id ? normalized : item)
    : [...settings.newApi.providers, normalized];
  return applyActiveProvider({
    ...settings,
    newApi: {
      ...settings.newApi,
      activeProviderId: normalized.id,
      providers
    }
  }, normalized.id);
}

export function selectNewApiProvider(settings, providerId) {
  return applyActiveProvider(settings, providerId);
}

export function selectProviderForCodexStatus(settings, codexStatus) {
  if (codexStatus?.accountType !== 'api') return settings;
  const providers = Array.isArray(settings?.newApi?.providers) ? settings.newApi.providers : [];
  if (providers.length === 0) return settings;

  const fingerprint = String(codexStatus?.apiKeyFingerprint ?? '').trim();
  const baseUrl = normalizeProviderBaseUrl(codexStatus?.baseUrl);
  const matched = fingerprint
    ? providers.find((provider) => keyFingerprint(provider.apiKey) === fingerprint)
    : providers.find((provider) => baseUrl && normalizeProviderBaseUrl(provider.baseUrl) === baseUrl);

  if (!matched || matched.id === settings.newApi.activeProviderId) return settings;
  return applyActiveProvider(settings, matched.id);
}

export function deleteNewApiProvider(settings, providerId) {
  if (settings.newApi.providers.length <= 1) return settings;
  const providers = settings.newApi.providers.filter((provider) => provider.id !== providerId);
  const activeProviderId = settings.newApi.activeProviderId === providerId
    ? providers[0]?.id
    : settings.newApi.activeProviderId;
  return applyActiveProvider({
    ...settings,
    newApi: {
      ...settings.newApi,
      providers,
      activeProviderId
    }
  }, activeProviderId);
}

function applyActiveProvider(settings, providerId) {
  const providers = settings.newApi.providers.length
    ? settings.newApi.providers
    : normalizeProviders([], settings.newApi, settings.pricingProfile);
  const activeProvider = providers.find((provider) => provider.id === providerId) ?? providers[0];
  return {
    ...settings,
    newApi: {
      ...settings.newApi,
      activeProviderId: activeProvider.id,
      providers,
      displayName: activeProvider.displayName,
      baseUrl: activeProvider.baseUrl,
      apiKey: activeProvider.apiKey,
      accessToken: activeProvider.accessToken,
      newApiUser: activeProvider.newApiUser
    },
    pricingProfile: activeProvider.pricingProfile
  };
}

function getActiveProvider(settings) {
  return settings?.newApi?.providers?.find?.((provider) => provider.id === settings.newApi.activeProviderId);
}

export function createNewApiProviderDraft(settings) {
  const index = Number(settings?.newApi?.providers?.length ?? 0) + 1;
  return normalizeProvider({
    id: createProviderId(),
    displayName: `供应商 ${index}`,
    baseUrl: '',
    apiKey: '',
    accessToken: '',
    newApiUser: '',
    pricingProfile: {
      ...settings.pricingProfile,
      id: createProviderId('pricing'),
      name: `供应商 ${index} 估算`
    }
  }, settings.newApi, settings.pricingProfile);
}

export function duplicateNewApiProvider(settings, providerId) {
  const source = settings.newApi.providers.find((provider) => provider.id === providerId);
  if (!source) return settings;
  return upsertNewApiProvider(settings, {
    ...source,
    id: createProviderId(),
    displayName: `${source.displayName} 副本`,
    pricingProfile: {
      ...source.pricingProfile,
      id: createProviderId('pricing'),
      name: `${source.displayName} 副本估算`
    }
  });
}

export function applyLocalRecharge(settings, value) {
  const amount = Number(value);
  if (!Number.isFinite(amount) || amount <= 0) {
    return settings;
  }

  const currentBalance = numberOrZero(settings?.pricingProfile?.initialBalance);
  const currentTotal = numberOrZero(settings?.pricingProfile?.totalRecharged, currentBalance);

  return {
    ...settings,
    pricingProfile: {
      ...settings.pricingProfile,
      initialBalance: roundCurrency(currentBalance + amount),
      totalRecharged: roundCurrency(currentTotal + amount)
    }
  };
}

export function updateNewApiSettings(settings, key, value) {
  const normalizedValue =
    key === 'refreshIntervalSeconds'
      ? coerceEditableInteger(value)
      : key === 'codexTokenPollIntervalSeconds'
        ? coerceEditableInteger(value)
      : key === 'platformSyncIntervalSeconds'
        ? coerceEditableInteger(value)
      : key === 'accountRefreshIntervalSeconds'
        ? coerceEditableInteger(value)
      : key === 'topupRefreshIntervalSeconds'
        ? coerceEditableInteger(value)
      : key === 'spendToastSeconds'
        ? coerceEditableInteger(value)
      : key === 'apiKey'
        ? normalizeApiKey(value)
      : key === 'accessToken'
        ? normalizeBearerToken(value)
        : value;
  const newApi = {
    ...settings.newApi,
    [key]: normalizedValue
  };
  const providerKeys = new Set(['displayName', 'baseUrl', 'apiKey', 'accessToken', 'newApiUser']);
  const providers = providerKeys.has(key)
    ? settings.newApi.providers.map((provider) =>
        provider.id === settings.newApi.activeProviderId
          ? {
              ...provider,
              [key]: normalizedValue,
              displayName: key === 'displayName' ? normalizedValue : provider.displayName
            }
          : provider
      )
    : settings.newApi.providers;
  return {
    ...settings,
    newApi: {
      ...newApi,
      providers
    }
  };
}

export function updateCapsulePosition(settings, position) {
  return {
    ...settings,
    window: {
      ...settings.window,
      capsulePosition: normalizeCapsulePosition(position)
    }
  };
}

function normalizeApiKey(value) {
  const text = String(value ?? '').trim();
  const bearerMatch = text.match(/Bearer\s+([A-Za-z0-9._-]*sk-[A-Za-z0-9._-]+|sk-[A-Za-z0-9._-]+)/i);
  if (bearerMatch?.[1]) return bearerMatch[1].trim();
  const keyMatch = text.match(/\bsk-[A-Za-z0-9._-]+/);
  if (keyMatch?.[0]) return keyMatch[0].trim();
  return text;
}

function normalizeBearerToken(value) {
  const text = String(value ?? '').trim();
  const bearerMatch = text.match(/\bBearer\s+([^\s,]+)/i);
  if (bearerMatch?.[1]) return bearerMatch[1].trim();
  const authorizationMatch = text.match(/^Authorization\s*:\s*([^\s,]+)/i);
  if (authorizationMatch?.[1]) return authorizationMatch[1].trim();
  return text.split(/\s+/)[0] ?? '';
}

function normalizeProviders(value, baseNewApi, basePricingProfile) {
  const list = Array.isArray(value) ? value : [];
  const providers = list
    .filter(isObject)
    .map((provider) => normalizeProvider(provider, baseNewApi, basePricingProfile));
  if (providers.length > 0) return dedupeProviders(providers);
  return [
    normalizeProvider({
      id: 'default-new-api',
      displayName: baseNewApi.displayName,
      baseUrl: baseNewApi.baseUrl,
      apiKey: baseNewApi.apiKey,
      accessToken: baseNewApi.accessToken,
      newApiUser: baseNewApi.newApiUser,
      pricingProfile: basePricingProfile
    }, baseNewApi, basePricingProfile)
  ];
}

function applyTopLevelProviderOverrides(providers, activeProviderId, rawNewApi, baseNewApi) {
  const keys = ['displayName', 'baseUrl', 'apiKey', 'accessToken', 'newApiUser'];
  const overrides = {};
  for (const key of keys) {
    if (Object.prototype.hasOwnProperty.call(rawNewApi, key)) {
      overrides[key] = baseNewApi[key];
    }
  }
  if (Object.keys(overrides).length === 0) return providers;
  return providers.map((provider) =>
    provider.id === activeProviderId ? { ...provider, ...overrides } : provider
  );
}

function dedupeProviders(providers) {
  const seen = new Set();
  return providers.map((provider) => {
    let id = provider.id || createProviderId();
    while (seen.has(id)) {
      id = createProviderId();
    }
    seen.add(id);
    return { ...provider, id };
  });
}

function normalizeProvider(provider, baseNewApi, basePricingProfile) {
  const pricingProfile = normalizePricingProfile({
    ...DEFAULT_APP_SETTINGS.pricingProfile,
    ...basePricingProfile,
    ...(isObject(provider?.pricingProfile) ? provider.pricingProfile : {}),
    id: stringOrFallback(provider?.pricingProfile?.id, createProviderId('pricing')),
    name: stringOrFallback(provider?.pricingProfile?.name, `${stringOrFallback(provider?.displayName, 'New API')} 估算`)
  });
  return {
    id: stringOrFallback(provider?.id, createProviderId()),
    displayName: stringOrFallback(provider?.displayName ?? provider?.name, baseNewApi.displayName),
    baseUrl: String(provider?.baseUrl ?? baseNewApi.baseUrl ?? ''),
    apiKey: normalizeApiKey(provider?.apiKey ?? baseNewApi.apiKey ?? ''),
    accessToken: normalizeBearerToken(provider?.accessToken ?? baseNewApi.accessToken ?? ''),
    newApiUser: String(provider?.newApiUser ?? baseNewApi.newApiUser ?? ''),
    pricingProfile
  };
}

function normalizeProviderBaseUrl(value) {
  let text = String(value ?? '').trim().replace(/\/+$/, '');
  text = text.replace(/\/v1$/i, '');
  return text.toLowerCase();
}

function keyFingerprint(value) {
  const text = normalizeApiKey(value);
  if (!text) return '';
  let hash = 0x811c9dc5;
  for (let index = 0; index < text.length; index += 1) {
    hash ^= text.charCodeAt(index);
    hash = Math.imul(hash, 0x01000193);
  }
  return `fnv1a:${(hash >>> 0).toString(16).padStart(8, '0')}`;
}

function normalizeActiveProviderId(value, providers) {
  const id = String(value ?? '');
  if (providers.some((provider) => provider.id === id)) return id;
  return providers[0]?.id ?? 'default-new-api';
}

function createProviderId(prefix = 'provider') {
  return `${prefix}-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 8)}`;
}

function stringOrFallback(value, fallback) {
  const text = String(value ?? '').trim();
  return text || fallback;
}

function toPersistedSettings(settings) {
  const merged = mergeAppSettings(settings);

  return {
    ...merged,
    newApi: merged.newApi,
    window: merged.window
  };
}

function cloneDefaultSettings() {
  return mergeAppSettings({});
}

function coerceNumber(value, fallback) {
  if (value === '') return '';
  if (typeof value === 'string' && value === '.') return value;
  if (typeof value === 'string' && /^\d+\.$/.test(value)) return value;
  const number = Number(value);
  return Number.isFinite(number) ? number : fallback;
}

function coerceEditableInteger(value) {
  const text = String(value ?? '').replace(/[^\d]/g, '');
  if (text === '') return '';
  const number = Number(text);
  return Number.isFinite(number) ? number : '';
}

function numberOrZero(value, fallback = 0) {
  const number = Number(value);
  return Number.isFinite(number) ? number : fallback;
}

function roundCurrency(value) {
  return Math.round(value * 100) / 100;
}

function normalizeCapsulePosition(position) {
  if (!isObject(position)) return DEFAULT_APP_SETTINGS.window.capsulePosition;
  const x = Number(position.x);
  const y = Number(position.y);
  return {
    x: Number.isFinite(x) ? Math.round(x) : DEFAULT_APP_SETTINGS.window.capsulePosition.x,
    y: Number.isFinite(y) ? Math.round(y) : DEFAULT_APP_SETTINGS.window.capsulePosition.y
  };
}

function isObject(value) {
  return value !== null && typeof value === 'object' && !Array.isArray(value);
}

function browserStorage() {
  return typeof window !== 'undefined' ? window.localStorage : undefined;
}
