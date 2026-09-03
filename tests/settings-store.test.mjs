import assert from 'node:assert/strict';
import {
  DEFAULT_APP_SETTINGS,
  loadAppSettings,
  saveAppSettings,
  upsertNewApiProvider,
  selectNewApiProvider,
  deleteNewApiProvider,
  updateCapsulePosition,
  updateNewApiSettings,
  updatePricingProfile,
  selectProviderForCodexStatus
} from '../src/lib/settingsStore.mjs';

const emptyStorage = createMemoryStorage();
assert.deepEqual(loadAppSettings(emptyStorage), DEFAULT_APP_SETTINGS);
assert.equal(DEFAULT_APP_SETTINGS.pricingProfile.initialBalance, 0);
assert.equal(DEFAULT_APP_SETTINGS.pricingProfile.totalRecharged, 0);
assert.deepEqual(DEFAULT_APP_SETTINGS.window.capsulePosition, { x: 28, y: 28 });

const existingStorage = createMemoryStorage({
  codexQuotaGlanceSettings: JSON.stringify({
    newApi: {
      displayName: 'Prod API',
      baseUrl: 'https://new-api.example.test'
    },
    pricingProfile: {
      initialBalance: 88,
      modelRatio: 1.7
    },
    window: {
      capsulePosition: {
        x: 120.6,
        y: 42.2
      }
    }
  })
});

const loaded = loadAppSettings(existingStorage);
assert.equal(loaded.newApi.displayName, 'Prod API');
assert.equal(loaded.newApi.baseUrl, 'https://new-api.example.test');
assert.equal(loaded.newApi.newApiUser, '1');
assert.equal(loaded.newApi.amountDisplayMode, 'cny');
assert.equal(loaded.newApi.refreshIntervalSeconds, 30);
assert.equal(loaded.newApi.codexTokenPollIntervalSeconds, 5);
assert.equal(loaded.newApi.accountRefreshIntervalSeconds, 300);
assert.equal(loaded.newApi.topupRefreshIntervalSeconds, 600);
assert.equal(loaded.newApi.spendToastSeconds, 5);
assert.equal(loaded.pricingProfile.cnyPerUsd, 7.2);
assert.equal(loaded.pricingProfile.initialBalance, 88);
assert.equal(loaded.pricingProfile.modelRatio, 1.7);
assert.equal(loaded.pricingProfile.outputPricePerMillion, 8);
assert.deepEqual(loaded.window.capsulePosition, { x: 121, y: 42 });

const movedCapsule = updateCapsulePosition(loaded, { x: 333.4, y: 88.8 });
assert.deepEqual(movedCapsule.window.capsulePosition, { x: 333, y: 89 });
assert.deepEqual(loaded.window.capsulePosition, { x: 121, y: 42 });

const invalidCapsulePosition = updateCapsulePosition(loaded, { x: 'bad', y: -12 });
assert.deepEqual(invalidCapsulePosition.window.capsulePosition, { x: 28, y: -12 });

const pollutedKeyStorage = createMemoryStorage({
  codexQuotaGlanceSettings: JSON.stringify({
    newApi: {
      apiKey: 'Authorization: Bearer sk-clean\\nNew-Api-User: 5781'
    }
  })
});
const cleanedLoadedKey = loadAppSettings(pollutedKeyStorage);
assert.equal(cleanedLoadedKey.newApi.apiKey, 'sk-clean');
assert.equal(cleanedLoadedKey.newApi.accessToken, '');

const legacyAccessTokenStorage = createMemoryStorage({
  codexQuotaGlanceSettings: JSON.stringify({
    newApi: {
      apiKey: 'fakeAccessTokenWithPadding='
    }
  })
});
const migratedAccessToken = loadAppSettings(legacyAccessTokenStorage);
assert.equal(migratedAccessToken.newApi.apiKey, '');
assert.equal(migratedAccessToken.newApi.accessToken, 'fakeAccessTokenWithPadding=');

const legacyDefaultStorage = createMemoryStorage({
  codexQuotaGlanceSettings: JSON.stringify({
    pricingProfile: {
      initialBalance: 120,
      totalRecharged: 120
    }
  })
});
const migratedLegacyDefault = loadAppSettings(legacyDefaultStorage);
assert.equal(migratedLegacyDefault.pricingProfile.initialBalance, 0);
assert.equal(migratedLegacyDefault.pricingProfile.totalRecharged, 0);

const previousDefaultStorage = createMemoryStorage({
  codexQuotaGlanceSettings: JSON.stringify({
    pricingProfile: {
      initialBalance: 90,
      totalRecharged: 90
    }
  })
});
const migratedPreviousDefault = loadAppSettings(previousDefaultStorage);
assert.equal(migratedPreviousDefault.pricingProfile.initialBalance, 0);
assert.equal(migratedPreviousDefault.pricingProfile.totalRecharged, 0);

