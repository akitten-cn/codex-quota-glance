import assert from 'node:assert/strict';
import { existsSync, readFileSync, rmSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

const output = join(tmpdir(), `codex-quota-glance-release-notes-${Date.now()}.md`);
const script = fileURLToPath(new URL('../scripts/extract-release-notes.mjs', import.meta.url));
const cwd = fileURLToPath(new URL('..', import.meta.url));
if (existsSync(output)) rmSync(output);

execFileSync(process.execPath, [
  script,
  'v0.1.4',
  output
], {
  cwd,
  stdio: 'pipe'
});

const body = readFileSync(output, 'utf8');
assert.match(body, /Reworked the desktop detail card/);
assert.match(body, /local Codex token summary estimation/);
assert.doesNotMatch(body, /## 0\.1\.3/);
rmSync(output);

execFileSync(process.execPath, [
  script,
  'v0.1.13',
  output
], {
  cwd,
  stdio: 'pipe'
});

const chineseBody = readFileSync(output, 'utf8');
assert.match(chineseBody, /胶囊仍重复显示一条相同 5h 额度/);
assert.match(chineseBody, /正确归类为 7d/);
assert.doesNotMatch(chineseBody, /## 0\.1\.12/);
rmSync(output);

console.log('release notes script tests passed');
