// Run with the release binary inside a --network=none container. All test state
// is private and temporary. No account credentials or external service is used.
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { once } from 'node:events';
import { mkdtemp, readFile, writeFile, rm, readdir } from 'node:fs/promises';
import http from 'node:http';
import os from 'node:os';
import path from 'node:path';

const artifact = process.argv[2];
assert(artifact, 'pass the release binary path');
assert(Object.values(os.networkInterfaces()).flat().every(address => address.internal), 'run this check in a --network=none container');
const directory = await mkdtemp(path.join(os.tmpdir(), 'frona-provider-artifact-'));
const requests = [];
const denied = [];
let logs = '';
let child;
let token;
const pause = ms => new Promise(resolve => setTimeout(resolve, ms));
const secrets = ['fixture-key-a-025', 'fixture-key-b-025', 'fixture-invalid-key-025'];
function noSecrets(value) {
  const text = typeof value === 'string' ? value : JSON.stringify(value);
  for (const secret of secrets) assert(!text.includes(secret), 'credential leaked');
}
async function listen(server) {
  server.listen(0, '127.0.0.1');
  await once(server, 'listening');
  return `http://127.0.0.1:${server.address().port}`;
}
const provider = http.createServer(async (req, res) => {
  let raw = '';
  for await (const chunk of req) raw += chunk;
  const body = raw ? JSON.parse(raw) : null;
  requests.push({ url: req.url, method: req.method, auth: req.headers.authorization, body });
  const account = req.url.startsWith('/a/') ? 'a' : 'b';
  if (req.headers.authorization !== `Bearer fixture-key-${account}-025`) {
    res.writeHead(401).end('invalid fixture credential');
    return;
  }
  if (req.url.endsWith('/models')) {
    res.setHeader('content-type', 'application/json');
    res.end(JSON.stringify({ data: [{ id: 'fixture-listed', object: 'model', created: 0, owned_by: 'fixture' }] }));
    return;
  }
  assert(req.url.endsWith('/chat/completions'), 'unexpected inference path');
  assert.equal(body.model, 'manual-unlisted');
  assert.equal(body.max_completion_tokens, 77);
  assert.deepEqual(body.fixture, { nested: [null, 1], is_set: true });
  const text = `artifact-answer-${account}`;
  if (body.stream) {
    res.setHeader('content-type', 'text/event-stream');
    for (const chunk of [
      { choices: [{ index: 0, delta: { role: 'assistant', content: text }, finish_reason: null }] },
      { choices: [{ index: 0, delta: {}, finish_reason: 'stop' }], usage: { prompt_tokens: 2, completion_tokens: 3, total_tokens: 5 } },
    ]) res.write(`data: ${JSON.stringify({ id: 'fixture', object: 'chat.completion.chunk', created: 0, model: body.model, ...chunk })}\n\n`);
    res.end('data: [DONE]\n\n');
  } else {
    res.setHeader('content-type', 'application/json');
    res.end(JSON.stringify({ id: 'fixture', object: 'chat.completion', created: 0, model: body.model,
      choices: [{ index: 0, message: { role: 'assistant', content: text }, finish_reason: 'stop' }],
      usage: { prompt_tokens: 2, completion_tokens: 3, total_tokens: 5 } }));
  }
});
const proxy = http.createServer((req, res) => { denied.push(req.url); res.writeHead(403).end(); });
proxy.on('connect', (req, socket) => { denied.push(req.url); socket.end('HTTP/1.1 403 Forbidden\r\n\r\n'); });
const providerUrl = await listen(provider);
const proxyUrl = await listen(proxy);
const portReservation = http.createServer();
const base = await listen(portReservation);
await new Promise(resolve => portReservation.close(resolve));
const port = new URL(base).port;
const configFile = path.join(directory, 'config.yaml');
const connection = account => ({ provider: 'openai', base_url: `${providerUrl}/${account}` });
await writeFile(configFile, JSON.stringify({
  sandbox: { disabled: true, default_network_access: false },
  browser: { ws_url: 'ws://127.0.0.1:9' },
  providers: { account: { ...connection('a'), api_key: secrets[0] } },
  models: { primary: { provider: 'account', model: 'manual-unlisted', api: 'completions', max_tokens: 77,
    extra_params: { fixture: { nested: [null, 1], is_set: true } }, retry: { max_retries: 0 } } },
}));

