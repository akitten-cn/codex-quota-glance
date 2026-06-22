import { readdir, readFile } from 'node:fs/promises';
import { dirname, extname, join, relative, resolve, sep } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const patterns = [
  /sk-[A-Za-z0-9._-]{20,}/i,
  /Bearer\s+sk-[A-Za-z0-9._-]{12,}/i,
  /"accessToken"\s*:\s*"[A-Za-z0-9+/=_-]{12,}"/i,
  /OPENAI_API_KEY\s*=/i,
  /Authorization\s*:\s*Bearer\s+[A-Za-z0-9+/=._-]{20,}/i
];
const excludedDirectories = new Set([
  '.git',
  'node_modules',
  'dist',
  'dist-electron',
  'build',
  'release',
  'release-electron',
  'data',
  'src-tauri/target',
  '__pycache__',
  '.codex-signal-glance-ref',
  'docs/superpowers'
]);
const excludedExtensions = new Set(['.exe', '.dll', '.bin', '.pak', '.dat', '.pyc', '.zip', '.sqlite3', '.ico', '.png']);
const findings = [];

await walk(root);

if (findings.length > 0) {
  for (const finding of findings) console.error(finding);
  throw new Error('Potential secrets found. Remove or mask them before committing.');
}

console.log('Secret scan passed.');

async function walk(directory) {
  const entries = await readdir(directory, { withFileTypes: true });
  for (const entry of entries) {
    const fullPath = join(directory, entry.name);
    const rel = toPosix(relative(root, fullPath));
    if (isExcluded(rel, entry)) continue;
    if (entry.isDirectory()) {
      await walk(fullPath);
    } else if (entry.isFile()) {
      await scanFile(fullPath, rel);
    }
  }
}

function isExcluded(rel, entry) {
  for (const directory of excludedDirectories) {
    if (rel === directory || rel.startsWith(`${directory}/`)) return true;
  }
  if (entry.isFile() && excludedExtensions.has(extname(entry.name).toLowerCase())) return true;
  if (rel === 'scripts/scan-secrets.mjs') return true;
  return false;
}

async function scanFile(filePath, rel) {
  let content = '';
  try {
    content = await readFile(filePath, 'utf8');
  } catch {
    return;
  }
  for (const pattern of patterns) {
    if (pattern.test(content)) findings.push(`${rel} matches ${pattern}`);
  }
}

function toPosix(value) {
  return value.split(sep).join('/');
}
