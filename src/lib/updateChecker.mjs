export const APP_VERSION = '0.1.12';
export const GITHUB_REPOSITORY_URL = 'https://github.com/akitten-cn/codex-quota-glance';
export const GITHUB_RELEASES_URL = `${GITHUB_REPOSITORY_URL}/releases`;
export const GITHUB_LATEST_RELEASE_API_URL =
  'https://api.github.com/repos/akitten-cn/codex-quota-glance/releases/latest';
export const LOCAL_LATEST_RELEASE_API_URL = '/local-api/update/latest';

export function normalizeVersionTag(value) {
  return String(value ?? '').trim().replace(/^v/i, '');
}

export function parseVersion(value) {
  const normalized = normalizeVersionTag(value);
  const match = normalized.match(/^(\d+)\.(\d+)\.(\d+)(?:[-+].*)?$/);
  if (!match) return null;
  return match.slice(1, 4).map((part) => Number(part));
}

export function compareVersions(currentVersion, latestTagName) {
  const current = parseVersion(currentVersion);
  const latest = parseVersion(latestTagName);
  if (!current || !latest) {
    return {
      comparable: false,
      isNewer: false
    };
  }

  for (let index = 0; index < 3; index += 1) {
    if (latest[index] > current[index]) {
      return {
        comparable: true,
        isNewer: true
      };
    }
    if (latest[index] < current[index]) {
      return {
        comparable: true,
        isNewer: false
      };
    }
  }

  return {
    comparable: true,
    isNewer: false
  };
}

export function selectWindowsInstallerAsset(assets) {
  if (!Array.isArray(assets)) return undefined;
  const normalized = assets
    .map((asset) => ({
      name: String(asset?.name || ''),
      url: String(asset?.browser_download_url || asset?.url || ''),
      size: Number.isFinite(Number(asset?.size)) ? Number(asset.size) : undefined
    }))
    .filter((asset) => {
      const name = asset.name.toLowerCase();
      return asset.name &&
        asset.url &&
        name.endsWith('.exe') &&
        name.includes('win') &&
        !name.includes('portable') &&
        !name.includes('blockmap');
    });
  return normalized.find((asset) => /win-x64\.exe$/i.test(asset.name)) || normalized[0];
}

export async function checkLatestRelease(options = {}) {
  const {
    currentVersion = APP_VERSION,
    fetchImpl = globalThis.fetch,
    apiUrl = LOCAL_LATEST_RELEASE_API_URL
  } = options;

  if (typeof fetchImpl !== 'function') {
    throw new Error('当前环境不支持 fetch，无法检查更新');
  }

  const release = await fetchReleaseJson(fetchImpl, apiUrl);
  const latestTagName = release?.tag_name;
  if (!latestTagName) {
    throw new Error('GitHub Releases 返回缺少 tag_name');
  }

  const comparison = compareVersions(currentVersion, latestTagName);
  return {
    currentVersion,
    latestTagName,
    releaseUrl: release.html_url || `${GITHUB_RELEASES_URL}/latest`,
    installerAsset: selectWindowsInstallerAsset(release.assets),
    isNewer: comparison.isNewer,
    comparable: comparison.comparable,
    checkedAt: new Date().toISOString()
  };
}

async function fetchReleaseJson(fetchImpl, apiUrl) {
  const response = await fetchImpl(apiUrl, {
    headers: {
      Accept: 'application/json'
    }
  });

  if (!response?.ok) {
    throw new Error(`GitHub Releases 检查失败：HTTP ${response?.status ?? 'unknown'}`);
  }

  const release = await response.json();
  if (release?.ok === false) {
    throw new Error(release.message || 'GitHub Releases 检查失败');
  }
  return release;
}
