#!/usr/bin/env node
/**
 * Actual isolated Nyro admin server + scratch SQLite + Chromium/CDP; no npm packages.
 * Build first: cargo build -p nyro-server --no-default-features; (cd webui && npm run build)
 * Run: node tests/webui/performance-smoke.mjs (Node >=22, Python >=3.9 sqlite3, Chromium).
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
const scratch = await mkdtemp(join(tmpdir(), 'nyro-performance-smoke-'));
const children = [];
const report = { scratch, binary, webui, chrome, checks: [], screenshots: [], apiCalls: [], injected: [], upstreamCalls: [], consoleErrors: [], runtimeErrors: [], networkErrors: [], expectedNetworkErrors: [], browserWarnings: [], childLogs: {}, success: false };
let trap, cdp, sessionId, base, faultMode = null;
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
const ratingPath = pair => `/providers/${pair.provider.id}/model-rating-profile?model=${encodeURIComponent(pair.model)}`;
const tiers = ['low', 'medium', 'high', 'xhigh', 'max'];
const profileInput = (common, overrides = {}) => ({ common, overrides: Object.fromEntries(tiers.map(tier => [tier, overrides[tier] ?? null])) });
const usagePath = pair => `/providers/${pair.provider.id}/model-usage?model=${encodeURIComponent(pair.model)}`;
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
const refresh = () => clickExpression(`document.querySelector('button[aria-label="Refresh performance"]')`);
async function navigate(path) {
  assert.equal((await fetch(`${base}${path}`, { signal: AbortSignal.timeout(10_000) })).status, 200, `SPA route ${path}`);
  await send('Page.navigate', { url: `${base}${path}` });
  await waitFor(() => evaluate(`location.pathname===${literal(path)} && Boolean(document.querySelector('h1'))`), `navigate ${path}`);
}
async function reload() {
  const marker = `${Date.now()}-${Math.random()}`;
  await evaluate(`window.__performanceSmokeReloadToken=${literal(marker)}`);
  await send('Page.reload');
  await waitFor(() => evaluate(`window.__performanceSmokeReloadToken!==${literal(marker)} && Boolean(document.querySelector('h1'))`), 'fresh browser document after reload');
}
async function key(key, code = key, windowsVirtualKeyCode = key === 'Enter' ? 13 : key === 'Escape' ? 27 : 32) {
  for (const type of ['keyDown', 'keyUp']) await send('Input.dispatchKeyEvent', { type, key, code, windowsVirtualKeyCode });
}
async function fill(selector, value) {
  assert.ok(await evaluate(`(() => {const el=document.querySelector(${literal(selector)});if(!el)return false;el.focus();el.select();return true})()`));
  if (value) await send('Input.insertText', { text: value });
  else await key('Backspace', 'Backspace', 8);
}
const indexSelector = id => `[data-testid="performance-index-row"][data-point-id="${id}"]`;
const circleExpression = id => `[...document.querySelectorAll('[data-testid="performance-point"]')].find(el=>el.dataset.pointIds.split(',').includes(${literal(id)}))`;
async function select(label, text) {
  await clickExpression(`document.querySelector('[aria-label="${label}"]')`);
  const option = `[...document.querySelectorAll('[role="option"]')].find(el=>el.textContent.trim()===${literal(text)})`;
  await waitFor(() => evaluate(`Boolean(${option})`), `option ${text}`);
  await clickExpression(option);
}
async function screenshot(name) {
  await evaluate(`document.activeElement?.blur(); document.fonts.ready.then(() => new Promise(resolve => setTimeout(resolve, 350)))`);
  const { data } = await send('Page.captureScreenshot', { format: 'png', captureBeyondViewport: false });
  const path = join(scratch, `${name}.png`);
  await writeFile(path, Buffer.from(data, 'base64'));
  report.screenshots.push(path); console.log(`SCREENSHOT ${path}`);
}
async function detailRow(pair) {
  return evaluate(`(() => {
    const row=[...document.querySelectorAll('tbody tr')].find(row=>row.cells[0]?.innerText.includes(${literal(pair.provider.name)}) && row.cells[0]?.innerText.includes(${literal(pair.model)}));
    return row ? [...row.cells].map(cell=>cell.innerText) : null;
  })()`);
}
async function chartState() {
  return evaluate(`(() => {
    const chart=document.querySelector('[data-testid="performance-chart"]'), summary=document.querySelector('[data-testid="performance-summary"]');
    const attrs=el=>el ? Object.fromEntries([...el.attributes].filter(a=>a.name.startsWith('data-')).map(a=>[a.name,a.value])) : {};
    const points=[...document.querySelectorAll('[data-testid="performance-point"]')].map(el=>({ids:el.dataset.pointIds.split(','),members:Number(el.dataset.memberCount),score:Number(el.dataset.score),tps:Number(el.dataset.tps),x:el.cx.baseVal.value,y:el.cy.baseVal.value,fill:el.getAttribute('fill'),pressed:el.parentElement.getAttribute('aria-pressed'),opacity:Number(el.parentElement.style.opacity)}));
    const rows=[...document.querySelectorAll('[data-testid="performance-index-row"]')].map(el=>({id:el.dataset.pointId,text:el.innerText,pressed:el.getAttribute('aria-pressed'),opacity:Number(el.style.opacity)}));
    return {points,rows,busy:Boolean(document.querySelector('button[aria-label="Refresh performance"],button[aria-label="刷新性能数据"]')?.disabled),counts:attrs(summary),axis:attrs(chart),summary:summary?.innerText,labels:[...document.querySelectorAll('[data-testid="performance-label"] text')].map(el=>el.textContent),body:document.body.innerText};
  })()`);
}
async function ready({ plotted = 11, missing = 7, errors = 0 } = {}) {
  return waitFor(async () => {
    const state=await chartState();
    return state.rows.length===plotted && Number(state.counts['data-plotted-count'])===plotted && Number(state.counts['data-missing-count'])===missing && Number(state.counts['data-error-count'])===errors && !state.busy ? state : false;
  }, `settled chart: ${plotted} plotted, ${missing} missing, ${errors} errors`);
}
function assertAxes(state, yMax) {
  const a=state.axis;
  assert.equal(Number(a['data-x-min']),0); assert.equal(Number(a['data-x-max']),100); assert.equal(Number(a['data-y-max']),yMax);
  const left=Number(a['data-plot-left']),right=Number(a['data-plot-right']),top=Number(a['data-plot-top']),bottom=Number(a['data-plot-bottom']);
  for(const point of state.points){
    assert.ok(Math.abs(point.x-(left+point.score/100*(right-left)))<1e-4, 'Actual SVG X equals score, never jitter/centroid');
    assert.ok(Math.abs(point.y-(bottom-point.tps/yMax*(bottom-top)))<1e-4, 'Actual SVG Y equals exact backend TPS');
  }
}
function expectedRows(snapshot, pairs) {
  return snapshot.models.flatMap(model => (model.profile.display_mode==='common' ? ['mixed'] : tiers).map(tier=>{
    const pair=pairs.find(p=>p.provider.id===model.profile.provider_id && p.model===model.profile.upstream_model);
    const score=tier==='mixed' ? model.profile.common?.score ?? null : model.profile.effective[tier].score;
    return {pair,tier,score,source:tier==='mixed' ? 'common' : model.profile.effective[tier].source,stats:tier==='mixed' ? model.mixed : model.tiers[tier],key:JSON.stringify([pair.provider.id,pair.model,tier])};
  })).sort((a,b)=>a.key<b.key?-1:a.key>b.key?1:0).map((row,i)=>({...row,id:`P${String(i+1).padStart(2,'0')}`}));
}
function assertPoints(state, rows) {
  const plotted=rows.filter(row=>row.score!==null && row.stats.average_tps!==null && row.stats.valid_tps_count>0);
  assert.deepEqual(state.rows.map(row=>row.id).sort(),plotted.map(row=>row.id).sort());
  assert.deepEqual(state.points.flatMap(point=>point.ids).sort(),plotted.map(row=>row.id).sort());
  for(const row of plotted){
    const point=state.points.find(point=>point.ids.includes(row.id)), index=state.rows.find(item=>item.id===row.id);
    assert.equal(point.score,row.score); assert.equal(point.tps,row.stats.average_tps);
    for(const text of [row.pair.provider.name,row.pair.provider.id,row.pair.model,row.tier==='mixed'?'Mixed':row.tier,row.source==='override'?'Override':'Common',`${row.score}/100`,`${row.stats.average_tps} tok/s`,`Valid TPS ${row.stats.valid_tps_count} / selected requests ${row.stats.selected_request_count}`]) assert.ok(index.text.includes(text),`${row.id} full index identity/metrics: ${text}`);
    if(point.members===1) assert.equal(point.fill==='white',row.stats.valid_tps_count<3, 'One/two samples hollow; three or more solid');
  }
  assert.ok(state.labels.every(label=>/^P\d+( ×\d+)?$/.test(label)), 'SVG labels are IDs only, full names in index');
  assert.ok(state.points.every(point=>point.members===point.ids.length));
}

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
rows = json.loads(seed.read_text())
connection = sqlite3.connect(db.as_uri() + '?mode=rw', uri=True, timeout=10)
with connection:
    assert connection.execute('SELECT COUNT(*) FROM request_logs').fetchone()[0] == 0, 'only seed a fresh empty logs table'
    for row in rows:
        columns = list(row)
        connection.execute('INSERT INTO request_logs (' + ','.join(columns) + ') VALUES (' + ','.join('?' for _ in columns) + ')', [row[column] for column in columns])
    count = connection.execute('SELECT COUNT(*) FROM request_logs').fetchone()[0]
connection.close()
print(json.dumps({'database': str(db), 'request_logs': count}))
`;

try {
  trap = createServer((req, res) => {
    report.upstreamCalls.push({ method: req.method, url: req.url });
    res.writeHead(503, { 'Content-Type': 'application/json' });
    res.end(JSON.stringify({ error: 'Performance smoke forbids upstream/catalog calls' }));
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
  const alpha = await createProvider('Alpha Smoke'), beta = await createProvider('Beta Smoke'), disabled = await createProvider('Disabled Smoke');
  await api(`/providers/${disabled.id}`, 'PUT', { is_enabled: false });
  const pairs = [
    { provider: alpha, model: 'model/shared', score: 0 },
    { provider: beta, model: 'model/shared', score: 100 },
    { provider: alpha, model: 'model/overlap', score: 50 },
    { provider: beta, model: 'model/overlap', score: 50 },
    { provider: disabled, model: 'retired/模型/full-unambiguous-name', score: 73 },
    { provider: alpha, model: 'model/near-overlap', score: 51 },
    { provider: alpha, model: 'model/effort-profile', score: 80, overrides: { high: 90, max: 0 } },
    { provider: alpha, model: 'model/no-history', score: 25 },
    { provider: beta, model: 'model/legacy-only', score: 90 },
    { provider: beta, model: 'model/equal-override', score: 55, overrides: { high: 55 } },
  ];
  const unrated = { provider: alpha, model: 'model/unrated-fast' };
  for (const pair of pairs) await api(ratingPath(pair), 'PUT', profileInput(pair.score, pair.overrides));
  const logs = [], now = Date.now();
  const log = (pair, output, upstream = 1000, extra = {}) => logs.push({
    id: `performance-smoke-${String(logs.length).padStart(4,'0')}`, created_at: now - 60_000 + logs.length * 100,
    provider_id: pair.provider.id, provider_name: pair.provider.name, upstream_model: pair.model,
    model_name: 'unrelated-logical-route', client_model: 'unrelated-client-alias',
    client_protocol: 'openai', upstream_protocol: 'openai', method: 'POST', path: '/v1/chat/completions',
    client_status_code: 200, upstream_status_code: 200, input_tokens: 10, output_tokens: output,
    // Legacy metric intentionally disagrees: never fill trusted values from it.
    cache_read_tokens: 0, latency_upstream_ms: 100, latency_total_ms: 200,
    is_stream: 0, stream_chunks_count: 0, stream_first_chunk_ms: null,
    performance_metadata_version: 1, upstream_effort_status: 'present', upstream_effort_raw: 'high', upstream_effort_tier: 'high',
    request_completion: 'completed', completion_reason: 'stop', upstream_response_mode: 'buffered',
    performance_upstream_ms: upstream, performance_first_chunk_ms: null, performance_completed_at: now - 60_000 + logs.length * 100,
    ...extra,
  });
  log(pairs[0], 9000); log(pairs[0], 8000);
  for (let i=0;i<5;i++) {
    log(pairs[0],100,2000,{upstream_response_mode:'stream',performance_first_chunk_ms:500});
    log(pairs[0],50);
  }
  log(pairs[1],225);
  for(const pair of pairs.slice(2,6)) log(pair,pair===pairs[4]?80:60);
  const effort=pairs[6];
  log(effort,9999,1000,{upstream_effort_raw:'minimal',upstream_effort_tier:'low'});
  for(let i=0;i<10;i++) log(effort,40,1000,{upstream_effort_raw:i%2?'low':'minimal',upstream_effort_tier:'low'});
  for(let i=0;i<3;i++) log(effort,70,1000,{upstream_effort_raw:'medium',upstream_effort_tier:'medium'});
  log(effort,100); log(effort,0); // selected two, valid one (no refill from older rows)
  log(effort,120,1000,{upstream_effort_raw:'max',upstream_effort_tier:'max'});
  for(const status of ['absent','unknown']) log(effort,150,1000,{upstream_effort_status:status,upstream_effort_raw:null,upstream_effort_tier:null});
  for(const completion of ['failed','incomplete','cancelled','unknown']) log(effort,9999,1000,{request_completion:completion,completion_reason:completion==='incomplete'?'length':completion});
  log(effort,9999,1000,{client_status_code:500}); log(effort,9999,1000,{upstream_status_code:429});
  log(effort,9999,1000,{performance_completed_at:now-8*86400000});
  log(pairs[8],500,1000,{performance_metadata_version:0,request_completion:'unknown',upstream_effort_status:'unknown',upstream_effort_raw:null,upstream_effort_tier:null,performance_upstream_ms:null,performance_completed_at:null});
  log(pairs[9],55); log(unrated,9999);
  const seedPath = join(scratch, 'seed-logs.json'); await writeFile(seedPath, JSON.stringify(logs, null, 2));
  const python = trackChild('sqlite-seed', 'python3', ['-c', seedPython, scratch, dataDir, seedPath]);
  const seedExit = await new Promise((resolve, reject) => { python.once('error', reject); python.once('exit', resolve); });
  assert.equal(seedExit, 0, `Scratch SQLite seed failed: ${report.childLogs['sqlite-seed']}`);
  report.seed = JSON.parse(report.childLogs['sqlite-seed'].trim());
  const snapshot=await api('/model-performance'); report.snapshot=snapshot;
  assert.equal(snapshot.as_of-snapshot.window_start,7*86400000); assert.equal(snapshot.models.length,pairs.length);
  const statsFor=pair=>snapshot.models.find(item=>item.profile.provider_id===pair.provider.id && item.profile.upstream_model===pair.model);
  assert.ok(Math.abs(statsFor(pairs[0]).mixed.average_tps-175/3)<1e-10);
  assert.equal(statsFor(pairs[0]).mixed.selected_request_count,10);
  const grouped=statsFor(effort);
  assert.equal(grouped.tiers.low.selected_request_count,10); assert.equal(grouped.tiers.low.valid_tps_count,10); assert.equal(grouped.tiers.low.average_tps,40);
  assert.equal(grouped.tiers.medium.valid_tps_count,3); assert.equal(grouped.tiers.medium.average_tps,70);
  assert.equal(grouped.tiers.high.selected_request_count,2); assert.equal(grouped.tiers.high.valid_tps_count,1); assert.equal(grouped.tiers.high.average_tps,100);
  assert.equal(grouped.tiers.xhigh.average_tps,null); assert.equal(grouped.mixed.selected_request_count,10);
  assert.equal(grouped.unclassified_count,2); assert.equal(grouped.untrusted_count,1);
  assert.equal(statsFor(pairs[8]).mixed.average_tps,null); assert.equal(statsFor(pairs[8]).untrusted_count,1);
  assert.ok((await api(usagePath(pairs[8]))).average_tps>0, 'Legacy average exists but does not fill trusted unknown');
  for(const item of snapshot.models) for(const stats of [item.mixed,...Object.values(item.tiers)]){
    assert.ok(stats.selected_request_count<=10);
    if(stats.average_tps!==null) assert.ok(Number.isFinite(stats.first_sample_at) && stats.first_sample_at>=snapshot.window_start && stats.last_sample_at<=snapshot.as_of);
  }
  const expected=expectedRows(snapshot,pairs);
  check('real profiles and trusted seven-day per-group ten completed requests; invalid/old/error/cancel/token-limit/legacy excluded',report.seed.database);

  const browser = trackChild('chrome', chrome, ['--headless', '--no-sandbox', '--disable-gpu', '--disable-dev-shm-usage', '--disable-background-networking', '--no-first-run', '--no-default-browser-check', '--remote-debugging-port=0', `--user-data-dir=${join(scratch, 'chrome')}`, 'about:blank']);
  const ws = await waitFor(() => {
    if (browser.exitCode !== null) throw new Error(`Chrome exited ${browser.exitCode}: ${report.childLogs.chrome}`);
    return report.childLogs.chrome.match(/DevTools listening on (ws:\/\/[^\s]+)/)?.[1];
  }, 'Chrome debugging endpoint');
  cdp = await CDP.connect(ws);
  const target = await cdp.send('Target.createTarget', { url: 'about:blank' });
  ({ sessionId } = await cdp.send('Target.attachToTarget', { targetId: target.targetId, flatten: true }));
  const expectedFailureUrls = new Map();
  cdp.on('Runtime.exceptionThrown', params => report.runtimeErrors.push(params.exceptionDetails));
  cdp.on('Runtime.consoleAPICalled', params => { if (params.type === 'error') report.consoleErrors.push(params.args.map(arg => arg.value ?? arg.description).join(' ')); });
  cdp.on('Log.entryAdded', ({ entry }) => {
    if (entry.level === 'error' && entry.source === 'network') {
      const expected = expectedFailureUrls.get(entry.url) ?? 0;
      (expected > 0 ? report.expectedNetworkErrors : report.networkErrors).push(entry);
      if (expected > 0) expectedFailureUrls.set(entry.url, expected - 1);
    } else if (entry.level === 'warning') report.browserWarnings.push(entry);
  });
  cdp.on('Fetch.requestPaused', (params, sid) => {
    const url = new URL(params.request.url);
    report.apiCalls.push({mode:faultMode,method:params.request.method,path:url.pathname});
    const isTarget=url.pathname==='/api/v1/model-performance';
    const fail=faultMode==='snapshot' && isTarget;
    const injected=Boolean(faultMode && isTarget);
    if(fail) expectedFailureUrls.set(params.request.url,(expectedFailureUrls.get(params.request.url)??0)+1);
    const payload=structuredClone(snapshot);
    const targetModel=payload.models.find(model=>model.profile.provider_id===pairs[1].provider.id && model.profile.upstream_model===pairs[1].model);
    if(faultMode==='partial'){targetModel.status='error';targetModel.error='Injected profile statistics failure';}
    if(['negative','zero','string','null','infinity'].includes(faultMode)) targetModel.mixed.average_tps=faultMode==='negative'?-5:faultMode==='zero'?0:faultMode==='string'?'NaN':null;
    if(faultMode==='null') Object.assign(targetModel.mixed,{valid_tps_count:0,first_sample_at:null,last_sample_at:null});
    const body=fail?{error:'Injected performance snapshot failure'}:{data:payload};
    const jsonBody=faultMode==='infinity'?JSON.stringify(body).replace('"average_tps":null','"average_tps":1e400'):JSON.stringify(body);
    if(injected) report.injected.push({url:params.request.url,mode:faultMode,status:fail?500:200});
    const command = injected ? cdp.send('Fetch.fulfillRequest', { requestId: params.requestId, responseCode: fail ? 500 : 200, responseHeaders: [{ name: 'Content-Type', value: 'application/json' }], body: Buffer.from(jsonBody).toString('base64') }, sid)
      : cdp.send('Fetch.continueRequest', { requestId: params.requestId }, sid);
    command.catch(error => report.runtimeErrors.push({ interceptionError: error.message }));
  });
  await send('Page.enable'); await send('Runtime.enable'); await send('Log.enable');
  await send('Fetch.enable', { patterns: [{ urlPattern: `${base}/api/v1/*`, requestStage: 'Request' }] });
  await send('Emulation.setDeviceMetricsOverride', { width: 1440, height: 1000, deviceScaleFactor: 1, mobile: false });
  await send('Page.addScriptToEvaluateOnNewDocument', { source: `localStorage.setItem('nyro-locale','en-US');localStorage.setItem('nyro-theme','light');` });
  await navigate('/performance');
  let state=await ready();
  const nav=await evaluate(`[...document.querySelectorAll('aside nav a')].map(el=>el.getAttribute('href'))`);
  assert.deepEqual(nav.slice(nav.indexOf('/stats'),nav.indexOf('/stats')+3),['/stats','/performance','/extensions']);
  assertPoints(state,expected); assertAxes(state,250);
  assert.equal(report.apiCalls.filter(call=>call.path==='/api/v1/model-performance').length,1,'One batch snapshot on initial navigation');
  assert.ok(state.rows.some(row=>row.text.includes('Provider disabled')));
  assert.ok(!state.body.includes(unrated.model));
  for(const pair of [pairs[7],pairs[8]]) {const row=await detailRow(pair);assert.ok(row[2].includes('–') && row[5].includes('No valid TPS'));}
  assert.ok(!expected.some(row=>row.pair.overrides && row.tier==='mixed'),'Any override including equal common has no mixed point');
  check('batch snapshot, provider/model/tier identities, fallback scores, one-sample hollow, full numbered index, actual coordinates');
  await screenshot('performance-en-desktop');

  const overlap=state.points.find(point=>point.members===2);
  assert.ok(overlap); assert.ok(state.labels.some(label=>label.endsWith(' ×2')));
  const nearby=expected.find(row=>row.pair===pairs[5]);
  await evaluate(`(${circleExpression(overlap.ids[0])}).parentElement.focus()`);
  await waitFor(async()=> (await chartState()).rows.filter(row=>row.opacity===1).length===3,'focus highlights both overlap members and near point');
  await key('Enter');
  await waitFor(()=>evaluate(`document.querySelector('section[aria-label="Selected region candidates"]')?.querySelectorAll('button').length===3`),'near/overlap candidate selection');
  const candidateText=await evaluate(`document.querySelector('section[aria-label="Selected region candidates"]').innerText`);
  for(const id of [...overlap.ids,nearby.id]) assert.ok(candidateText.includes(id));
  await screenshot('performance-overlap-near-candidates');
  await clickExpression(`[...document.querySelectorAll('section[aria-label="Selected region candidates"] button')].find(el=>el.textContent.includes(${literal(overlap.ids[1])}))`);
  await waitFor(async()=> (await chartState()).rows.filter(row=>row.pressed==='true').length===1,'select one coincident member');
  await key('Escape');
  await waitFor(async()=> (await chartState()).rows.every(row=>row.pressed==='false'),'Escape clears pin');
  const selected=expected.find(row=>row.pair===effort && row.tier==='high');
  await evaluate(`document.querySelector(${literal(indexSelector(selected.id))}).focus()`); await key(' ','Space');
  await waitFor(async()=> (await chartState()).rows.find(row=>row.id===selected.id)?.pressed==='true','Space pins focused numbered index');
  await send('Input.dispatchMouseEvent',{type:'mouseMoved',x:5,y:5});
  assert.equal((await chartState()).rows.find(row=>row.id===selected.id).pressed,'true','Pin survives mouseleave');
  await key('Escape');
  check('focus links both views; Enter/Space pins, Escape clears; exact and near members independently selectable');

  await select('Filter by tier','low (includes minimal)');
  state=await ready({plotted:1,missing:1}); assertAxes(state,200);
  assertPoints(state,expected.filter(row=>row.tier==='low'));
  await select('Filter by tier','All tiers'); await ready();
  await fill('input[aria-label="Search IDs, providers or models"]','overlap');
  state=await ready({plotted:3,missing:0}); assertAxes(state,200);
  assertPoints(state,expected.filter(row=>row.pair.model.includes('overlap')));
  await fill('input[aria-label="Search IDs, providers or models"]',''); await ready();
  await select('Filter by provider',disabled.name);
  state=await ready({plotted:1,missing:0}); assertPoints(state,expected.filter(row=>row.pair.provider===disabled));
  await select('Filter by provider','All providers'); await ready();
  const beforeZoom=(await chartState()).points.map(({opacity,pressed,...point})=>point);
  await evaluate(`(() => {const el=document.querySelector('select');el.value='2';el.dispatchEvent(new Event('change',{bubbles:true}))})()`);
  assert.deepEqual((await chartState()).points.map(({opacity,pressed,...point})=>point),beforeZoom,'Zoom cannot change actual SVG coordinate, membership or IDs');
  await evaluate(`(() => {const el=document.querySelector('select');el.value='1';el.dispatchEvent(new Event('change',{bubbles:true}))})()`);
  check('IDs stable across tier/provider/search filters and zoom; X fixed 100, Y default 200 and 225 expands to 250');

  await clickExpression(`document.querySelector('button[title="切换到中文"]')`);
  await waitFor(() => evaluate(`document.querySelector('h1')?.textContent === '性能'`), 'Chinese locale');
  const chineseState = await ready();
  assert.ok(chineseState.summary.includes('无有效 TPS'), 'Chinese omission summary');
  await screenshot('performance-zh-desktop');
  await send('Emulation.setDeviceMetricsOverride', { width: 390, height: 844, deviceScaleFactor: 1, mobile: true });
  await waitFor(() => evaluate(`document.querySelector('main').getBoundingClientRect().width >= 220`), 'usable mobile main content width');
  assert.ok(await evaluate(`document.documentElement.scrollWidth <= innerWidth + 1`), 'No whole-page mobile horizontal overflow');
  await ready();
  await screenshot('performance-zh-mobile');
  await clickExpression(`document.querySelector('button[title="Switch to English"]')`);
  await waitFor(() => evaluate(`document.querySelector('h1')?.textContent === 'Performance'`), 'English locale');
  await reload(); await ready();
  assert.ok(await evaluate(`document.documentElement.scrollWidth <= innerWidth + 1 && document.querySelector('main').getBoundingClientRect().width >= 220`), 'Mobile direct load preserves width and avoids page overflow');
  await screenshot('performance-en-mobile');
  assertPoints(await chartState(),expected);
  assert.ok(await evaluate(`document.querySelector('aside[aria-label="Complete numbered index"]').getBoundingClientRect().top>=document.querySelector('[data-testid="performance-chart"]').getBoundingClientRect().bottom`),'Mobile full index is below chart');
  await clickExpression(`document.querySelector(${literal(indexSelector(selected.id))})`);
  await screenshot('performance-en-mobile-selected-index');
  assert.equal((await chartState()).rows.find(row=>row.id===selected.id).pressed,'true');
  check('EN/ZH desktop/mobile, direct mobile load, list below chart, full names and selected score/source/TPS/count');

  await send('Emulation.setDeviceMetricsOverride', { width: 1440, height: 1000, deviceScaleFactor: 1, mobile: false });
  faultMode='partial'; await reload(); state=await ready({plotted:10,errors:1});
  assert.ok(state.body.includes('Injected profile statistics failure')); await screenshot('performance-partial-error');
  faultMode=null; await refresh(); await ready();
  for(const mode of ['negative','zero','string','infinity']){
    faultMode=mode; await reload();
    await waitFor(()=>evaluate(`Boolean(document.querySelector('[role="alert"]'))`),`${mode} invalid snapshot warning`);
    assert.equal((await chartState()).points.length,0,'Invalid batch must not invent zero or plot stale values');
  }
  faultMode='null'; await reload(); await ready({plotted:10,missing:8});
  await screenshot('performance-null-tps');
  faultMode=null; await refresh(); await ready();
  faultMode='snapshot'; await refresh();
  await waitFor(()=>evaluate(`Boolean(document.querySelector('[role="alert"]'))`),'warm snapshot HTTP500');
  assert.equal((await chartState()).points.length,0); await screenshot('performance-snapshot-error');
  await reload(); await waitFor(()=>evaluate(`Boolean(document.querySelector('[role="alert"]'))`),'cold snapshot HTTP500');
  state=await chartState(); assert.equal(state.points.length,0); assert.ok(state.body.includes('Unknown does not mean unrated'));
  faultMode=null; await refresh(); await ready();
  check('partial profile failure preserves others; invalid batch, null TPS and warm/cold HTTP500 distinguish unknown and recover');

  assert.ok(report.apiCalls.every(call=>!call.path.endsWith('/model-usage') && !/\/providers\/[^/]+\/(models|test|benchmark)$/.test(call.path)),'No legacy usage, catalog or benchmark queries in any browser path');
  assert.deepEqual(report.upstreamCalls, [], 'Chart must never call real upstream, including model catalogs');
  assert.deepEqual(report.consoleErrors, [], 'Unexpected browser console.error');
  assert.deepEqual(report.runtimeErrors, [], 'Unexpected browser runtime errors');
  assert.deepEqual(report.networkErrors, [], 'Unexpected browser network errors');
  assert.ok(report.injected.some(item => item.mode === 'partial') && report.injected.some(item => item.mode === 'snapshot'), 'Both intentional HTTP500 failure paths actually exercised');
  check('no upstream calls or unexpected browser errors', `${report.expectedNetworkErrors.length} deliberate HTTP500 network errors recorded separately`);
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
