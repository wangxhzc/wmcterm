import assert from 'node:assert/strict';
import { mkdtempSync, mkdirSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { spawn, execFileSync } from 'node:child_process';
import { once } from 'node:events';

// Run under xvfb-run. This script controls only its disposable application.
const binary = resolve(process.argv[2] ?? 'target/debug/wmcterm');
const directory = mkdtempSync(join(tmpdir(), 'wmcterm-claude-input-'));
const env = Object.fromEntries(Object.entries(process.env).filter(([key]) => key !== 'WMCTERM' && !key.startsWith('WMCTERM_')));
for (const name of ['runtime', 'config', 'data', 'workspace']) mkdirSync(join(directory, name), { mode: 0o700 });
Object.assign(env, {
  XDG_RUNTIME_DIR: join(directory, 'runtime'), XDG_CONFIG_HOME: join(directory, 'config'),
  XDG_DATA_HOME: join(directory, 'data'), WGPU_BACKEND: 'gl', WMCTERM_UI_LANGUAGE: 'en',
});
const app = spawn(binary, ['gui'], { env, cwd: join(directory, 'workspace'), stdio: 'ignore' });
const appExit = once(app, 'exit');
const children = new Set();
const rpc = (method, params = {}) => JSON.parse(execFileSync(binary, ['rpc', method, JSON.stringify(params)], {
  env, stdio: ['ignore', 'pipe', 'pipe'], timeout: 5000,
}));
const wait = async (predicate, message) => {
  for (let i = 0; i < 100; i++) {
    try { const value = predicate(); if (value) return value; } catch (error) { if (i === 99) throw error; }
    await new Promise(resolve => setTimeout(resolve, 50));
  }
  throw new Error(message);
};

try {
  const workspace = await wait(() => rpc('workspace.list')[0]?.id, 'isolated GUI did not start');
  const surface = rpc('surface.list', { workspace })[0].id;
  const terminal = rpc('app.diagnostics').terminals.find(item => item.surface_id === surface);
  const hookEnv = { ...env, WMCTERM: '1', WMCTERM_SURFACE_ID: surface,
    WMCTERM_WORKSPACE_ID: workspace, WMCTERM_TERMINAL_LIFECYCLE_ID: terminal.lifecycle_id };
  const plan = JSON.parse(execFileSync(binary, ['hook', 'plan', '--agent', 'claude'], { env }));
  const hooks = JSON.parse(plan.snippets[0].content).hooks;
  assert.ok(Array.isArray(hooks.Elicitation[0].hooks[0].args));
  assert.equal(hooks.Elicitation[0].hooks[0].onFailure, 'block');
  const payload = (event, extra = {}) => ({ hook_event_name: event, session_id: 'input-smoke',
    cwd: join(directory, 'workspace'), permission_mode: 'default', ...extra });
  const receive = (event, extra = {}, context = hookEnv) => {
    const hook = hooks[event][0].hooks[0];
    return JSON.parse(execFileSync(hook.command, hook.args, { env: context,
      input: JSON.stringify(payload(event, extra)), timeout: 5000, stdio: ['pipe', 'pipe', 'pipe'] }));
  };
  const begin = extra => {
    const hook = hooks.Elicitation[0].hooks[0];
    const child = spawn(hook.command, hook.args, { env: hookEnv, stdio: ['pipe', 'pipe', 'pipe'] });
    children.add(child);
    let stdout = '';
    let stderr = '';
    child.stdout.on('data', data => { stdout += data; });
    child.stderr.on('data', data => { stderr += data; });
    const exited = once(child, 'exit');
    child.stdin.end(JSON.stringify(payload('Elicitation', extra)));
    return async () => {
      const timer = setTimeout(() => child.kill('SIGKILL'), 5000);
      try {
        const [code] = await exited;
        assert.equal(code, 0, `hook failed: ${stderr}`);
        return JSON.parse(stdout);
      } finally { clearTimeout(timer); children.delete(child); }
    };
  };
  receive('SessionStart');
  await wait(() => rpc('surface.list', { workspace }).find(item => item.id === surface)?.agent?.session_id === 'input-smoke',
    'Claude SessionStart was not observed');
  const schema = { type: 'object', required: ['count', 'enabled', 'mode'], properties: {
    count: { type: 'integer', title: 'Count' }, enabled: { type: 'boolean', title: 'Enabled' },
    mode: { type: 'string', enum: ['safe', 'fast'], title: 'Mode' }, label: { type: 'string', title: 'Label' },
  } };
  const form = (id, extra = {}) => ({ mode: 'form', mcp_server_name: 'smoke-server', elicitation_id: id,
    message: 'Configure local fixture', requested_schema: schema, ...extra });
  const nextRequest = () => wait(() => rpc('agent.request.list').find(item => item.kind === 'input'), 'input hook did not register');
  const reply = (request, text) => rpc('agent.request.reply', { request_id: request.request_id,
    resolution: { text: JSON.stringify(text) } });

  const answered = begin(form('typed'));
  const request = await nextRequest();
  assert.equal(request.surface_id, surface);
  assert.equal(request.lifecycle_id, terminal.lifecycle_id);
  // Marking notification history read must preserve the actionable form.
  for (const notification of rpc('notification.list')) rpc('notification.mark', { id: notification.id, read: true });
  assert.equal(rpc('agent.request.list')[0].request_id, request.request_id);
  reply(request, { count: '3', enabled: 'false', mode: 'safe', label: '\u4e2d\u6587\ud83d\ude42' });
  assert.deepEqual((await answered()).hookSpecificOutput, { hookEventName: 'Elicitation', action: 'accept',
    content: { count: 3, enabled: false, mode: 'safe', label: '\u4e2d\u6587\ud83d\ude42' } });
  reply(request, { count: '4', enabled: 'true', mode: 'fast' });
  assert.equal(JSON.parse(rpc('agent.input.poll', { request_id: request.request_id }).resolution.text).count, '3',
    'late replies must not replace the first answer');

  const repeated = begin(form('duplicate-native-id'));
  const repeatedRequest = await nextRequest();
  const repeatedAgain = begin(form('duplicate-native-id'));
  await new Promise(resolve => setTimeout(resolve, 200));
  assert.equal(rpc('agent.request.list').length, 1, 'duplicate native IDs must share one actionable request');
  reply(repeatedRequest, { count: '2', enabled: 'true', mode: 'safe' });
  assert.equal((await repeated()).hookSpecificOutput.action, 'accept');
  assert.deepEqual(await repeatedAgain(), { hookSpecificOutput: {
    hookEventName: 'Elicitation', action: 'accept', content: { count: 2, enabled: true, mode: 'safe' },
  } });

  const cancelled = begin(form('cancelled'));
  const cancelRequest = await nextRequest();
  rpc('agent.request.reply', { request_id: cancelRequest.request_id, resolution: { choice_id: 'deny', approved: false } });
  assert.equal((await cancelled()).hookSpecificOutput.action, 'cancel');

  // Native results reconcile only the exact session/server/elicitation tuple.
  const native = begin(form('native-result'));
  const nativeRequest = await nextRequest();
  const other = begin(form('other-result'));
  await wait(() => rpc('agent.request.list').length === 2, 'concurrent form did not register');
  receive('ElicitationResult', { mcp_server_name: 'different-server', elicitation_id: 'native-result', action: 'cancel' });
  receive('ElicitationResult', { mcp_server_name: 'smoke-server', action: 'cancel' });
  receive('ElicitationResult', { mcp_server_name: 'smoke-server', elicitation_id: 'native-result', action: 'cancel' },
    { ...hookEnv, WMCTERM_TERMINAL_LIFECYCLE_ID: 'stale-lifecycle' });
  assert.equal(rpc('agent.request.list').length, 2);
  receive('ElicitationResult', { mcp_server_name: 'smoke-server', elicitation_id: 'native-result', action: 'cancel' });
  assert.equal((await native()).hookSpecificOutput.action, 'cancel');
  const remaining = rpc('agent.request.list');
  assert.equal(remaining.length, 1);
  assert.notEqual(remaining[0].request_id, nativeRequest.request_id);
  reply(remaining[0], { count: '1', enabled: 'true', mode: 'fast' });
  assert.equal((await other()).hookSpecificOutput.action, 'accept');

  for (const extra of [
    { mode: 'url', url: 'https://example.invalid/auth' },
    { requested_schema: { type: 'object', properties: { password: { type: 'string', format: 'password' } } } },
    { requested_schema: { type: 'object', properties: { count: { type: 'integer', minimum: 1 } } } },
  ]) assert.deepEqual(receive('Elicitation', form('native', extra)), {});
  assert.deepEqual(receive('Elicitation', form('outside'), env), {});
  assert.equal(receive('Elicitation', form('stale'), { ...hookEnv,
    WMCTERM_TERMINAL_LIFECYCLE_ID: 'stale-lifecycle' }).hookSpecificOutput.action, 'cancel');
  assert.equal(receive('Elicitation', form('stale-session', { session_id: 'other-session' })).hookSpecificOutput.action, 'cancel');
  assert.deepEqual(rpc('agent.request.list'), []);
  console.log('Claude centralized MCP input, cancellation, exact reconciliation, stale routing, and native fallback smoke passed');
} finally {
  for (const child of children) child.kill('SIGKILL');
  if (app.exitCode === null && app.signalCode === null) app.kill('SIGTERM');
  const timer = setTimeout(() => app.kill('SIGKILL'), 5000);
  try { await appExit; } finally { clearTimeout(timer); rmSync(directory, { recursive: true, force: true }); }
}
