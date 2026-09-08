#!/usr/bin/env node
/**
 * Actual isolated Nyro admin server + scratch SQLite + Chromium/CDP; no npm packages.
 * Build first: cargo build -p nyro-server; (cd webui && npm run build)
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
let trap, cdp, sessionId, base, faultMode = null, envelopeSnapshot = null;
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
const ratingPath = pair => `/providers/${pair.provider.id}/model-rating?model=${encodeURIComponent(pair.model)}`;
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
const circleExpression = id => `[...document.querySelectorAll('[data-testid="performance-point"]')].find(el=>el.dataset.pointIds.split(',').includes(${literal(id)}))`;
async function select(label, text) {
  await clickExpression(`document.querySelector('[aria-label="${label}"]')`);
  const option = `[...document.querySelectorAll('[role="option"]')].find(el=>el.textContent.trim()===${literal(text)})`;
  await waitFor(() => evaluate(`Boolean(${option})`), `option ${text}`);
  await clickExpression(option);
}
async function screenshot(name, { preserveFocus = false } = {}) {
  await evaluate(`${preserveFocus ? '' : 'document.activeElement?.blur();'} document.fonts.ready.then(() => new Promise(resolve => setTimeout(resolve, 350)))`);
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
    const lines=[...(chart?.querySelectorAll('line')??[])].map(el=>({testid:el.dataset.testid,leader:Boolean(el.closest('[data-testid="performance-label"]')),x1:el.x1.baseVal.value,x2:el.x2.baseVal.value,y1:el.y1.baseVal.value,y2:el.y2.baseVal.value,stroke:getComputedStyle(el).stroke}));
    const envelopes=[...(chart?.querySelectorAll('[data-testid="performance-envelope"]')??[])].map(el=>({tag:el.tagName,points:Array.from({length:el.points?.numberOfItems??0},(_,index)=>{const p=el.points.getItem(index);return {x:p.x,y:p.y}}),fill:getComputedStyle(el).fill,stroke:getComputedStyle(el).stroke,dash:getComputedStyle(el).strokeDasharray,pointerEvents:getComputedStyle(el).pointerEvents,attrs:attrs(el)}));
    const axisTexts=[...(chart?.querySelectorAll('text')??[])].filter(el=>!el.closest('[data-testid="performance-label"]')).map(el=>({text:el.textContent,x:Number(el.getAttribute('x')),y:Number(el.getAttribute('y'))}));
    return {points,rows,lines,envelopes,axisTexts,polygons:chart?.querySelectorAll('polygon').length??0,busy:Boolean(document.querySelector('button[aria-label="Refresh performance"],button[aria-label="刷新性能数据"]')?.disabled),counts:attrs(summary),axis:attrs(chart),summary:summary?.innerText,labels:[...document.querySelectorAll('[data-testid="performance-label"] text')].map(el=>el.textContent),body:document.body.innerText};
  })()`);
}
async function ready({ plotted = 10, missing = 2, errors = 0 } = {}) {
  return waitFor(async () => {
    const state=await chartState();
    return state.points.reduce((n,p)=>n+p.members,0)===plotted && Number(state.counts['data-plotted-count'])===plotted && Number(state.counts['data-missing-count'])===missing && Number(state.counts['data-error-count'])===errors && !state.busy ? state : false;
  }, `settled chart: ${plotted} plotted, ${missing} missing, ${errors} errors`);
}
function assertAxes(state, yMax, xMin, xMax) {
  const a=state.axis;
  assert.equal(Number(a['data-x-min']),xMin); assert.equal(Number(a['data-x-max']),xMax); assert.equal(Number(a['data-y-max']),yMax);
  const left=Number(a['data-plot-left']),right=Number(a['data-plot-right']),top=Number(a['data-plot-top']),bottom=Number(a['data-plot-bottom']);
  for(const point of state.points){
    assert.ok(Math.abs(point.x-(left+(point.score-xMin)/(xMax-xMin)*(right-left)))<1e-4, 'Actual SVG X uses visible minimum/maximum score ticks, never jitter/centroid');
    assert.ok(Math.abs(point.y-(bottom-point.tps/yMax*(bottom-top)))<1e-4, 'Actual SVG Y equals exact backend TPS');
  }
}
function expectedRows(snapshot, pairs) {
  return snapshot.models.map(model => {
    const pair=pairs.find(p=>p.provider.id===model.rating.provider_id && p.model===model.rating.upstream_model);
    return {pair,score:model.rating.score,stats:model.mixed,status:model.status,key:JSON.stringify([pair.provider.id,pair.model])};
  }).sort((a,b)=>a.key<b.key?-1:a.key>b.key?1:0).map((row,i)=>({...row,id:`P${String(i+1).padStart(2,'0')}`}));
}
function assertPoints(state, rows) {
  const plotted=rows.filter(row=>row.status==='ready' && row.score!==null && row.stats.average_tps!==null && row.stats.valid_tps_count>0);
  assert.equal(state.rows.length,0,'No permanent numbered index');
  assert.deepEqual(state.points.flatMap(point=>point.ids).sort(),plotted.map(row=>row.id).sort());
  for(const row of plotted){
    const point=state.points.find(point=>point.ids.includes(row.id));
    assert.equal(point.score,row.score); assert.equal(point.tps,row.stats.average_tps);
    if(point.members===1) assert.equal(point.fill==='white',row.stats.valid_tps_count<3, 'One/two samples hollow; three or more solid');
  }
  assert.ok(state.labels.length>0 && state.labels.every(label=>!/^P\d+( ×\d+)?$/.test(label)), 'SVG labels contain models, not IDs');
  assert.ok(!/\bP\d{2}\b/.test(state.body),'No visible point IDs in page or diagnostics');
  assert.ok(state.points.every(point=>point.members===point.ids.length));
}

const sameCoordinate = (a,b) => Math.abs(a.x-b.x)<1e-3 && Math.abs(a.y-b.y)<1e-3;
const plottedRows = rows => rows.filter(row=>row.status==='ready' && Number.isInteger(row.score) && row.score>=0 && row.score<=100 && Number.isFinite(row.stats.average_tps) && row.stats.average_tps>0 && row.stats.valid_tps_count>0);

/** Independent small-fixture oracle: enumerate upper supporting lines, not the UI's hull algorithm. */
function expectedEnvelope(rows) {
  const groups = new Map();
  for(const row of plottedRows(rows)) {
    const x=row.score,y=row.stats.average_tps,key=JSON.stringify([x,y]);
    if(!groups.has(key)) groups.set(key,{x,y,keys:[]});
    groups.get(key).keys.push(row.key);
  }
  const coordinates=[...groups.values()];
  const candidates=coordinates.filter(p=>!coordinates.some(q=>(q.x>p.x || q.y>p.y) && q.x>=p.x && q.y>=p.y));
  const supported=new Set();
  if(candidates.length===1) supported.add(candidates[0]);
  for(const a of candidates) for(const b of candidates) {
    if(a.x>=b.x || a.y<=b.y) continue;
    // For left-to-right negative-slope segments, every input lies on/below an upper supporting line.
    const cross=p=>(b.x-a.x)*(p.y-a.y)-(b.y-a.y)*(p.x-a.x);
    if(coordinates.every(p=>cross(p)<=1e-8)) {
      for(const p of candidates) if(Math.abs(cross(p))<1e-8 && p.x>=a.x && p.x<=b.x) supported.add(p);
    }
  }
  const nodes=[...supported].sort((a,b)=>a.x-b.x);
  return {nodes,keys:nodes.flatMap(node=>node.keys).sort()};
}
function assertChartScaffolding(state, isZh=false) {
  const a=state.axis,left=Number(a['data-plot-left']),right=Number(a['data-plot-right']),top=Number(a['data-plot-top']),bottom=Number(a['data-plot-bottom']);
  const axes=state.lines.filter(line=>line.testid==='performance-axis');
  assert.equal(axes.length,2,'Keep exactly the X and Y axis lines');
  for(const [start,end] of [[{x:left,y:bottom},{x:right,y:bottom}],[{x:left,y:top},{x:left,y:bottom}]]) {
    assert.ok(axes.some(line=>(sameCoordinate({x:line.x1,y:line.y1},start) && sameCoordinate({x:line.x2,y:line.y2},end)) || (sameCoordinate({x:line.x2,y:line.y2},start) && sameCoordinate({x:line.x1,y:line.y1},end))),'Axes span the plot edges, not interior grid positions');
  }
  const ticks=state.lines.filter(line=>line.testid==='performance-tick');
  const xMin=Number(a['data-x-min']),xMax=Number(a['data-x-max']),yMax=Number(a['data-y-max']);
  assert.ok(ticks.length>=(xMax-xMin)/10+1+6,'Both axes retain short ticks');
  for(const line of [...axes,...ticks]) assert.ok(line.stroke && line.stroke!=='none','Axis/tick strokes are visible');
  for(const tick of ticks) {
    const length=Math.hypot(tick.x2-tick.x1,tick.y2-tick.y1);
    assert.ok(length>0 && length<=12,'Ticks are short, not replacement gridlines');
    assert.ok((tick.x1===tick.x2 && (Math.abs(tick.y1-bottom)<1e-3 || Math.abs(tick.y2-bottom)<1e-3)) || (tick.y1===tick.y2 && (Math.abs(tick.x1-left)<1e-3 || Math.abs(tick.x2-left)<1e-3)),'Ticks attach only to axes');
  }
  for(const line of state.lines) {
    const longHorizontal=Math.abs(line.y2-line.y1)<1e-3 && Math.abs(line.x2-line.x1)>(right-left)/2;
    const longVertical=Math.abs(line.x2-line.x1)<1e-3 && Math.abs(line.y2-line.y1)>(bottom-top)/2;
    if(!line.leader && (longHorizontal || longVertical)) assert.equal(line.testid,'performance-axis','No long horizontal/vertical gridlines; model-label leaders are intentionally permitted');
  }
  const leaders=state.lines.filter(line=>line.leader);
  assert.equal(leaders.length,state.labels.length,'Every placed model label retains its leader line');
  for(const leader of leaders) assert.ok(state.points.some(p=>sameCoordinate(p,{x:leader.x1,y:leader.y1}) || sameCoordinate(p,{x:leader.x2,y:leader.y2})),'Label leaders remain anchored at actual data points');
  for(let score=xMin;score<=xMax;score+=10) {
    const x=left+(score-xMin)/(xMax-xMin)*(right-left);
    assert.ok(state.axisTexts.some(t=>Number(t.text)===score && Math.abs(t.x-x)<1e-3 && t.y>bottom),'Keep X numeric tick labels at shared-scale positions');
  }
  for(let index=0;index<=5;index++) {
    const value=yMax*index/5,y=bottom-(bottom-top)*index/5;
    assert.ok(state.axisTexts.some(t=>Number(t.text)===value && t.x<left && Math.abs(t.y-y)<=8),'Keep Y numeric tick labels');
  }
  assert.ok(state.axisTexts.some(t=>t.text.includes(isZh?'能力评分':'Capability score')),'Keep localized X axis title');
  assert.ok(state.axisTexts.some(t=>t.text==='TPS (tok/s)'),'Keep TPS axis title');
  assert.equal(state.polygons,0,'The upper-right envelope must never become a closed polygon');
}
function assertEnvelope(state, rows, isZh=false) {
  assertChartScaffolding(state,isZh);
  const expected=expectedEnvelope(rows),a=state.axis;
  const declared=a['data-envelope-member-keys'];
  if(declared!==undefined) assert.deepEqual(JSON.parse(declared).sort(),expected.keys,'Chart membership matches independent supporting-line oracle');
  assert.equal(state.envelopes.length,expected.nodes.length>=2?1:0,'Empty/single-coordinate boundary has membership but no line');
  if(expected.nodes.length<2) return expected;
  const line=state.envelopes[0];
  assert.equal(line.tag,'polyline','Use one open SVG polyline, never a closed path/polygon');
  assert.equal(line.fill,'none'); assert.equal(line.pointerEvents,'none');
  assert.ok(line.stroke && line.stroke!=='none' && line.stroke!=='rgba(0, 0, 0, 0)','Envelope stroke remains visible');
  assert.ok(line.dash!=='none' && line.dash.split(/[ ,]+/).some(n=>parseFloat(n)>0),'Envelope is dashed');
  if(line.attrs['data-member-keys']!==undefined) assert.deepEqual(JSON.parse(line.attrs['data-member-keys']).sort(),expected.keys,'Polyline metadata includes every exact boundary key');
  const left=Number(a['data-plot-left']),right=Number(a['data-plot-right']),top=Number(a['data-plot-top']),bottom=Number(a['data-plot-bottom']);
  const mapped=expected.nodes.map(p=>({x:left+(p.x-Number(a['data-x-min']))/(Number(a['data-x-max'])-Number(a['data-x-min']))*(right-left),y:bottom-p.y/Number(a['data-y-max'])*(bottom-top)}));
  assert.ok(line.points.length>=2);
  assert.ok(sameCoordinate(line.points[0],mapped[0]) && sameCoordinate(line.points.at(-1),mapped.at(-1)),'Polyline ends at fastest/strongest boundary models, with no axis connections or closing segment');
  for(const [index,point] of line.points.entries()) {
    assert.ok(mapped.some(p=>sameCoordinate(p,point)),'Every polyline vertex maps a real supporting-boundary coordinate using shared X min/max and Y scale');
    if(index) assert.ok(point.x>line.points[index-1].x && point.y>line.points[index-1].y,'Open upper-right polyline is score-increasing/TPS-decreasing, with no horizontal/vertical tails');
  }
  // Collinear members may be retained as vertices or lie on one unsplit straight segment.
  for(const point of mapped) assert.ok(line.points.slice(1).some((b,i)=>{
    const a=line.points[i],t=(point.x-a.x)/(b.x-a.x);
    return t>=-1e-6 && t<=1+1e-6 && Math.abs(point.y-(a.y+t*(b.y-a.y)))<1e-3;
  }),'Every supporting coordinate lies on the rendered line, including true corners and collinear members');
  return expected;
}
async function assertEnvelopeTooltips(state, rows, isZh=false) {
  const expected=expectedEnvelope(rows),seen=new Set();
  for(const point of state.points) {
    await evaluate(`document.activeElement?.blur(); (${circleExpression(point.ids[0])}).scrollIntoView({block:'center'}); (${circleExpression(point.ids[0])}).parentElement.focus()`);
    await key('Enter');
    const details=await waitFor(()=>evaluate(`(()=>{const tip=document.querySelector('[role="tooltip"]');if(!tip)return null;const rows=[...tip.querySelectorAll('[data-point-id]')].map(el=>({id:el.dataset.pointId,text:el.innerText,badges:[...el.querySelectorAll('[data-testid="performance-envelope-member"]')].map(b=>({key:b.dataset.pointKey,text:b.textContent}))}));return rows.some(r=>r.id===${literal(point.ids[0])})?rows:null})()`),'Envelope tooltip for actual point ID');
    for(const detail of details) {
      const row=rows.find(row=>row.id===detail.id); assert.ok(row,'Tooltip IDs refer to visible snapshot rows'); seen.add(row.key);
      assert.ok(detail.text.includes(`${row.score}/100`) && detail.text.includes(`${row.stats.average_tps.toFixed(1)} tok/s`),'Envelope membership never changes score/TPS or one-decimal tooltip display');
      const member=expected.keys.includes(row.key);
      assert.equal(detail.badges.length,member?1:0,'Badge appears only within each boundary model, not all Pareto or all coincident-hit models');
      if(member) {assert.equal(detail.badges[0].key,row.key);assert.equal(detail.badges[0].text,isZh?'位于当前可见模型的包络线':'On the visible-model envelope');}
      if(row.stats.valid_tps_count<3) assert.ok(detail.text.includes(isZh?'低样本量':'Low sample count'),'Low-sample membership keeps the sample warning');
    }
    await key('Escape');
  }
  assert.deepEqual([...seen].sort(),plottedRows(rows).map(row=>row.key).sort(),'Every plotted model, including identical-coordinate keys, received a tooltip membership check');
  const stable=points=>points.map(({opacity,pressed,...point})=>point);
  assert.deepEqual(stable((await chartState()).points),stable(state.points),'Boundary hover/focus does not move any actual point or alter raw TPS');
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
    { provider: alpha, model: 'model/effort-profile', score: 80 },
    { provider: alpha, model: 'model/no-history', score: 25 },
    { provider: beta, model: 'model/legacy-only', score: 90 },
    { provider: beta, model: 'model/single-rating', score: 55 },
    { provider: beta, model: 'MiniMax-M3', score: 68 },
    { provider: alpha, model: 'model/invalid-tokens-or-time', score: 35 },
  ];
  const unrated = { provider: alpha, model: 'model/unrated-fast' };
  for (const pair of pairs) await api(ratingPath(pair), 'PUT', { score: pair.score });
  const logs = [], now = Date.now();
  const log = (pair, output, upstream = 1000, extra = {}) => logs.push({
    id: `performance-smoke-${String(logs.length).padStart(4,'0')}`, created_at: now - 60_000 + logs.length * 100,
    provider_id: pair.provider.id, provider_name: pair.provider.name, upstream_model: pair.model,
    model_name: 'unrelated-logical-route', client_model: 'unrelated-client-alias',
    client_protocol: 'openai', upstream_protocol: 'openai', method: 'POST', path: '/v1/chat/completions',
    client_status_code: 200, upstream_status_code: 200, input_tokens: 10, output_tokens: output,
    // TPS must use these same legacy fields as logs/model-usage: 1000ms by default.
    cache_read_tokens: 0, latency_upstream_ms: upstream, latency_total_ms: upstream === null ? null : upstream + 100,
    is_stream: 0, stream_chunks_count: 0, stream_first_chunk_ms: null,
    performance_metadata_version: 1, upstream_effort_status: 'present', upstream_effort_raw: 'high', upstream_effort_tier: 'high',
    request_completion: 'completed', completion_reason: 'stop', upstream_response_mode: 'buffered',
    // Strict timing deliberately disagrees, including an old completion timestamp.
    // Neither strict timing, version, completion, nor HTTP status may gate legacy TPS.
    performance_upstream_ms: 7, performance_first_chunk_ms: 3, performance_completed_at: now - 30 * 86400000,
    ...extra,
  });
  log(pairs[0],900); log(pairs[0],800); // Older raw rows are outside the latest ten.
  for (let i=0;i<5;i++) {
    // 100 / (2s - 0.5s) and 50 / 1s average to the existing 175/3 TPS.
    log(pairs[0],100,2000,{is_stream:1,stream_chunks_count:10,stream_first_chunk_ms:500});
    log(pairs[0],50);
  }
  log(pairs[1],225); // Keep the expanded chart ceiling deterministic at 250.
  for(const pair of pairs.slice(2,6)) log(pair,pair===pairs[4]?80:60);
  const effort=pairs[6];
  for(let i=0;i<3;i++) log(effort,200,1000,{upstream_effort_raw:'minimal',upstream_effort_tier:'low'});
  // Exactly these ten raw retained rows are selected, including one invalid token sample.
  log(effort,0);
  log(effort,120,1000,{upstream_effort_raw:'max',upstream_effort_tier:'max'});
  for(const status of ['absent','unknown']) log(effort,150,1000,{upstream_effort_status:status,upstream_effort_raw:null,upstream_effort_tier:null});
  for(const [i,completion] of ['failed','incomplete','cancelled','unknown'].entries()) log(effort,40+i*10,1000,{
    request_completion:completion,completion_reason:completion==='incomplete'?'length':completion,
    performance_metadata_version:i===0?0:i===1?99:1,
  });
  log(effort,80,1000,{client_status_code:500}); log(effort,90,1000,{upstream_status_code:429});
  // Retained history older than seven days remains valid, even with metadata absent.
  log(pairs[8],100,null,{created_at:now-8*86400000,latency_total_ms:2000});
  for(const field of ['performance_metadata_version','upstream_effort_status','upstream_effort_raw','upstream_effort_tier',
    'request_completion','completion_reason','upstream_response_mode','performance_upstream_ms','performance_first_chunk_ms','performance_completed_at']) delete logs.at(-1)[field];
  log(pairs[9],55);
  // Legacy non-incremental fallbacks: <50ms generation, or TTFT >=80% of upstream.
  log(pairs[9],55,1000,{is_stream:1,stream_first_chunk_ms:980});
  log(pairs[9],55,1000,{stream_chunks_count:10,stream_first_chunk_ms:900});
  // New MiniMax logs can have unknown completion despite valid legacy tokens/timing.
  log(pairs[10],2007,20617,{is_stream:1,stream_chunks_count:200,stream_first_chunk_ms:1798,
    performance_metadata_version:1,request_completion:'unknown',completion_reason:null});
  log(pairs[11],200); // Must not refill from this older valid row after selecting ten invalid rows.
  for(let i=0;i<10;i++) {
    const invalid=[{output_tokens:0},{output_tokens:-5},{output_tokens:null},
      {latency_upstream_ms:0},{latency_upstream_ms:-1},{latency_upstream_ms:null,latency_total_ms:null}][i%6];
    log(pairs[11],100,1000,invalid);
  }
  log(unrated,220);
  const seedPath = join(scratch, 'seed-logs.json'); await writeFile(seedPath, JSON.stringify(logs, null, 2));
  const python = trackChild('sqlite-seed', 'python3', ['-c', seedPython, scratch, dataDir, seedPath]);
  const seedExit = await new Promise((resolve, reject) => { python.once('error', reject); python.once('exit', resolve); });
  assert.equal(seedExit, 0, `Scratch SQLite seed failed: ${report.childLogs['sqlite-seed']}`);
  report.seed = JSON.parse(report.childLogs['sqlite-seed'].trim());
  const snapshot=await api('/model-performance'); report.snapshot=snapshot;
  assert.equal(snapshot.window_start,null,'Retained logs have no seven-day performance cutoff');
  assert.equal(snapshot.models.length,pairs.length);
  const statsFor=pair=>snapshot.models.find(item=>item.rating.provider_id===pair.provider.id && item.rating.upstream_model===pair.model);
  assert.ok(Math.abs(statsFor(pairs[0]).mixed.average_tps-175/3)<1e-10);
  assert.equal(statsFor(pairs[0]).mixed.selected_request_count,10);
  const grouped=statsFor(effort);
  assert.equal(grouped.mixed.selected_request_count,10); assert.equal(grouped.mixed.valid_tps_count,9);
  assert.equal(grouped.mixed.average_tps,90); assert.ok(!Object.hasOwn(grouped,'tiers'));
  assert.equal(grouped.rating.score,80);
  assert.equal(statsFor(pairs[8]).mixed.average_tps,50,'No metadata, total-time fallback and retained >7-day history all remain valid');
  assert.ok(statsFor(pairs[8]).mixed.first_sample_at<snapshot.as_of-7*86400000);
  assert.equal(statsFor(pairs[10]).mixed.average_tps,2007/((20617-1798)/1000),'MiniMax unknown version1 uses legacy generation timing');
  assert.equal(statsFor(pairs[10]).mixed.valid_tps_count,1);
  assert.equal(statsFor(pairs[9]).mixed.average_tps,55,'Legacy streaming fallbacks use upstream duration for non-incremental responses');
  assert.equal(statsFor(pairs[9]).mixed.valid_tps_count,3);
  assert.equal(statsFor(pairs[7]).mixed.selected_request_count,0);
  assert.equal(statsFor(pairs[11]).mixed.selected_request_count,10);
  assert.equal(statsFor(pairs[11]).mixed.valid_tps_count,0,'Invalid latest samples do not refill from older valid history');
  assert.deepEqual(snapshot.models.filter(item=>item.mixed.average_tps===null).map(item=>item.rating.upstream_model).sort(),
    [pairs[7].model,pairs[11].model].sort(),'Only no-history and invalid tokens/timing are missing TPS');
  report.usageComparisons=[];
  for(const pair of pairs){
    const item=statsFor(pair),stats=item.mixed,usage=await api(usagePath(pair));
    assert.equal(stats.average_tps,usage.average_tps,`Exact /model-performance vs /model-usage average_tps parity: ${pair.provider.name}/${pair.model}`);
    assert.equal(stats.selected_request_count,usage.recent_sample_count);
    assert.ok(stats.selected_request_count<=10);
    assert.equal(item.untrusted_count,0,'Valid legacy logs must not be labeled untrusted');
    assert.ok(!Object.hasOwn(item,'profile') && !Object.hasOwn(item,'tiers'));
    if(stats.average_tps!==null) assert.ok(Number.isFinite(stats.first_sample_at) && stats.first_sample_at>=0 && stats.last_sample_at<=snapshot.as_of);
    report.usageComparisons.push({provider_id:pair.provider.id,model:pair.model,mixed:stats,usage});
  }
  const expected=expectedRows(snapshot,pairs);
  check('exact legacy usage TPS parity for every pair; latest ten raw logs across statuses/versions/completion; retained old and MiniMax unknown logs valid',report.seed.database);

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
    const payload=structuredClone(faultMode?.startsWith('envelope:') ? envelopeSnapshot : snapshot);
    const targetModel=payload.models.find(model=>model.rating.provider_id===pairs[1].provider.id && model.rating.upstream_model===pairs[1].model);
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
  assertPoints(state,expected); assertAxes(state,250,0,100);
  assert.equal(report.apiCalls.filter(call=>call.path==='/api/v1/model-performance').length,1,'One batch snapshot on initial navigation');
  assert.ok(!await evaluate(`document.querySelector('aside[aria-label="Complete numbered index"]')!==null`));
  assert.ok(!state.body.includes(unrated.model));
  for(const pair of [pairs[7],pairs[11]]) {const row=await detailRow(pair);assert.ok(row[1].includes('–') && row[4].includes('No valid TPS'));}
  for(const pair of [pairs[8],pairs[10]]) {
    const row=await detailRow(pair);
    assert.ok(row[1].includes(`${statsFor(pair).mixed.average_tps.toFixed(1)} tok/s`) && row[4].includes('Plotted'),'Retained old/unknown logs plot with one-decimal TPS');
  }
  assert.ok(!/untrusted|unconfirmed requests/i.test(state.body),'No untrusted-history warning for legacy-valid samples');
  assert.equal(expected.length,pairs.length,'Exactly one row per rated provider/model');
  assert.ok(!await evaluate(`document.querySelector('[aria-label="Filter by tier"]')!==null`),'No effort selector');
  check('batch snapshot, single provider/model scores, legacy-valid mixed TPS, one-decimal display, one-sample hollow, model labels and actual coordinates');
  await screenshot('performance-en-desktop');
  const baselineEnvelope=assertEnvelope(state,expected);
  assert.deepEqual(baselineEnvelope.keys,[expected.find(row=>row.pair===pairs[1]).key],'Real fixture has one dominating (100,225) boundary model, not a multi-point line');
  await assertEnvelopeTooltips(state,expected);
  const dominant=expected.find(row=>row.pair===pairs[1]);
  await evaluate(`(${circleExpression(dominant.id)}).parentElement.focus()`); await key('Enter');
  await waitFor(()=>evaluate(`Boolean(document.querySelector('[data-testid="performance-envelope-member"]'))`),'Dominating point retains its boundary badge without a line');
  await screenshot('performance-en-dominant-envelope-tooltip',{preserveFocus:true}); await key('Escape');
  check('real-data singleton envelope membership; no gridlines, retained axes, short ticks, numeric titles and model-label leaders');
  const miniMax=expected.find(row=>row.pair===pairs[10]);
  await evaluate(`(${circleExpression(miniMax.id)}).scrollIntoView({block:'center'}); (${circleExpression(miniMax.id)}).parentElement.focus()`); await key('Enter');
  await waitFor(()=>evaluate(`document.querySelector('[role="tooltip"]')?.innerText.includes('106.6 tok/s')`),'MiniMax unknown-completion tooltip uses one decimal');
  await screenshot('performance-minimax-unknown-tooltip',{preserveFocus:true});
  assert.ok(await evaluate(`document.querySelector('[role="tooltip"]')?.innerText.includes('106.6 tok/s')`),'MiniMax tooltip remains visible during evidence capture');
  await key('Escape');

  const overlap=state.points.find(point=>point.members===2);
  assert.ok(overlap);
  const overlapNames=expected.filter(row=>overlap.ids.includes(row.id)).map(row=>row.pair.model);
  const overlapLabel=await evaluate(`document.querySelector('[data-testid="performance-label"][data-point-ids="${overlap.ids.join(',')}"]')?.textContent`);
  for(const name of overlapNames) assert.ok(overlapLabel?.includes(name),'Coincident label names every member');
  const nearby=expected.find(row=>row.pair===pairs[5]);
  await evaluate(`(${circleExpression(overlap.ids[0])}).parentElement.focus()`);
  await key('Enter');
  const tooltip=()=>evaluate(`document.querySelector('[role="tooltip"]')?.innerText`);
  await waitFor(tooltip,'focus tooltip');
  const text=await tooltip();
  for(const row of expected.filter(row=>[...overlap.ids,nearby.id].includes(row.id))) {
    for(const value of [row.pair.model,row.pair.provider.name,row.pair.provider.id,`${row.score}/100`,`${row.stats.average_tps.toFixed(1)} tok/s`,`Valid TPS ${row.stats.valid_tps_count} / selected requests ${row.stats.selected_request_count}`]) assert.ok(text.includes(value),`Tooltip full detail ${value}`);
  }
  await key('Escape'); await waitFor(async()=>!await tooltip(),'Escape dismisses tooltip');
  const selected=expected.find(row=>row.pair===effort);
  await evaluate(`(${circleExpression(selected.id)}).parentElement.focus()`); await key(' ','Space');
  await waitFor(tooltip,'Space opens focused tooltip');
  await key('Escape');
  await evaluate(`document.activeElement?.blur(); (${circleExpression(overlap.ids[0])}).scrollIntoView({block:'center'})`);
  const dot=await evaluate(`(()=>{const r=(${circleExpression(overlap.ids[0])}).getBoundingClientRect();return {x:r.x+r.width/2,y:r.y+r.height/2}})()`);
  await send('Input.dispatchMouseEvent',{type:'mouseMoved',...dot}); await waitFor(tooltip,'mouse hover details');
  const blank=await evaluate(`(()=>{const svg=document.querySelector('[data-testid="performance-chart"]');const p=new DOMPoint(70,30).matrixTransform(svg.getScreenCTM());return {x:p.x,y:p.y}})()`);
  await send('Input.dispatchMouseEvent',{type:'mouseMoved',...blank}); await waitFor(async()=>!await tooltip(),'Moving from dot to chart blank dismisses details');
  await send('Input.dispatchMouseEvent',{type:'mouseMoved',...dot}); await waitFor(tooltip,'hover reopens details');
  const tip=await evaluate(`(()=>{const r=document.querySelector('[role="tooltip"]').getBoundingClientRect();return {x:r.x+10,y:r.y+10}})()`);
  await send('Input.dispatchMouseEvent',{type:'mouseMoved',...tip}); await delay(300); assert.ok(await tooltip(),'Tooltip remains while pointer reads it');
  await screenshot('performance-hover-details');
  await send('Input.dispatchMouseEvent',{type:'mouseMoved',x:10,y:10}); await waitFor(async()=>!await tooltip(),'Leaving tooltip closes details');
  check('direct model labels, all overlapping names, complete hover/focus details, Enter/Space and Escape, no permanent index');

  await fill('input[aria-label="Search providers or models"]','overlap');
  state=await ready({plotted:3,missing:0}); assertAxes(state,100,50,60);
  assertPoints(state,expected.filter(row=>row.pair.model.includes('overlap')));
  assertEnvelope(state,expected.filter(row=>row.pair.model.includes('overlap')));
  await fill('input[aria-label="Search providers or models"]',''); state=await ready(); assertAxes(state,250,0,100);
  await select('Filter by provider',disabled.name);
  state=await ready({plotted:1,missing:0}); assertPoints(state,expected.filter(row=>row.pair.provider===disabled)); assertAxes(state,100,70,80);
  assertEnvelope(state,expected.filter(row=>row.pair.provider===disabled));
  await select('Filter by provider','All providers'); state=await ready(); assertAxes(state,250,0,100);
  const beforeZoom=(await chartState()).points.map(({opacity,pressed,...point})=>point);
  await evaluate(`(() => {const el=document.querySelector('select');el.value='2';el.dispatchEvent(new Event('change',{bubbles:true}))})()`);
  assert.deepEqual((await chartState()).points.map(({opacity,pressed,...point})=>point),beforeZoom,'Zoom cannot change actual SVG coordinate, membership or IDs');
  await evaluate(`(() => {const el=document.querySelector('select');el.value='1';el.dispatchEvent(new Event('change',{bubbles:true}))})()`);
  check('IDs stable across filters and zoom; X adapts to visible overlap 50–60 and disabled 70–80, restores0–100; Y default100 and225→250');

  await clickExpression(`document.querySelector('button[title="切换到中文"]')`);
  await waitFor(() => evaluate(`document.querySelector('h1')?.textContent === '性能'`), 'Chinese locale');
  const chineseState = await ready();
  assert.ok(chineseState.summary.includes('无有效 TPS'), 'Chinese omission summary');
  assert.ok(!chineseState.body.includes('不可信历史'),'No Chinese untrusted-history warning for legacy-valid samples');
  assertEnvelope(chineseState,expected,true);
  await assertEnvelopeTooltips(chineseState,expected,true);
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
  assert.ok(!await evaluate(`document.querySelector('aside[aria-label="Complete numbered index"]')!==null`),'No mobile index');
  await clickExpression(`(${circleExpression(selected.id)})`);
  await waitFor(()=>evaluate(`Boolean(document.querySelector('[role="tooltip"]'))`),'Mobile tap opens tooltip');
  assert.ok(await evaluate(`(()=>{const r=document.querySelector('[role="tooltip"]').getBoundingClientRect();return r.left>=0 && r.top>=0 && r.right<=innerWidth && r.bottom<=innerHeight})()`),'Mobile tooltip stays within viewport');
  await screenshot('performance-en-mobile-tooltip',{preserveFocus:true});
  assert.ok(await tooltip(),'Mobile tooltip remains visible during evidence capture');
  await key('Escape');
  check('EN/ZH desktop/mobile, no permanent index, bounded mobile tap tooltip, no page overflow');

  await send('Emulation.setDeviceMetricsOverride', { width: 1440, height: 1000, deviceScaleFactor: 1, mobile: false });
  faultMode='partial'; await reload(); state=await ready({plotted:9,errors:1});
  assert.ok(state.body.includes('Injected profile statistics failure')); await screenshot('performance-partial-error');
  faultMode=null; await refresh(); await ready();
  for(const mode of ['negative','zero','string','infinity']){
    faultMode=mode; await reload();
    await waitFor(()=>evaluate(`Boolean(document.querySelector('[role="alert"]'))`),`${mode} invalid snapshot warning`);
    assert.equal((await chartState()).points.length,0,'Invalid batch must not invent zero or plot stale values');
  }
  faultMode='null'; await reload(); await ready({plotted:9,missing:3});
  await screenshot('performance-null-tps');
  faultMode=null; await refresh(); await ready();
  faultMode='snapshot'; await refresh();
  await waitFor(()=>evaluate(`Boolean(document.querySelector('[role="alert"]'))`),'warm snapshot HTTP500');
  assert.equal((await chartState()).points.length,0); await screenshot('performance-snapshot-error');
  await reload(); await waitFor(()=>evaluate(`Boolean(document.querySelector('[role="alert"]'))`),'cold snapshot HTTP500');
  state=await chartState(); assert.equal(state.points.length,0); assert.ok(state.body.includes('Unknown does not mean unrated'));
  faultMode=null; await refresh(); await ready();
  check('partial model failure preserves others; invalid batch, null TPS and warm/cold HTTP500 distinguish unknown and recover');

  // Geometry-only CDP snapshots reuse the real response contract and known local providers.
  // They never alter the persisted ratings/logs or the exact baseline API parity assertions.
  const fixturePair=(model,score,tps,samples=1,provider=alpha,status='ready')=>({provider,model,score,tps,samples,status});
  const convex=[fixturePair('envelope-convex/keep-A',40,200),fixturePair('envelope-convex/keep-B',60,120,2),fixturePair('envelope-convex/C',90,100,3,beta)];
  const scenarios=[
    {name:'convex-not-all-pareto',pairs:convex,boundary:[0,2],axes:[250,40,90]},
    {name:'convex-bend',pairs:[...convex,fixturePair('envelope-convex/D',70,190,1,beta)],boundary:[0,3,2],axes:[250,40,90]},
    {name:'same-x-same-y-duplicates',pairs:[
      fixturePair('envelope-ties/fastest',50,200),fixturePair('envelope-ties/fastest-copy',50,200,3,beta),
      fixturePair('envelope-ties/same-x-slower',50,170),fixturePair('envelope-ties/same-y-weaker',40,200),
      fixturePair('envelope-ties/strongest',90,100),fixturePair('envelope-ties/strongest-copy',90,100,2,beta),
      fixturePair('envelope-ties/same-y-weaker-right',70,100),fixturePair('envelope-ties/interior',70,130),
    ],boundary:[0,1,4,5],axes:[250,40,90]},
    {name:'collinear-all-members',pairs:[
      fixturePair('envelope-collinear/A',40,200),fixturePair('envelope-collinear/B',60,160,2),
      fixturePair('envelope-collinear/C',90,100,3),fixturePair('envelope-collinear/B-copy',60,160,1,beta),
    ],boundary:[0,1,2,3],axes:[250,40,90]},
    {name:'same-score-fastest-only',pairs:[
      fixturePair('envelope-same-score/slower',55,10),fixturePair('envelope-same-score/fastest',55,50),fixturePair('envelope-same-score/fastest-copy',55,50,2,beta),
    ],boundary:[1,2],axes:[100,50,60]},
    {name:'same-tps-strongest-only',pairs:[
      fixturePair('envelope-same-tps/weaker',40,80),fixturePair('envelope-same-tps/middle',60,80),
      fixturePair('envelope-same-tps/strongest',90,80),fixturePair('envelope-same-tps/strongest-copy',90,80,2,beta),
    ],boundary:[2,3],axes:[100,40,90]},
    {name:'singleton-high-score',pairs:[fixturePair('envelope-single/only',100,125,2)],boundary:[0],axes:[150,90,100]},
    {name:'missing-error-excluded',pairs:[...convex,fixturePair('envelope-excluded/missing-dominant',100,null,0),fixturePair('envelope-excluded/error-dominant',100,999,1,beta,'error')],boundary:[0,2],axes:[250,40,90]},
    {name:'unrounded-coordinates',pairs:[fixturePair('envelope-precision/A',43,200.123456),fixturePair('envelope-precision/B',67,110.987654,2),fixturePair('envelope-precision/C',87,100.123456,3,beta)],boundary:[0,2],axes:[250,40,90]},
    {name:'zero-score-valid',pairs:[fixturePair('envelope-zero/A',0,80),fixturePair('envelope-zero/B',25,25,2,beta)],boundary:[0,1],axes:[100,0,30]},
    {name:'empty',pairs:[],boundary:[],axes:[100,0,100]},
  ];
  report.envelopeScenarios=[];
  for(const scenario of scenarios) {
    envelopeSnapshot={...structuredClone(snapshot),models:scenario.pairs.map(pair=>({
      ...structuredClone(snapshot.models[0]),rating:{...snapshot.models[0].rating,provider_id:pair.provider.id,upstream_model:pair.model,score:pair.score},
      mixed:{selected_request_count:pair.samples,valid_tps_count:pair.samples,average_tps:pair.tps,first_sample_at:pair.samples?snapshot.as_of-1000:null,last_sample_at:pair.samples?snapshot.as_of:null},
      unclassified_count:0,untrusted_count:0,status:pair.status,...(pair.status==='error'?{error:'Injected geometry-only statistics failure'}:{}),
    }))};
    faultMode=`envelope:${scenario.name}`;
    await reload();
    const rows=expectedRows(envelopeSnapshot,scenario.pairs),plotted=plottedRows(rows).length;
    const missing=rows.filter(row=>row.status==='ready' && row.stats.average_tps===null).length,errors=rows.filter(row=>row.status==='error').length;
    state=await ready({plotted,missing,errors});
    if(plotted) assertPoints(state,rows);
    assertAxes(state,...scenario.axes);
    const envelope=assertEnvelope(state,rows);
    assert.deepEqual(envelope.keys,scenario.boundary.map(index=>JSON.stringify([scenario.pairs[index].provider.id,scenario.pairs[index].model])).sort(),'Independent supporting-line oracle also agrees with the explicit deterministic scenario membership');
    await assertEnvelopeTooltips(state,rows);
    const evidence={name:scenario.name,snapshot:structuredClone(envelopeSnapshot),expectedBoundaryKeys:envelope.keys,svg:state.envelopes,axes:state.axis,points:state.points};
    report.envelopeScenarios.push(evidence);
    if(['convex-not-all-pareto','convex-bend','same-x-same-y-duplicates','collinear-all-members','empty'].includes(scenario.name)) await screenshot(`performance-envelope-${scenario.name}`);
    if(scenario.name==='convex-not-all-pareto') {
      const b=rows.find(row=>row.pair===convex[1]);
      assert.ok(!envelope.keys.includes(b.key),'B(60,120) is nondominated but below the A(40,200)–C(90,100) convex segment');
      const values=rows=>plottedRows(rows).map(row=>({id:row.id,score:row.score,tps:row.stats.average_tps})).sort((a,b)=>a.id.localeCompare(b.id));
      const originalValues=values(rows),beforeFilterCalls=report.apiCalls.filter(call=>call.path==='/api/v1/model-performance').length;
      await fill('input[aria-label="Search providers or models"]','keep-');
      const kept=rows.filter(row=>row.pair.model.includes('keep-'));
      state=await ready({plotted:2,missing:0});assertPoints(state,kept);assertAxes(state,250,40,60);
      assert.deepEqual(assertEnvelope(state,kept).keys,kept.map(row=>row.key).sort(),'Removing C promotes previously interior B onto the recomputed visible envelope');
      await assertEnvelopeTooltips(state,kept);await screenshot('performance-envelope-search-recomputed');
      await fill('input[aria-label="Search providers or models"]','keep-B');
      state=await ready({plotted:1,missing:0});assertAxes(state,150,60,70);assertEnvelope(state,[b]);await assertEnvelopeTooltips(state,[b]);
      await fill('input[aria-label="Search providers or models"]','no-envelope-model-matches');
      state=await ready({plotted:0,missing:0});assertAxes(state,100,0,100);assertEnvelope(state,[]);
      await fill('input[aria-label="Search providers or models"]','');
      state=await ready({plotted:3,missing:0});assertAxes(state,250,40,90);assertEnvelope(state,rows);
      await select('Filter by provider',alpha.name);
      state=await ready({plotted:2,missing:0});assertAxes(state,250,40,60);assertEnvelope(state,kept);await assertEnvelopeTooltips(state,kept);
      await select('Filter by provider','All providers');
      state=await ready({plotted:3,missing:0});assertAxes(state,250,40,90);assertEnvelope(state,rows);
      assert.deepEqual(state.points.flatMap(p=>p.ids.map(id=>({id,score:p.score,tps:p.tps}))).sort((a,b)=>a.id.localeCompare(b.id)),originalValues,'Filtering rescales axes but never changes scores, raw TPS, or stable point IDs');
      assert.equal(report.apiCalls.filter(call=>call.path==='/api/v1/model-performance').length,beforeFilterCalls,'Text/provider envelope recomputation is local, with no extra performance API requests');
      const beforeZoom=structuredClone(state.envelopes);
      await evaluate(`(()=>{const el=document.querySelector('select');el.value='2';el.dispatchEvent(new Event('change',{bubbles:true}))})()`);
      assert.deepEqual((await chartState()).envelopes,beforeZoom,'Zoom cannot move or change the underlying SVG envelope');
      await evaluate(`(()=>{const el=document.querySelector('select');el.value='1';el.dispatchEvent(new Event('change',{bubbles:true}))})()`);
      await clickExpression(`document.querySelector('button[title="切换到中文"]')`);
      await waitFor(()=>evaluate(`document.querySelector('h1')?.textContent==='性能'`),'Chinese envelope locale');
      state=await ready({plotted:3,missing:0});assertEnvelope(state,rows,true);await assertEnvelopeTooltips(state,rows,true);
      const a=rows.find(row=>row.pair===convex[0]);
      await evaluate(`(${circleExpression(a.id)}).parentElement.focus()`);await key('Enter');
      await waitFor(()=>evaluate(`document.querySelector('[data-testid="performance-envelope-member"]')?.textContent==='位于当前可见模型的包络线'`),'Chinese low-sample envelope membership badge');
      await screenshot('performance-envelope-zh-tooltip',{preserveFocus:true});await key('Escape');
      await clickExpression(`document.querySelector('button[title="Switch to English"]')`);
      await waitFor(()=>evaluate(`document.querySelector('h1')?.textContent==='Performance'`),'English envelope locale restored');
      envelopeSnapshot.models.reverse();await reload();state=await ready({plotted:3,missing:0});
      assertPoints(state,rows);assertEnvelope(state,rows);
      assert.deepEqual(state.envelopes,beforeZoom,'Snapshot input ordering does not alter hull geometry or sorted exact-key membership');
    }
    check(`envelope ${scenario.name}`,`${envelope.keys.length} model members; ${state.envelopes.length} open line`);
  }
  faultMode=null;envelopeSnapshot=null;await reload();state=await ready();assertPoints(state,expected);assertAxes(state,250,0,100);assertEnvelope(state,expected);
  const finalSnapshot=await api('/model-performance');
  assert.deepEqual(finalSnapshot.models,snapshot.models,'Geometry snapshots/filters never mutate real ratings, raw logs, API membership or TPS semantics');
  check('independent convex-support oracle, exact SVG scaling, EN/ZH per-key badges, low samples, filtered recomputation and restored real backend snapshot');

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
