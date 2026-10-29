#!/usr/bin/env node
// Deterministic OpenCode shell/session workload. No model or credentials are used.
import { spawn } from 'node:child_process';
import { createHash } from 'node:crypto';
import * as fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const SCRIPT = fileURLToPath(import.meta.url);
const FILES = 16;
const SIZE = 8192;
const DEADLINE_MS = 60000;

export function localAddress(line) {
  const match = /^opencode server listening on http:\/\/127\.0\.0\.1:([0-9]+)\r?$/.exec(line);
  if (!match) return null;
  const port = Number(match[1]);
  if (!Number.isInteger(port) || port < 1 || port > 65535) return null;
  return `http://127.0.0.1:${port}`;
}

export function isolatedEnvironment(root) {
  return {
    PATH: '/usr/local/bin:/usr/bin:/bin', LANG: 'C.UTF-8', SHELL: '/bin/bash',
    HOME: path.join(root, 'home'), XDG_CONFIG_HOME: path.join(root, 'config'),
    XDG_DATA_HOME: path.join(root, 'data'), XDG_CACHE_HOME: path.join(root, 'cache'),
    XDG_STATE_HOME: path.join(root, 'state'), TMPDIR: path.join(root, 'tmp'),
    OPENCODE_DISABLE_AUTOUPDATE: 'true', OPENCODE_DISABLE_MODELS_FETCH: 'true',
    OPENCODE_DISABLE_DEFAULT_PLUGINS: 'true', OPENCODE_DISABLE_EXTERNAL_SKILLS: 'true',
    OPENCODE_DISABLE_PROJECT_CONFIG: 'true', OPENCODE_DISABLE_CLAUDE_CODE: 'true',
    OPENCODE_DISABLE_LSP_DOWNLOAD: 'true', OPENCODE_DISABLE_SHARE: 'true',
    OPENCODE_CONFIG_CONTENT: JSON.stringify({ autoupdate: false, share: 'disabled', plugin: [] }),
  };
}

function fail(code) { throw new Error(code); }
function childExit(child) {
  return new Promise(resolve => {
    child.once('exit', (code, signal) => resolve({ code, signal }));
    child.once('error', () => resolve({ code: null, signal: 'spawn_error' }));
  });
}

export async function stopServer(server) {
  let forced = false;
  if (server.child.exitCode === null && server.child.signalCode === null) {
    server.child.kill('SIGTERM');
    let deadline;
    try {
      const stopped = await Promise.race([
        server.exited.then(() => true),
        new Promise(resolve => { deadline = setTimeout(() => resolve(false), 3000); }),
      ]);
      if (!stopped) { forced = true; server.child.kill('SIGKILL'); }
    } finally { clearTimeout(deadline); }
  }
  return { ...await server.exited, forced };
}

export async function startServer(binary, project, env, timeoutMs = DEADLINE_MS) {
  const child = spawn(binary, ['serve', '--pure', '--hostname', '127.0.0.1', '--port', '0'], {
    cwd: project, env, stdio: ['ignore', 'pipe', 'ignore'],
  });
  const server = { child, exited: childExit(child) };
  let timer;
  try {
    server.url = await new Promise((resolve, reject) => {
      let buffer = '';
      let received = 0;
      timer = setTimeout(() => reject(new Error('server_start_timeout')), timeoutMs);
      server.exited.then(() => reject(new Error('server_exited')));
      child.stdout.on('data', data => {
        received += data.length;
        if (received > 65536) return reject(new Error('server_output_limit'));
        buffer += data.toString('utf8');
        for (;;) {
          const end = buffer.indexOf('\n');
          if (end < 0) break;
          const line = buffer.slice(0, end);
          buffer = buffer.slice(end + 1);
          const url = localAddress(line);
          if (url) resolve(url);
        }
      });
    });
    return server;
  } catch (error) {
    await stopServer(server);
    throw error;
  } finally { clearTimeout(timer); }
}

