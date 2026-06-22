import assert from 'node:assert/strict';
import { existsSync, readFileSync } from 'node:fs';

const root = new URL('..', import.meta.url);
const packageJson = JSON.parse(readFileSync(new URL('package.json', root), 'utf8'));

assert.equal(packageJson.scripts.tauri, 'tauri');
assert.equal(packageJson.scripts['tauri:dev'], 'tauri dev');
assert.equal(packageJson.scripts['tauri:build'], 'tauri build');
assert.ok(packageJson.dependencies['@tauri-apps/api']);
assert.ok(packageJson.devDependencies['@tauri-apps/cli']);

assert.ok(existsSync(new URL('src-tauri/Cargo.toml', root)));
assert.ok(existsSync(new URL('src-tauri/tauri.conf.json', root)));
assert.ok(existsSync(new URL('src-tauri/build.rs', root)));
assert.ok(existsSync(new URL('src-tauri/src/main.rs', root)));
assert.ok(existsSync(new URL('src-tauri/src/lib.rs', root)));
assert.ok(existsSync(new URL('src/lib/desktopBridge.ts', root)));

const tauriConfig = readFileSync(new URL('src-tauri/tauri.conf.json', root), 'utf8');
assert.match(tauriConfig, /"productName"\s*:\s*"Codex Quota Glance"/);
assert.match(tauriConfig, /"beforeDevCommand"\s*:\s*"pnpm dev"/);
assert.match(tauriConfig, /"beforeBuildCommand"\s*:\s*"pnpm build"/);
assert.match(tauriConfig, /"frontendDist"\s*:\s*"\.\.\/dist"/);
assert.match(tauriConfig, /"devUrl"\s*:\s*"http:\/\/localhost:5173"/);
assert.match(tauriConfig, /"label"\s*:\s*"capsule"/);
assert.match(tauriConfig, /"transparent"\s*:\s*true/);
assert.match(tauriConfig, /"decorations"\s*:\s*false/);
assert.match(tauriConfig, /"alwaysOnTop"\s*:\s*true/);

const rustSource = readFileSync(new URL('src-tauri/src/lib.rs', root), 'utf8');
assert.match(rustSource, /desktop_drag_start/);
assert.match(rustSource, /desktop_update_open_window/);
assert.match(rustSource, /desktop_update_download/);
assert.match(rustSource, /trusted_update_asset/);
assert.match(rustSource, /reqwest::Client::new\(\)/);
assert.match(rustSource, /std::process::Command::new/);
assert.match(rustSource, /app\.exit\(0\)/);
assert.doesNotMatch(rustSource, /Tauri 更新下载正在迁移中/);
assert.match(rustSource, /local_api_request/);
assert.match(rustSource, /local_api_update_latest/);
assert.match(rustSource, /get_latest_codex_activity/);
assert.match(rustSource, /parse_codex_activity/);
assert.match(rustSource, /find_latest_codex_session_file/);
assert.match(rustSource, /function_call_needs_user/);
assert.match(rustSource, /get_latest_codex_token_usage/);
assert.match(rustSource, /read_latest_codex_token_event/);
assert.match(rustSource, /normalize_codex_rate_limits/);
assert.match(rustSource, /get_codex_token_summary/);
assert.match(rustSource, /summarize_codex_token_rows/);
assert.doesNotMatch(rustSource, /"GET", "\/local-api\/codex\/token\/latest"\) => json!\(\{\s*"ok": true,\s*"available": false\s*\}\)/s);
assert.match(rustSource, /TrayIconBuilder::with_id/);
assert.match(rustSource, /build_tray_menu/);
assert.match(rustSource, /toggle_capsule_window/);
assert.match(rustSource, /desktop_open_settings/);
assert.match(rustSource, /tauri::generate_handler!/);

const bridgeSource = readFileSync(new URL('src/lib/desktopBridge.ts', root), 'utf8');
assert.match(bridgeSource, /@tauri-apps\/api\/core/);
assert.match(bridgeSource, /window\.codexQuotaDesktop/);
assert.match(bridgeSource, /desktop_drag_start/);
assert.match(bridgeSource, /desktop_update_download/);
assert.match(bridgeSource, /installTauriLocalApiFetchBridge/);
assert.match(bridgeSource, /\/local-api\//);

const mainSource = readFileSync(new URL('src/main.tsx', root), 'utf8');
assert.match(mainSource, /import '\.\/lib\/desktopBridge';/);

console.log('tauri migration source tests passed');
