import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

const electronMain = readFileSync(new URL('../electron/main.cjs', import.meta.url), 'utf8');
const electronPreload = readFileSync(new URL('../electron/preload.cjs', import.meta.url), 'utf8');
const backend = readFileSync(new URL('../electron/local-backend.cjs', import.meta.url), 'utf8');
const packageJson = JSON.parse(readFileSync(new URL('../package.json', import.meta.url), 'utf8'));
const readme = readFileSync(new URL('../README.md', import.meta.url), 'utf8');
const capsuleWindowSource = electronMain.slice(
  electronMain.indexOf('async function createCapsuleWindow()'),
  electronMain.indexOf('async function ensureDetailWindow()')
);
const detailWindowSource = electronMain.slice(
  electronMain.indexOf('async function ensureDetailWindow()'),
  electronMain.indexOf('async function openSettingsWindow()')
);

assert.equal(packageJson.main, 'electron/main.cjs');
assert.match(packageJson.scripts.electron, /electron \./);
assert.equal(packageJson.scripts.dist, 'pnpm run tauri:build');
assert.equal(packageJson.scripts['dist:win'], 'pnpm run tauri:build');
assert.equal(packageJson.scripts['dist:electron'], 'node scripts/clean-generated.mjs && vite build && electron-builder --win');
assert.equal(packageJson.scripts['dist:win:electron'], 'node scripts/clean-generated.mjs && vite build && electron-builder --win');
assert.equal(packageJson.build.asar, true);
assert.equal(packageJson.scripts.desktop, undefined);
assert.equal(packageJson.scripts['package-electron'], undefined);
assert.equal(packageJson.scripts['package-desktop'], undefined);
assert.equal(packageJson.scripts['install-shortcut'], undefined);
assert.equal(packageJson.build.directories.output, 'dist-electron');

assert.match(electronMain, /BrowserWindow/);
assert.match(electronMain, /Tray/);
assert.match(electronMain, /setAppUserModelId\(APP_USER_MODEL_ID\)/);
assert.match(electronMain, /\\u663e\\u793a\/\\u9690\\u85cf\\u80f6\\u56ca/);
assert.match(electronMain, /icon\.ico/);
assert.match(electronMain, /frame:\s*false/);
assert.match(electronMain, /transparent:\s*true/);
assert.match(electronMain, /skipTaskbar:\s*true/);
assert.match(electronMain, /alwaysOnTop:\s*true/);
assert.match(electronMain, /openSettingsWindow/);
assert.match(electronMain, /desktop=1/);
assert.match(electronMain, /DEFAULT_CAPSULE_SIZE\s*=\s*\{\s*width:\s*620,\s*height:\s*72\s*\}/);
assert.match(electronMain, /DEFAULT_DETAIL_SIZE/);
assert.match(electronMain, /MIN_DETAIL_SIZE/);
assert.match(electronMain, /WINDOW_MARGIN/);
assert.match(electronMain, /desktop-detail-open/);
assert.match(electronMain, /desktop-layout-update/);
assert.match(electronMain, /desktop-detail-layout-update/);
assert.match(electronMain, /desktop-toast-open/);
assert.match(electronMain, /RESERVED_TOAST_SPACE/);
assert.match(electronMain, /setCapsuleWindowBounds/);
assert.match(electronPreload, /setToastOpen/);
assert.match(electronMain, /desktop-popover-placement/);
assert.match(electronMain, /resizeCapsuleWindow/);
assert.match(electronMain, /normalizeLayout/);
assert.match(electronMain, /ensureDetailWindow/);
assert.match(electronMain, /showDetailWindow/);
assert.match(electronMain, /hideDetailWindow/);
assert.match(electronMain, /positionDetailWindow/);
assert.match(electronMain, /waitForDetailReadiness/);
assert.match(electronMain, /detailContentReady/);
assert.match(electronMain, /detailLayoutReady/);
assert.match(electronMain, /setIgnoreMouseEvents\(true\)/);
assert.match(electronMain, /setIgnoreMouseEvents\(false\)/);
assert.match(electronMain, /setOpacity\(0\)/);
assert.match(electronMain, /setOpacity\(1\)/);
assert.doesNotMatch(electronMain, /detailWindow\.hide\(\)/);
assert.doesNotMatch(capsuleWindowSource, /opacity:\s*0/);
assert.match(detailWindowSource, /opacity:\s*0/);
assert.match(electronMain, /\?view=detail/);
assert.match(electronMain, /detailWindow\.setBounds\(\{\s*x,\s*y,\s*width:\s*detailSize\.width,\s*height:\s*detailSize\.height\s*\}/);
assert.match(electronMain, /detail:\s*\{\s*width:\s*0,\s*height:\s*0\s*\}/);
assert.doesNotMatch(electronMain, /const detail = normalizeRect\(layout\?\.detail,\s*DEFAULT_DETAIL_SIZE\)/);
assert.match(electronMain, /capsuleAnchor/);
assert.doesNotMatch(electronMain, /pendingBoundsCommit/);
assert.doesNotMatch(electronMain, /desktop-window-layout-applied/);
assert.doesNotMatch(electronMain, /shouldWaitForLayoutBeforeBounds/);
assert.doesNotMatch(electronMain, /shouldCommitBoundsBeforeLayout/);
assert.doesNotMatch(electronMain, /hideCapsule/);
assert.match(electronMain, /keepFloatingWindowOnTop/);
assert.match(electronMain, /moveTop\(\)/);
assert.doesNotMatch(electronMain, /setAlwaysOnTop\(false\)/);
assert.doesNotMatch(electronMain, /startCapsuleVisibilityGuard/);
assert.doesNotMatch(electronMain, /restoreCapsuleAfterDisplayChange/);
assert.doesNotMatch(electronMain, /isDisplayFullscreenForCapsule/);
assert.doesNotMatch(electronMain, /screen\.on\('display-metrics-changed'/);
assert.match(electronMain, /recoverCapsuleWindow/);
assert.match(electronMain, /desktop-hit-test-regions/);
assert.match(electronMain, /createTrayImage/);
assert.match(electronMain, /startLocalBackend/);
assert.match(electronMain, /app\.getAppPath\(\)/);
assert.match(electronMain, /local-api\/health/);
assert.match(electronMain, /electron-local-backend/);
assert.match(backend, /startLocalBackend/);
assert.match(backend, /CodexQuotaGlance/);
assert.match(electronPreload, /updateDetailLayout/);
assert.doesNotMatch(electronMain, /CAPSULE_EXPANDED_SIZE/);
assert.doesNotMatch(electronMain, /local-server\.py/);
assert.doesNotMatch(electronMain, /local-server\.exe/);
assert.doesNotMatch(readme, /npm run package-electron/);
assert.match(readme, /npm run dist:win/);
assert.match(readme, /src-tauri\\target\\release\\bundle\\nsis\\.*\.exe/);

console.log('desktop launcher tests passed');