async function request(server, project, route, body) {
  let response;
  try {
    response = await fetch(server.url + route, {
      method: body === undefined ? 'GET' : 'POST',
      headers: { 'content-type': 'application/json', 'x-opencode-directory': project },
      body: body === undefined ? undefined : JSON.stringify(body),
      signal: AbortSignal.timeout(DEADLINE_MS), redirect: 'error',
    });
    if (!response.ok) fail('api_status');
    // The fixed workload produces a bounded history; never retain raw transcripts.
    const chunks = [];
    let size = 0;
    for await (const chunk of response.body) {
      size += chunk.length;
      if (size > 1024 * 1024) fail('api_output_limit');
      chunks.push(Buffer.from(chunk));
    }
    return JSON.parse(Buffer.concat(chunks).toString('utf8'));
  } catch (error) {
    if (error.message === 'api_status' || error.message === 'api_output_limit') throw error;
    fail('api_request_failed');
  }
}

function expected(step) {
  const digest = createHash('sha256');
  for (let i = 0; i < FILES; i++) digest.update(Buffer.alloc(SIZE, (step + i) & 255));
  return { step, sum: [-3, 0, 8, 12].reduce((a, b) => a + b, 0),
    files: FILES, bytes: FILES * SIZE, digest: digest.digest('hex') };
}

export function toolStep(project, step) {
  const state = path.join(project, 'result.json');
  if (step === 1 ? fs.existsSync(state) : JSON.parse(fs.readFileSync(state)).step !== step - 1) {
    fail('tool_sequence');
  }
  const digest = createHash('sha256');
  for (let i = 0; i < FILES; i++) {
    const target = path.join(project, `data-${i}`);
    const fd = fs.openSync(target, 'w', 0o600);
    try {
      fs.writeFileSync(fd, Buffer.alloc(SIZE, (step + i) & 255));
      fs.fsyncSync(fd);
    } finally { fs.closeSync(fd); }
    digest.update(fs.readFileSync(target));
  }
  const result = expected(step);
  if (digest.digest('hex') !== result.digest) fail('tool_data');
  fs.writeFileSync(state, JSON.stringify(result), { mode: 0o600 });
  process.stdout.write(`MM192_TOOL_OK ${step} ${result.digest}\n`);
}

export function verifyFiles(project, step) {
  const result = expected(step);
  if (JSON.stringify(JSON.parse(fs.readFileSync(path.join(project, 'result.json')))) !== JSON.stringify(result)) {
    fail('result_mismatch');
  }
  const digest = createHash('sha256');
  for (let i = 0; i < FILES; i++) {
    const bytes = fs.readFileSync(path.join(project, `data-${i}`));
    if (bytes.length !== SIZE) fail('file_size');
    digest.update(bytes);
  }
  if (digest.digest('hex') !== result.digest) fail('file_digest');
  return `MM192_TOOL_OK ${step} ${result.digest}\n`;
}

function ticks(pid) {
  const text = fs.readFileSync(`/proc/${pid}/stat`, 'utf8');
  const fields = text.slice(text.lastIndexOf(')') + 2).trim().split(/\s+/);
  return { user: Number(fields[11]), system: Number(fields[12]), start: fields[19] };
}
function quote(value) { return `'${value.replaceAll("'", "'\\''")}'`; }

