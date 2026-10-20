'use strict';
// Parser/checker tests use local fixtures; they are not real-Agent acceptance evidence.
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawnSync } = require('node:child_process');
const { check } = require('./check.cjs');

function fixture(t) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'agent-check-'));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const workspace = path.join(root, 'workspace');
  const output = path.join(root, 'output');
  fs.mkdirSync(path.join(workspace, 'project'), { recursive: true });
  fs.mkdirSync(output);
  fs.writeFileSync(path.join(workspace, 'project/input.json'), '[-3,0,8,12]');
  fs.writeFileSync(path.join(workspace, 'project/result.json'), '{"sum":17}');
  fs.writeFileSync(path.join(workspace, 'project/sum.mjs'), `
    import fs from 'node:fs';
    import {pathToFileURL} from 'node:url';
    export const sum = values => values.reduce((a,b) => a+b, 0);
    if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href)
      console.log(JSON.stringify({sum:sum(JSON.parse(fs.readFileSync(process.argv[2], 'utf8')))}));`);
  const event = (type, part) => ({ type, sessionID: 'ses_fixture123', part });
  const tool = name => event('tool_use', { tool: name,
    state: { status: 'completed', input: name === 'read' ? { filePath: 'input.json' } : { command: 'node sum.mjs input.json' } } });
  const events = [tool('read'), tool('write'), tool('bash'), event('step_finish', { reason: 'stop' })];
  const write = list => fs.writeFileSync(path.join(output, 'events.jsonl'), list.map(JSON.stringify).join('\n') + '\n');
  write(events);
  return { workspace, output, events, write, event, tool };
}

test('real independent Node checks and session continuation', async t => {
  const f = fixture(t);
  let result = await check(f.workspace, f.output, 'coding', 0);
  assert.equal(result.success, true);
  assert.equal(result.session_id, 'ses_fixture123');
  // JSON object property order is not part of the artifact contract.
  fs.writeFileSync(path.join(f.workspace, 'project/resume-result.json'), '{"nextSum":18,"previousSum":17}');
  f.write([f.tool('bash'), f.event('step_finish', { reason: 'stop' })]);
  result = await check(f.workspace, f.output, 'resume', 0);
  assert.equal(result.success, true);
});

test('a text marker without completed tools cannot pass', async t => {
  const f = fixture(t);
  f.write([f.event('text', { text: 'AGENT_TOOL_OK' }), f.event('step_finish', { reason: 'stop' })]);
  assert.equal((await check(f.workspace, f.output, 'coding', 0)).failure, 'missing_completed_work');
});

test('a legitimate realpath main guard imports from a real Node entry file', async t => {
  const f = fixture(t);
  fs.writeFileSync(path.join(f.workspace, 'project/sum.mjs'), `
    import {readFileSync,realpathSync} from 'node:fs';
    import {pathToFileURL} from 'node:url';
    export function sum(values) { return values.reduce((a,b) => a+b, 0); }
    const direct = process.argv[1] !== undefined &&
      import.meta.url === pathToFileURL(realpathSync(process.argv[1])).href;
    if (direct) console.log(JSON.stringify({sum:sum(JSON.parse(readFileSync(process.argv[2], 'utf8')))}));`);
  const result = await check(f.workspace, f.output, 'coding', 0);
  assert.equal(result.success, true);
  assert.deepEqual(JSON.parse(fs.readFileSync(path.join(f.output, 'function-probe.stdout.log'))), [0, -3, 20, 17]);
  assert.deepEqual(JSON.parse(fs.readFileSync(path.join(f.output, 'cli-probe.stdout.log'))), {sum:17});
  for (const name of ['function-probe.stdout.log', 'function-probe.stderr.log', 'cli-probe.stdout.log', 'cli-probe.stderr.log']) {
    const st = fs.statSync(path.join(f.output, name));
    assert.equal(st.mode & 0o777, 0o600);
    assert.ok(st.size <= 65536);
  }
});

test('provider error details are never published', async t => {
  const f = fixture(t);
  f.write([...f.events, { type: 'error', error: { message: 'private-token-fixture' } }]);
  const result = await check(f.workspace, f.output, 'cold', 0);
  assert.equal(result.failure, 'provider_or_agent_error');
  assert.ok(!fs.readFileSync(path.join(f.output, 'workload.json'), 'utf8').includes('private-token-fixture'));
});

test('failed probe diagnostics remain private and byte bounded', async t => {
  const f = fixture(t);
  fs.writeFileSync(path.join(f.workspace, 'project/sum.mjs'),
    "console.error('private-probe-fixture'.repeat(8192)); export const sum = values => 0;");
  const result = await check(f.workspace, f.output, 'coding', 0);
  assert.equal(result.success, false);
  assert.ok(!JSON.stringify(result).includes('private-probe-fixture'));
  const stderr = fs.statSync(path.join(f.output, 'function-probe.stderr.log'));
  assert.ok(stderr.size > 0 && stderr.size <= 65536);
  assert.equal(stderr.mode & 0o777, 0o600);
});

test('nonzero CLI exit stays failed even with good artifacts', async t => {
  const f = fixture(t);
  assert.equal((await check(f.workspace, f.output, 'coding', 7)).failure, 'agent_exit');
});

