// Run with: node --test tools/agent_memory/workloads/test_offline_opencode.mjs
import assert from 'node:assert/strict';
import { test } from 'node:test';
import * as fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { isolatedEnvironment, localAddress, runOffline, startServer, stopServer,
  toolStep, verifyFiles } from './offline-opencode.mjs';

function temporary() { return fs.mkdtempSync(path.join(os.tmpdir(), 'offline-opencode-test-')); }
function fixture(root, body) {
  const binary = path.join(root, 'server');
  fs.writeFileSync(binary, `#!${process.execPath}\n${body}\n`, { mode: 0o700 });
  return binary;
}

test('only the exact loopback service announcement supplies an address', () => {
  assert.equal(localAddress('opencode server listening on http://127.0.0.1:38271'), 'http://127.0.0.1:38271');
  for (const address of ['http://localhost:123', 'http://127.0.0.1:0', 'http://127.0.0.1:65536',
    'http://127.0.0.1:123/path', 'https://127.0.0.1:123', 'http://user@127.0.0.1:123',
    'http://192.0.2.1:123', 'http://127.0.0.1:123 extra']) {
    assert.equal(localAddress(`opencode server listening on ${address}`), null);
  }
});

test('server environment excludes inherited credentials and configuration', () => {
  const before = process.env.DEEPSEEK_API_KEY;
  process.env.DEEPSEEK_API_KEY = 'fixture-secret-not-forwarded';
  try {
    const env = isolatedEnvironment('/private/workspace');
    assert.equal(env.DEEPSEEK_API_KEY, undefined);
    assert.equal(env.OPENCODE_SERVER_PASSWORD, undefined);
    assert.equal(env.HOME, '/private/workspace/home');
    assert.equal(env.OPENCODE_DISABLE_MODELS_FETCH, 'true');
    assert.equal(env.OPENCODE_DISABLE_PROJECT_CONFIG, 'true');
    assert.equal(JSON.parse(env.OPENCODE_CONFIG_CONTENT).share, 'disabled');
  } finally {
    if (before === undefined) delete process.env.DEEPSEEK_API_KEY;
    else process.env.DEEPSEEK_API_KEY = before;
  }
});

test('independent checker rejects corrupt files and out-of-order state', () => {
  const root = temporary();
  const original = process.stdout.write;
  // Tool success markers are protocol output, not test runner output.
  process.stdout.write = () => true;
  try {
    toolStep(root, 1);
    verifyFiles(root, 1);
    assert.throws(() => toolStep(root, 3), /tool_sequence/);
    fs.writeFileSync(path.join(root, 'data-7'), Buffer.alloc(8192, 255));
    assert.throws(() => verifyFiles(root, 1), /file_digest/);
    toolStep(root, 2);
    verifyFiles(root, 2);
    fs.writeFileSync(path.join(root, 'result.json'), '{}');
    assert.throws(() => verifyFiles(root, 2), /result_mismatch/);
  } finally {
    process.stdout.write = original;
    fs.rmSync(root, { recursive: true });
  }
});

test('fragmented startup announcement is accepted and normal exit is reaped', async () => {
  const root = temporary();
  try {
    const binary = fixture(root, "process.stdout.write('opencode server listening on http://127.'); setTimeout(() => process.stdout.write('0.0.1:32123\\n'), 20); setInterval(() => {}, 1000);");
    const server = await startServer(binary, root, isolatedEnvironment(root), 2000);
    assert.equal(server.url, 'http://127.0.0.1:32123');
    await stopServer(server);
    assert.throws(() => process.kill(server.child.pid, 0), { code: 'ESRCH' });
  } finally { fs.rmSync(root, { recursive: true }); }
});

test('startup timeout cleans the child instead of connecting a nonlocal announcement', async () => {
  const root = temporary();
  try {
    const pidFile = path.join(root, 'pid');
    const binary = fixture(root, `require('node:fs').writeFileSync(${JSON.stringify(pidFile)}, String(process.pid)); console.log('opencode server listening on http://192.0.2.1:123'); setInterval(() => {}, 1000);`);
    await assert.rejects(startServer(binary, root, isolatedEnvironment(root), 500), /server_start_timeout/);
    const pid = Number(fs.readFileSync(pidFile));
    assert.throws(() => process.kill(pid, 0), { code: 'ESRCH' });
  } finally { fs.rmSync(root, { recursive: true }); }
});

test('spawn failure reports a bounded failure and does not hang cleanup', async () => {
  const root = temporary();
  try {
    await assert.rejects(startServer(path.join(root, 'absent'), root, isolatedEnvironment(root), 500), /server_exited/);
  } finally { fs.rmSync(root, { recursive: true }); }
});

test('failed workload remains in results and leaves no admitted server', async () => {
  const root = temporary();
  try {
    const binary = fixture(root, 'process.exit(7)');
    const output = path.join(root, 'output');
    const result = await runOffline(binary, process.execPath, path.join(root, 'workspace'), output, 1);
    assert.equal(result.success, false);
    assert.equal(result.failure_phase, 'cold_start');
    assert.equal(result.phases.length, 1);
    assert.equal(result.phases[0].success, false);
    assert.equal(JSON.parse(fs.readFileSync(path.join(output, 'workload.json'))).success, false);
  } finally { fs.rmSync(root, { recursive: true }); }
});

test('HTTP success and completed tool cannot replace the independent artifact oracle', async () => {
  const root = temporary();
  try {
    const pidFile = path.join(root, 'pid');
    const binary = fixture(root, `
      require('node:fs').writeFileSync(${JSON.stringify(pidFile)}, String(process.pid));
      const server = require('node:http').createServer((req, res) => {
        req.resume();
        res.setHeader('content-type', 'application/json');
        res.end(JSON.stringify(req.url === '/session' ? { id: 'ses_fixture' } :
          { parts: [{type: 'tool', tool: 'bash', state: { status: 'completed', output: 'MM192_TOOL_OK' }}] }));
      });
      server.listen(0, '127.0.0.1', () => console.log('opencode server listening on http://127.0.0.1:' + server.address().port));
    `);
    const output = path.join(root, 'output');
    const result = await runOffline(binary, process.execPath, path.join(root, 'workspace'), output, 1);
    assert.equal(result.success, false);
    assert.equal(result.failure_phase, 'first_tool');
    assert.equal(result.phases.at(-1).success, false);
    assert.throws(() => process.kill(Number(fs.readFileSync(pidFile)), 0), { code: 'ESRCH' });
  } finally { fs.rmSync(root, { recursive: true }); }
});

test('service which ignores graceful stop is killed and reaped', async () => {
  const root = temporary();
  try {
    const binary = fixture(root, "process.on('SIGTERM', () => {}); console.log('opencode server listening on http://127.0.0.1:32123'); setInterval(() => {}, 1000);");
    const server = await startServer(binary, root, isolatedEnvironment(root), 2000);
    await stopServer(server);
    assert.equal(server.child.signalCode, 'SIGKILL');
    assert.throws(() => process.kill(server.child.pid, 0), { code: 'ESRCH' });
  } finally { fs.rmSync(root, { recursive: true }); }
});
