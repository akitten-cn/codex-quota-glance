import assert from 'node:assert/strict';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
const { _internals } = require('../electron/local-backend.cjs');

assert.equal(
  _internals.cleanAuthorizationToken('Authorization: Bearer AbC-12_./+xy==\nNew-Api-User: 5781'),
  'AbC-12_./+xy=='
);
assert.equal(
  _internals.cleanAuthorizationToken('Bearer eyJhbGciOiJIUzI1NiJ9.a-b_c.signature'),
  'eyJhbGciOiJIUzI1NiJ9.a-b_c.signature'
);

console.log('local backend auth tests passed');
