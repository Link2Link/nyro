#!/usr/bin/env node
/**
 * Real-browser smoke for failure observability: versioned attempt outcomes,
 * correlated client results, bounded payload evidence, and outcome-aware usage.
 * Same isolated pattern as performance-smoke.mjs: disposable admin server +
 * scratch SQLite (Python seed, mode=rw) + Chromium/CDP; no npm packages.
 * Build first: cargo build -p nyro-server; (cd webui && npm run build)
 * Run: node tests/webui/log-outcomes-smoke.mjs (Node >=22, Python >=3.9, Chromium).
 * Optional absolute paths: NYRO_SMOKE_BINARY, NYRO_SMOKE_WEBUI, CHROME_BIN.
 * Never uses a running Nyro instance, production DB, or external upstream.
 */
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { createServer } from 'node:http';
import { createServer as createTcpServer } from 'node:net';
import { access, mkdir, mkdtemp, writeFile } from 'node:fs/promises';
import { constants } from 'node:fs';
import { homedir, tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '../..');
const binary = resolve(process.env.NYRO_SMOKE_BINARY || join(root, 'target/debug/nyro-server'));
const webui = resolve(process.env.NYRO_SMOKE_WEBUI || join(root, 'webui/dist'));
const chromeCandidates = [process.env.CHROME_BIN,
  join(homedir(), '.cache/ms-playwright/chromium_headless_shell-1223/chrome-headless-shell-linux64/chrome-headless-shell'),
  join(homedir(), '.cache/ms-playwright/chromium-1223/chrome-linux64/chrome'),
  '/usr/bin/chromium', '/usr/bin/chromium-browser', '/usr/bin/google-chrome'].filter(Boolean);
const delay = ms => new Promise(resolve => setTimeout(resolve, ms));
async function waitFor(fn, label, timeout = 20_000) {
  const until = Date.now() + timeout;
  let last;
  while (Date.now() < until) {
    try { const result = await fn(); if (result) return result; } catch (error) { last = error; }
    await delay(100);
  }
  throw new Error(`Timed out: ${label}${last ? ` (${last.message})` : ''}`);
}
async function freePort() {
  const server = createTcpServer();
  await new Promise((resolve, reject) => { server.once('error', reject); server.listen(0, '127.0.0.1', resolve); });
  const port = server.address().port;
  await new Promise(resolve => server.close(resolve));
  return port;
}
class CDP {
  constructor(socket) {
    this.socket = socket; this.nextId = 1; this.pending = new Map(); this.listeners = new Map();
    socket.addEventListener('message', event => {
      const message = JSON.parse(String(event.data));
      if (message.id) {
        const callback = this.pending.get(message.id);
        if (!callback) return;
        this.pending.delete(message.id); clearTimeout(callback.timer);
        if (message.error) callback.reject(new Error(JSON.stringify(message.error)));
        else callback.resolve(message.result);
      } else for (const listener of this.listeners.get(message.method) ?? []) listener(message.params, message.sessionId);
    });
  }
  static async connect(url) {
    const socket = new WebSocket(url);
    await new Promise((resolve, reject) => { socket.addEventListener('open', resolve, { once: true }); socket.addEventListener('error', reject, { once: true }); });
    return new CDP(socket);
  }
  on(method, fn) { this.listeners.set(method, [...(this.listeners.get(method) ?? []), fn]); }
  send(method, params = {}, sessionId) {
    const id = this.nextId++;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => { this.pending.delete(id); reject(new Error(`CDP timeout: ${method}`)); }, 15_000);
      this.pending.set(id, { resolve, reject, timer });
      this.socket.send(JSON.stringify({ id, method, params, ...(sessionId ? { sessionId } : {}) }));
    });
  }
  close() { this.socket.close(); }
}

