import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

const backend = readFileSync(new URL('../electron/local-backend.cjs', import.meta.url), 'utf8');
const capsule = readFileSync(new URL('../src/components/FloatingCapsule.tsx', import.meta.url), 'utf8');
const css = readFileSync(new URL('../src/styles.css', import.meta.url), 'utf8');

assert.match(backend, /activityUpdate\('thinking'/);
assert.match(backend, /activityUpdate\('executing'/);
assert.match(backend, /payloadType === 'reasoning'[\s\S]*activityUpdate\('thinking'/);
assert.match(backend, /isToolStartEvent\(eventType,\s*payloadType,\s*payload\)[\s\S]*activityUpdate\('executing'/);
assert.match(backend, /payloadType === 'patch_apply_end'[\s\S]*activityUpdate\('thinking'/);
assert.match(backend, /\['function_call_output',\s*'custom_tool_call_output'\][\s\S]*activityUpdate\('thinking'/);
assert.doesNotMatch(backend, /\['function_call_output',\s*'custom_tool_call_output',\s*'custom_tool_call',\s*'web_search_call'\][\s\S]*activityUpdate\('executing'/);
assert.doesNotMatch(backend, /isExecutionCommentary\(payload\)[\s\S]{0,120}activityUpdate\('executing'/);
assert.doesNotMatch(backend, /function isExecutionCommentary/);
assert.match(backend, /codexActivityLabel\(status/);
assert.match(backend, /CODEX_ACTIVITY_STALE_MS = 15 \* 1000/);
assert.match(backend, /activeTurn:\s*isInsideTurn/);
assert.match(backend, /if \(activity\.activeTurn\) return activity/);
assert.doesNotMatch(backend, /shouldKeepFinalAnswerVisible/);
assert.match(backend, /status === 'thinking'[\s\S]*思考/);
assert.match(backend, /status === 'executing'[\s\S]*执行/);

assert.match(capsule, /status === 'executing'/);
assert.match(capsule, /status === 'thinking'/);
assert.match(capsule, /status === 'unknown' \|\| !status[\s\S]*green:\s*false/);
assert.match(capsule, /traffic-light[^`]+is-breathing/);
assert.match(css, /@keyframes signal-breathe/);
assert.doesNotMatch(css, /@keyframes signal-blink/);

console.log('codex activity source tests passed');
