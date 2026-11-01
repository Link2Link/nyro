#!/usr/bin/env node
/**
 * Offline built-WebUI smoke: native Node >=22 WebSocket + Chromium CDP only.
 * Build webui first, then: node tests/webui/system-monitor-smoke.mjs
 * Optional CHROME_BIN / NYRO_SMOKE_WEBUI absolute paths. No HTTP server, running
 * Nyro/DSH, database, credentials, Playwright dependency or external network.
 * Every browser request is fulfilled by Fetch from dist or deterministic fixtures.
 */
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { access, mkdtemp, readFile, writeFile } from 'node:fs/promises';
import { constants } from 'node:fs';
import { homedir, tmpdir } from 'node:os';
import { dirname, extname, join, resolve, sep } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '../..');
const build = resolve(process.env.NYRO_SMOKE_WEBUI || join(root, 'webui/dist'));
const chrome = process.env.CHROME_BIN || join(homedir(), '.cache/ms-playwright/chromium_headless_shell-1223/chrome-headless-shell-linux64/chrome-headless-shell');
const base = 'http://nyro.test';
const metricsPath = '/api/v1/system/status';
const delay = ms => new Promise(resolve => setTimeout(resolve, ms));
const literal = JSON.stringify;
async function waitFor(fn, label, timeout = 12_000) {
  const until = Date.now() + timeout;
  let last;
  while (Date.now() < until) {
    try { const result = await fn(); if (result) return result; } catch (error) { last = error; }
    await delay(40);
  }
  throw new Error(`Timed out: ${label}${last ? ` (${last.message})` : ''}`);
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
// Large loopback traffic must remain visible in its row, never in the totals.
const GiB = 1024 ** 3, MiB = 1024 ** 2;
function fixture(index, empty = false) {
  const warming = index === 0;
  return {
    sampled_at: 1_800_000_000_000 + index * 3000, sample_interval_seconds: warming ? null : 3,
    hostname: 'nyro-smoke-host', os: 'Fixture Linux 24.04', kernel_version: '6.8.0-smoke', architecture: 'x86_64', uptime_seconds: 183845,
    cpu: { brand: 'Deterministic eight-core CPU', logical_cores: 8, physical_cores: 4, usage_percent: warming ? null : index % 2 ? 42.5 : 57.5,
      frequency_mhz: 3200, cores: warming ? [] : [10, 20, 30, 40, 50, 60, 70, 80] },
    memory: { total_bytes: 16 * GiB, used_bytes: 8 * GiB, available_bytes: 8 * GiB, swap_total_bytes: 2 * GiB, swap_used_bytes: GiB / 2 },
    disks: empty ? [] : [
      { name: '/dev/fixture-root', mount_point: '/', file_system: 'ext4', total_bytes: 100 * GiB, available_bytes: 25 * GiB, removable: false },
      { name: '/dev/fixture-data', mount_point: '/mnt/fixture-data', file_system: 'xfs', total_bytes: 1024 * GiB, available_bytes: 512 * GiB, removable: true },
    ],
    networks: empty ? [] : [
      { name: 'eth0', received_bytes: MiB, transmitted_bytes: 2 * MiB, receive_bytes_per_second: warming ? null : 1024, transmit_bytes_per_second: warming ? null : 2048 },
      { name: 'wlan0', received_bytes: 3 * MiB, transmitted_bytes: 4 * MiB, receive_bytes_per_second: warming ? null : 3072, transmit_bytes_per_second: warming ? null : 4096 },
      { name: 'lo', received_bytes: 900 * GiB, transmitted_bytes: 800 * GiB, receive_bytes_per_second: warming ? null : 900 * MiB, transmit_bytes_per_second: warming ? null : 800 * MiB },
    ],
    process: { pid: 4242, memory_bytes: 128 * MiB, cpu_percent: warming ? null : 7.5 },
  };
}
const scratch = await mkdtemp(join(tmpdir(), 'nyro-system-monitor-smoke-'));
const report = { scratch, build, chrome, startedAt: new Date().toISOString(), checks: [], screenshots: [], apiCalls: [], requests: [],
  runtimeErrors: [], consoleErrors: [], networkErrors: [], browserWarnings: [], interceptionErrors: [], unexpectedRequests: [], virtualTime: [], success: false };
const pages = new Map();
let browser, cdp, activePage, chromeLog = '';
const check = (name, detail) => { report.checks.push({ name, ...(detail === undefined ? {} : { detail }) }); console.log(`PASS ${name}`); };
async function stop(child) {
  if (!child || child.exitCode !== null || child.signalCode !== null || child.pid === undefined) return;
  child.kill('SIGTERM');
  await Promise.race([new Promise(resolve => child.once('exit', resolve)), delay(2000)]);
  if (child.exitCode === null && child.signalCode === null) { child.kill('SIGKILL'); await new Promise(resolve => child.once('exit', resolve)); }
}
const send = (page, method, params = {}) => cdp.send(method, params, page.sessionId);
async function evaluate(page, expression) {
  const result = await send(page, 'Runtime.evaluate', { expression, returnByValue: true, awaitPromise: true });
  assert.ok(!result.exceptionDetails, `Browser evaluation failed: ${JSON.stringify(result.exceptionDetails)}`);
  return result.result.value;
}
async function click(page, expression) {
  const point = await evaluate(page, `(() => { const el=(${expression}); if (!el || el.disabled) return null; el.scrollIntoView({block:'center',inline:'center'}); const r=el.getBoundingClientRect(); return {x:r.left+r.width/2,y:r.top+r.height/2}; })()`);
  assert.ok(point, `No enabled element: ${expression}`);
  for (const type of ['mousePressed', 'mouseReleased']) await send(page, 'Input.dispatchMouseEvent', { type, ...point, button: 'left', clickCount: 1 });
  // Paused virtual time also pauses React's scheduled rendering/RAF work.
  if (page.virtualPaused) await tick(page, 50);
}
const button = text => `[...document.querySelectorAll('.system-monitor button')].find(el=>el.textContent.trim()===${literal(text)})`;
const count = page => report.apiCalls.filter(call => call.scenario === page.name && call.path === metricsPath).length;
async function state(page) {
  return evaluate(page, `(() => {
    const panel=document.querySelector('.system-monitor'); if (!panel) return null;
    const sections=[...panel.querySelectorAll('section')];
    const charts=[...panel.querySelectorAll('[role="img"]')].map(el=>({label:el.getAttribute('aria-label'),text:el.textContent,
      width:el.getBoundingClientRect().width,svg:!!el.querySelector('svg.recharts-surface'),
      curves:[...el.querySelectorAll('path.recharts-area-curve')].map(p=>p.getAttribute('d')),
      dots:el.querySelectorAll('.recharts-area-dot').length}));
    return { title:panel.querySelector('h1')?.textContent, theme:document.documentElement.dataset.theme,locale:document.documentElement.lang,
      badge:panel.querySelector('.monitor-live')?.textContent,badgeState:panel.querySelector('.monitor-live')?.dataset.state,
      alert:panel.querySelector('[role="alert"]')?.innerText??null,
      cards:sections.slice(0,4).map(el=>({label:el.querySelector('h2')?.textContent,value:el.querySelector('p')?.textContent,text:el.innerText})),
      meters:[...panel.querySelectorAll('[role="meter"]')].map(el=>({label:el.getAttribute('aria-label'),value:el.getAttribute('aria-valuenow'),text:el.getAttribute('aria-valuetext')})),
      sections:sections.map(el=>({title:el.querySelector('h2')?.textContent,text:el.innerText})),
      rows:[...panel.querySelectorAll('tbody tr')].map(row=>[...row.cells].map(cell=>cell.textContent)),charts,
      busy:!!panel.querySelector('button[aria-label="Refresh system metrics"],button[aria-label="刷新系统指标"]')?.disabled,
      body:panel.innerText,
      sizes:[document.documentElement,document.body,document.querySelector('main'),panel].map(el=>({tag:el.tagName,width:el.clientWidth,scroll:el.scrollWidth})),
    };
  })()`);
}
async function ready(page) {
  return waitFor(async () => {
    const value = await state(page);
    if (value && !value.busy && (value.cards.length === 4 || value.alert)) return value;
    if (page.virtualPaused) await tick(page, 50);
    return false;
  }, `${page.name} settled snapshot`);
}
async function domWait(page, expression, label) {
  return waitFor(async () => {
    if (await evaluate(page, expression)) return true;
    if (page.virtualPaused) await tick(page, 50);
    return false;
  }, label);
}
async function tick(page, milliseconds) {
  let expired;
  const done = new Promise(resolve => { expired = resolve; });
  page.virtualExpired = expired;
  await send(page, 'Emulation.setVirtualTimePolicy', { policy: 'pauseIfNetworkFetchesPending', budget: milliseconds });
  let timeout;
  try {
    await Promise.race([done, new Promise((_, reject) => { timeout = setTimeout(() => reject(new Error('Virtual-time budget did not expire')), 10_000); })]);
  } finally { clearTimeout(timeout); page.virtualExpired = null; }
  report.virtualTime.push({ scenario: page.name, milliseconds, requests: count(page) });
}
async function advance(page, milliseconds) {
  await tick(page, milliseconds);
  await ready(page);
}
async function refresh(page) {
  const before = count(page);
  await click(page, `document.querySelector('.system-monitor button[aria-label=${literal(page.zh ? '刷新系统指标' : 'Refresh system metrics')} ]')`);
  await waitFor(() => count(page) === before + 1, 'one manual metrics request');
  return ready(page);
}
async function screenshot(page, name, bottom = false) {
  await evaluate(page, `document.activeElement?.blur();document.querySelector('main').scrollTop=${bottom ? 'document.querySelector("main").scrollHeight' : '0'};document.fonts.ready`);
  if (page.virtualPaused) await tick(page, 50);
  const { data } = await send(page, 'Page.captureScreenshot', { format: 'png', captureBeyondViewport: false });
  const path = join(scratch, `${name}.png`);
  await writeFile(path, Buffer.from(data, 'base64')); report.screenshots.push(path);
}
async function intercept(params, sid) {
  const page = pages.get(sid);
  if (!page) throw new Error(`Unregistered target ${sid}`);
  const url = new URL(params.request.url);
  const record = { scenario: page.name, url: url.href, method: params.request.method, networkId: params.networkId, status: 200, source: 'fixture' };
  report.requests.push(record);
  let body = '', type = 'application/json';
  if (url.origin !== base) {
    // Built CSS imports Google Fonts. Fulfill empty CSS rather than contacting it.
    if (url.hostname !== 'fonts.googleapis.com') report.unexpectedRequests.push(record);
    type = 'text/css'; record.source = 'offline-external';
  } else if (url.pathname.startsWith('/api/')) {
    report.apiCalls.push({ scenario: page.name, phase: page.phase, path: url.pathname, mode: page.mode, method: params.request.method });
    if (params.request.method !== 'GET') report.unexpectedRequests.push(record);
    let data;
    if (url.pathname === metricsPath) {
      if (page.mode === 'error') { record.status = 503; body = JSON.stringify({ error: 'Deterministic system monitor fixture unavailable' }); }
      else data = fixture(page.sample++, page.mode === 'empty');
    } else if (url.pathname === '/api/v1/system/version') data = { version: '0.0.0-smoke', update_check: false, self_update: false };
    else if (url.pathname === '/api/v1/status') data = { running: true, version: '0.0.0-smoke' };
    else if (url.pathname === '/api/v1/system/extensions') data = [];
    else { record.status = 404; body = JSON.stringify({ error: `No fixture: ${url.pathname}` }); report.unexpectedRequests.push(record); }
    if (!body) body = JSON.stringify({ data });
  } else {
    const staticPath = url.pathname.startsWith('/assets/') || url.pathname === '/nyro-logo.png';
    const file = staticPath ? resolve(build, `.${decodeURIComponent(url.pathname)}`) : join(build, 'index.html');
    assert.ok(file.startsWith(`${build}${sep}`), 'Static file stays inside dist');
    assert.ok(staticPath || ['/system-monitor', '/extensions'].includes(url.pathname), `Unexpected SPA route ${url.pathname}`);
    record.source = 'dist';
    type = ({ '.js': 'application/javascript', '.css': 'text/css', '.png': 'image/png', '.svg': 'image/svg+xml', '.woff2': 'font/woff2' })[extname(file)] || 'text/html';
    body = await readFile(file);
  }
  await cdp.send('Fetch.fulfillRequest', { requestId: params.requestId, responseCode: record.status,
    responseHeaders: [{ name: 'Content-Type', value: type }, { name: 'Cache-Control', value: 'no-store' }],
    body: Buffer.from(body).toString('base64') }, sid);
  record.fulfilled = true;
}
async function openPage(name, { locale = 'en-US', theme = 'light', width = 1440, mode = 'data' } = {}) {
  const { browserContextId } = await cdp.send('Target.createBrowserContext');
  const { targetId } = await cdp.send('Target.createTarget', { url: 'about:blank', browserContextId });
  const { sessionId } = await cdp.send('Target.attachToTarget', { targetId, flatten: true });
  const page = { name, sessionId, browserContextId, zh: locale === 'zh-CN', mode, sample: 0, phase: 'initial', requested: new Set(), intercepted: new Set() };
  pages.set(sessionId, page); activePage = page;
  for (const domain of ['Page', 'Runtime', 'Log', 'Network']) await send(page, `${domain}.enable`);
  await send(page, 'Network.setCacheDisabled', { cacheDisabled: true });
  await send(page, 'Network.setBypassServiceWorker', { bypass: true });
  await send(page, 'Fetch.enable', { patterns: [{ urlPattern: '*', requestStage: 'Request' }] });
  await send(page, 'Emulation.setDeviceMetricsOverride', { width, height: 1100, deviceScaleFactor: 1, mobile: false });
  await send(page, 'Page.addScriptToEvaluateOnNewDocument', { source: `localStorage.setItem('nyro-locale',${literal(locale)});localStorage.setItem('nyro-theme',${literal(theme)});` });
  await send(page, 'Page.navigate', { url: `${base}/system-monitor` });
  await ready(page);
  await send(page, 'Emulation.setVirtualTimePolicy', { policy: 'pause' });
  page.virtualPaused = true;
  const value = await ready(page);
  assert.equal(value.theme, theme); assert.equal(value.locale, locale);
  assert.equal(value.title, page.zh ? '系统监控' : 'System Monitor');
  assert.ok(value.sizes.every(size => size.scroll <= size.width + 1), `No horizontal page/panel overflow: ${JSON.stringify(value.sizes)}`);
  check(`${name}: localized route, theme and no horizontal overflow`, value.sizes);
  return page;
}
async function closePage(page) {
  for (const id of page.requested) assert.ok(page.intercepted.has(id), `HTTP request was not intercepted: ${id}`);
  await cdp.send('Target.disposeBrowserContext', { browserContextId: page.browserContextId });
  pages.delete(page.sessionId); activePage = null;
}
function assertWarm(value, zh) {
  assert.equal(value.cards[0].value, '—', 'First CPU occupancy must not impersonate 0%');
  assert.equal(value.meters[0].value, null, 'Unknown CPU has no numeric meter value');
  assert.equal(value.cards[1].value, '50.0%'); assert.equal(value.cards[2].value, '75.0%');
  assert.ok(value.rows.every(row => row[1] === '—' && row[2] === '—'), 'First network rates must not impersonate zero');
  assert.match(value.body, /CPU —/); assert.match(value.charts[1].text, zh ? /等待第二次采样/ : /Waiting for a second sample/);
  assert.equal(value.charts[1].svg, false, 'No invented zero-throughput graph');
}
function assertMetrics(value, zh) {
  assert.match(value.cards[0].value, /^(42\.5|57\.5)%$/); assert.equal(value.cards[1].value, '50.0%'); assert.equal(value.cards[2].value, '75.0%');
  assert.match(value.cards[1].text, /8\.0 GiB \/ 16\.0 GiB/);
  const volumes = value.sections.find(section => section.title === (zh ? '磁盘与挂载点' : 'Storage volumes'));
  assert.match(volumes.text, /\/mnt\/fixture-data/); assert.match(volumes.text, /75\.0%/); assert.match(volumes.text, /50\.0%/);
  const network = value.sections.find(section => section.title === (zh ? '网络实时吞吐' : 'Network throughput'));
  assert.match(network.text, /2 (interfaces|个接口)/); assert.match(network.text, /4\.0 KiB\/s/); assert.match(network.text, /6\.0 KiB\/s/);
  const interfaces = value.sections.find(section => section.title === (zh ? '网络接口' : 'Network interfaces'));
  assert.match(interfaces.text, /4\.0 MiB/); assert.match(interfaces.text, /6\.0 MiB/);
  assert.equal(value.rows.length, 3); assert.match(value.rows[2][0], /lo(loopback|回环)/); assert.equal(value.rows[2][1], '900 MiB/s');
  assert.ok(value.charts.every(chart => chart.svg && chart.width > 100 && chart.curves.length === 2 && chart.curves.every(d => d && !/NaN|Infinity/.test(d))), 'Both real Recharts SVGs have finite nonempty series geometry');
}

try {
  assert.equal(typeof WebSocket, 'function', 'Native Node WebSocket required');
  await access(chrome, constants.X_OK); await access(join(build, 'index.html'));
  const env = Object.fromEntries(Object.entries(process.env).filter(([key]) => !/^(http|https|all|no)_proxy$/i.test(key)));
  browser = spawn(chrome, ['--headless', '--no-sandbox', '--disable-gpu', '--disable-dev-shm-usage', '--disable-background-networking',
    '--disable-component-update', '--disable-sync', '--no-first-run', '--no-default-browser-check', '--host-resolver-rules=MAP * ~NOTFOUND',
    '--remote-debugging-port=0', `--user-data-dir=${join(scratch, 'chrome-profile')}`, 'about:blank'], { cwd: root, env, stdio: ['ignore', 'pipe', 'pipe'] });
  report.browserPid = browser.pid;
  for (const stream of [browser.stdout, browser.stderr]) stream.on('data', data => { chromeLog += data; });
  browser.on('error', error => { chromeLog += `\nSPAWN ERROR: ${error.stack}`; });
  const endpoint = await waitFor(() => {
    assert.equal(browser.exitCode, null, `Chrome exited: ${chromeLog}`);
    return chromeLog.match(/DevTools listening on (ws:\/\/[^\s]+)/)?.[1];
  }, 'Chrome native CDP endpoint');
  cdp = await CDP.connect(endpoint);
  cdp.on('Fetch.requestPaused', (params, sid) => {
    if (params.networkId) pages.get(sid)?.intercepted.add(params.networkId);
    intercept(params, sid).catch(error => report.interceptionErrors.push(error.stack));
  });
  cdp.on('Network.requestWillBeSent', (params, sid) => { if (/^https?:/.test(params.request.url)) pages.get(sid)?.requested.add(params.requestId); });
  cdp.on('Emulation.virtualTimeBudgetExpired', (_, sid) => pages.get(sid)?.virtualExpired?.());
  cdp.on('Runtime.exceptionThrown', (params, sid) => report.runtimeErrors.push({ scenario: pages.get(sid)?.name, ...params.exceptionDetails }));
  cdp.on('Runtime.consoleAPICalled', (params, sid) => { if (params.type === 'error') report.consoleErrors.push({ scenario: pages.get(sid)?.name, args: params.args }); });
  cdp.on('Log.entryAdded', ({ entry }, sid) => {
    if (entry.level === 'error') report.networkErrors.push({ scenario: pages.get(sid)?.name, ...entry });
    else if (entry.level === 'warning') report.browserWarnings.push({ scenario: pages.get(sid)?.name, ...entry });
  });
  for (const scenario of [
    { name: 'zh-dark-desktop', locale: 'zh-CN', theme: 'dark', width: 1440 },
    { name: 'en-light-desktop', locale: 'en-US', theme: 'light', width: 1440 },
    { name: 'en-light-mobile', locale: 'en-US', theme: 'light', width: 390 },
  ]) {
    const page = await openPage(scenario.name, scenario);
    assertWarm(await ready(page), page.zh);
    check(`${page.name}: null first CPU/process/rates remain unavailable, not zero`);
    await screenshot(page, `${page.name}-first-sample`);
    const before = count(page);
    await advance(page, 3100);
    assert.equal(count(page), before + 1, 'Default 3s interval performs one poll');
    assertMetrics(await ready(page), page.zh);
    await advance(page, 3100);
    const populated = await ready(page);
    assertMetrics(populated, page.zh);
    assert.ok(populated.charts.every(chart => chart.curves.every(d => /L/.test(d))), 'Real multi-sample trend lines are visible');
    check(`${page.name}: CPU/memory/most-used disk and real trend SVGs; lo excluded from rates and totals`);
    await screenshot(page, `${page.name}-live`);
    await screenshot(page, `${page.name}-details`, true);
    assert.ok((await ready(page)).sizes.every(size => size.scroll <= size.width + 1), 'Populated charts/details do not overflow the page horizontally');
    const nav = await evaluate(page, `(() => {const a=document.querySelector('aside nav a[href="/system-monitor"]');return {label:a?.getAttribute('aria-label'),current:a?.getAttribute('aria-current')};})()`);
    assert.equal(nav.label, page.zh ? '系统监控' : 'System Monitor'); assert.equal(nav.current, 'page');
    await click(page, `document.querySelector('aside nav a[href="/extensions"]')`);
    await domWait(page, `location.pathname==='/extensions' && !document.querySelector('.system-monitor')`, 'leave monitor via sidebar');
    await click(page, `document.querySelector('aside nav a[href="/system-monitor"]')`);
    await ready(page);
    assert.equal(await evaluate(page, 'location.pathname'), '/system-monitor');
    check(`${page.name}: sidebar navigation enters /system-monitor`);
    if (page.name === 'en-light-desktop') {
      page.phase = 'pause';
      await click(page, button('Pause'));
      await waitFor(async () => (await ready(page)).badge === 'Paused', 'paused badge');
      let n = count(page);
      await advance(page, 10_000);
      assert.equal(count(page), n, 'Pause stops all scheduled metrics polling');
      await refresh(page); assert.equal(count(page), n + 1, 'Manual refresh works while paused');
      assert.equal((await ready(page)).badge, 'Paused');
      n = count(page); await advance(page, 4000); assert.equal(count(page), n, 'Manual refresh must not unpause polling');
      check('pause stops polling; manual refresh remains usable without unpausing');
      page.phase = 'resume';
      await click(page, button('Resume')); await ready(page);
      await waitFor(async () => (await ready(page)).badge === 'Live monitoring', 'resumed live badge');
      // Enabling a stale query may immediately refetch; count only after it settles.
      n = count(page); await advance(page, 3100); assert.equal(count(page), n + 1);
      check('resume restarts scheduled 3s polling');
      page.phase = 'interval';
      await click(page, `document.querySelector('[aria-label="Refresh interval"]')`);
      await click(page, `[...document.querySelectorAll('[role="option"]')].find(el=>el.textContent.trim()==='10s refresh')`);
      await domWait(page, `document.querySelector('[aria-label="Refresh interval"]').textContent.includes('10s refresh')`, '10s interval selected');
      n = count(page); await advance(page, 9000); assert.equal(count(page), n, '10s selection replaces previous 3s poll');
      await advance(page, 1100); assert.equal(count(page), n + 1, '10s refresh performs its next poll');
      check('refresh interval changes actual request cadence from 3s to 10s');
      page.phase = 'last-snapshot-error';
      await click(page, button('Pause'));
      const last = await ready(page);
      page.mode = 'error';
      const failed = await refresh(page);
      assert.equal(failed.badge, 'Connection lost'); assert.equal(failed.badgeState, 'error');
      assert.match(failed.alert, /last successful snapshot remains visible and is not live/);
      assert.deepEqual(failed.cards, last.cards, 'Failure preserves the last metrics, including latency');
      assert.deepEqual(failed.rows, last.rows); assert.deepEqual(failed.charts, last.charts);
      await screenshot(page, 'en-last-snapshot-connection-lost');
      page.mode = 'data'; await click(page, button('Retry'));
      await waitFor(async () => !(await ready(page)).alert, 'retry clears failure');
      assert.equal((await ready(page)).badge, 'Paused');
      check('failed refresh retains last snapshot with Connection lost; Retry recovers while paused');
    }
    await closePage(page);
  }
  const failedPage = await openPage('en-initial-failure', { mode: 'error' });
  let initialFailure = await ready(failedPage);
  assert.equal(initialFailure.badge, 'Connection lost'); assert.equal(initialFailure.cards.length, 0);
  assert.match(initialFailure.alert, /backend includes the system monitor endpoint/);
  await screenshot(failedPage, 'en-initial-failure');
  const n = count(failedPage); await advance(failedPage, 2500); assert.equal(count(failedPage), n, 'No hidden immediate retries');
  failedPage.mode = 'data'; failedPage.phase = 'retry-recovery';
  await click(failedPage, button('Retry')); await ready(failedPage);
  initialFailure = await waitFor(async () => { const value = await ready(failedPage); return value.cards.length === 4 && !value.alert ? value : false; }, 'initial-failure Retry recovery');
  assertWarm(initialFailure, false); assert.equal(initialFailure.badge, 'Live monitoring');
  await screenshot(failedPage, 'en-initial-failure-recovered');
  check('initial endpoint failure has no fabricated snapshot; explicit Retry restores live monitoring');
  await closePage(failedPage);
  const emptyPage = await openPage('en-no-disks-or-networks', { mode: 'empty', width: 390 });
  const empty = await ready(emptyPage);
  assert.equal(empty.cards[2].value, '—'); assert.equal(empty.rows.length, 0);
  assert.match(empty.body, /No accessible volumes reported by this environment/); assert.match(empty.body, /No network interfaces reported/);
  assert.match(empty.sections.find(section => section.title === 'Network throughput').text, /0 interfaces/);
  assert.equal(empty.charts[1].svg, false); assert.match(empty.charts[1].text, /Waiting for a second sample/);
  assert.equal(empty.meters.find(meter => meter.label === 'Disk usage').value, null);
  await refresh(emptyPage); assert.equal((await ready(emptyPage)).charts[1].svg, false);
  await screenshot(emptyPage, 'en-empty-mobile'); await screenshot(emptyPage, 'en-empty-mobile-details', true);
  check('no disks/network interfaces are explicit empty states, not invented 0% or 0 B/s');
  await closePage(emptyPage);
  assert.deepEqual(report.runtimeErrors, [], 'No runtime exceptions'); assert.deepEqual(report.consoleErrors, [], 'No console.error');
  assert.deepEqual(report.interceptionErrors, [], 'All interception handlers succeeded'); assert.deepEqual(report.unexpectedRequests, [], 'Only whitelisted fixtures/static assets requested');
  assert.ok(report.requests.every(request => request.fulfilled), 'Every HTTP(S) request fulfilled locally; none continued');
  assert.ok(report.networkErrors.every(error => error.source === 'network' && error.url === `${base}${metricsPath}` && /503/.test(error.text)), 'Only deliberate fixture HTTP503 errors permitted');
  assert.equal(report.networkErrors.length, report.requests.filter(request => request.status === 503).length, 'All deliberate HTTP failures accounted for');
  check('all HTTP(S) locally intercepted; zero runtime/console errors; only deliberate HTTP503 network diagnostics');
  report.success = true;
} catch (error) {
  report.error = error.stack; process.exitCode = 1; console.error(error.stack);
  if (activePage && cdp) { try { report.failureDom = await evaluate(activePage, 'document.body.innerText'); await screenshot(activePage, 'failure'); } catch {} }
} finally {
  if (cdp) cdp.close();
  await stop(browser);
  report.childrenStopped = !browser || browser.exitCode !== null || browser.signalCode !== null;
  report.chromeLog = chromeLog; report.finishedAt = new Date().toISOString();
  await writeFile(join(scratch, 'report.json'), JSON.stringify(report, null, 2));
  console.log(`REPORT ${join(scratch, 'report.json')}`);
  console.log(`RESULT ${report.success ? 'PASS' : 'FAIL'}; child cleanup=${report.childrenStopped}`);
}