await access(binary, constants.X_OK);
await access(join(webui, 'index.html'));
assert.equal(typeof WebSocket, 'function', 'Node with native WebSocket is required');
let chrome;
for (const candidate of chromeCandidates) { try { await access(candidate, constants.X_OK); chrome = candidate; break; } catch {} }
assert.ok(chrome, 'Set CHROME_BIN to an executable Chromium binary');
const scratch = await mkdtemp(join(tmpdir(), 'nyro-log-outcomes-smoke-'));
const children = [];
const report = { scratch, binary, webui, chrome, checks: [], screenshots: [], upstreamCalls: [], consoleErrors: [], runtimeErrors: [], networkErrors: [], browserWarnings: [], childLogs: {}, usageSnapshots: {}, success: false };
let trap, cdp, sessionId, base;
const childEnv = Object.fromEntries(Object.entries(process.env).filter(([key]) => !key.startsWith('NYRO_') && !/^(http|https|all|no)_proxy$/i.test(key)));
const trackChild = (name, command, args) => {
  const child = spawn(command, args, { cwd: root, env: childEnv, stdio: ['ignore', 'pipe', 'pipe'] });
  children.push(child); report.childLogs[name] = '';
  for (const stream of [child.stdout, child.stderr]) stream.on('data', chunk => { report.childLogs[name] += chunk.toString(); });
  child.on('error', error => { report.childLogs[name] += `\nSPAWN ERROR: ${error.stack}`; });
  return child;
};
async function stop(child) {
  if (child.exitCode !== null || child.signalCode !== null || child.pid === undefined) return;
  child.kill('SIGTERM');
  await Promise.race([new Promise(resolve => child.once('exit', resolve)), delay(3000)]);
  if (child.exitCode === null && child.signalCode === null) { child.kill('SIGKILL'); await new Promise(resolve => child.once('exit', resolve)); }
}
async function api(path, method = 'GET', body) {
  const response = await fetch(`${base}/api/v1${path}`, {
    method, headers: { 'Content-Type': 'application/json' },
    ...(body === undefined ? {} : { body: JSON.stringify(body) }), signal: AbortSignal.timeout(10_000),
  });
  const json = await response.json();
  assert.ok(response.ok && !json.error, `${method} ${path}: ${response.status} ${JSON.stringify(json)}`);
  return Object.hasOwn(json, 'data') ? json.data : json;
}
const check = (name, detail = '') => { report.checks.push({ name, detail }); console.log(`PASS ${name}${detail ? ` — ${detail}` : ''}`); };
const send = (method, params = {}) => cdp.send(method, params, sessionId);
const literal = JSON.stringify;
async function evaluate(expression) {
  const result = await send('Runtime.evaluate', { expression, returnByValue: true, awaitPromise: true });
  assert.ok(!result.exceptionDetails, `Browser evaluation failed: ${JSON.stringify(result.exceptionDetails)}`);
  return result.result.value;
}
async function clickExpression(expression) {
  const point = await evaluate(`(() => { const el=(${expression}); if (!el || el.disabled) return null; el.scrollIntoView({block:'center',inline:'center'}); const r=el.getBoundingClientRect(); return {x:r.left+r.width/2,y:r.top+r.height/2}; })()`);
  assert.ok(point, `No enabled element: ${expression}`);
  await send('Input.dispatchMouseEvent', { type: 'mousePressed', ...point, button: 'left', clickCount: 1 });
  await send('Input.dispatchMouseEvent', { type: 'mouseReleased', ...point, button: 'left', clickCount: 1 });
}
async function navigate(path) {
  assert.equal((await fetch(`${base}${path}`, { signal: AbortSignal.timeout(10_000) })).status, 200, `SPA route ${path}`);
  await send('Page.navigate', { url: `${base}${path}` });
  await waitFor(() => evaluate(`location.pathname===${literal(path)} && Boolean(document.querySelector('h1'))`), `navigate ${path}`);
}
async function key(key, code = key, windowsVirtualKeyCode = key === 'Enter' ? 13 : key === 'Escape' ? 27 : 32) {
  for (const type of ['keyDown', 'keyUp']) await send('Input.dispatchKeyEvent', { type, key, code, windowsVirtualKeyCode });
}
async function select(label, text) {
  await clickExpression(`document.querySelector('[aria-label="${label}"]')`);
  const option = `[...document.querySelectorAll('[role="option"]')].find(el=>el.textContent.trim()===${literal(text)})`;
  await waitFor(() => evaluate(`Boolean(${option})`), `option ${text}`);
  await clickExpression(option);
}
/** Dialog entity selects append counts to option labels; match by substring. */
async function selectIncluding(label, text) {
  await clickExpression(`document.querySelector('[aria-label="${label}"]')`);
  const option = `[...document.querySelectorAll('[role="option"]')].find(el=>el.textContent.includes(${literal(text)}))`;
  await waitFor(() => evaluate(`Boolean(${option})`), `option ~${text}`);
  await clickExpression(option);
}
async function selectByTrigger(triggerText, optionText) {
  await clickExpression(`[...document.querySelectorAll('[role="combobox"]')].find(el=>el.textContent.trim()===${literal(triggerText)})`);
  const option = `[...document.querySelectorAll('[role="option"]')].find(el=>el.textContent.trim()===${literal(optionText)})`;
  await waitFor(() => evaluate(`Boolean(${option})`), `option ${optionText}`);
  await clickExpression(option);
}
async function screenshot(name, { preserveFocus = false } = {}) {
  await evaluate(`${preserveFocus ? '' : 'document.activeElement?.blur();'} document.fonts.ready.then(() => new Promise(resolve => setTimeout(resolve, 350)))`);
  const { data } = await send('Page.captureScreenshot', { format: 'png', captureBeyondViewport: false });
  const path = join(scratch, `${name}.png`);
  await writeFile(path, Buffer.from(data, 'base64'));
  report.screenshots.push(path); console.log(`SCREENSHOT ${path}`);
}
const totalText = () => evaluate(`document.querySelector('h1+p')?.innerText ?? ''`);
async function waitForTotal(expected, label = 'attempt total') {
  return waitFor(async () => (await totalText()) === `${expected} total attempts`, `${label}: ${expected} total attempts`);
}
const rowOf = model => evaluate(`(() => { const r=[...document.querySelectorAll('tbody tr')].find(r=>r.cells[3]?.innerText.includes(${literal(model)})); return r?[...r.cells].map(c=>c.innerText):null; })()`);
const outcomeCells = () => evaluate(`[...document.querySelectorAll('tbody tr')].map(r=>({http:r.cells[1]?.innerText??'', model:r.cells[3]?.innerText.split('\\n')[0]??''}))`);
const clientModelsVisible = () => evaluate(`[...document.querySelectorAll('tbody tr')].map(r=>r.cells[3]?.innerText.split('\\n')[0])`);
async function openDetail(model) {
  await clickExpression(`[...document.querySelectorAll('tbody tr')].find(r=>r.cells[3]?.innerText.includes(${literal(model)}))`);
  await waitFor(() => evaluate(`Boolean(document.querySelector('[role="dialog"]'))`), `detail dialog for ${model}`);
  // The full log (with payloads) arrives via get_log; the enabled Download button proves it.
  await waitFor(() => evaluate(`[...document.querySelectorAll('[role="dialog"] button')].some(b=>/Download|下载/.test(b.textContent) && !b.disabled)`), `detail payload data loaded for ${model}`);
  return waitFor(() => evaluate(`(() => { const d=document.querySelector('[role="dialog"]'); if(!d) return null; const t=d.innerText; return /Attempt result|本次尝试结果/.test(t) && /Stored attempts for this request|No correlation ID|Correlated attempts unavailable|此请求的已保存尝试|无关联请求 ID|关联尝试不可用/.test(t) ? t : null; })()`), `detail banner (correlation settled) for ${model}`);
}
/** Expand one payload block by its header title; returns summarized evidence. */
async function payloadBlock(title) {
  const blockRoot = `(() => { const btn=[...document.querySelectorAll('[role="dialog"] button')].find(b=>b.textContent.trim()===${literal(title)}); return btn ? btn.closest('div.rounded-lg') : null; })()`;
  assert.ok(await evaluate(`Boolean(${blockRoot})`), `payload block present: ${title}`);
  const expandExpr = `(() => { const root=${blockRoot}; if(!root) return null; return [...root.querySelectorAll('button')].find(b=>/Click to expand|点击展开/.test(b.textContent)) ?? false; })()`;
  // Never return DOM elements from evaluate(): CDP deep serialization explodes.
  const collapsed = await evaluate(`Boolean((${expandExpr}))`);
  if (collapsed) await clickExpression(expandExpr);
  return waitFor(() => evaluate(`(() => { const root=${blockRoot}; if(!root) return null; if([...root.querySelectorAll('button')].some(b=>/Click to expand|点击展开/.test(b.textContent))) return false; return { header:[...root.querySelectorAll('p')].map(p=>p.textContent).join(' | ')+' || '+(root.querySelector('pre')?.textContent.slice(0,200)??''), pres:[...root.querySelectorAll('pre')].map(p=>({len:p.textContent.length,start:p.textContent.slice(0,200),end:p.textContent.slice(-64)})) }; })()`), `payload block expanded: ${title}`);
}
const dialogText = () => evaluate(`document.querySelector('[role="dialog"]')?.innerText ?? ''`);
const spaNav = path => clickExpression(`document.querySelector('aside nav a[href=${literal(path)}]')`);
/** Open a confirm dialog by clicking its trigger; retries transient layout shifts. */
async function openConfirm(triggerExpr, marker, label) {
  for (let attempt = 0; attempt < 3; attempt++) {
    await clickExpression(triggerExpr);
    const text = await waitFor(() => evaluate(`document.querySelector('[role="dialog"]')?.innerText.includes(${literal(marker)}) ? document.querySelector('[role="dialog"]').innerText : null`), `dialog: ${label}`, 4000).catch(() => null);
    if (text) return text;
    const fallback = await evaluate(`(() => { const el=(${triggerExpr}); if (!el) return false; el.click(); return true; })()`);
    if (fallback) {
      const text2 = await waitFor(() => evaluate(`document.querySelector('[role="dialog"]')?.innerText.includes(${literal(marker)}) ? document.querySelector('[role="dialog"]').innerText : null`), `dialog via fallback click: ${label}`, 4000).catch(() => null);
      if (text2) return text2;
    }
  }
  throw new Error(`confirm dialog never opened: ${label}`);
}
const cancelDialog = async () => {
  await clickExpression(`[...document.querySelectorAll('[role="dialog"] button')].find(b=>/^(Cancel|取消)$/.test(b.textContent.trim()))`);
  await waitFor(() => evaluate(`!document.querySelector('[role="dialog"]')`), 'dialog cancelled');
};

// Python only opens the one existing SQLite DB discovered INSIDE this run's fresh data dir.
// URI mode=rw prevents accidental creation; realpath checks reject links outside scratch.
const seedPython = String.raw`
import json, pathlib, sqlite3, sys
scratch = pathlib.Path(sys.argv[1]).resolve(strict=True)
data = pathlib.Path(sys.argv[2]).resolve(strict=True)
assert data.is_relative_to(scratch) and data != scratch, 'data directory must be inside this scratch run'
candidates = [p for p in data.rglob('*.db') if p.is_file()]
assert len(candidates) == 1, f'expected exactly one freshly initialized SQLite DB: {candidates}'
db = candidates[0].resolve(strict=True)
assert db.is_relative_to(data), 'refuse any DB outside scratch data directory'
seed = pathlib.Path(sys.argv[3]).resolve(strict=True)
assert seed.is_relative_to(scratch), 'seed payload must also be scratch-local'
payload = json.loads(seed.read_text())
connection = sqlite3.connect(db.as_uri() + '?mode=rw', uri=True, timeout=10)
with connection:
    assert connection.execute('SELECT COUNT(*) FROM request_logs').fetchone()[0] == 0, 'only seed a fresh empty logs table'
    for row in payload['logs']:
        columns = list(row)
        connection.execute('INSERT INTO request_logs (' + ','.join(columns) + ') VALUES (' + ','.join('?' for _ in columns) + ')', [row[column] for column in columns])
    assert connection.execute('SELECT COUNT(*) FROM request_results').fetchone()[0] == 0, 'only seed a fresh empty results table'
    for row in payload['results']:
        columns = list(row)
        connection.execute('INSERT INTO request_results (' + ','.join(columns) + ') VALUES (' + ','.join('?' for _ in columns) + ')', [row[column] for column in columns])
    counts = {table: connection.execute(f'SELECT COUNT(*) FROM {table}').fetchone()[0] for table in ('request_logs', 'request_results')}
connection.close()
print(json.dumps({'database': str(db), **counts}))
`;

// ── Bounded payload evidence builders (mirror backend capture semantics) ──
const HEAD_LIMIT = 524_288, TAIL_LIMIT = 524_288, CAPTURE_LIMIT = HEAD_LIMIT + TAIL_LIMIT;
const HEAD_MARK = '[log-outcomes-smoke HEAD begins]', TAIL_MARK = '[log-outcomes-smoke TAIL ends]';
const bigHead = HEAD_MARK + 'a'.repeat(HEAD_LIMIT - HEAD_MARK.length);
const bigTail = 'z'.repeat(TAIL_LIMIT - TAIL_MARK.length) + TAIL_MARK;
const bigBody = bigHead + bigTail; // exactly 1,048,576 ASCII bytes: head+tail, middle discarded
const BIG_TOTAL = 1_200_000;
const b64Raw = Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, ...Array.from({ length: 56 }, (_, i) => (i * 7 + 3) & 0xff)]);
const b64Body = b64Raw.toString('base64');
const absentMeta = () => ({ total_observed_bytes: 0, retained_bytes: 0, head_bytes: 0, tail_bytes: 0, truncated: false, complete: false, encoding: 'none', capture_state: 'absent' });
const utf8BodyMeta = (text, { complete = true } = {}) => {
  const bytes = Buffer.byteLength(text);
  return { total_observed_bytes: bytes, retained_bytes: bytes, head_bytes: bytes, tail_bytes: 0, truncated: false, complete, encoding: 'utf8', capture_state: 'captured' };
};
const headerMeta = (observed, retained, total, kept, omitted, redacted) => ({ total_observed_bytes: observed, retained_bytes: retained, truncated: total !== kept, complete: true, encoding: 'utf8', capture_state: 'captured', total_headers: total, retained_headers: kept, omitted_headers: omitted, redacted_headers: redacted });
const metadata = entries => JSON.stringify(entries);
const SSE_ERROR = 'event: error\ndata: {"type":"error","error":{"message":"upstream read terminated before final chunk"},"request_id":"crq-minimax-read"}\n\ndata: [DONE]\n\n';

