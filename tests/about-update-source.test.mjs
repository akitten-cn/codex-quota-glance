import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

const appSource = readFileSync(new URL('../src/App.tsx', import.meta.url), 'utf8');
const settingsSource = readFileSync(new URL('../src/components/SettingsPage.tsx', import.meta.url), 'utf8');
const capsuleSource = readFileSync(new URL('../src/components/FloatingCapsule.tsx', import.meta.url), 'utf8');
const cssSource = readFileSync(new URL('../src/styles.css', import.meta.url), 'utf8');

assert.match(appSource, /import\s+\{\s*APP_VERSION,\s*checkLatestRelease,\s*GITHUB_RELEASES_URL\s*\}\s+from\s+'\.\/lib\/updateChecker\.mjs';/);
assert.match(appSource, /updateReminderDismissed/);
assert.match(appSource, /runUpdateCheck/);
assert.match(appSource, /checkLatestRelease\(\{\s*currentVersion:\s*APP_VERSION\s*\}\)/);
assert.match(appSource, /role="dialog"/);
assert.match(appSource, /setUpdateReminderDismissed\(true\)/);
assert.doesNotMatch(appSource, /window\.alert/);
assert.match(appSource, /updateCheckState=\{updateCheckState\}/g);
assert.match(appSource, /onCheckUpdate=\{\(\) => runUpdateCheck\(\)\}/g);
assert.match(appSource, /onOpenUpdateWindow=\{\(options\)\s*=>\s*window\.codexQuotaDesktop\?\.openUpdateWindow\?\.\(options\)\}/);
assert.match(appSource, /if \(isDetailWindow\) return;/);
assert.match(appSource, /!isDesktopCapsule && \(\s*<UpdateReminder/);
assert.match(appSource, /updateAvailable=\{Boolean\(updateCheckState\.isNewer\)\}/);

assert.match(settingsSource, /type SettingsTab = 'api' \| 'sync' \| 'about';/);
assert.match(settingsSource, /关于\/更新/);
assert.match(settingsSource, /AboutUpdateSection/);
assert.match(settingsSource, /GITHUB_REPOSITORY_URL/);
assert.match(settingsSource, /GITHUB_RELEASES_URL/);
assert.match(settingsSource, /formatUpdateStatus/);
assert.match(settingsSource, /检查更新/);
assert.match(settingsSource, /onOpenUpdateWindow/);
assert.match(settingsSource, /onOpenUpdateWindow\(\{\s*autoDownload:\s*true\s*\}\)/);
assert.match(settingsSource, /updateCheckState\.isNewer/);
assert.match(settingsSource, /update-tab-badge/);
assert.match(settingsSource, /settings-update-notice/);

assert.match(capsuleSource, /updateAvailable\?:\s*boolean/);
assert.match(capsuleSource, /capsule-update-badge/);
assert.doesNotMatch(capsuleSource, /capsule-update-badge[\s\S]*鈫/);
assert.match(cssSource, /\.capsule-update-badge/);
assert.match(cssSource, /\.capsule-update-badge\s*{[\s\S]*width:\s*8px;/);
assert.match(cssSource, /\.capsule-update-badge\s*{[\s\S]*height:\s*8px;/);
assert.match(cssSource, /\.capsule-update-badge\s*{[\s\S]*background:\s*#ff9f0a;/);
assert.match(cssSource, /\.update-tab-badge/);
assert.match(cssSource, /\.settings-update-notice/);

console.log('about update source tests passed');