const customBalanceStorage = createMemoryStorage({
  codexQuotaGlanceSettings: JSON.stringify({
    pricingProfile: {
      initialBalance: 120,
      totalRecharged: 150
    }
  })
});
const preservedCustomBalance = loadAppSettings(customBalanceStorage);
assert.equal(preservedCustomBalance.pricingProfile.initialBalance, 120);
assert.equal(preservedCustomBalance.pricingProfile.totalRecharged, 150);

const zeroTotalStorage = createMemoryStorage({
  codexQuotaGlanceSettings: JSON.stringify({
    pricingProfile: {
      initialBalance: 90,
      totalRecharged: 0
    }
  })
});
const normalizedZeroTotal = loadAppSettings(zeroTotalStorage);
assert.equal(normalizedZeroTotal.pricingProfile.initialBalance, 90);
assert.equal(normalizedZeroTotal.pricingProfile.totalRecharged, 0);

const updated = updatePricingProfile(loaded, 'outputPricePerMillion', '12.5');
assert.equal(updated.pricingProfile.outputPricePerMillion, 12.5);
assert.equal(loaded.pricingProfile.outputPricePerMillion, 8);
const updatedRate = updatePricingProfile(loaded, 'cnyPerUsd', '7.35');
assert.equal(updatedRate.pricingProfile.cnyPerUsd, 7.35);
const partialDecimalRate = updatePricingProfile(loaded, 'cnyPerUsd', '7.');
assert.equal(partialDecimalRate.pricingProfile.cnyPerUsd, '7.');
const partialLeadingDecimal = updatePricingProfile(loaded, 'inputPricePerMillion', '0.');
assert.equal(partialLeadingDecimal.pricingProfile.inputPricePerMillion, '0.');

const fasterRefresh = updateNewApiSettings(loaded, 'refreshIntervalSeconds', '12');
assert.equal(fasterRefresh.newApi.refreshIntervalSeconds, 12);
const tooFastRefresh = updateNewApiSettings(loaded, 'refreshIntervalSeconds', '1');
assert.equal(tooFastRefresh.newApi.refreshIntervalSeconds, 1);
const emptyRefresh = updateNewApiSettings(loaded, 'refreshIntervalSeconds', '');
assert.equal(emptyRefresh.newApi.refreshIntervalSeconds, '');
const fasterCodexPoll = updateNewApiSettings(loaded, 'codexTokenPollIntervalSeconds', '1');
assert.equal(fasterCodexPoll.newApi.codexTokenPollIntervalSeconds, 1);
const tooSlowCodexPoll = updateNewApiSettings(loaded, 'codexTokenPollIntervalSeconds', '999');
assert.equal(tooSlowCodexPoll.newApi.codexTokenPollIntervalSeconds, 999);
const customPlatformSync = updateNewApiSettings(loaded, 'platformSyncIntervalSeconds', '180');
assert.equal(customPlatformSync.newApi.platformSyncIntervalSeconds, 180);
const tooFastPlatformSync = updateNewApiSettings(loaded, 'platformSyncIntervalSeconds', '30');
assert.equal(tooFastPlatformSync.newApi.platformSyncIntervalSeconds, 30);
const customAccountRefresh = updateNewApiSettings(loaded, 'accountRefreshIntervalSeconds', '900');
assert.equal(customAccountRefresh.newApi.accountRefreshIntervalSeconds, 900);
const tooFastAccountRefresh = updateNewApiSettings(loaded, 'accountRefreshIntervalSeconds', '30');
assert.equal(tooFastAccountRefresh.newApi.accountRefreshIntervalSeconds, 30);
const customTopupRefresh = updateNewApiSettings(loaded, 'topupRefreshIntervalSeconds', '1800');
assert.equal(customTopupRefresh.newApi.topupRefreshIntervalSeconds, 1800);
const tooFastTopupRefresh = updateNewApiSettings(loaded, 'topupRefreshIntervalSeconds', '30');
assert.equal(tooFastTopupRefresh.newApi.topupRefreshIntervalSeconds, 30);
const longerToast = updateNewApiSettings(loaded, 'spendToastSeconds', '12');
assert.equal(longerToast.newApi.spendToastSeconds, 12);
const tooLongToast = updateNewApiSettings(loaded, 'spendToastSeconds', '999');
assert.equal(tooLongToast.newApi.spendToastSeconds, 999);
const normalizedBearerKey = updateNewApiSettings(loaded, 'apiKey', 'Authorization: Bearer sk-test\\nNew-Api-User: 5781');
assert.equal(normalizedBearerKey.newApi.apiKey, 'sk-test');
const normalizedPlainKey = updateNewApiSettings(loaded, 'apiKey', '  sk-plain-key  ');
assert.equal(normalizedPlainKey.newApi.apiKey, 'sk-plain-key');
const normalizedAccessToken = updateNewApiSettings(loaded, 'accessToken', 'Authorization: Bearer acct-tok\nNew-Api-User: 5781');
assert.equal(normalizedAccessToken.newApi.accessToken, 'acct-tok');
const normalizedBase64AccessToken = updateNewApiSettings(loaded, 'accessToken', 'Authorization: Bearer tok=');
assert.equal(normalizedBase64AccessToken.newApi.accessToken, 'tok=');
const normalizedModernAccessToken = updateNewApiSettings(
  loaded,
  'accessToken',
  'Authorization: Bearer AbC-12_./+xy==\nNew-Api-User: 5781'
);
assert.equal(normalizedModernAccessToken.newApi.accessToken, 'AbC-12_./+xy==');