test('unfinished tools, missing final stop and malformed JSON fail', async t => {
  const f = fixture(t);
  const broken = f.tool('bash');
  broken.part.state.status = 'error';
  f.write([broken]);
  assert.equal((await check(f.workspace, f.output, 'coding', 0)).failure, 'tool_not_completed');
  f.write([...f.events, f.tool('bash')]);
  assert.equal((await check(f.workspace, f.output, 'coding', 0)).failure, 'missing_completed_work');
  fs.writeFileSync(path.join(f.output, 'events.jsonl'), '{bad json');
  assert.equal((await check(f.workspace, f.output, 'coding', 0)).failure, 'malformed_events');
});

test('modified input or hardcoded result cannot replace an implementation', async t => {
  const f = fixture(t);
  fs.writeFileSync(path.join(f.workspace, 'project/sum.mjs'), 'export const sum = () => 17;');
  assert.equal((await check(f.workspace, f.output, 'coding', 0)).failure, 'artifact_mismatch');
  fs.writeFileSync(path.join(f.workspace, 'project/input.json'), '[17]');
  assert.equal((await check(f.workspace, f.output, 'coding', 0)).failure, 'artifact_mismatch');
});

test('mixed or wrong resume session cannot pass', async t => {
  const f = fixture(t);
  fs.writeFileSync(path.join(f.workspace, 'session-id'), 'ses_other');
  f.write([f.tool('bash'), f.event('step_finish', { reason: 'stop' })]);
  assert.equal((await check(f.workspace, f.output, 'resume', 0)).failure, 'session_not_resumed');
  f.events[1].sessionID = 'ses_other';
  f.write(f.events);
  assert.equal((await check(f.workspace, f.output, 'coding', 0)).failure, 'mixed_sessions');
});

test('prepare is exclusive, credential-free, outside the measured output', t => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'agent-prepare-'));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const workspace = path.join(root, 'workspace with spaces');
  const output = path.join(root, 'output with spaces');
  const payload = path.join(root, 'payload with spaces');
  fs.mkdirSync(path.join(payload, 'bin'), { recursive: true });
  fs.symlinkSync(process.execPath, path.join(payload, 'bin/node'));
  // This stub only tests launch gating. It never claims Agent/provider acceptance.
  fs.writeFileSync(path.join(payload, 'bin/opencode'), '#!/bin/sh\nexit 99\n', { mode: 0o700 });
  const pkg = path.join(payload, 'sdk/@ai-sdk/deepseek/node_modules/@ai-sdk/deepseek');
  fs.mkdirSync(pkg, { recursive: true });
  fs.writeFileSync(path.join(pkg, 'package.json'), '{"version":"3.0.49"}');
  const run = (mode, dest = output, envFile = '/nonexistent-private-env') => spawnSync('/bin/bash', [path.join(__dirname, 'opencode.sh'),
    workspace, payload, dest, mode], { encoding: 'utf8', env: { ...process.env, AGENT_ENV_FILE: envFile } });
  assert.equal(run('prepare').status, 0);
  assert.equal(fs.existsSync(output), false);
  assert.equal(fs.readFileSync(path.join(workspace, 'prepared'), 'utf8'), 'coding\n');
  assert.deepEqual(fs.readdirSync(path.join(workspace, 'project')), ['input.json']);
  assert.notEqual(run('prepare').status, 0);
  assert.notEqual(run('cold').status, 0);
  assert.equal(JSON.parse(fs.readFileSync(path.join(output, 'workload.json'))).failure, 'wrong_preparation');
  const measured = path.join(root, 'measured');
  assert.notEqual(run('coding', measured).status, 0);
  assert.equal(JSON.parse(fs.readFileSync(path.join(measured, 'workload.json'))).failure, 'missing_credentials');
  assert.equal(fs.existsSync(path.join(workspace, 'prepared')), false);
  assert.notEqual(run('coding', path.join(root, 'retry')).status, 0);
  fs.writeFileSync(path.join(workspace, 'session-id'), 'ses_fixture123\n');
  for (const name of ['result.json', 'sum.mjs', 'resume-result.json']) {
    fs.writeFileSync(path.join(workspace, 'project', name), 'old verified fixture');
  }
  const database = path.join(workspace, 'data/retained.sqlite');
  fs.writeFileSync(database, 'database fixture must survive resume');
  const envFile = path.join(root, 'private-env');
  fs.writeFileSync(envFile, 'DEEPSEEK_API_KEY=unit-test-not-a-real-credential\n', {mode:0o600});
  assert.notEqual(run('resume', path.join(root, 'resume'), envFile).status, 0); // Stub exits99.
  for (const name of ['input.json', 'result.json', 'sum.mjs', 'resume-result.json']) {
    assert.equal(fs.existsSync(path.join(workspace, 'project', name)), false);
  }
  assert.equal(fs.readFileSync(database, 'utf8'), 'database fixture must survive resume');
  assert.equal(fs.readFileSync(path.join(workspace, 'session-id'), 'utf8'), 'ses_fixture123\n');
  assert.ok(fs.existsSync(path.join(workspace, 'cache/opencode/packages/@ai-sdk/deepseek/node_modules/@ai-sdk/deepseek/package.json')));
});