async function api(route, method = 'GET', body, expected) {
  const response = await fetch(base + route, { method, signal: AbortSignal.timeout(30000), headers: {
    'content-type': 'application/json', ...(token ? { authorization: `Bearer ${token}` } : {}),
  }, body: body === undefined ? undefined : JSON.stringify(body) });
  const text = await response.text();
  noSecrets(text);
  assert(expected ? response.status === expected : response.ok, `${method} ${route}: ${response.status}: ${text.slice(0, 500)}`);
  return text ? JSON.parse(text) : null;
}
async function boot() {
  const loader = process.env.FRONA_ARTIFACT_LOADER;
  const args = loader ? ['--library-path', '/host-libs', artifact] : [];
  child = spawn(loader || artifact, args, { cwd: directory, stdio: ['ignore', 'pipe', 'pipe'], env: {
    PATH: '/usr/local/bin:/usr/bin:/bin', FRONA_CONFIG: configFile,
    FRONA_SERVER_DATA_DIR: directory, FRONA_SERVER_PORT: port,
    FRONA_SERVER_STATIC_DIR: '/artifact/static', FRONA_SERVER_SHUTDOWN_TIMEOUT_SECS: '3',
    FRONA_STORAGE_SHARED_CONFIG_DIR: '/artifact',
    FRONA_AUTH_ENCRYPTION_SECRET: 'fixture-encryption-secret-025', FRONA_LOG_LEVEL: 'info',
    HTTP_PROXY: proxyUrl, HTTPS_PROXY: proxyUrl, ALL_PROXY: proxyUrl, NO_PROXY: '127.0.0.1,localhost',
  } });
  for (const stream of [child.stdout, child.stderr]) stream.on('data', chunk => { logs += chunk; assert(logs.length < 8 * 1024 * 1024); });
  for (let attempt = 0; attempt < 240; attempt++) {
    assert(child.exitCode === null, `server exited: ${logs.slice(-4000)}`);
    try {
      const response = await fetch(base + '/api/auth/config', { signal: AbortSignal.timeout(1000) });
      if (response.ok) return;
    } catch { /* not listening yet */ }
    await pause(250);
  }
  throw new Error(`server did not become ready: ${logs.slice(-4000)}`);
}
async function stop() {
  if (!child || child.exitCode !== null) return;
  const exited = once(child, 'exit');
  child.kill('SIGTERM');
  const timer = setTimeout(() => child.kill('SIGKILL'), 6000);
  await exited;
  clearTimeout(timer);
}
async function turn(chat, account) {
  const before = requests.filter(request => request.method === 'POST').length;
  const existing = new Set((await api(`/api/chats/${chat}/messages`)).messages.map(message => message.id));
  await api(`/api/chats/${chat}/messages/stream`, 'POST', { content: 'Reply with the fixture answer.' });
  for (let attempt = 0; attempt < 120; attempt++) {
    const messages = await api(`/api/chats/${chat}/messages`);
    if (messages.messages.some(message => !existing.has(message.id) && message.role === 'agent' && message.content === `artifact-answer-${account}` && message.status === 'completed')) {
      assert(requests.filter(request => request.method === 'POST').length > before);
      return;
    }
    await pause(250);
  }
  throw new Error(`inference did not complete: ${logs.slice(-2500)}`);
}

try {
  assert((await readdir(directory)).length === 1, 'metadata cache must start empty');
  await boot();
  const setupPage = await fetch(base + '/setup');
  assert(setupPage.ok && (await setupPage.text()).toLowerCase().includes('<!doctype html'), 'static production frontend is unavailable');
  const registration = await api('/api/auth/register', 'POST', { handle: 'artifact-admin', email: 'artifact@example.test', name: 'Artifact', password: 'Fixture-password-025!' });
  token = registration.token;
  const catalog = await api('/api/config/provider-catalog');
  assert(catalog.providers.length > 5, 'bundled authoring seeds unavailable');
  assert(!catalog.providers.some(provider => provider.id?.startsWith('google-vertex')));
  for (const source of Object.values(catalog.source_status)) assert.equal(source.origin, 'bundled');
  for (const brand of ['anthropic', 'google']) {
    assert(catalog.providers.find(provider => provider.id === brand).auth_methods.every(method => method.credential_method === 'api_key'));
  }
  const agent = await api('/api/agents', 'POST', { name: 'Artifact fixture', description: 'Release acceptance', model_group: 'primary', tools: [], skills: [] });
  await api(`/api/agents/${agent.id}`, 'PUT', { prompt: 'Reply with the fixture answer. Do not call tools.' });
  const chat = await api('/api/chats', 'POST', { agent_id: agent.id, title: 'Artifact acceptance' });
  await turn(chat.id, 'a');
  // Wait out the bounded background refresh before measuring inference calls.
  await pause(11000);
  const remoteBefore = denied.length;
  const failed = await api('/api/config/providers/account/validate', 'POST', {
    config: connection('b'), credential: { source: 'api_key', api_key: secrets[2] },
  }, 400);
  noSecrets(failed);
  const proof = await api('/api/config/providers/account/validate', 'POST', {
    config: connection('b'), credential: { source: 'api_key', api_key: secrets[1] },
  });
  const current = await api('/api/config');
  const saved = await api('/api/config', 'PUT', { expected_persisted_revision: current.persisted_revision,
    validation_ids: [proof.validation_id], patch: { providers: { account: { ...connection('b'), api_key: null } } } });
  assert(saved.restart_required);
  await turn(chat.id, 'a');
  assert.equal(denied.length, remoteBefore, 'inference triggered a remote catalog call');
  await stop();
  await boot();
  await turn(chat.id, 'b');
  const listing = await api('/api/config/providers/account/models?manual_model=manual-unlisted');
  assert(listing.models.some(model => model.id === 'manual-unlisted'));
  assert.equal(listing.source, 'account');
  noSecrets(await readFile(configFile, 'utf8'));
  await stop();
  noSecrets(logs);
  console.log(JSON.stringify({ result: 'passed', artifact, network: 'container network none',
    inferenceRequests: requests.filter(request => request.method === 'POST').length,
    deniedRemoteRequests: denied, authoringBrands: catalog.providers.length,
    checks: ['empty cache boot', 'bundled seeds and notices', 'administrator setup', 'manual model inference', 'failed validation', 'draft save retains active endpoint/key', 'restart activates matching pair', 'custom JSON', 'no inference catalog calls', 'secret scan'] }, null, 2));
} finally {
  await stop();
  provider.closeAllConnections(); proxy.closeAllConnections();
  await Promise.all([new Promise(resolve => provider.close(resolve)), new Promise(resolve => proxy.close(resolve))]);
  await rm(directory, { recursive: true, force: true });
}
