import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

const backendSource = readFileSync(new URL('../electron/local-backend.cjs', import.meta.url), 'utf8');
const mainSource = readFileSync(new URL('../electron/main.cjs', import.meta.url), 'utf8');
const packageJson = JSON.parse(readFileSync(new URL('../package.json', import.meta.url), 'utf8'));

assert.match(backendSource, /require\('node:sqlite'\)/);
assert.match(backendSource, /create table if not exists newapi_logs/);
assert.match(backendSource, /create table if not exists codex_token_events/);
assert.match(backendSource, /\/local-api\/health/);
assert.match(backendSource, /electron-local-backend/);
assert.match(backendSource, /\/local-api\/newapi\/logs\/summary/);
assert.match(backendSource, /\/local-api\/newapi\/logs\/sync/);
assert.match(backendSource, /\/local-api\/codex\/token\/latest/);
assert.match(backendSource, /\/local-api\/codex\/status/);
assert.match(backendSource, /\/local-api\/update\/latest/);
assert.match(backendSource, /GITHUB_LATEST_RELEASE_URL/);
assert.match(backendSource, /Authorization: `Bearer/);
assert.match(backendSource, /New-Api-User/);
assert.match(backendSource, /account\/rateLimits\/read/);
assert.match(mainSource, /startLocalBackend/);
assert.match(mainSource, /stopLocalBackend/);
assert.doesNotMatch(mainSource, /local-server\.py/);
assert.doesNotMatch(mainSource, /local-server\.exe/);
assert.doesNotMatch(mainSource, /spawn\(backend\.command/);
assert.equal(packageJson.scripts['dist:win:electron'], 'node scripts/clean-generated.mjs && vite build && electron-builder --win');
assert.equal(packageJson.scripts['build:sidecar'], undefined);
assert.equal(packageJson.build.files.includes('local-server.exe'), false);

console.log('local backend source tests passed');
