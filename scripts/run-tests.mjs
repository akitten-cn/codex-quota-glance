import { spawnSync } from 'node:child_process';

const nodeTests = [
  'tests/usage-parser.test.mjs',
  'tests/pricing.test.mjs',
  'tests/new-api-log-sync.test.mjs',
  'tests/display.test.mjs',
  'tests/quota-amount.test.mjs',
  'tests/spend-events.test.mjs',
  'tests/codex-token-ingestion.test.mjs',
  'tests/codex-rate-limit-windows.test.mjs',
  'tests/codex-runtime-state.test.mjs',
  'tests/codex-activity-behavior.test.mjs',
  'tests/codex-overview-store.test.mjs',
  'tests/refresh-coordinator.test.mjs',
  'tests/spend-toast-source.test.mjs',
  'tests/floating-capsule-source.test.mjs',
  'tests/draggable-capsule-source.test.mjs',
  'tests/app-sync-toast-source.test.mjs',
  'tests/app-fast-token-toast-source.test.mjs',
  'tests/app-official-token-source.test.mjs',
  'tests/app-codex-status-source.test.mjs',
  'tests/about-update-source.test.mjs',
  'tests/capsule-stability-source.test.mjs',
  'tests/codex-activity-source.test.mjs',
  'tests/update-window-source.test.mjs',
  'tests/quota-recovery-dialog-source.test.mjs',
  'tests/local-backend-source.test.mjs',
  'tests/detail-panel-source.test.mjs',
  'tests/style.test.mjs',
  'tests/desktop-launcher.test.mjs',
  'tests/settings-store.test.mjs',
  'tests/update-checker.test.mjs',
  'tests/validation.test.mjs',
  'tests/new-api-client.test.mjs',
  'tests/local-log-store.test.mjs',
  'tests/codex-status-store.test.mjs',
  'tests/snapshot-factory.test.mjs',
  'tests/settings-page-copy.test.mjs',
  'tests/release-notes-script.test.mjs'
];

for (const test of nodeTests) {
  run(process.execPath, [test]);
}

console.log('all tests passed');

function run(command, args) {
  const result = spawnSync(command, args, {
    stdio: 'inherit'
  });
  if (result.status !== 0) {
    process.exit(result.status ?? 1);
  }
}