const withSecret = {
  ...updated,
  newApi: {
    ...updated.newApi,
    apiKey: 'sk-real-secret',
    accessToken: 'acct-tok'
  }
};

saveAppSettings(existingStorage, withSecret);
const saved = JSON.parse(existingStorage.getItem('codexQuotaGlanceSettings'));
assert.equal(saved.newApi.displayName, 'Prod API');
assert.equal(saved.newApi.apiKey, 'sk-real-secret');
assert.equal(saved.newApi.accessToken, 'acct-tok');
assert.equal(saved.pricingProfile.outputPricePerMillion, 12.5);
assert.deepEqual(saved.window.capsulePosition, loaded.window.capsulePosition);

const withSecondProvider = upsertNewApiProvider(loaded, {
  id: 'provider-2',
  displayName: 'Provider 2',
  baseUrl: 'https://two.example.test',
  apiKey: 'sk-two',
  accessToken: 'token-two',
  newApiUser: '2',
  pricingProfile: {
    ...loaded.pricingProfile,
    id: 'pricing-2',
    name: 'Provider 2 pricing',
    inputPricePerMillion: 9
  }
});
assert.equal(withSecondProvider.newApi.activeProviderId, 'provider-2');
assert.equal(withSecondProvider.newApi.apiKey, 'sk-two');
assert.equal(withSecondProvider.pricingProfile.inputPricePerMillion, 9);

const selectedDefault = selectNewApiProvider(withSecondProvider, 'default-new-api');
assert.equal(selectedDefault.newApi.activeProviderId, 'default-new-api');
assert.equal(selectedDefault.newApi.displayName, 'Prod API');
assert.equal(selectedDefault.pricingProfile.inputPricePerMillion, 2);

const deletedDefault = deleteNewApiProvider(withSecondProvider, 'default-new-api');
assert.equal(deletedDefault.newApi.providers.length, 1);
assert.equal(deletedDefault.newApi.activeProviderId, 'provider-2');
assert.equal(deletedDefault.newApi.baseUrl, 'https://two.example.test');

const autoSelectedProvider = selectProviderForCodexStatus(withSecondProvider, {
  accountType: 'api',
  baseUrl: 'https://www.cctq.ai/v1',
  apiKeyFingerprint: testKeyFingerprint('sk-two')
});
assert.equal(autoSelectedProvider.newApi.activeProviderId, 'provider-2');
assert.equal(autoSelectedProvider.newApi.apiKey, 'sk-two');

const urlOnlySelectedProvider = selectProviderForCodexStatus(withSecondProvider, {
  accountType: 'api',
  baseUrl: 'https://new-api.example.test/v1'
});
assert.equal(urlOnlySelectedProvider.newApi.activeProviderId, 'default-new-api');

const officialStatusDoesNotSwitch = selectProviderForCodexStatus(withSecondProvider, {
  accountType: 'official_login',
  apiKeyFingerprint: testKeyFingerprint('sk-two')
});
assert.equal(officialStatusDoesNotSwitch.newApi.activeProviderId, 'provider-2');

const brokenStorage = createMemoryStorage({
  codexQuotaGlanceSettings: '{bad json'
});
assert.deepEqual(loadAppSettings(brokenStorage), DEFAULT_APP_SETTINGS);

console.log('settings store tests passed');

function createMemoryStorage(initial = {}) {
  const values = new Map(Object.entries(initial));

  return {
    getItem(key) {
      return values.has(key) ? values.get(key) : null;
    },
    setItem(key, value) {
      values.set(key, String(value));
    }
  };
}

function testKeyFingerprint(value) {
  let hash = 0x811c9dc5;
  for (let index = 0; index < value.length; index += 1) {
    hash ^= value.charCodeAt(index);
    hash = Math.imul(hash, 0x01000193);
  }
  return `fnv1a:${(hash >>> 0).toString(16).padStart(8, '0')}`;
}