export async function runOffline(binary, node, workspace, output, repetitions = 4) {
  for (const value of [binary, node, workspace, output]) {
    if (!path.isAbsolute(value)) fail('absolute_paths_required');
  }
  if (fs.realpathSync(node) !== fs.realpathSync(process.execPath)) fail('node_runtime_mismatch');
  if (!Number.isInteger(repetitions) || repetitions < 1 || repetitions > 16) fail('invalid_repetitions');
  if (workspace === output || workspace.startsWith(output + '/') || output.startsWith(workspace + '/')) fail('overlapping_directories');
  fs.mkdirSync(output, { mode: 0o700 });
  const result = { schema: 1, success: false, workload: 'opencode-local-shell-session',
    model_requests: 0, service_restarts: 0, phases: [],
    cpu_scope: 'driver microseconds exclude children; service proc-stat ticks exclude shell/node children; helper leaf includes all descendants',
    production_slo_claimed: false };
  let server;
  let phase = 'setup';
  const measure = async (name, action) => {
    phase = name;
    const begin = process.hrtime.bigint();
    const cpu = process.cpuUsage();
    const before = server ? ticks(server.child.pid) : null;
    try {
      const value = await action();
      const after = server && before ? ticks(server.child.pid) : null;
      if (after && before.start !== after.start) fail('service_identity_changed');
      const used = process.cpuUsage(cpu);
      result.phases.push({ phase: name, success: true, elapsed_ns: Number(process.hrtime.bigint() - begin),
        driver_user_us: used.user, driver_system_us: used.system,
        service_user_ticks: after ? after.user - before.user : null,
        service_system_ticks: after ? after.system - before.system : null });
      return value;
    } catch (error) {
      result.phases.push({ phase: name, success: false, elapsed_ns: Number(process.hrtime.bigint() - begin) });
      throw error;
    }
  };
  try {
    fs.mkdirSync(workspace, { mode: 0o700 });
    for (const name of ['home', 'config', 'data', 'cache', 'state', 'tmp', 'project']) {
      fs.mkdirSync(path.join(workspace, name), { mode: 0o700 });
    }
    const project = path.join(workspace, 'project');
    const env = isolatedEnvironment(workspace);
    await measure('cold_start', async () => { server = await startServer(binary, project, env); });
    await measure('first_session_schema', async () => {
      const session = await request(server, project, '/session', { title: 'Deterministic local workload' });
      if (!/^ses_[A-Za-z0-9]+$/.test(session.id)) fail('invalid_session');
      result.session_id = session.id;
    });
    const execute = async step => {
      const message = await request(server, project, `/session/${result.session_id}/shell`, {
        agent: 'build', model: { providerID: 'offline', modelID: 'local' },
        command: `exec ${quote(node)} ${quote(SCRIPT)} --tool ${quote(project)} ${step}`,
      });
      const marker = verifyFiles(project, step);
      const parts = message.parts?.filter(p => p.type === 'tool' && p.tool === 'bash');
      if (parts?.length !== 1 || parts[0].state?.status !== 'completed' || parts[0].state.output !== marker) fail('tool_output');
    };
    // First tool initializes runtime paths separately from subsequent warm samples.
    await measure('first_tool', () => execute(1));
    for (let step = 2; step <= repetitions + 1; step++) await measure('warm_tool', () => execute(step));
    await measure('service_stop', async () => {
      const stopped = await stopServer(server);
      server = null;
      if (stopped.forced) fail('service_forced_stop');
    });
    result.service_restarts++;
    await measure('service_restart', async () => { server = await startServer(binary, project, env); });
    await measure('session_restore', async () => {
      const session = await request(server, project, `/session/${result.session_id}`);
      const history = await request(server, project, `/session/${result.session_id}/message`);
      if (session.id !== result.session_id || history.length !== 2 * (repetitions + 1)) fail('session_history');
      const previous = verifyFiles(project, repetitions + 1);
      if (!history.some(m => m.parts?.some(p => p.type === 'tool' && p.state?.output === previous))) fail('session_history');
    });
    await measure('restored_tool', () => execute(repetitions + 2));
    result.verified_steps = repetitions + 2;
    result.verified_bytes = result.verified_steps * FILES * SIZE;
    result.success = true;
  } catch (error) {
    result.failure_phase = phase;
    result.failure = /^[a-z_]+$/.test(error.message) ? error.message : 'workload_failed';
  } finally {
    if (server) {
      const stopped = await stopServer(server);
      if (stopped.forced && result.success) {
        result.success = false; result.failure_phase = 'cleanup'; result.failure = 'service_forced_stop';
      }
    }
    fs.writeFileSync(path.join(output, 'workload.json'), JSON.stringify(result, null, 2) + '\n', { mode: 0o600 });
  }
  return result;
}

if (process.argv[1] && path.resolve(process.argv[1]) === SCRIPT) {
  if (process.argv[2] === '--tool') {
    try {
      const step = Number(process.argv[4]);
      if (!Number.isInteger(step) || step < 1 || step > 18) fail('invalid_step');
      toolStep(process.argv[3], step);
    } catch { process.exitCode = 1; }
  } else if (process.argv.length < 6 || process.argv.length > 7) {
    console.error('Usage: node offline-opencode.mjs OPENCODE NODE FRESH_WORKSPACE FRESH_OUTPUT [WARM_REPETITIONS=4]');
    process.exitCode = 2;
  } else {
    try {
      const result = await runOffline(...process.argv.slice(2, 6), Number(process.argv[6] ?? 4));
      console.log(JSON.stringify(result));
      if (!result.success) process.exitCode = 1;
    } catch {
      console.error('offline workload setup failed');
      process.exitCode = 1;
    }
  }
}
