#!/usr/bin/env node
/**
 * Real Chromium/CDP + isolated SQLite/admin-server integration smoke, no packages.
 * Build first: cargo build -p nyro-server --no-default-features; (cd webui && npm run build)
 * Run: node tests/webui/model-ratings-smoke.mjs
 * Optional: NYRO_SMOKE_BINARY, NYRO_SMOKE_WEBUI, CHROME_BIN (absolute paths).
 * Node >= 22 required (native fetch/WebSocket). See README.md in this directory.
 */
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { createServer } from 'node:http';
import { createServer as createTcpServer } from 'node:net';
import { access, mkdir, mkdtemp, writeFile } from 'node:fs/promises';
import { constants } from 'node:fs';
import { tmpdir, homedir } from 'node:os';
import { dirname, resolve, join } from 'node:path';
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
    try { const value = await fn(); if (value) return value; } catch (error) { last = error; }
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
assert.equal(typeof WebSocket, 'function', 'Node with built-in WebSocket is required');
let chrome;
for (const candidate of chromeCandidates) { try { await access(candidate, constants.X_OK); chrome = candidate; break; } catch {} }
assert.ok(chrome, 'Set CHROME_BIN to an executable Chromium binary');
const scratch = await mkdtemp(join(tmpdir(), 'nyro-model-ratings-smoke-'));
const children = [];
const report = { scratch, binary, webui, chrome, checks: [], screenshots: [], consoleErrors: [], runtimeErrors: [], networkErrors: [], expectedNetworkErrors: [], browserWarnings: [], childLogs: {}, success: false };
let upstream, cdp, sessionId, base;
const childEnv = Object.fromEntries(Object.entries(process.env).filter(([key]) => !key.startsWith('NYRO_')));
const trackChild = (name, command, args) => {
  const child = spawn(command, args, { cwd: root, env: childEnv, stdio: ['ignore', 'pipe', 'pipe'] });
  children.push(child); report.childLogs[name] = '';
  for (const stream of [child.stdout, child.stderr]) stream.on('data', chunk => { report.childLogs[name] += chunk.toString(); });
  child.on('error', error => { report.childLogs[name] += `\nSPAWN ERROR: ${error.stack}`; });
  return child;
};
async function stop(child) {
  if (child.exitCode !== null || child.signalCode !== null) return;
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
const ratingPath = (provider, model) => `/providers/${provider.id}/model-rating?model=${encodeURIComponent(model)}`;
const check = (name, detail = '') => { report.checks.push({ name, detail }); console.log(`PASS ${name}${detail ? ` — ${detail}` : ''}`); };
const send = (method, params = {}) => cdp.send(method, params, sessionId);
async function evaluate(expression) {
  const result = await send('Runtime.evaluate', { expression, returnByValue: true, awaitPromise: true });
  assert.ok(!result.exceptionDetails, `Browser evaluation failed: ${JSON.stringify(result.exceptionDetails)}`);
  return result.result.value;
}
const literal = JSON.stringify;
async function clickExpression(expression) {
  const point = await evaluate(`(() => { const el = (${expression}); if (!el || el.disabled) return null; el.scrollIntoView({block:'center',inline:'center'}); const r=el.getBoundingClientRect(); return {x:r.left+r.width/2,y:r.top+r.height/2}; })()`);
  assert.ok(point, `No enabled element: ${expression}`);
  await send('Input.dispatchMouseEvent', { type: 'mousePressed', ...point, button: 'left', clickCount: 1 });
  await send('Input.dispatchMouseEvent', { type: 'mouseReleased', ...point, button: 'left', clickCount: 1 });
}
const byText = (text, selector = 'button') => `[...document.querySelectorAll(${literal(selector)})].find(el => el.textContent.trim() === ${literal(text)})`;
const button = text => clickExpression(byText(text));
const aria = label => clickExpression(`document.querySelector('[aria-label='+CSS.escape(${literal(label)})+']')`);
async function fill(selector, value) {
  assert.ok(await evaluate(`(() => {const el=document.querySelector(${literal(selector)}); if(!el || el.disabled) return false; el.focus(); el.select(); return true;})()`), `Input missing: ${selector}`);
  await send('Input.insertText', { text: value });
  if (!value) {
    await send('Input.dispatchKeyEvent', { type: 'keyDown', key: 'Backspace', code: 'Backspace', windowsVirtualKeyCode: 8 });
    await send('Input.dispatchKeyEvent', { type: 'keyUp', key: 'Backspace', code: 'Backspace', windowsVirtualKeyCode: 8 });
  }
}
async function select(label, text) {
  await aria(label);
  await waitFor(() => evaluate(`Boolean(${byText(text, '[role="option"]')})`), `option ${text}`);
  await clickExpression(byText(text, '[role="option"]'));
}
async function navigate(path) {
  const response = await fetch(`${base}${path}`, { signal: AbortSignal.timeout(10_000) });
  assert.equal(response.status, 200, `SPA route must return HTTP 200: ${path}`);
  await send('Page.navigate', { url: `${base}${path}` });
  await waitFor(() => evaluate(`location.pathname===${literal(path)} && Boolean(document.querySelector('h1'))`), `navigate ${path}`);
}
async function tableRows() {
  return evaluate(`[...document.querySelectorAll('tbody tr')].map(tr=>[...tr.querySelectorAll('td')].map(td=>td.innerText))`);
}
async function readyRows(count) {
  await waitFor(async () => (await tableRows()).length === count && !(await evaluate(`document.body.innerText.includes('Loading ratings;')`)), `${count} table rows`);
}
async function screenshot(name) {
  await evaluate(`document.activeElement?.blur()`);
  await evaluate(`document.fonts.ready.then(() => new Promise(resolve => setTimeout(resolve, 350)))`);
  const { data } = await send('Page.captureScreenshot', { format: 'png', captureBeyondViewport: false });
  const path = join(scratch, `${name}.png`);
  await writeFile(path, Buffer.from(data, 'base64'));
  report.screenshots.push(path); console.log(`SCREENSHOT ${path}`);
}
async function openEditor(providerName, model) {
  await aria(`Edit rating for ${providerName} / ${model}`);
  await waitFor(() => evaluate(`Boolean(document.querySelector('[role="dialog"] input'))`), 'rating editor');
}
async function waitDialogClosed() { await waitFor(() => evaluate(`!document.querySelector('[role="dialog"]')`), 'editor closes'); }

try {
  const catalog = ['model/shared', 'model/zero-target', 'model/unrated'];
  let catalogFailure = false;
  upstream = createServer((req, res) => {
    res.setHeader('Content-Type', 'application/json');
    if (req.url?.includes('models') && catalogFailure) { res.statusCode = 503; res.end(JSON.stringify({ error: 'Injected local catalog outage' })); }
    else if (req.url?.includes('models')) res.end(JSON.stringify({ object: 'list', data: catalog.map(id => ({ id, object: 'model' })) }));
    else res.end(JSON.stringify({ choices: [{ message: { content: 'ok' } }] }));
  });
  await new Promise(resolve => upstream.listen(0, '127.0.0.1', resolve));
  const upstreamBase = `http://127.0.0.1:${upstream.address().port}`;
  const port = await freePort(); base = `http://127.0.0.1:${port}`; report.base = base;
  const dataDir = join(scratch, 'data'); await mkdir(dataDir);
  const server = trackChild('nyro', binary, ['--mode', 'admin', '--admin-host', '127.0.0.1', '--admin-port', String(port), '--data-dir', dataDir, '--storage-backend', 'sqlite', '--migrate-on-start', 'true', '--webui-dir', webui]);
  await waitFor(async () => {
    if (server.exitCode !== null) throw new Error(`Nyro exited ${server.exitCode}: ${report.childLogs.nyro}`);
    return (await fetch(`${base}/healthz`, { signal: AbortSignal.timeout(1000) })).ok;
  }, 'isolated Nyro admin server', 30_000);
  const createProvider = name => api('/providers', 'POST', { name, protocol: 'openai', base_url: `${upstreamBase}/v1`, api_key: 'smoke-only-not-a-secret', models_source: `${upstreamBase}/v1/models`, static_models: '', use_proxy: false, fast_mode: false });
  const alpha = await createProvider('Alpha Smoke');
  const beta = await createProvider('Beta Smoke');
  const disabled = await createProvider('Disabled Smoke');
  await api(`/providers/${disabled.id}`, 'PUT', { is_enabled: false });
  for (const [provider, model, score] of [[alpha, 'model/shared', 85], [beta, 'model/shared', 60], [alpha, 'retired/模型', 90], [disabled, 'disabled/model', 70]]) await api(ratingPath(provider, model), 'PUT', { score });
  for (const name of ['smoke-route-one', 'smoke-route-two']) await api('/models', 'POST', { name, target_provider: alpha.id, target_model: 'model/shared', targets: [] });
  const initialUnrated = await api(ratingPath(alpha, 'model/zero-target'));
  assert.equal(initialUnrated.status, 'unrated'); assert.equal(initialUnrated.score, null);
  for (const invalid of [-1, 101, 1.5, null, '0']) {
    const response = await fetch(`${base}/api/v1${ratingPath(alpha, 'model/zero-target')}`, { method: 'PUT', headers: {'Content-Type':'application/json'}, body: JSON.stringify({ score: invalid }) });
    assert.ok(!response.ok, `Invalid score accepted: ${literal(invalid)}`);
  }
  check('isolated API seed and invalid-score boundaries', base);

  const browser = trackChild('chrome', chrome, ['--headless', '--no-sandbox', '--disable-gpu', '--disable-dev-shm-usage', '--disable-background-networking', '--no-first-run', '--no-default-browser-check', '--remote-debugging-port=0', `--user-data-dir=${join(scratch, 'chrome')}`, 'about:blank']);
  const ws = await waitFor(() => {
    if (browser.exitCode !== null) throw new Error(`Chrome exited ${browser.exitCode}`);
    return report.childLogs.chrome.match(/DevTools listening on (ws:\/\/[^\s]+)/)?.[1];
  }, 'Chrome debugging endpoint');
  cdp = await CDP.connect(ws);
  const target = await cdp.send('Target.createTarget', { url: 'about:blank' });
  ({ sessionId } = await cdp.send('Target.attachToTarget', { targetId: target.targetId, flatten: true }));
  let faultMode = null;
  cdp.on('Runtime.exceptionThrown', params => report.runtimeErrors.push(params.exceptionDetails));
  cdp.on('Runtime.consoleAPICalled', params => { if (params.type === 'error') report.consoleErrors.push(params.args.map(arg => arg.value ?? arg.description).join(' ')); });
  cdp.on('Log.entryAdded', ({ entry }) => {
    if (entry.level === 'error' && entry.source === 'network') {
      const path = entry.url ? new URL(entry.url).pathname : '';
      const expected = faultMode === 'list' && path === '/api/v1/provider-model-ratings'
        || faultMode === 'save' && path.endsWith('/model-rating')
        || faultMode === 'catalog' && path.includes('/providers/') && path.endsWith('/models');
      (expected ? report.expectedNetworkErrors : report.networkErrors).push(entry);
    } else if (entry.level === 'warning') report.browserWarnings.push(entry);
  });
  cdp.on('Fetch.requestPaused', (params, sid) => {
    const url = new URL(params.request.url);
    const fail = faultMode === 'list' && url.pathname === '/api/v1/provider-model-ratings'
      || faultMode === 'save' && params.request.method === 'PUT' && url.pathname.endsWith('/model-rating');
    const command = fail ? cdp.send('Fetch.fulfillRequest', { requestId: params.requestId, responseCode: 503, responseHeaders: [{name:'Content-Type', value:'application/json'}], body: Buffer.from(JSON.stringify({error:'Injected smoke rating failure'})).toString('base64') }, sid)
      : cdp.send('Fetch.continueRequest', { requestId: params.requestId }, sid);
    command.catch(error => report.runtimeErrors.push({ interceptionError: error.message }));
  });
  await send('Page.enable'); await send('Runtime.enable'); await send('Log.enable');
  await send('Fetch.enable', { patterns: [{ urlPattern: `${base}/api/v1/*`, requestStage: 'Request' }] });
  await send('Emulation.setDeviceMetricsOverride', { width: 1440, height: 1000, deviceScaleFactor: 1, mobile: false });
  await send('Page.addScriptToEvaluateOnNewDocument', { source: `localStorage.setItem('nyro-locale','en-US');localStorage.setItem('nyro-theme','light');` });
  await navigate('/model-ratings'); await readyRows(8);
  let rows = await tableRows();
  assert.ok(rows.some(row => row[1] === 'retired/模型' && row[4].includes('Not in catalog')));
  assert.ok(rows.some(row => row[1] === 'disabled/model' && row[4].includes('Disabled')));
  assert.deepEqual(rows.slice(0, 4).map(row => Number(row[2].split('/')[0].trim())), [90, 85, 70, 60]);
  assert.ok(rows.slice(4).every(row => row[2] === 'Unrated'));
  check('flat global list, disabled/stale retention, descending sort and unrated-last');
  await screenshot('ratings-en-desktop');

  await openEditor(alpha.name, 'model/zero-target');
  await fill('[role="dialog"] input', '1.5'); await button('Save');
  await waitFor(() => evaluate(`document.querySelector('[role="dialog"] [role="alert"]')?.textContent.includes('whole number')`), 'fraction validation');
  await fill('[role="dialog"] input', '0'); await button('Save'); await waitDialogClosed();
  const zero = await api(ratingPath(alpha, 'model/zero-target'));
  assert.equal(zero.status, 'rated'); assert.equal(zero.score, 0);
  await send('Page.reload'); await readyRows(8);
  rows = await tableRows();
  assert.ok(rows.some(row => row[0].startsWith(alpha.name) && row[1] === 'model/zero-target' && row[2].startsWith('0')));
  check('single edit, fractional validation, save zero and reload persistence');
  await select('Sort', 'Score: low to high');
  await waitFor(async () => (await tableRows())[0]?.[2].startsWith('0'), 'ascending scores');
  rows = await tableRows(); assert.ok(rows.slice(5).every(row => row[2] === 'Unrated'));
  check('ascending sort keeps unrated after zero and every scored row');
  await fill('input[placeholder="0"]', '70');
  await waitFor(async () => (await tableRows()).length === 3, 'minimum score filtering');
  await fill('input[placeholder="0"]', '');
  await select('Rating state', 'Unrated');
  await waitFor(async () => (await tableRows()).length === 3, 'unrated filter');
  assert.ok((await tableRows()).every(row => row[2] === 'Unrated'));
  await select('Rating state', 'Rated');
  await waitFor(async () => (await tableRows()).length === 5, 'rated filter including zero');
  await select('Filter by provider', 'Beta Smoke');
  await waitFor(async () => (await tableRows()).length === 1, 'provider filter');
  assert.equal((await tableRows())[0][1], 'model/shared');
  await select('Filter by provider', 'All providers'); await select('Rating state', 'All');
  await fill('input[aria-label="Search models or providers"]', 'retired');
  await waitFor(async () => (await tableRows()).length === 1, 'search stale model');
  await fill('input[aria-label="Search models or providers"]', ''); await readyRows(8);
  check('range, rated/unrated, provider, and text filters');

  await openEditor(alpha.name, 'model/shared'); await fill('[role="dialog"] input', '91');
  faultMode = 'save'; await button('Save');
  await waitFor(() => evaluate(`document.querySelector('[role="dialog"]')?.innerText.includes('Save failed:')`), 'save failure retained in editor');
  assert.equal(await evaluate(`document.querySelector('[role="dialog"] input').value`), '91');
  assert.equal((await api(ratingPath(alpha, 'model/shared'))).score, 85);
  await screenshot('ratings-save-error');
  faultMode = null; await button('Save'); await waitDialogClosed();
  assert.equal((await api(ratingPath(beta, 'model/shared'))).score, 60);
  check('failed save keeps draft and persisted value; provider scores independent');

  await openEditor(alpha.name, 'model/zero-target'); await button('Clear rating');
  await waitFor(() => evaluate(`document.body.innerText.includes('Clear this rating?')`), 'clear confirmation');
  await button('Confirm clear'); await waitDialogClosed();
  const cleared = await api(ratingPath(alpha, 'model/zero-target'));
  assert.equal(cleared.status, 'unrated'); assert.equal(cleared.score, null);
  check('confirmed clear restores explicit unrated');

  await navigate('/available-models');
  await waitFor(() => evaluate(`Boolean(document.querySelector('[aria-label="Edit rating"]'))`), 'existing catalog edit action');
  await aria('Edit rating');
  await waitFor(() => evaluate(`Boolean(document.querySelector('[role="dialog"] input'))`), 'existing-page editor');
  assert.equal(await evaluate(`document.querySelector('[role="dialog"] input').value`), '91');
  await fill('[role="dialog"] input', '0'); await button('Save'); await waitDialogClosed();
  assert.equal((await api(ratingPath(alpha, 'model/shared'))).score, 0);
  assert.equal((await api('/models')).length, 2);
  await screenshot('available-models-en-desktop');
  check('existing-page editor shares pair score across two route mappings');

  await navigate('/model-ratings'); await readyRows(8);
  await clickExpression(`document.querySelector('button[title="切换到中文"]')`);
  await waitFor(() => evaluate(`document.querySelector('h1')?.textContent === '模型评分'`), 'Chinese locale');
  await screenshot('ratings-zh-desktop');
  await send('Emulation.setDeviceMetricsOverride', { width: 390, height: 844, deviceScaleFactor: 1, mobile: true });
  await waitFor(() => evaluate(`document.querySelector('main').getBoundingClientRect().width >= 220`), 'mobile main content width >=220px');
  await screenshot('ratings-zh-mobile');
  await clickExpression(`document.querySelector('button[title="Switch to English"]')`);
  await waitFor(() => evaluate(`document.querySelector('h1')?.textContent === 'Model Ratings'`), 'English locale');
  await send('Page.reload'); await readyRows(8);
  await waitFor(() => evaluate(`document.querySelector('main').getBoundingClientRect().width >= 220`), 'initial mobile navigation content width >=220px');
  await screenshot('ratings-en-mobile');
  assert.ok(await evaluate(`document.documentElement.scrollWidth <= innerWidth + 1`), 'Unexpected whole-page horizontal overflow at mobile width');
  assert.ok(await evaluate(`document.querySelector('main').getBoundingClientRect().width >= 220`), 'Mobile sidebar must leave at least 220px for main content');
  await openEditor(alpha.name, 'model/shared');
  assert.equal(await evaluate(`document.querySelector('[role="dialog"] input').value`), '0');
  await screenshot('ratings-en-mobile-editor');
  await button('Cancel'); await waitDialogClosed();
  await evaluate(`document.querySelector('table').scrollIntoView({block:'start'}); document.querySelector('table').parentElement.scrollLeft=340;`);
  await screenshot('ratings-en-mobile-table');
  await evaluate(`document.querySelector('main').scrollTop=0; document.querySelector('table').parentElement.scrollLeft=0;`);
  check('English and Chinese desktop/mobile rendering and usable mobile editor');

  await send('Emulation.setDeviceMetricsOverride', { width: 1440, height: 1000, deviceScaleFactor: 1, mobile: false });
  faultMode = 'list'; await button('Refresh ratings');
  await waitFor(() => evaluate(`document.body.innerText.includes('Unknown states are not unrated')`), 'rating load failure alert');
  rows = await tableRows(); assert.ok(rows.length > 0 && rows.every(row => row[2] === 'Rating unknown'));
  assert.ok(await evaluate(`document.querySelector('[aria-label="Rating state"]').disabled`));
  assert.ok(await evaluate(`[...document.querySelectorAll('tbody button')].every(button=>button.disabled)`));
  await screenshot('ratings-list-error');
  faultMode = null; await button('Retry ratings'); await readyRows(8);
  await waitFor(() => evaluate(`!document.body.innerText.includes('Unknown states are not unrated')`), 'rating retry recovery');
  check('list failures are unknown, not unrated; editing/filtering gated; retry recovers');

  faultMode = 'catalog'; catalogFailure = true;
  const failedCatalog = await fetch(`${base}/api/v1/providers/${alpha.id}/models?require_catalog=true`);
  assert.equal(failedCatalog.status, 502, 'Strict catalog API must propagate remote failure');
  await button('Refresh catalogs');
  await waitFor(() => evaluate(`document.body.innerText.includes('Catalog requests failed:')`), 'catalog outage warning');
  rows = await tableRows();
  assert.ok(rows.some(row => row[1] === 'retired/模型' && row[4].includes('Catalog unknown')));
  assert.ok(rows.every(row => !row[4].includes('Not in catalog')));
  assert.ok(await evaluate(`[...document.querySelectorAll('tbody button')].every(button=>!button.disabled)`));
  await screenshot('ratings-catalog-error');
  faultMode = null; catalogFailure = false; await button('Refresh catalogs');
  await waitFor(() => evaluate(`!document.body.innerText.includes('Catalog requests failed:')`), 'catalog retry recovery');
  check('catalog outage preserves scores and editability without false missing markers');

  assert.deepEqual(report.consoleErrors, [], 'Unexpected console.error messages');
  assert.deepEqual(report.runtimeErrors, [], 'Unexpected browser runtime errors');
  assert.deepEqual(report.networkErrors, [], 'Unexpected browser network errors');
  check('no unexpected browser console/runtime/network errors', `${report.expectedNetworkErrors.length} deliberate HTTP failures recorded separately`);
  report.success = true;
} catch (error) {
  report.error = error.stack;
  if (cdp && sessionId) {
    try { report.failureDom = await evaluate('document.body.innerText'); await screenshot('failure'); } catch {}
  }
  console.error(error.stack); process.exitCode = 1;
} finally {
  if (cdp) cdp.close();
  await Promise.all(children.map(stop));
  if (upstream) await new Promise(resolve => upstream.close(resolve));
  report.childrenStopped = children.every(child => child.exitCode !== null || child.signalCode !== null);
  await writeFile(join(scratch, 'report.json'), JSON.stringify(report, null, 2));
  console.log(`REPORT ${join(scratch, 'report.json')}`);
  console.log(`RESULT ${report.success ? 'PASS' : 'FAIL'}; child cleanup=${report.childrenStopped}`);
}
