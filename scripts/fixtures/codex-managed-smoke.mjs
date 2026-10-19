// Real app-server, deterministic TUI client, and no model turns.
import assert from 'node:assert/strict';
import { appendFileSync } from 'node:fs';

const args = process.argv.slice(2);
assert(!args.some(arg => ['--approve-for-me', '--sandbox', '--ask-for-approval', '--dangerously-bypass-approvals-and-sandbox'].includes(arg)));
const endpoint = new URL(args[args.indexOf('--remote') + 1]);
assert.equal(endpoint.pathname, '/');
assert.equal(endpoint.search, '');
const token = process.env[args[args.indexOf('--remote-auth-token-env') + 1]];
assert(token);
const socket = new WebSocket(endpoint, { headers: { Authorization: `Bearer ${token}` } });
const pending = new Map();
let completedTurn;
const turnCompleted = new Promise(resolve => { completedTurn = resolve; });
let sequence = 0;
const timeout = setTimeout(() => { console.error('managed Codex smoke timed out'); process.exit(1); }, 20000);
socket.addEventListener('message', event => {
  const message = JSON.parse(event.data);
  if (message.method === 'turn/completed') completedTurn(message.params);
  if (!message.method && pending.has(message.id)) {
    const { resolve, reject } = pending.get(message.id);
    pending.delete(message.id);
    if (message.error) reject(new Error(JSON.stringify(message.error)));
    else resolve(message.result);
  }
});
const request = (method, params) => new Promise((resolve, reject) => {
  const id = ++sequence;
  pending.set(id, { resolve, reject });
  socket.send(JSON.stringify({ id, method, params }));
});
socket.addEventListener('open', async () => {
  try {
    await request('initialize', { clientInfo: { name: 'wmcterm_smoke_tui', version: '1' }, capabilities: { experimentalApi: true } });
    socket.send(JSON.stringify({ method: 'initialized' }));
    const resume = args.indexOf('resume');
    const result = resume >= 0
      ? await request('thread/resume', { threadId: args[resume + 1] })
      : await request('thread/start', { cwd: process.cwd() });
    const threadId = result.thread.id;
    if (process.env.WMCTERM_TEST_INPUT_ROUNDTRIP === '1') {
      const started = await request('turn/start', {
        threadId, input: [{ type: 'text', text: 'Ask for a target', text_elements: [] }],
        collaborationMode: { mode: 'plan', settings: { model: 'mock-model', reasoning_effort: 'medium', developer_instructions: null } },
      });
      const completed = await turnCompleted;
      assert.equal(completed.turn.id, started.turn.id);
      assert.equal(completed.turn.status, 'completed');
      appendFileSync(process.env.WMCTERM_TEST_RESULTS, JSON.stringify({ threadId, completed: true }) + '\n');
      clearTimeout(timeout);
      socket.close();
      return;
    }
    for (const method of ['thread/settings/update', 'turn/settings/update']) {
      await assert.rejects(request(method, { threadId, turnId: 'no-active-turn', approvalsReviewer: 'user' }), /fixed at launch/);
      await assert.rejects(request(method, { threadId: 'unverified-thread', model: 'test-model' }), /not been verified/);
    }
    await assert.rejects(request('thread/settings/update', { threadId, permissions: 'full-access' }), /fixed at launch/);
    await assert.rejects(request('thread/settings/update', { threadId, cwd: '/different-workspace' }), /fixed at launch/);
    await assert.rejects(request('thread/settings/update', { threadId, runtimeWorkspaceRoots: ['/different-workspace'] }), /fixed at launch/);
    // Invalid input also prevents a model turn if a broken proxy forwards these probes.
    for (const workspace of [{ cwd: '/different-workspace' }, { runtimeWorkspaceRoots: ['/different-workspace'] }]) {
      await assert.rejects(request('turn/start', { threadId, input: [{ type: 'wmcterm_invalid_fixture' }], ...workspace }), /fixed after thread verification/);
    }
    await request('thread/settings/update', { threadId, effort: 'high' });
    await request('thread/inject_items', { threadId, items: [{ type: 'message', role: 'user', content: [{ type: 'input_text', text: 'wmcterm restore smoke fixture; no model turn.' }] }] });
    // The same RPC used by the in-session /resume path.
    const resumed = await request('thread/resume', { threadId });
    assert.equal(resumed.approvalPolicy, result.approvalPolicy);
    assert.equal(resumed.approvalsReviewer, result.approvalsReviewer);
    assert.deepEqual(resumed.sandbox, result.sandbox);
    appendFileSync(process.env.WMCTERM_TEST_RESULTS, JSON.stringify({
      threadId, policy: resumed.approvalPolicy, reviewer: resumed.approvalsReviewer,
      sandbox: resumed.sandbox.type, resumed: resume >= 0,
    }) + '\n');
    clearTimeout(timeout);
    socket.close();
  } catch (error) {
    console.error(error);
    process.exitCode = 1;
    clearTimeout(timeout);
    socket.close();
  }
});
