#!/usr/bin/env node
'use strict';
// The independent checker and its fixed expectations live outside the Agent project.
const fs = require('node:fs');
const path = require('node:path');
const readline = require('node:readline');
const { spawnSync } = require('node:child_process');
const { isDeepStrictEqual } = require('node:util');

function reject(code) { throw new Error(code); }
function jsonFile(file) {
  const st = fs.lstatSync(file);
  if (!st.isFile() || st.size > 65536) reject('invalid_artifact');
  return JSON.parse(fs.readFileSync(file, 'utf8'));
}
function equal(actual, expected) {
  if (!isDeepStrictEqual(actual, expected)) reject('artifact_mismatch');
}
function saveProbe(output, name, result) {
  for (const stream of ['stdout', 'stderr']) {
    const bounded = Buffer.from(result[stream] || '').subarray(0, 65536);
    fs.writeFileSync(path.join(output, `${name}.${stream}.log`), bounded, { mode: 0o600 });
  }
}

async function check(workspace, output, mode, exitCode) {
  const report = { schema: 1, mode, success: false, agent_exit: exitCode,
    profile: 'opencode-built-in-tools-pure', events: 0, completed_tools: 0,
    sdk_condition: mode === 'cold' ? 'empty' : mode === 'resume' ? 'resumed' : 'preinstalled' };
  const sdkPackage = path.join(workspace, 'cache/opencode/packages/@ai-sdk/deepseek/node_modules/@ai-sdk/deepseek/package.json');
  report.sdk_installed_after = fs.existsSync(sdkPackage);
  if (report.sdk_installed_after) {
    try {
      const version = jsonFile(sdkPackage).version;
      if (typeof version === 'string' && /^[0-9]+\.[0-9]+\.[0-9]+(?:-[A-Za-z0-9.-]+)?$/.test(version)) report.sdk_version = version;
    } catch { /* A bad SDK will still fail actual execution; don't expose its text. */ }
  }
  try {
    if (!['coding', 'cold', 'resume'].includes(mode)) reject('invalid_mode');
    if (!Number.isSafeInteger(exitCode) || exitCode !== 0) reject('agent_exit');
    const trace = path.join(output, 'events.jsonl');
    if (fs.statSync(trace).size > 32 * 1024 * 1024) reject('trace_too_large');
    let session, stop = false, read = false, bash = false;
    const lines = readline.createInterface({ input: fs.createReadStream(trace), crlfDelay: Infinity });
    for await (const line of lines) {
      if (!line.trim()) continue;
      if (line.length > 1024 * 1024) reject('event_too_large');
      let event;
      try { event = JSON.parse(line); } catch { reject('malformed_events'); }
      report.events++;
      if (event.type === 'error') reject('provider_or_agent_error');
      if (typeof event.sessionID !== 'string' || !/^ses_[A-Za-z0-9]+$/.test(event.sessionID)) reject('invalid_session');
      if (session && session !== event.sessionID) reject('mixed_sessions');
      session = event.sessionID;
      const part = event.part || {};
      if (event.type === 'step_finish') {
        if (!['stop', 'tool-calls'].includes(part.reason)) reject('unfinished_step');
        stop = part.reason === 'stop';
      }
      if (event.type === 'tool_use') {
        stop = false;
        if (part.state?.status !== 'completed') reject('tool_not_completed');
        report.completed_tools++;
        if (part.tool === 'read' && path.basename(part.state.input?.filePath || '') === 'input.json') read = true;
        if (part.tool === 'bash') bash = true;
      }
    }
    if (!session || !stop || !bash) reject('missing_completed_work');
    if (mode !== 'resume' && !read) reject('input_not_read');
    if (mode === 'resume') {
      if (fs.readFileSync(path.join(workspace, 'session-id'), 'utf8').trim() !== session) reject('session_not_resumed');
      equal(jsonFile(path.join(workspace, 'project/resume-result.json')), { previousSum: 17, nextSum: 18 });
    } else {
      equal(jsonFile(path.join(workspace, 'project/input.json')), [-3, 0, 8, 12]);
      equal(jsonFile(path.join(workspace, 'project/result.json')), { sum: 17 });
      const module = path.join(workspace, 'project/sum.mjs');
      if (!fs.lstatSync(module).isFile()) reject('invalid_artifact');
      // Isolate generated module output, and bound checker execution independently.
      const opts = { encoding: 'utf8', timeout: 10000, maxBuffer: 65536, cwd: path.dirname(module) };
      const run = spawnSync(process.execPath, [path.join(__dirname, 'function-probe.mjs'), module], opts);
      saveProbe(output, 'function-probe', run);
      if (run.error || run.status !== 0) reject('independent_function_failed');
      equal(JSON.parse(run.stdout), [0, -3, 20, 17]);
      const cli = spawnSync(process.execPath, [module, 'input.json'], opts);
      saveProbe(output, 'cli-probe', cli);
      if (cli.error || cli.status !== 0) reject('independent_cli_failed');
      equal(JSON.parse(cli.stdout), { sum: 17 });
      fs.writeFileSync(path.join(workspace, 'session-id'), session + '\n', { mode: 0o600 });
    }
    report.session_id = session;
    report.checker = 'passed';
    report.success = true;
  } catch (error) {
    // Never publish exception messages, provider strings, paths, or model text.
    const allowed = new Set(['invalid_mode', 'agent_exit', 'invalid_artifact', 'artifact_mismatch',
      'trace_too_large', 'event_too_large', 'malformed_events', 'provider_or_agent_error',
      'invalid_session', 'mixed_sessions', 'unfinished_step', 'tool_not_completed',
      'missing_completed_work', 'input_not_read', 'session_not_resumed',
      'independent_function_failed', 'independent_cli_failed']);
    report.failure = allowed.has(error.message) ? error.message : 'checker_failed';
    report.checker = 'failed';
  }
  fs.writeFileSync(path.join(output, 'workload.json'), JSON.stringify(report) + '\n', { mode: 0o600 });
  return report;
}

if (require.main === module) {
  const [workspace, output, mode, exit] = process.argv.slice(2);
  if (!workspace || !output || !mode || exit === undefined) process.exit(2);
  check(workspace, output, mode, Number(exit)).then(result => { process.exitCode = result.success ? 0 : 1; });
}
module.exports = { check };