try {
  trap = createServer((req, res) => {
    report.upstreamCalls.push({ method: req.method, url: req.url });
    res.writeHead(503, { 'Content-Type': 'application/json' });
    res.end(JSON.stringify({ error: 'Log outcomes smoke forbids upstream calls' }));
  });
  await new Promise(resolve => trap.listen(0, '127.0.0.1', resolve));
  const trapBase = `http://127.0.0.1:${trap.address().port}`;
  const port = await freePort(); base = `http://127.0.0.1:${port}`; report.base = base;
  const dataDir = join(scratch, 'data'); await mkdir(dataDir);
  const server = trackChild('nyro', binary, ['--mode', 'admin', '--admin-host', '127.0.0.1', '--admin-port', String(port), '--data-dir', dataDir, '--storage-backend', 'sqlite', '--migrate-on-start', 'true', '--webui-dir', webui]);
  await waitFor(async () => {
    if (server.exitCode !== null) throw new Error(`Nyro exited ${server.exitCode}: ${report.childLogs.nyro}`);
    return (await fetch(`${base}/healthz`, { signal: AbortSignal.timeout(1000) })).ok;
  }, 'isolated Nyro admin-only server', 30_000);
  const createProvider = name => api('/providers', 'POST', { name, protocol: 'openai', base_url: `${trapBase}/v1`, api_key: 'smoke-only-not-a-secret', models_source: `${trapBase}/v1/models`, static_models: '', use_proxy: false, fast_mode: false });
  const providerA = await createProvider('MiniMax Smoke'), providerB = await createProvider('Beta Smoke');
  await api('/api-keys', 'POST', { name: 'Smoke Key', model_ids: [] });
  const apiKey = (await api('/api-keys')).find(key => key.name === 'Smoke Key');
  assert.ok(apiKey, 'created Smoke Key');

  const now = Date.now();
  const KEY = { api_key_id: apiKey.id, api_key_name: apiKey.name };
  const A = { provider_id: providerA.id, provider_name: providerA.name }, B = { provider_id: providerB.id, provider_name: providerB.name };
  const baseAttempt = { method: 'POST', path: '/v1/chat/completions', client_protocol: 'openai', upstream_protocol: 'openai', input_tokens: 100, output_tokens: 900, cache_read_tokens: 0, latency_total_ms: 1200, latency_upstream_ms: 1000, is_stream: 0, stream_chunks_count: 0, stream_first_chunk_ms: null, performance_metadata_version: 1, upstream_effort_status: 'present', upstream_response_mode: 'buffered' };
  const noPayload = metadata({ client_request_headers: absentMeta(), client_request_body: absentMeta(), client_response_headers: absentMeta(), client_response_body: absentMeta(), upstream_request_headers: absentMeta(), upstream_request_body: absentMeta(), upstream_response_headers: absentMeta(), upstream_response_body: absentMeta() });
  const logs = [];
  const log = (id, model, extra) => {
    const { payload_metadata, ...rest } = extra;
    logs.push({ id, created_at: now - (logs.length + 1) * 1500, client_model: `smoke/${model}`, upstream_url: `${trapBase}/v1/chat/completions`, model_name: 'unrelated-logical-route', ...baseAttempt, ...KEY, ...rest, ...(payload_metadata === undefined ? {} : { payload_metadata_json: payload_metadata }) });
  };

  // 1. MiniMax-like HTTP 200/200 upstream read failure: authoritative v1 "failed" IS an error.
  const miniHeaders = JSON.stringify({ 'content-type': 'application/json', 'retry-after': '3', 'x-request-id': 'upstream-789' });
  log('lo-mini-read-fail', 'mini-read', {
    ...A, upstream_model: 'MiniMax-M2', reasoning_effort: 'high', client_status_code: 200, upstream_status_code: 200,
    is_stream: 1, stream_chunks_count: 12, stream_first_chunk_ms: 180, latency_total_ms: 4200, latency_upstream_ms: 4000, output_tokens: 2400,
    client_request_id: 'crq-minimax-read', attempt_index: 0, outcome_version: 1, attempt_outcome: 'failed',
    failure_kind: 'upstream_read', failure_stage: 'read', error_message: 'Failed to read the upstream response',
    error_causes_json: '["stream ended mid-chunk","connection reset by peer"]',
    client_request_headers: JSON.stringify({ accept: 'application/json', authorization: '***' }),
    client_request_body: JSON.stringify({ model: 'smoke/mini-read', messages: [{ role: 'user', content: 'hello' }] }),
    upstream_request_headers: JSON.stringify({ 'content-type': 'application/json', authorization: '***' }),
    upstream_request_body: JSON.stringify({ model: 'MiniMax-M2', stream: true }),
    upstream_response_headers: miniHeaders,
    upstream_response_body: SSE_ERROR,
    client_response_headers: JSON.stringify({ 'content-type': 'text/event-stream', 'x-nyro-request-id': 'crq-minimax-read' }),
    client_response_body: 'event: error\ndata: {"error":"upstream read failure","request_id":"crq-minimax-read"}\n\ndata: [DONE]\n\n',
    payload_metadata: metadata({
      client_request_headers: headerMeta(96, 88, 3, 3, 0, 1), client_request_body: utf8BodyMeta(JSON.stringify({ model: 'smoke/mini-read', messages: [{ role: 'user', content: 'hello' }] })),
      upstream_request_headers: headerMeta(102, 90, 2, 2, 0, 1), upstream_request_body: utf8BodyMeta(JSON.stringify({ model: 'MiniMax-M2', stream: true })),
      upstream_response_headers: headerMeta(100_000, 65_535, 40, 38, 2, 1), upstream_response_body: utf8BodyMeta(SSE_ERROR, { complete: false }),
      client_response_headers: headerMeta(120, 118, 2, 2, 0, 0), client_response_body: utf8BodyMeta('event: error\ndata: {"error":"upstream read failure","request_id":"crq-minimax-read"}\n\ndata: [DONE]\n\n'),
    }),
  });
  // 2+8. Correlated client request: attempt A 502 failure, attempt B success, one final result.
  log('lo-retry-a-502', 'retry-a', {
    ...A, upstream_model: 'MiniMax-M2', client_status_code: 502, upstream_status_code: 502,
    client_request_id: 'crq-retry-ab', attempt_index: 0, outcome_version: 1, attempt_outcome: 'failed',
    failure_kind: 'upstream_protocol_error', failure_stage: 'response', error_message: 'The upstream response reported an error',
    error_causes_json: '["upstream HTTP 502: bad gateway"]',
    upstream_response_body: '{"error":{"message":"bad gateway","type":"upstream_error"}}',
    payload_metadata: metadata({ ...Object.fromEntries(['client_request_headers', 'client_request_body', 'client_response_headers', 'client_response_body', 'upstream_request_headers', 'upstream_request_body', 'upstream_response_headers'].map(f => [f, absentMeta()])), upstream_response_body: utf8BodyMeta('{"error":{"message":"bad gateway","type":"upstream_error"}}') }),
  });
  log('lo-timedout', 'timeout', {
    ...A, upstream_model: 'MiniMax-M2', client_status_code: 200, upstream_status_code: 200,
    is_stream: 1, stream_chunks_count: 3, stream_first_chunk_ms: 150, latency_total_ms: 65_000, latency_upstream_ms: 64_000,
    client_request_id: 'crq-timeout', attempt_index: 0, outcome_version: 1, attempt_outcome: 'timed_out',
    failure_kind: 'timeout', failure_stage: 'total', error_message: 'The request timed out',
    error_causes_json: '["deadline elapsed after 60000ms"]',
    upstream_request_body: JSON.stringify({ model: 'MiniMax-M2', stream: true }),
    upstream_response_body: bigBody,
    payload_metadata: metadata({
      client_request_headers: absentMeta(), client_request_body: utf8BodyMeta(JSON.stringify({ model: 'MiniMax-M2', stream: true })),
      client_response_headers: absentMeta(), client_response_body: absentMeta(),
      upstream_request_headers: absentMeta(), upstream_request_body: absentMeta(),
      upstream_response_headers: absentMeta(),
      upstream_response_body: { total_observed_bytes: BIG_TOTAL, retained_bytes: CAPTURE_LIMIT, head_bytes: HEAD_LIMIT, tail_bytes: TAIL_LIMIT, truncated: true, complete: false, encoding: 'utf8', capture_state: 'captured' },
    }),
  });
  log('lo-cancelled', 'cancelled', {
    ...A, upstream_model: 'MiniMax-M2', client_status_code: 200, upstream_status_code: 200,
    is_stream: 1, stream_chunks_count: 5, stream_first_chunk_ms: 200,
    client_request_id: 'crq-cancelled', attempt_index: 0, outcome_version: 1, attempt_outcome: 'cancelled',
    failure_kind: 'downstream_disconnect', failure_stage: 'delivery', error_message: 'The downstream response closed before completion',
    error_causes_json: '["client disconnected mid-stream"]',
    upstream_response_body: 'data: {"delta":"partial"}\n\ndata: [DONE]\n\n',
    payload_metadata: metadata({
      upstream_response_body: utf8BodyMeta('data: {"delta":"partial"}\n\ndata: [DONE]\n\n', { complete: false }),
      ...Object.fromEntries(['client_request_headers', 'client_request_body', 'client_response_headers', 'client_response_body', 'upstream_request_headers', 'upstream_request_body', 'upstream_response_headers'].map(f => [f, absentMeta()])),
    }),
  });
  log('lo-output-limited', 'limit', {
    ...A, upstream_model: 'MiniMax-M2', client_status_code: 200, upstream_status_code: 200, output_tokens: 4096, completion_reason: 'length',
    client_request_id: 'crq-limit', attempt_index: 0, outcome_version: 1, attempt_outcome: 'output_limited',
    failure_kind: 'output_limit', failure_stage: 'generation', error_message: 'The response reached its output token limit',
    error_causes_json: '["finish_reason length at max_tokens=4096"]',
    client_response_body: '{"id":"resp-limit","choices":[{"finish_reason":"length"}],"usage":{"output_tokens":4096}}',
    payload_metadata: metadata({
      client_response_body: utf8BodyMeta('{"id":"resp-limit","choices":[{"finish_reason":"length"}],"usage":{"output_tokens":4096}}'),
      ...Object.fromEntries(['client_request_headers', 'client_request_body', 'client_response_headers', 'upstream_request_headers', 'upstream_request_body', 'upstream_response_headers', 'upstream_response_body'].map(f => [f, absentMeta()])),
    }),
  });
  // Legacy v0 "failed" marker must NOT be an error.
  log('lo-legacy-failed', 'legacy-fail', { ...A, upstream_model: 'MiniMax-M2', client_status_code: 200, upstream_status_code: 200, outcome_version: 0, attempt_outcome: 'failed', request_completion: 'failed', payload_metadata: noPayload });
  // Future outcome version never promotes its markers.
  log('lo-future-version', 'future', { ...B, upstream_model: 'model/beta-chat', client_status_code: 200, upstream_status_code: 200, outcome_version: 99, attempt_outcome: 'completed', request_completion: 'completed', payload_metadata: noPayload });
  log('lo-retry-b-success', 'retry-b', {
    ...B, upstream_model: 'model/beta-chat', client_status_code: 200, upstream_status_code: 200,
    client_request_id: 'crq-retry-ab', attempt_index: 1, outcome_version: 1, attempt_outcome: 'completed', completion_reason: 'stop',
    payload_metadata: noPayload,
  });
  log('lo-base64', 'base64', {
    ...B, upstream_model: 'model/beta-chat', client_status_code: 200, upstream_status_code: 200,
    client_request_id: 'crq-base64', attempt_index: 0, outcome_version: 1, attempt_outcome: 'completed',
    upstream_response_body: b64Body,
    payload_metadata: metadata({
      upstream_response_body: { total_observed_bytes: b64Raw.length, retained_bytes: b64Raw.length, head_bytes: b64Raw.length, tail_bytes: 0, truncated: false, complete: true, encoding: 'base64', capture_state: 'captured' },
      ...Object.fromEntries(['client_request_headers', 'client_request_body', 'client_response_headers', 'client_response_body', 'upstream_request_headers', 'upstream_request_body', 'upstream_response_headers'].map(f => [f, absentMeta()])),
    }),
  });
  // Payload recording disabled: explicit not_retained marker, no stored bodies.
  const notRetained = { total_observed_bytes: 8123, retained_bytes: 0, head_bytes: 0, tail_bytes: 0, truncated: true, complete: true, encoding: 'none', capture_state: 'not_retained' };
  log('lo-not-retained', 'notretained', {
    ...B, upstream_model: 'model/beta-chat', client_status_code: 200, upstream_status_code: 200,
    client_request_id: 'crq-notretained', attempt_index: 0, outcome_version: 1, attempt_outcome: 'completed',
    payload_metadata: metadata(Object.fromEntries(['client_request_headers', 'client_request_body', 'client_response_headers', 'client_response_body', 'upstream_request_headers', 'upstream_request_body', 'upstream_response_headers', 'upstream_response_body'].map(f => [f, notRetained]))),
  });
  log('lo-absent', 'absent', { ...A, upstream_model: 'MiniMax-M2', client_status_code: 200, upstream_status_code: 200, client_request_id: 'crq-absent', attempt_index: 0, outcome_version: 1, attempt_outcome: 'completed', completion_reason: 'stop', payload_metadata: noPayload });
  // Legacy row: stored body text but no payload metadata, no correlation id, no api key.
  log('lo-legacy-payload', 'legacy-row', { ...B, upstream_model: 'model/beta-chat', client_status_code: 200, upstream_status_code: 200, outcome_version: 0, api_key_id: null, api_key_name: null, client_request_body: '{"legacy":"stored without metadata"}' });
  log('lo-http-500', 'http-500', { ...B, upstream_model: 'model/beta-chat', client_status_code: 500, upstream_status_code: 500, outcome_version: 0, attempt_outcome: 'unknown', request_completion: 'unknown', payload_metadata: noPayload });
  log('lo-headers-65535', 'headers', {
    ...B, upstream_model: 'model/beta-chat', client_status_code: 200, upstream_status_code: 200,
    client_request_id: 'crq-headers', attempt_index: 0, outcome_version: 1, attempt_outcome: 'completed',
    upstream_request_headers: JSON.stringify({ 'content-type': 'application/json', authorization: '***', 'x-request-id': 'beta-42' }),
    payload_metadata: metadata({
      upstream_request_headers: headerMeta(120_000, 65_535, 61, 59, 2, 1),
      ...Object.fromEntries(['client_request_headers', 'client_request_body', 'client_response_headers', 'client_response_body', 'upstream_request_body', 'upstream_response_headers', 'upstream_response_body'].map(f => [f, absentMeta()])),
    }),
  });
  assert.equal(logs.length, 14);
  const results = [
    { client_request_id: 'crq-minimax-read', final_outcome: 'failed', final_attempt_id: 'lo-mini-read-fail', attempt_count: 1, finished_at: now - 1000 },
    { client_request_id: 'crq-retry-ab', final_outcome: 'completed', final_attempt_id: 'lo-retry-b-success', attempt_count: 2, finished_at: now - 11_500 },
    { client_request_id: 'crq-timeout', final_outcome: 'timed_out', final_attempt_id: 'lo-timedout', attempt_count: 1, finished_at: now - 4000 },
    { client_request_id: 'crq-cancelled', final_outcome: 'cancelled', final_attempt_id: 'lo-cancelled', attempt_count: 1, finished_at: now - 5500 },
    { client_request_id: 'crq-limit', final_outcome: 'output_limited', final_attempt_id: 'lo-output-limited', attempt_count: 1, finished_at: now - 7000 },
    { client_request_id: 'crq-base64', final_outcome: 'completed', final_attempt_id: 'lo-base64', attempt_count: 1, finished_at: now - 13_000 },
    { client_request_id: 'crq-notretained', final_outcome: 'completed', final_attempt_id: 'lo-not-retained', attempt_count: 1, finished_at: now - 14_500 },
    { client_request_id: 'crq-absent', final_outcome: 'completed', final_attempt_id: 'lo-absent', attempt_count: 1, finished_at: now - 16_000 },
    { client_request_id: 'crq-headers', final_outcome: 'completed', final_attempt_id: 'lo-headers-65535', attempt_count: 1, finished_at: now - 20_500 },
  ];
  const seedPath = join(scratch, 'seed-logs.json');
  await writeFile(seedPath, JSON.stringify({ logs, results }));
  const python = trackChild('sqlite-seed', 'python3', ['-c', seedPython, scratch, dataDir, seedPath]);
  const seedExit = await new Promise((resolve, reject) => { python.once('error', reject); python.once('exit', resolve); });
  assert.equal(seedExit, 0, `Scratch SQLite seed failed: ${report.childLogs['sqlite-seed']}`);
  report.seed = JSON.parse(report.childLogs['sqlite-seed'].trim());
  assert.deepEqual(report.seed, { database: report.seed.database, request_logs: 14, request_results: 9 });

  // ── API oracle over the real admin surface ──
  const page = query => api(`/logs?${new URLSearchParams(query)}`);
  const ids = async query => (await page(query)).items.map(item => item.id).sort();
  assert.deepEqual(await ids({ is_error: 'true' }), ['lo-http-500', 'lo-mini-read-fail', 'lo-retry-a-502', 'lo-timedout'], 'is_error includes HTTP200/200 v1 failed + timed_out + HTTP 500');
  for (const item of (await page({ is_error: 'true' })).items) { assert.equal(item.is_error, true); assert.equal(item.effective_outcome, 'error'); }
  assert.deepEqual(await ids({ outcome: 'completed' }), ['lo-absent', 'lo-base64', 'lo-headers-65535', 'lo-not-retained', 'lo-retry-b-success']);
  assert.deepEqual(await ids({ outcome: 'cancelled' }), ['lo-cancelled']);
  assert.deepEqual(await ids({ outcome: 'output_limited' }), ['lo-output-limited']);
  assert.deepEqual(await ids({ outcome: 'unknown' }), ['lo-future-version', 'lo-legacy-failed', 'lo-legacy-payload'], 'legacy v0 failed marker and future version classify unknown, never error');
  for (const item of (await page({ outcome: 'unknown' })).items) assert.equal(item.is_error, false);
  assert.equal((await page({ status_min: '200', status_max: '200' })).items.length, 12, 'raw client-HTTP filter is independent of outcome');  assert.deepEqual(await ids({ is_error: 'true', status_min: '200', status_max: '200' }), ['lo-mini-read-fail', 'lo-timedout'], 'error ∧ HTTP200 keeps the HTTP200 read failure, drops cancelled/limit/unknown');
  assert.deepEqual(await ids({ is_error: 'false' }), ['lo-absent', 'lo-base64', 'lo-cancelled', 'lo-future-version', 'lo-headers-65535', 'lo-legacy-failed', 'lo-legacy-payload', 'lo-not-retained', 'lo-output-limited', 'lo-retry-b-success']);
  const mini = await api('/logs/lo-mini-read-fail');
  assert.equal(mini.is_error, true); assert.equal(mini.effective_outcome, 'error');
  assert.equal(mini.failure_kind, 'upstream_read'); assert.equal(mini.failure_stage, 'read');
  assert.deepEqual(JSON.parse(mini.error_causes), ['stream ended mid-chunk', 'connection reset by peer']);
  assert.deepEqual(mini.request_result, { client_request_id: 'crq-minimax-read', final_outcome: 'failed', final_attempt_id: 'lo-mini-read-fail', attempt_count: 1, finished_at: now - 1000 });
  const correlated = await api('/log-requests/crq-retry-ab');
  assert.deepEqual(correlated.attempts.map(attempt => [attempt.attempt_index, attempt.id, attempt.effective_outcome]), [[0, 'lo-retry-a-502', 'error'], [1, 'lo-retry-b-success', 'completed']]);
  assert.equal(correlated.result.final_attempt_id, 'lo-retry-b-success'); assert.equal(correlated.result.attempt_count, 2);
  const health = await api('/logging/status');
  assert.deepEqual(Object.keys(health).sort(), ['channel_closed_dropped', 'counts_reset_on_restart', 'database_write_dropped', 'queue_full_dropped']);
  assert.deepEqual([health.queue_full_dropped, health.channel_closed_dropped, health.database_write_dropped, health.counts_reset_on_restart], [0, 0, 0, true]);
  const providerStats = await api('/stats/providers');
  const statFor = name => providerStats.find(item => item.provider === name);
  assert.equal(statFor('MiniMax Smoke').request_count, 7); assert.equal(statFor('MiniMax Smoke').error_count, 3);
  assert.equal(statFor('Beta Smoke').request_count, 7); assert.equal(statFor('Beta Smoke').error_count, 1);
  const usageFamilies = {
    provider: await api(`/stats/providers/${providerA.id}?hours=24`),
    apiKey: await api(`/stats/api-keys/${apiKey.id}?hours=24`),
    model: await api(`/stats/models/MiniMax-M2?hours=24`),
  };
  const exclusiveSum = detail => detail.success_count + detail.error_count + detail.unknown_count + detail.cancelled_count + detail.output_limited_count === detail.request_count;
  assert.deepEqual([usageFamilies.provider.request_count, usageFamilies.provider.success_count, usageFamilies.provider.error_count, usageFamilies.provider.unknown_count, usageFamilies.provider.cancelled_count, usageFamilies.provider.output_limited_count], [7, 1, 3, 1, 1, 1], 'provider counts attempt-based; success = confirmed completed only');
  assert.deepEqual([usageFamilies.model.request_count, usageFamilies.model.success_count, usageFamilies.model.error_count, usageFamilies.model.unknown_count, usageFamilies.model.cancelled_count, usageFamilies.model.output_limited_count], [7, 1, 3, 1, 1, 1]);
  assert.deepEqual([usageFamilies.apiKey.request_count, usageFamilies.apiKey.success_count, usageFamilies.apiKey.error_count, usageFamilies.apiKey.unknown_count, usageFamilies.apiKey.cancelled_count, usageFamilies.apiKey.output_limited_count], [13, 5, 4, 2, 1, 1]);
  for (const [name, detail] of Object.entries(usageFamilies)) { assert.ok(exclusiveSum(detail), `${name} outcome counts exclusive-sum to total`); assert.equal(detail.outcome_stats_version, 1); }
  report.usageSnapshots = usageFamilies;
  check('seeded SQLite oracle: outcome filters, is_error semantics, correlation, logging status, usage exclusive sums', report.seed.database);

  // ── Real browser ──
  const browser = trackChild('chrome', chrome, ['--headless', '--no-sandbox', '--disable-gpu', '--disable-dev-shm-usage', '--disable-background-networking', '--no-first-run', '--no-default-browser-check', '--remote-debugging-port=0', `--user-data-dir=${join(scratch, 'chrome')}`, 'about:blank']);
  const ws = await waitFor(() => {
    if (browser.exitCode !== null) throw new Error(`Chrome exited ${browser.exitCode}: ${report.childLogs.chrome}`);
    return report.childLogs.chrome.match(/DevTools listening on (ws:\/\/[^\s]+)/)?.[1];
  }, 'Chrome debugging endpoint');
  cdp = await CDP.connect(ws);
  const target = await cdp.send('Target.createTarget', { url: 'about:blank' });
  ({ sessionId } = await cdp.send('Target.attachToTarget', { targetId: target.targetId, flatten: true }));
  cdp.on('Runtime.exceptionThrown', params => report.runtimeErrors.push(params.exceptionDetails));
  cdp.on('Runtime.consoleAPICalled', params => { if (params.type === 'error') report.consoleErrors.push(params.args.map(arg => arg.value ?? arg.description).join(' ')); });
  cdp.on('Log.entryAdded', ({ entry }) => {
    if (entry.level === 'error' && entry.source === 'network') report.networkErrors.push(entry);
    else if (entry.level === 'warning') report.browserWarnings.push(entry);
  });
  await send('Page.enable'); await send('Runtime.enable'); await send('Log.enable');
  await send('Emulation.setDeviceMetricsOverride', { width: 1440, height: 1000, deviceScaleFactor: 1, mobile: false });
  await send('Page.addScriptToEvaluateOnNewDocument', { source: `localStorage.setItem('nyro-locale','en-US');localStorage.setItem('nyro-theme','light');` });

  // Logs page: badges with separate HTTP number.
  await navigate('/logs');
  await waitForTotal(14);
  const miniRow = await rowOf('smoke/mini-read');
  assert.ok(miniRow[1].includes('200') && miniRow[1].includes('Error') && miniRow[1].includes('Upstream HTTP 200'), `HTTP number and outcome badge stay separate: ${JSON.stringify(miniRow[1])}`);
  const badgeOf = async model => (await rowOf(model))[1];
  assert.ok((await badgeOf('smoke/cancelled')).includes('Cancelled'));
  assert.ok((await badgeOf('smoke/limit')).includes('Output limited'));
  assert.ok((await badgeOf('smoke/legacy-fail')).includes('Unknown'), 'legacy failed marker is Unknown, not Error');
  assert.ok((await badgeOf('smoke/future')).includes('Unknown'));
  assert.ok((await badgeOf('smoke/retry-b')).includes('Completed'));
  await screenshot('log-outcomes-en-desktop');
  check('logs page shows error|completed|cancelled|output_limited|unknown badges beside raw HTTP numbers');

  // Outcome filter: is_error=true includes the HTTP200 failure, excludes cancelled/limit/unknown.
  await select('Attempt result filter', 'Error');
  await waitForTotal(4, 'error filter');
  let cells = await outcomeCells();
  assert.deepEqual(cells.map(cell => cell.model).sort(), ['smoke/http-500', 'smoke/mini-read', 'smoke/retry-a', 'smoke/timeout']);
  assert.ok(cells.every(cell => cell.http.includes('Error')), 'every error-filtered row carries the Error badge');
  assert.ok(cells.find(cell => cell.model === 'smoke/mini-read').http.includes('200'), 'HTTP200 read failure present in error filter');
  await select('Attempt result filter', 'Cancelled'); await waitForTotal(1, 'cancelled filter');
  assert.ok((await outcomeCells())[0].http.includes('Cancelled'));
  await select('Attempt result filter', 'Output limited'); await waitForTotal(1, 'limit filter');
  assert.ok((await outcomeCells())[0].http.includes('Output limited'));
  await select('Attempt result filter', 'Unknown'); await waitForTotal(3, 'unknown filter');
  assert.deepEqual((await clientModelsVisible()).sort(), ['smoke/future', 'smoke/legacy-fail', 'smoke/legacy-row']);
  assert.ok((await rowOf('smoke/legacy-row'))[2].includes('–'), 'row without api key shows dash');
  await select('Attempt result filter', 'Completed'); await waitForTotal(5, 'completed filter');
  await select('Attempt result filter', 'All attempt results'); await waitForTotal(14, 'reset');
  // Raw HTTP filter independent; combined with outcome via AND.
  await selectByTrigger('All HTTP status', 'HTTP 200'); await waitForTotal(12, 'http 200 filter');
  assert.ok((await clientModelsVisible()).includes('smoke/mini-read'));
  assert.ok(!(await clientModelsVisible()).includes('smoke/retry-a'), 'client 502 excluded by raw HTTP filter');
  await select('Attempt result filter', 'Error'); await waitForTotal(2, 'error ∧ HTTP200');
  assert.deepEqual((await clientModelsVisible()).sort(), ['smoke/mini-read', 'smoke/timeout']);
  await selectByTrigger('HTTP 200', 'All HTTP status'); await select('Attempt result filter', 'All attempt results');
  await waitForTotal(14, 'reset filters');
  check('outcome filter (error includes HTTP200 failure, excludes cancelled/limit/unknown) and independent raw HTTP filter combine with AND');

  // Logging health snapshot.
  const bodyText = () => evaluate('document.body.innerText');
  let body = await bodyText();
  assert.ok(body.includes('Logging health snapshot') && body.includes('Queue full dropped: 0') && body.includes('Channel closed dropped: 0') && body.includes('Database write dropped: 0'), 'logging health snapshot visible with zero drops');
  check('logging health snapshot rendered from /api/v1/logging/status');

  // Detail dialog: failure observability + correlation.
  let detail = await openDetail('smoke/mini-read');
  for (const expected of ['Attempt result', 'Error', 'Authoritative completion: Failed · v1', 'Actual HTTP status: client 200 / upstream 200',
    'Failure stage: read', 'Failure kind: upstream_read', 'Failed to read the upstream response',
    'stream ended mid-chunk', 'connection reset by peer', 'crq-minimax-read', 'Attempt index: 0',
    'Final client result (summary, not an additional attempt)', 'Failed', 'Attempt count: 1', 'lo-mini-read-fail',
    'Stored attempts for this request']) assert.ok(detail.includes(expected), `mini detail missing: ${expected}`);
  let block = await payloadBlock('Client Response Headers');
  assert.ok(block.header.includes('x-nyro-request-id') && block.header.includes('crq-minimax-read'), 'response carries X-Nyro-Request-Id');
  block = await payloadBlock('Upstream Response Body');
  assert.ok(block.pres[0].start.includes('event: error') && block.pres[0].start.includes('crq-minimax-read'), 'SSE error event carries request_id');
  block = await payloadBlock('Upstream Response Headers');
  assert.ok(block.header.includes('Omitted headers: 2') && block.header.includes('Observed 100000 B · Retained 65535 B'), `headers have separate accounting: ${block.header}`);
  await screenshot('log-outcomes-detail-mini-en', { preserveFocus: true });
  await key('Escape'); await waitFor(() => evaluate(`!document.querySelector('[role="dialog"]')`), 'detail closed');

  // Correlated attempts: two rows, one final client result.
  detail = await openDetail('smoke/retry-a');
  for (const expected of ['Stored attempts for this request', '#0', 'lo-retry-a-502', '#1', 'lo-retry-b-success', 'Final attempt',
    'Final client result (summary, not an additional attempt)', 'Completed', 'Attempt count: 2']) assert.ok(detail.includes(expected), `retry detail missing: ${expected}`);
  const attemptButtons = await evaluate(`[...document.querySelectorAll('[role="dialog"] button')].filter(b=>b.innerText.includes('lo-retry-')).map(b=>b.innerText)`);
  assert.equal(attemptButtons.length, 2, 'two correlated attempt rows');
  await screenshot('log-outcomes-detail-retry-en', { preserveFocus: true });
  await clickExpression(`[...document.querySelectorAll('[role="dialog"] button')].find(b=>b.innerText.includes('lo-retry-b-success'))`);
  await waitFor(() => evaluate(`document.querySelector('[role="dialog"]')?.innerText.includes('model/beta-chat') && document.querySelector('[role="dialog"]')?.innerText.includes('Completed')`), 'switch to correlated attempt B');
  await key('Escape'); await waitFor(() => evaluate(`!document.querySelector('[role="dialog"]')`), 'retry detail closed');
  check('detail dialog: failure kind/stage/message/causes, correlated attempts list, final client result summary, cross-attempt navigation');

  // Payload evidence: exact head/tail split, missing bytes, base64, not_retained, absent, legacy.
  detail = await openDetail('smoke/timeout');
  block = await payloadBlock('Upstream Response Body');
  assert.equal(block.pres.length, 2, 'truncated body renders exactly head and tail segments');
  assert.ok(block.header.includes('HEAD') && block.header.includes('TAIL (not contiguous with head)'), 'head/tail labels mark the discontinuity');
  assert.ok(block.header.includes('151424 observed bytes missing from the middle'), `missing middle bytes computed from metadata: ${block.header}`);
  assert.ok(block.header.includes('Observed 1200000 B · Retained 1048576 B') && block.header.includes('Truncated') && block.header.includes('Capture incomplete'), 'observed/retained accounting shown');
  assert.equal(block.pres[0].len, HEAD_LIMIT); assert.equal(block.pres[1].len, TAIL_LIMIT);
  assert.ok(block.pres[0].start.startsWith(HEAD_MARK), 'head segment keeps its opening marker');
  assert.ok(block.pres[1].end.endsWith(TAIL_MARK), 'tail segment keeps its closing marker');
  await screenshot('log-outcomes-detail-bigbody-en', { preserveFocus: true });
  await key('Escape'); await waitFor(() => evaluate(`!document.querySelector('[role="dialog"]')`), 'timeout detail closed');

  detail = await openDetail('smoke/base64');
  block = await payloadBlock('Upstream Response Body');
  assert.ok(block.header.includes('Base64 raw bytes (not parsed as JSON)'), 'base64 segment labelled, never JSON-parsed');
  assert.equal(block.pres[0].len, b64Body.length); assert.ok(block.pres[0].start === b64Body.slice(0, 200) && block.pres[0].end === b64Body.slice(-64), 'stored base64 bytes round-trip exactly');
  await key('Escape'); await waitFor(() => evaluate(`!document.querySelector('[role="dialog"]')`), 'base64 detail closed');

  detail = await openDetail('smoke/notretained');
  block = await payloadBlock('Upstream Request Body');
  assert.ok(block.header.includes('Not retained (payload recording disabled)') && block.pres.length === 0, 'not_retained marker shown without body');
  await key('Escape'); await waitFor(() => evaluate(`!document.querySelector('[role="dialog"]')`), 'notretained detail closed');

  detail = await openDetail('smoke/absent');
  block = await payloadBlock('Client Request Body');
  assert.ok(block.header.includes('Not captured') && block.pres.length === 0, 'absent capture state shown');
  await key('Escape'); await waitFor(() => evaluate(`!document.querySelector('[role="dialog"]')`), 'absent detail closed');

  // Legacy row lives past page one; the Unknown filter surfaces it.
  await select('Attempt result filter', 'Unknown'); await waitForTotal(3, 'unknown filter for legacy row');
  detail = await openDetail('smoke/legacy-row');
  assert.ok(detail.includes('Capture state unknown (legacy / metadata unavailable)'), 'legacy rows never guess a capture state');
  assert.ok(detail.includes('No correlation ID (historical or unavailable)'), 'legacy row has no correlation id');
  block = await payloadBlock('Client Request Body');
  assert.ok(block.header.includes('Raw stored text (encoding unverified)') && block.pres[0].start.includes('{"legacy":"stored without metadata"}'), 'legacy raw text still displayed');
  await key('Escape'); await waitFor(() => evaluate(`!document.querySelector('[role="dialog"]')`), 'legacy detail closed');
  await select('Attempt result filter', 'All attempt results'); await waitForTotal(14, 'reset after legacy detail');
  check('payload evidence: >1MiB exact head/tail split + missing bytes, base64, not_retained, absent, legacy unknown');

  // Cancelled / output_limited payloads are retained.
  detail = await openDetail('smoke/cancelled');
  block = await payloadBlock('Upstream Response Body');
  assert.ok(block.header.includes('Captured') && block.pres[0].start.includes('data: {"delta":"partial"}'), 'cancelled attempt keeps diagnostic payloads');
  await key('Escape'); await waitFor(() => evaluate(`!document.querySelector('[role="dialog"]')`), 'cancelled detail closed');
  detail = await openDetail('smoke/limit');
  block = await payloadBlock('Client Response Body');
  assert.ok(block.pres[0].start.includes('finish_reason') && block.pres[0].start.includes('length'), 'output-limited attempt keeps diagnostic payloads');
  await key('Escape'); await waitFor(() => evaluate(`!document.querySelector('[role="dialog"]')`), 'limit detail closed');
  check('cancelled and output_limited retain payloads while staying outside error filters/counts');

  // Header-only accounting row (also past page one; Completed filter surfaces it).
  await select('Attempt result filter', 'Completed'); await waitForTotal(5, 'completed filter for headers row');
  detail = await openDetail('smoke/headers');
  block = await payloadBlock('Upstream Request Headers');
  assert.ok(block.header.includes('Observed 120000 B · Retained 65535 B') && block.header.includes('Omitted headers: 2'), 'header capture accounting is separate from bodies');
  await key('Escape'); await waitFor(() => evaluate(`!document.querySelector('[role="dialog"]')`), 'headers detail closed');
  await select('Attempt result filter', 'All attempt results'); await waitForTotal(14, 'reset after headers detail');

  // Usage dialogs: provider / model / api-key, attempt-based error counts, exclusive sums.
  await navigate('/stats');
  await waitFor(() => evaluate(`Boolean(document.querySelector('tbody tr'))`), 'stats tables');
  const statsRow = name => evaluate(`(() => { const r=[...document.querySelectorAll('tbody tr')].find(r=>r.innerText.includes(${literal(name)})); return r?[...r.cells].map(c=>c.innerText):null; })()`);
  const providerRow = await statsRow('MiniMax Smoke');
  assert.ok(providerRow && providerRow[1] === '7' && providerRow[2] === '3', `stats provider row counts attempts/errors: ${JSON.stringify(providerRow)}`);
  const card = async label => evaluate(`document.querySelector('[role="dialog"] [aria-label=${literal(label)}]')?.innerText ?? null`);
  const cardNumber = async label => Number((((await card(label)) ?? '').split('\n')[1]));
  const assertDialogCounts = async (expected, note) => {
    const picked = {};
    for (const label of ['Attempts', 'Error Attempts', 'Unknown Attempts', 'Cancelled Attempts', 'Output-limited Attempts']) {
      picked[label] = await waitFor(() => card(label), `card ${label}`).then(() => cardNumber(label));
    }
    const { success, ...counts } = expected;
    assert.deepEqual(picked, counts, note);
    const sum = picked['Error Attempts'] + expected.success + picked['Unknown Attempts'] + picked['Cancelled Attempts'] + picked['Output-limited Attempts'];
    assert.equal(sum, picked.Attempts, `exclusive outcome sum equals total for ${note}`);
    const successCard = await waitFor(() => card('Full Success Rate'), 'success card');
    assert.ok(successCard.includes(`${expected.success} confirmed completed / ${expected.Attempts} attempts`), `success = confirmed completed only: ${successCard}`);
  };
  await clickExpression(`[...document.querySelectorAll('tbody tr')].find(r=>r.innerText.includes('MiniMax Smoke'))`);
  await waitFor(() => evaluate(`Boolean(document.querySelector('[role="dialog"] [aria-label="Error Attempts"]'))`), 'provider dialog cards');
  await assertDialogCounts({ Attempts: 7, 'Error Attempts': 3, 'Unknown Attempts': 1, 'Cancelled Attempts': 1, 'Output-limited Attempts': 1, success: 1 }, 'provider MiniMax Smoke: HTTP200 failure + 502 + timeout are 3 error attempts');
  await screenshot('log-outcomes-provider-usage-en', { preserveFocus: true });
  // Attempts tab with error outcome filter: attempt-based listing (MiniMax selected).
  await clickExpression(`[...document.querySelectorAll('[role="dialog"] [role="tab"]')].find(t=>t.textContent.trim()==='Attempts')`);
  await waitFor(() => evaluate(`Boolean(document.querySelector('[role="dialog"] tbody tr')) || document.querySelector('[role="dialog"]')?.innerText.includes('No matching attempts')`), 'attempts tab rows');
  await clickExpression(`document.querySelector('[role="dialog"] [aria-label="Attempt result filter"]')`);
  const errorOption = `[...document.querySelectorAll('[role="option"]')].find(el=>el.textContent.trim()==='Error')`;
  await waitFor(() => evaluate(`Boolean(${errorOption})`), 'error option in dialog');
  await clickExpression(errorOption);
  await waitFor(() => evaluate(`document.querySelector('[role="dialog"]')?.innerText.includes('3 attempts')`), 'provider dialog error-filtered attempts count');
  await clickExpression(`[...document.querySelectorAll('[role="dialog"] [role="tab"]')].find(t=>t.textContent.trim()==='Overview')`);
  await waitFor(() => evaluate(`Boolean(document.querySelector('[role="dialog"] [aria-label="Error Attempts"]'))`), 'overview cards back');
  await selectIncluding('Provider', 'Beta Smoke');
  await waitFor(async () => (await cardNumber('Unknown Attempts')) === 2, 'beta unknown attempts 2');
  await assertDialogCounts({ Attempts: 7, 'Error Attempts': 1, 'Unknown Attempts': 2, 'Cancelled Attempts': 0, 'Output-limited Attempts': 0, success: 4 }, 'provider Beta Smoke');
  await key('Escape'); await waitFor(() => evaluate(`!document.querySelector('[role="dialog"]')`), 'provider dialog closed');
  check('provider usage dialog: attempt-based error counts, success = confirmed completed only, unknown/cancelled/limit shown, exclusive sum = total');

  await clickExpression(`[...document.querySelectorAll('tbody tr')].find(r=>r.innerText.includes('Smoke Key'))`);
  await waitFor(() => evaluate(`Boolean(document.querySelector('[role="dialog"] [aria-label="Error Attempts"]'))`), 'api key dialog cards');
  await assertDialogCounts({ Attempts: 13, 'Error Attempts': 4, 'Unknown Attempts': 2, 'Cancelled Attempts': 1, 'Output-limited Attempts': 1, success: 5 }, 'api key Smoke Key');
  await key('Escape'); await waitFor(() => evaluate(`!document.querySelector('[role="dialog"]')`), 'api key dialog closed');
  check('api-key usage dialog counts attempts across providers with exclusive sum');

  await clickExpression(`[...document.querySelectorAll('tbody tr')].find(r=>r.innerText.includes('MiniMax-M2'))`);
  await waitFor(() => evaluate(`Boolean(document.querySelector('[role="dialog"] [aria-label="Error Attempts"]'))`), 'model dialog cards');
  await assertDialogCounts({ Attempts: 7, 'Error Attempts': 3, 'Unknown Attempts': 1, 'Cancelled Attempts': 1, 'Output-limited Attempts': 1, success: 1 }, 'model MiniMax-M2');
  await selectIncluding('Model', 'model/beta-chat');
  await waitFor(async () => (await cardNumber('Unknown Attempts')) === 2, 'beta model unknown 2');
  await assertDialogCounts({ Attempts: 7, 'Error Attempts': 1, 'Unknown Attempts': 2, 'Cancelled Attempts': 0, 'Output-limited Attempts': 0, success: 4 }, 'model model/beta-chat');
  await key('Escape'); await waitFor(() => evaluate(`!document.querySelector('[role="dialog"]')`), 'model dialog closed');
  check('model usage dialog counts attempts with exclusive sum');

  // ZH locale: badges, health snapshot, destructive dialog copy, detail labels.
  await spaNav('/logs');
  await clickExpression(`document.querySelector('button[title="切换到中文"]')`);
  await waitFor(() => evaluate(`document.querySelector('h1')?.textContent === '请求日志'`), 'Chinese logs page');
  await waitFor(() => evaluate(`document.body.innerText.includes('共 14 次尝试')`), 'zh total');
  body = await bodyText();
  assert.ok(body.includes('错误') && body.includes('已取消') && body.includes('输出受限') && body.includes('未知'), 'zh outcome badges');
  assert.ok(body.includes('日志写入健康快照') && body.includes('队列已满丢失: 0'), 'zh health snapshot');
  await screenshot('log-outcomes-zh-desktop');
  detail = await openDetail('smoke/timeout');
  block = await payloadBlock('上游响应体');
  assert.ok(block.header.includes('头部') && block.header.includes('尾部（与头部不连续）') && block.header.includes('151424 个已观测字节'), 'zh head/tail labels and missing bytes');
  await screenshot('log-outcomes-zh-detail', { preserveFocus: true });
  await key('Escape'); await waitFor(() => evaluate(`!document.querySelector('[role="dialog"]')`), 'zh detail closed');
  let confirmText = await openConfirm(`document.querySelector('button[title="清除已记录载荷"]')`, '仅删除载荷，不删除日志记录，也不改变结果分类', 'zh clear payloads');
  assert.ok(confirmText.includes('所有日志（包括全部错误日志，无例外）') && confirmText.includes('仅删除载荷，不删除日志记录，也不改变结果分类'), 'zh clear-payloads copy covers ALL incl errors');
  await cancelDialog();
  confirmText = await openConfirm(`document.querySelector('button[title="清除报错日志"]')`, '仅取消、输出受限或未知结果不属于错误，不会删除', 'zh delete errors');
  assert.ok(confirmText.includes('客户端或上游 HTTP 4xx/5xx，或核心确认失败/超时（即使 HTTP 200）') && confirmText.includes('仅取消、输出受限或未知结果不属于错误，不会删除'), 'zh delete-errors copy confirmed-only');
  await cancelDialog();
  check('zh locale: badges, health snapshot, destructive dialog copy, head/tail labels');

  // Mobile ZH + EN, no overflow.
  await send('Emulation.setDeviceMetricsOverride', { width: 390, height: 844, deviceScaleFactor: 1, mobile: true });
  await waitFor(() => evaluate(`document.querySelector('main').getBoundingClientRect().width >= 220`), 'usable mobile main width');
  assert.ok(await evaluate(`document.documentElement.scrollWidth <= innerWidth + 1`), 'no whole-page mobile overflow');
  await screenshot('log-outcomes-zh-mobile');
  await clickExpression(`document.querySelector('button[title="Switch to English"]')`);
  await waitFor(() => evaluate(`document.querySelector('h1')?.textContent === 'Request Logs'`), 'English locale');
  assert.ok(await evaluate(`document.documentElement.scrollWidth <= innerWidth + 1`), 'no overflow after locale switch');
  await screenshot('log-outcomes-en-mobile');
  await send('Emulation.setDeviceMetricsOverride', { width: 1440, height: 1000, deviceScaleFactor: 1, mobile: false });
  check('EN/ZH desktop+mobile screenshots without page overflow');

  // Destructive: clear payloads ALL (incl errors) then verify cleared markers and unchanged classification.
  await waitForTotal(14, 'pre-destructive total');
  let destructive = await openConfirm(`document.querySelector('button[title="Clear Recorded Payloads"]')`, 'Only payloads are removed; log records and result classifications are unchanged', 'clear payloads copy');
  assert.ok(destructive.includes('including ALL error logs without exceptions') && destructive.includes('Only payloads are removed; log records and result classifications are unchanged'), `clear-payloads copy: ${destructive.slice(0, 200)}`);
  await cancelDialog();
  detail = await openDetail('smoke/mini-read');
  assert.ok((await payloadBlock('Upstream Response Body')).pres[0].start.includes('event: error'), 'payloads still present after cancel');
  await key('Escape'); await waitFor(() => evaluate(`!document.querySelector('[role="dialog"]')`), 'closed before destructive');
  await openConfirm(`document.querySelector('button[title="Clear Recorded Payloads"]')`, 'Only payloads are removed', 'clear payloads reopen');
  await clickExpression(`[...document.querySelectorAll('[role="dialog"] button')].find(b=>b.textContent.trim()==='Clear Payloads')`);
  await waitFor(() => evaluate(`!document.querySelector('[role="dialog"]')`), 'clear payloads confirmed');
  await waitForTotal(14, 'total unchanged after payload clear');
  detail = await openDetail('smoke/mini-read');
  assert.ok(detail.includes('Attempt result') && detail.includes('Error') && detail.includes('Failed to read the upstream response'), 'classification and failure diagnostics unchanged after payload clear');
  block = await payloadBlock('Upstream Response Body');
  assert.ok(block.header.includes('Manually cleared') && block.pres.length === 0, `cleared marker with timestamp: ${block.header}`);
  await screenshot('log-outcomes-detail-cleared-en', { preserveFocus: true });
  await key('Escape'); await waitFor(() => evaluate(`!document.querySelector('[role="dialog"]')`), 'cleared detail closed');
  const overviewAfterClear = await api('/stats/overview');
  assert.equal(overviewAfterClear.total_requests, 14); assert.equal(overviewAfterClear.error_count, 4, 'clearing payloads never reclassifies outcomes');
  check('clear payloads removes ALL payloads incl errors, marks cleared, keeps classifications');

  // Destructive: delete error logs (confirmed errors only) + cache invalidation across families.
  // /stats must be freshly cached (staleTime 10s) right before the mutation so the
  // post-mutation values can only come from cross-family query invalidation.
  await spaNav('/stats');
  const providerRowBefore = () => evaluate(`(() => { const r=[...document.querySelectorAll('tbody tr')].find(r=>r.innerText.includes('MiniMax Smoke')); return r?[...r.cells].map(c=>c.innerText):null; })()`);
  await waitFor(async () => { const row = await providerRowBefore(); return row && row[1] === '7' && row[2] === '3'; }, 'fresh stats cached before destructive mutation');
  await spaNav('/logs');
  await waitFor(() => evaluate(`(() => { const b=document.querySelector('button[title="Delete Error Logs"]'); return b && !b.disabled; })()`), 'logs page ready after SPA navigation');
  destructive = await openConfirm(`document.querySelector('button[title="Delete Error Logs"]')`, 'Pure cancellation, output-limited and unknown results are excluded', 'delete errors copy');
  assert.ok(destructive.includes('Permanently delete all error attempt logs: client or upstream HTTP 4xx/5xx, or core-confirmed failure/timeout (including HTTP 200). Pure cancellation, output-limited and unknown results are excluded.'), `delete-errors copy confirmed-only: ${destructive.slice(0, 240)}`);
  await cancelDialog();
  await waitForTotal(14, 'still 14 after cancel');
  destructive = await openConfirm(`document.querySelector('button[title="Clear Logs"]')`, 'All request logs will be permanently deleted', 'clear all copy');
  assert.ok(destructive.includes('All request logs will be permanently deleted. This action cannot be undone.'), 'clear-all copy');
  await cancelDialog();
  await openConfirm(`document.querySelector('button[title="Delete Error Logs"]')`, 'Delete Error Logs', 'delete errors reopen');
  await clickExpression(`[...document.querySelectorAll('[role="dialog"] button')].find(b=>b.textContent.trim()==='Delete')`);
  await waitFor(() => evaluate(`!document.querySelector('[role="dialog"]')`), 'delete errors confirmed');
  await waitForTotal(10, 'after delete errors');
  cells = await outcomeCells();
  assert.equal(cells.length, 10);
  assert.ok(cells.every(cell => !cell.http.includes('Error')), 'no error rows remain');
  const remaining = (await clientModelsVisible()).sort();
  assert.ok(!remaining.includes('smoke/mini-read') && !remaining.includes('smoke/retry-a') && !remaining.includes('smoke/timeout') && !remaining.includes('smoke/http-500'), 'confirmed errors (incl HTTP200 read failure) deleted');
  assert.ok(remaining.includes('smoke/cancelled') && remaining.includes('smoke/limit') && remaining.includes('smoke/legacy-fail') && remaining.includes('smoke/legacy-row') && remaining.includes('smoke/future'), 'cancelled/limit/unknown/completed survive');
  // Cross-family cache invalidation: stats page was freshly cached before the mutation.
  await spaNav('/stats');
  await waitFor(() => evaluate(`document.body.innerText.includes('Total Attempts')`), 'stats back');
  const invalidated = await waitFor(async () => {
    const row = await providerRowBefore();
    return row && row[1] === '4' && row[2] === '0' ? row : false;
  }, 'stats cache invalidated across families without reload');
  await clickExpression(`[...document.querySelectorAll('tbody tr')].find(r=>r.innerText.includes('MiniMax Smoke'))`);
  await waitFor(() => evaluate(`Boolean(document.querySelector('[role="dialog"] [aria-label="Error Attempts"]'))`), 'provider dialog reopened');
  await assertDialogCounts({ Attempts: 4, 'Error Attempts': 0, 'Unknown Attempts': 1, 'Cancelled Attempts': 1, 'Output-limited Attempts': 1, success: 1 }, 'provider usage cache after deletion');
  await key('Escape'); await waitFor(() => evaluate(`!document.querySelector('[role="dialog"]')`), 'provider dialog closed');
  await screenshot('log-outcomes-after-delete-errors');
  const correlatedAfter = await api('/log-requests/crq-retry-ab');
  assert.deepEqual(correlatedAfter.attempts.map(attempt => attempt.id), ['lo-retry-b-success'], 'only the surviving correlated attempt is listed');
  assert.equal(correlatedAfter.result.final_attempt_id, 'lo-retry-b-success');
  check('delete errors removes only confirmed errors; logs/stats/usage caches invalidated across families without reload');

  assert.deepEqual(report.upstreamCalls, [], 'No upstream call left the local fixture');
  assert.deepEqual(report.consoleErrors, [], 'Unexpected browser console.error');
  assert.deepEqual(report.runtimeErrors, [], 'Unexpected browser runtime errors');
  assert.deepEqual(report.networkErrors, [], 'Unexpected browser network errors');
  check('no upstream calls or unexpected browser errors');
  report.success = true;
} catch (error) {
  report.error = error.stack;
  if (cdp && sessionId) { try { report.failureDom = await evaluate('document.body.innerText'); await screenshot('failure'); } catch {} }
  console.error(error.stack); process.exitCode = 1;
} finally {
  if (cdp) cdp.close();
  await Promise.all(children.map(stop));
  if (trap) await new Promise(resolve => trap.close(resolve));
  report.childrenStopped = children.every(child => child.exitCode !== null || child.signalCode !== null || child.pid === undefined);
  await writeFile(join(scratch, 'report.json'), JSON.stringify(report, null, 2));
  console.log(`REPORT ${join(scratch, 'report.json')}`);
  console.log(`RESULT ${report.success ? 'PASS' : 'FAIL'}; child cleanup=${report.childrenStopped}`);
}
