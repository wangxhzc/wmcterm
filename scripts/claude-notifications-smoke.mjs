import assert from 'node:assert/strict';
import { mkdtempSync, mkdirSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { spawn, execFileSync } from 'node:child_process';
import { once } from 'node:events';

// Run under xvfb-run; only this temporary application's RPC socket is used.
const binary = resolve(process.argv[2] ?? 'target/release/wmcterm');
const directory = mkdtempSync(join(tmpdir(), 'wmcterm-claude-notifications-'));
const env = Object.fromEntries(Object.entries(process.env).filter(([key]) => !key.startsWith('WMCTERM_')));
for (const name of ['runtime', 'config', 'data', 'workspace']) {
  mkdirSync(join(directory, name), { mode: 0o700 });
}
Object.assign(env, {
  XDG_RUNTIME_DIR: join(directory, 'runtime'), XDG_CONFIG_HOME: join(directory, 'config'),
  XDG_DATA_HOME: join(directory, 'data'), WGPU_BACKEND: 'gl',
});
const app = spawn(binary, ['gui'], { env, cwd: join(directory, 'workspace'), stdio: 'ignore' });
const exited = once(app, 'exit');
let permission;
const rpc = (method, params = {}) => JSON.parse(execFileSync(binary, [
  'rpc', method, JSON.stringify(params),
], { env, stdio: ['ignore', 'pipe', 'pipe'], timeout: 5000 }));
try {
  let surfaces;
  let workspace;
  for (let i = 0; i < 100; i++) {
    try {
      workspace = rpc('workspace.list')[0].id;
      surfaces = rpc('surface.list', { workspace });
      if (surfaces.length) break;
    } catch (error) {
      if (i === 99) throw error;
    }
    await new Promise(resolve => setTimeout(resolve, 100));
  }
  assert.ok(surfaces?.length, 'isolated GUI did not start');
  const surface = surfaces[0].id;
  const metadata = () => rpc('surface.list', { workspace }).find(item => item.id === surface).agent;
  const terminal = rpc('app.diagnostics').terminals.find(item => item.surface_id === surface);
  const hookEnv = { ...env, WMCTERM: '1', WMCTERM_SURFACE_ID: surface,
    WMCTERM_WORKSPACE_ID: workspace, WMCTERM_TERMINAL_LIFECYCLE_ID: terminal.lifecycle_id };
  const launchEnv = { ...hookEnv, CLAUDE_CONFIG_DIR: join(directory, 'claude') };
  delete launchEnv.CLAUDE_CODE_SIMPLE;
  delete launchEnv.CLAUDE_CODE_SAFE_MODE;
  for (const options of [
    ['--bare'], ['--safe-mode'], ['--restricted'], ['--setting-sources=user'],
    ['--settings', '{"disableAllHooks":true}'],
    ['--settings={"allowManagedHooksOnly":true}'],
  ]) {
    assert.throws(() => execFileSync(binary, ['agent', 'claude', ...options, '--version'], {
      env: launchEnv, cwd: join(directory, 'workspace'), timeout: 5000, stdio: ['ignore', 'pipe', 'pipe'],
    }), /disable wmcterm hooks/);
  }
  for (const flag of ['CLAUDE_CODE_SIMPLE', 'CLAUDE_CODE_SAFE_MODE']) {
    assert.throws(() => execFileSync(binary, ['agent', 'claude', '--version'], {
      env: { ...launchEnv, [flag]: '1' }, cwd: join(directory, 'workspace'),
      timeout: 5000, stdio: ['ignore', 'pipe', 'pipe'],
    }), /disable wmcterm hooks/);
  }
  const plan = JSON.parse(execFileSync(binary, ['hook', 'plan', '--agent', 'claude'], { env }));
  const hooks = JSON.parse(plan.snippets[0].content).hooks;
  assert.equal(hooks.PermissionRequest[0].hooks[0].onFailure, 'block');
  for (const option of ['--bg', '--background', '--desktop', '--cloud', '--tmux=classic']) {
    assert.throws(() => execFileSync(binary, ['agent', 'claude', option, '--version'], {
      env: launchEnv, cwd: join(directory, 'workspace'), timeout: 5000, stdio: ['ignore', 'pipe', 'pipe'],
    }), /detaches from the managed terminal/);
  }
  const receive = (event, payload) => execFileSync('/bin/sh', ['-c', hooks[event][0].hooks[0].command], {
    env: hookEnv, input: JSON.stringify({ session_id: 'smoke-session', ...payload }), timeout: 5000,
  });
  const hook = (notification_type, _event, _extra, permission_mode = 'auto') => receive('Notification', {
    notification_type, message: 'smoke notification', permission_mode,
  });
  receive('SessionStart', { permission_mode: 'default' });
  receive('UserPromptSubmit', { prompt: 'smoke prompt', permission_mode: 'default' });
  assert.equal(metadata().state, 'running');
  receive('Stop', { permission_mode: 'default' });
  hook('agent_completed', 'notification', { message: 'smoke notification' });
  assert.equal(metadata().state, 'idle');
  assert.equal(metadata().run_mode, 'unattended');
  for (const kind of ['elicitation_dialog', 'elicitation_url_dialog', 'quota_auto_resume_stale']) {
    hook(kind, 'needs_input', { message: 'smoke notification' });
    assert.equal(metadata().state, 'waiting_for_input');
    assert.deepEqual(rpc('agent.request.list'), []);
    hook('auth_success', 'notification', { message: 'smoke notification' });
    assert.equal(metadata().state, 'waiting_for_input');
    hook('quota_auto_resume_fired', 'tool_use', { tool: 'smoke notification' });
    assert.equal(metadata().state, 'running');
    assert.deepEqual(rpc('agent.request.list'), []);
  }
  hook('agent_completed', 'notification', { message: 'smoke notification' }, 'bypassPermissions');
  assert.equal(metadata().state, 'running');
  assert.equal(metadata().run_mode, 'full-access');
  hook('permission_prompt', 'permission_request', { message: 'smoke notification' });
  assert.equal(metadata().state, 'waiting_for_permission');
  assert.deepEqual(rpc('agent.request.list'), []);
  hook('auth_success', 'notification', { message: 'smoke notification' }, 'default');
  assert.equal(metadata().run_mode, 'interactive');
  const diagnostics = JSON.parse(execFileSync(binary, ['diagnostics'], { env, timeout: 10000 }));
  const observed = diagnostics.terminals.find(item => item.surface_id === surface);
  assert.equal(observed.last_hook.executable, binary);
  assert.ok(observed.last_hook.started_at_unix_ns > 0);
  assert.equal(diagnostics.client_executable, binary);
  assert.equal(diagnostics.server_executable, binary);
  receive('SessionStart', { session_id: 'second-session', permission_mode: 'default' });
  assert.equal(metadata().session_id, 'second-session');
  receive('SessionStart', { permission_mode: 'auto' });
  assert.equal(metadata().session_id, 'smoke-session');
  permission = spawn('/bin/sh', ['-c', `exec ${hooks.PermissionRequest[0].hooks[0].command}`], {
    env: hookEnv, stdio: ['pipe', 'pipe', 'pipe'],
  });
  let permissionOutput = '';
  let permissionError = '';
  permission.stdout.on('data', bytes => { permissionOutput += bytes.toString(); });
  permission.stderr.on('data', bytes => { permissionError += bytes.toString(); });
  const permissionExit = once(permission, 'exit');
  permission.stdin.end(JSON.stringify({ hook_event_name: 'PermissionRequest', session_id: 'smoke-session',
    cwd: join(directory, 'workspace'), permission_mode: 'default', tool_name: 'Bash', tool_use_id: 'smoke-tool', tool_input: { command: 'pwd' } }));
  let request;
  for (let i = 0; i < 100; i++) {
    request = rpc('agent.request.list')[0];
    if (request) break;
    await new Promise(resolve => setTimeout(resolve, 50));
  }
  assert.ok(request, `real PermissionRequest Hook did not register: ${permissionOutput} ${permissionError}`);
  hook('auth_success', 'notification', {}, 'default');
  assert.equal(rpc('agent.request.list')[0].request_id, request.request_id);
  rpc('agent.request.reply', { request_id: request.request_id, resolution: { choice_id: 'once', approved: true } });
  const permissionTimer = setTimeout(() => permission.kill('SIGKILL'), 5000);
  try {
    assert.equal((await permissionExit)[0], 0);
    assert.equal(JSON.parse(permissionOutput).hookSpecificOutput.decision.behavior, 'allow');
  } finally { clearTimeout(permissionTimer); }
  console.log('Claude launch guards, notification and mode smoke passed');
} finally {
  if (permission && permission.exitCode === null && permission.signalCode === null) permission.kill('SIGKILL');
  if (app.exitCode === null && app.signalCode === null) app.kill('SIGTERM');
  const timer = setTimeout(() => app.kill('SIGKILL'), 5000);
  try { await exited; } finally {
    clearTimeout(timer);
    rmSync(directory, { recursive: true, force: true });
  }
}
