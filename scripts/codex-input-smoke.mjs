import assert from 'node:assert/strict';
import { mkdtempSync, mkdirSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { spawn, execFileSync } from 'node:child_process';
import { once } from 'node:events';

// All RPC targets and processes belong to this isolated application.
const binary = resolve(process.argv[2] ?? 'target/release/wmcterm');
const scaleFactor = Number(process.env.WINIT_X11_SCALE_FACTOR ?? 1);
const directory = mkdtempSync(join(tmpdir(), 'wmcterm-codex-input-'));
const env = Object.fromEntries(Object.entries(process.env).filter(([key]) => !key.startsWith('WMCTERM_')));
for (const name of ['runtime', 'config', 'data', 'workspace']) mkdirSync(join(directory, name), { mode: 0o700 });
Object.assign(env, { XDG_RUNTIME_DIR: join(directory, 'runtime'), XDG_CONFIG_HOME: join(directory, 'config'), XDG_DATA_HOME: join(directory, 'data'), WGPU_BACKEND: 'gl' });
const app = spawn(binary, ['gui'], { env, cwd: join(directory, 'workspace'), stdio: 'ignore' });
const exited = once(app, 'exit');
let clipboardOwner;
const rpc = (method, params = {}) => JSON.parse(execFileSync(binary, ['rpc', method, JSON.stringify(params)], { env, timeout: 5000, stdio: ['ignore', 'pipe', 'pipe'] }));
const delay = ms => new Promise(resolve => setTimeout(resolve, ms));
const waitUntil = async predicate => {
  for (let attempt = 0; attempt < 100; attempt++) {
    const result = predicate();
    if (result) return result;
    await delay(50);
  }
  assert.fail('isolated input smoke condition timed out');
};
try {
  let workspace, surface;
  for (let i = 0; i < 100; i++) {
    try { workspace = rpc('workspace.list')[0].id; surface = rpc('surface.list', { workspace })[0]?.id; if (surface) break; } catch { /* Startup races are bounded below. */ }
    await delay(100);
  }
  assert.ok(surface, 'isolated application did not start');
  // Real PTY bytes traverse the scanner, terminal actor and UI fallback branch.
  const emitter = `import sys\nfor line in sys.stdin:\n report=line.strip(); print(chr(27)+"]7501;"+report+chr(7),end="",flush=True); print("OSC_APPLIED_"+report,flush=True)`;
  const oscSurface = rpc('surface.new', { pane: rpc('pane.list')[0].id, focus: false,
    command: `python3 -u -c '${emitter}'` }).surface_id;
  const oscTerminal = rpc('app.diagnostics').terminals.find(item => item.surface_id === oscSurface);
  const oscMetadata = () => rpc('surface.list', { workspace }).find(item => item.id === oscSurface).agent;
  const emit = async report => {
    rpc('send.line', { surface: oscSurface, text: report });
    await waitUntil(() => JSON.stringify(rpc('screen.read', { surface: oscSurface })).includes(`OSC_APPLIED_${report}`));
    await delay(100);
  };
  const notificationsBeforeOsc = rpc('notification.list').length;
  await emit('state=working:id=unknown');
  assert.ok(!oscMetadata(), 'anonymous OSC must not create Agent identity');
  rpc('agent.update', { event: 'session_start', agent: 'codex', session_id: 'osc-thread',
    surface: oscSurface, surface_id: oscSurface, lifecycle_id: oscTerminal.lifecycle_id });
  await emit('state=clear:id=unknown');
  await emit('state=done:id=known');
  await waitUntil(() => oscMetadata().state === 'completed');
  await emit('state=working:id=known');
  await waitUntil(() => oscMetadata().state === 'running');
  await emit('state=blocked:kind=question:id=known');
  await waitUntil(() => oscMetadata().state === 'waiting_for_input');
  assert.deepEqual(rpc('agent.request.list'), [], 'a blocked OSC report has no reply channel');
  await emit('state=working:id=known');
  const oscApproval = rpc('agent.approval.register', { source: 'codex_app_server', workspace_id: workspace, surface_id: oscSurface,
    request: { jsonrpc_id: 'osc-permission', method: 'item/permissions/requestApproval', kind: 'permissions', thread_id: 'osc-thread', turn_id: 'turn', item_id: 'osc', params: { permissions: {} } }, decisions: ['deny', 'once'] }).approval_id;
  await emit('state=done:id=known');
  assert.equal(oscMetadata().state, 'waiting_for_permission');
  assert.equal(rpc('agent.approval.poll', { approval_id: oscApproval }).status, 'pending');
  rpc('agent.request.cancel', { request_id: oscApproval });
  const oscInput = rpc('agent.input.register', { source: 'codex_app_server', workspace_id: workspace, surface_id: oscSurface,
    request: { jsonrpc_id: 'osc-input', method: 'item/tool/requestUserInput', kind: 'input', thread_id: 'osc-thread', turn_id: 'turn', item_id: 'osc-input',
      params: { id: 'osc-input', method: 'item/tool/requestUserInput', thread_id: 'osc-thread', turn_id: 'turn', item_id: 'osc-input', message: 'Input',
        fields: [{ id: 'answer', label: 'Answer', kind: 'answer', required: true, options: [] }] } }, decisions: ['deny', 'once'] }).approval_id;
  await emit('state=working:id=known');
  assert.equal(oscMetadata().state, 'waiting_for_input');
  assert.equal(rpc('agent.input.poll', { request_id: oscInput }).status, 'pending');
  rpc('agent.request.cancel', { request_id: oscInput });
  rpc('agent.update', { event: 'tool_use', agent: 'codex', tool: 'fixture', surface: oscSurface, surface_id: oscSurface,
    lifecycle_id: oscTerminal.lifecycle_id, hook_executable: binary, hook_started_at_unix_ns: 1 });
  const authoritativeState = oscMetadata().state;
  await emit('state=blocked:kind=question:id=known');
  assert.equal(oscMetadata().state, authoritativeState, 'identity-bearing hook must precede anonymous OSC');
  assert.equal(rpc('notification.list').length, notificationsBeforeOsc, 'OSC must not generate notifications');
  assert.deepEqual(rpc('agent.request.list'), [], 'OSC must not create actionable requests');
  console.log('PASS real PTY OSC fallback, unknown Agent, Hook precedence and pending permission/input precedence');
  const terminal = rpc('app.diagnostics').terminals.find(item => item.surface_id === surface);
  rpc('agent.update', { event: 'session_start', agent: 'codex', session_id: 'input-thread', surface, surface_id: surface, lifecycle_id: terminal.lifecycle_id });
  const register = (id, extra = {}) => rpc('agent.input.register', {
    source: 'codex_app_server', workspace_id: workspace, surface_id: surface, ...extra,
    request: { jsonrpc_id: id, method: 'item/tool/requestUserInput', kind: 'input', thread_id: 'input-thread', turn_id: 'turn', item_id: id,
      params: { id, method: 'item/tool/requestUserInput', thread_id: 'input-thread', turn_id: 'turn', item_id: id, message: '目标选择 / Target selection',
        fields: [{ id: 'target', label: '目标 / Target', kind: 'answer', required: true, options: ['test', 'production'] }] } },
    decisions: ['deny', 'once'],
  }).approval_id;
  const poll = request_id => rpc('agent.input.poll', { request_id });
  let id = register('question-one');
  assert.equal(register('question-one'), id, 'duplicate registration must preserve the ID');
  assert.equal(poll(id).status, 'pending');
  assert.equal(rpc('agent.request.list')[0].kind, 'input');
  assert.equal(rpc('surface.list', { workspace })[0].agent.state, 'waiting_for_input');
  rpc('notify', { title: 'unrelated', body: 'unrelated', surface });
  for (const notification of rpc('notification.list')) rpc('notification.mark', { id: notification.id, read: true });
  assert.equal(poll(id).status, 'pending');
  assert.throws(() => rpc('agent.request.reply', { request_id: id, resolution: { text: '{}' } }), /answer is required/);
  const resolution = { text: JSON.stringify({ target: '测试🙂' }) };
  rpc('agent.request.reply', { request_id: id, resolution });
  assert.deepEqual(poll(id).resolution, resolution);
  rpc('agent.request.reply', { request_id: id, resolution: { text: JSON.stringify({ target: 'late' }) } });
  assert.deepEqual(poll(id).resolution, resolution, 'late replies must not replace an answer');
  id = register('question-ui');
  const window = execFileSync('xdotool', ['search', '--pid', String(app.pid), '--name', '^wmcterm$'], { encoding: 'utf8' }).trim().split('\n')[0];
  assert.ok(window);
  execFileSync('xdotool', ['windowsize', window, String(1000 * scaleFactor), String(650 * scaleFactor)]);
  execFileSync('xdotool', ['windowfocus', '--sync', window]);
  await delay(200);
  execFileSync('xdotool', ['key', '--clearmodifiers', 'Right', 'Return']);
  await delay(300);
  const screenshot = resolve(process.argv[3] ?? '/tmp/wmcterm-codex-input.png');
  execFileSync('import', ['-window', window, screenshot]);
  execFileSync('xdotool', ['windowsize', window, String(640 * scaleFactor), String(480 * scaleFactor)]);
  await delay(300);
  execFileSync('import', ['-window', window, screenshot.replace(/\.png$/, '-compact.png')]);
  execFileSync('xdotool', ['key', '--clearmodifiers', 'Down', 'Return']);
  for (let i = 0; i < 50 && poll(id).status === 'pending'; i++) await delay(100);
  assert.equal(poll(id).resolution.text, JSON.stringify({ target: 'test' }), 'GUI option selection must submit the selected answer');
  const strict = rpc('agent.approval.register', { source: 'codex_app_server', workspace_id: workspace, surface_id: surface,
    request: { jsonrpc_id: 'strict-one', method: 'item/permissions/requestApproval', kind: 'permissions', thread_id: 'input-thread', turn_id: 'turn', item_id: 'permissions', params: { permissions: { network: { enabled: true } } } },
    decisions: ['deny', 'once', 'always', 'strict'],
  }).approval_id;
  execFileSync('xdotool', ['key', '--clearmodifiers', 'Left', 'Left', 'Left', 'Right', 'Right', 'Right', 'Return']);
  for (let i = 0; i < 50 && rpc('agent.approval.poll', { approval_id: strict }).status === 'pending'; i++) await delay(100);
  assert.equal(rpc('agent.approval.poll', { approval_id: strict }).decision.mode, 'strict');
  assert.equal(poll(id).resolution.text, JSON.stringify({ target: 'test' }), 'another approval must not discard a completed input reply');
  // Primitive MCP fields exercise the GUI's sequential editor and validation retry.
  const registerForm = (name, fields, extra = {}) => rpc('agent.input.register', {
    source: 'codex_app_server', workspace_id: workspace, surface_id: surface, ...extra,
    request: { jsonrpc_id: name, method: 'mcpServer/elicitation/request', kind: 'input', thread_id: 'input-thread', turn_id: 'turn', item_id: name,
      params: { id: name, method: 'mcpServer/elicitation/request', thread_id: 'input-thread', turn_id: 'turn', item_id: name, message: 'Configure', fields } }, decisions: ['deny', 'once'],
  }).approval_id;
  const field = (id, kind, required = true, options = []) => ({ id, label: id, kind, required, options });
  const keys = (...values) => execFileSync('xdotool', ['key', '--clearmodifiers', ...values]);
  const openAnswer = async () => { keys('Left', 'Right', 'Return'); await delay(150); };
  id = registerForm('multi-gui', [field('count', 'integer'), field('note', 'string', false), field('enabled', 'boolean', true, ['true', 'false'])]);
  await openAnswer();
  execFileSync('xdotool', ['type', '--clearmodifiers', '3.5']);
  keys('Return');
  await delay(150);
  assert.equal(poll(id).status, 'pending');
  keys('ctrl+a');
  execFileSync('xdotool', ['type', '--clearmodifiers', '3']);
  keys('Return');
  await delay(100);
  keys('Return'); // Optional note omitted.
  await delay(100);
  keys('Down', 'Return');
  await waitUntil(() => poll(id).status !== 'pending');
  assert.deepEqual(JSON.parse(poll(id).resolution.text), { count: '3', note: '', enabled: 'true' });
  id = registerForm('unicode-editor', [field('value', 'string')]);
  await openAnswer();
  clipboardOwner = spawn('xclip', ['-selection', 'clipboard', '-quiet'], { stdio: ['pipe', 'ignore', 'ignore'] });
  clipboardOwner.stdin.end('中文🙂e\u0301');
  await delay(200);
  keys('ctrl+v');
  await delay(100);
  keys('Return');
  await waitUntil(() => poll(id).status !== 'pending');
  assert.equal(JSON.parse(poll(id).resolution.text).value, '中文🙂e\u0301');
  clipboardOwner.kill('SIGTERM');
  // Long bilingual labels and all 32 options: select the final option, then click it.
  const options = Array.from({ length: 32 }, (_, index) => `选项${index} / ${'long label '.repeat(12)}`);
  id = registerForm('long-options', [{ ...field('choice', 'string', true, options), label: '很长的问题 / Long question '.repeat(40) }]);
  await openAnswer();
  for (let index = 0; index < 32; index++) keys('Down');
  await delay(150);
  execFileSync('import', ['-window', window, screenshot.replace(/\.png$/, '-long-options.png')]);
  // Six visible rows occupy the lower portion of the centered 276px prompt.
  execFileSync('xdotool', ['mousemove', '--window', window, String(320 * scaleFactor), String(324 * scaleFactor), 'click', '1']);
  keys('Return');
  await waitUntil(() => poll(id).status !== 'pending');
  assert.equal(JSON.parse(poll(id).resolution.text).choice, options[31]);
  for (const expire of [false, true]) {
    id = registerForm(expire ? 'expire-editor' : 'cancel-editor', [field('value', 'string')], expire ? { deadline_unix_ms: Date.now() + 1000 } : {});
    await openAnswer();
    if (expire) await waitUntil(() => poll(id).status === 'expired');
    else rpc('agent.request.cancel', { request_id: id });
    keys('Return');
    assert.equal(poll(id).status, expire ? 'expired' : 'cancelled');
    // Escape closes an obsolete editor without completing another request.
    keys('Escape');
  }
  console.log('PASS multi-field GUI, numeric retry, optional blank, 32 options and mouse selection');
  id = register('question-cancel');
  rpc('agent.request.cancel', { request_id: id });
  assert.equal(poll(id).status, 'cancelled');
  id = register('question-expire', { deadline_unix_ms: 1 });
  for (let i = 0; i < 50 && poll(id).status === 'pending'; i++) await delay(100);
  assert.equal(poll(id).status, 'expired');
  id = register('question-stale');
  rpc('agent.update', { event: 'session_start', agent: 'codex', session_id: 'replacement-thread', surface, surface_id: surface, lifecycle_id: terminal.lifecycle_id });
  assert.equal(poll(id).status, 'cancelled');
  console.log('PASS Codex input GUI/menu, strict review GUI, routing, reply validation, notification independence, duplicates, cancellation, expiry and stale sessions');
} finally {
  if (clipboardOwner && clipboardOwner.exitCode === null) clipboardOwner.kill('SIGTERM');
  if (app.exitCode === null && app.signalCode === null) app.kill('SIGTERM');
  await exited;
  rmSync(directory, { recursive: true, force: true });
}
