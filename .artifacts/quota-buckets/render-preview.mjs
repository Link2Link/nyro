import { spawn } from 'node:child_process';
import { writeFile } from 'node:fs/promises';
import { resolve, dirname } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const directory = dirname(fileURLToPath(import.meta.url));
const chrome = process.env.CHROME_BIN || 'chromium';
const browser = spawn(chrome, [
  '--headless=new', '--no-sandbox', '--disable-gpu', '--disable-dev-shm-usage',
  '--remote-debugging-pipe', '--no-first-run', '--no-default-browser-check',
  '--disable-background-networking', '--disable-extensions', 'about:blank',
], { stdio: ['ignore', 'ignore', 'pipe', 'pipe', 'pipe'] });
const pending = new Map();
let nextId = 1;
let buffer = '';
let errors = '';
browser.stderr.on('data', data => { errors = (errors + data).slice(-5000); });
browser.stdio[4].on('data', data => {
  buffer += data;
  let end;
  while ((end = buffer.indexOf('\0')) >= 0) {
    const line = buffer.slice(0, end);
    buffer = buffer.slice(end + 1);
    if (!line.trim()) continue;
    const message = JSON.parse(line);
    const waiter = pending.get(message.id);
    if (!waiter) continue;
    pending.delete(message.id);
    if (message.error) waiter.reject(new Error(JSON.stringify(message.error)));
    else waiter.resolve(message.result);
  }
});
browser.on('exit', code => {
  for (const waiter of pending.values()) waiter.reject(new Error(`Chromium exited ${code}: ${errors}`));
  pending.clear();
});
function send(method, params = {}, sessionId) {
  const id = nextId++;
  return new Promise((resolve, reject) => {
    const timeout = setTimeout(() => { pending.delete(id); reject(new Error(`CDP timeout: ${method}: ${errors}`)); }, 20000);
    pending.set(id, {
      resolve: result => { clearTimeout(timeout); resolve(result); },
      reject: error => { clearTimeout(timeout); reject(error); },
    });
    browser.stdio[3].write(JSON.stringify({ id, method, params, ...(sessionId ? { sessionId } : {}) }) + '\0');
  });
}
let session;
async function evaluate(expression) {
  const result = await send('Runtime.evaluate', { expression, awaitPromise: true, returnByValue: true }, session);
  if (result.exceptionDetails) throw new Error(JSON.stringify(result.exceptionDetails));
  return result.result.value;
}
async function renderTick() {
  await evaluate('new Promise(r => requestAnimationFrame(() => requestAnimationFrame(r)))');
}
async function screenshot(filename, fullPage = false) {
  await renderTick();
  const metrics = await send('Page.getLayoutMetrics', {}, session);
  const rect = metrics.cssContentSize;
  const { data } = await send('Page.captureScreenshot', {
    format: 'png', captureBeyondViewport: true,
    ...(fullPage ? { clip: { x: 0, y: 0, width: rect.width, height: rect.height, scale: 1 } } : {}),
  }, session);
  const path = resolve(directory, filename);
  await writeFile(path, Buffer.from(data, 'base64'));
  console.log('Screenshot:', path);
}
function assert(condition, message) { if (!condition) throw new Error(message); }
const receipt = { artifact: 'quota-buckets-preview.html', checks: [], screenshots: [], containsDemoData: true };
try {
  console.log('Browser:', (await send('Browser.getVersion')).product);
  const { targetId } = await send('Target.createTarget', { url: 'about:blank' });
  ({ sessionId: session } = await send('Target.attachToTarget', { targetId, flatten: true }));
  await send('Page.enable', {}, session);
  await send('Runtime.enable', {}, session);
  await send('Emulation.setTimezoneOverride', { timezoneId: 'Asia/Shanghai' }, session);
  await send('Emulation.setDeviceMetricsOverride', { width: 1600, height: 1000, deviceScaleFactor: 1, mobile: false }, session);
  await send('Page.navigate', { url: pathToFileURL(resolve(directory, 'quota-buckets-preview.html')).href }, session);
  await evaluate(`new Promise((resolve,reject)=>{let count=0; const check=()=>{if(window.__QUOTA_PREVIEW_READY__)document.fonts.ready.then(resolve);else if(++count>100)reject('preview not ready');else setTimeout(check,20)};check()})`);
  for (const [width,height] of [[1440,900],[1600,1000],[1920,1080],[2048,1320],[390,844]]) {
    await send('Emulation.setDeviceMetricsOverride', { width,height,deviceScaleFactor:1,mobile:false }, session);
    await renderTick();
    const layout = await evaluate(`({width:innerWidth,height:innerHeight,scrollWidth:document.documentElement.scrollWidth,scrollHeight:document.documentElement.scrollHeight,overflowing:[...document.querySelectorAll('.bucket, .pricing, .provider-header, .route-result')].filter(el=>el.scrollWidth>el.clientWidth+1).map(el=>({class:el.className,width:el.clientWidth,scroll:el.scrollWidth}))})`);
    console.log('Layout:', JSON.stringify(layout));
    if(width===1440) console.log('Heights:',JSON.stringify(await evaluate(`Object.fromEntries(['.heading','.scenario','.provider','.provider-header','.section-toolbar','.bucket','.bucket-top','.quota-content','.bucket-token','.pricing','.bucket-footer','.status-note','.provider-total','.route','.footnote'].map(s=>{const el=document.querySelector(s),r=el.getBoundingClientRect();return [s,{y:r.y,h:r.height,lineHeight:getComputedStyle(el).lineHeight}]}))`)));
    assert(layout.scrollWidth <= width, `Horizontal overflow at ${width}`);
    if (width >= 1440) assert(layout.scrollHeight <= height, `Desktop vertical overflow at ${width}: ${layout.scrollHeight}`);
    assert(layout.overflowing.length === 0, `Clipped card content at ${width}: ${JSON.stringify(layout.overflowing)}`);
    receipt.checks.push({ type:'layout', ...layout });
    if (width === 390) await screenshot('quota-buckets-mobile.png', true);
    if (width === 2048) await screenshot('quota-buckets-large-desktop.png');
  }
  await send('Emulation.setDeviceMetricsOverride', { width:1600,height:1000,deviceScaleFactor:1,mobile:false }, session);
  await screenshot('quota-buckets-desktop.png', true);
  receipt.screenshots.push('quota-buckets-desktop.png');
  assert((await evaluate("document.querySelector('#provider-status').textContent")).includes('全部可用'), 'Normal scenario status incorrect');
  await evaluate("document.querySelector('[data-scenario=exhausted]').click()");
  assert((await evaluate("document.querySelector('#provider-status').textContent")).includes('部分可用'), 'Exhausted scenario should be partial');
  assert((await evaluate("document.querySelector('#route-outcome').textContent")).includes('跳过'), 'Flash must be skipped');
  assert((await evaluate("document.querySelector('.bucket.argon .bucket-heading .badge').textContent")).includes('可调度'), 'Argon must remain eligible');
  await screenshot('quota-buckets-flash-exhausted.png', true);
  receipt.screenshots.push('quota-buckets-flash-exhausted.png');
  await evaluate("document.querySelector('#route-model').value='argon';document.querySelector('#route-model').dispatchEvent(new Event('change'))");
  assert((await evaluate("document.querySelector('#route-outcome').textContent")).includes('允许参与调度'), 'Argon should pass independently');
  receipt.checks.push({ type:'behavior', name:'Flash exhaustion does not block Argon', passed:true });
  await evaluate("document.querySelector('[data-detail=flash]').click()");
  assert(await evaluate("document.querySelector('#details').open"), 'Detail dialog should open');
  assert((await evaluate("document.querySelector('#detail-metrics').textContent")).includes('$184.20'), 'Flash detail total incorrect');
  assert((await evaluate("document.querySelectorAll('#detail-rows tr').length"))===2, 'Flash details should filter to two rows');
  await evaluate("document.querySelector('[data-filter=all]').click()");
  assert((await evaluate("document.querySelector('#detail-metrics').textContent")).includes('$439.00'), 'Provider reconciliation total incorrect');
  await screenshot('quota-buckets-details.png');
  receipt.screenshots.push('quota-buckets-details.png');
  receipt.checks.push({ type:'behavior', name:'Bucket filtering and provider cost reconciliation', passed:true });
  await evaluate("document.querySelector('#details').close();document.querySelector('[data-scenario=stale]').click()");
  assert((await evaluate("document.querySelector('.bucket.flash .badge').textContent")).includes('耗尽'), 'Failed refresh must preserve Flash blocking evidence');
  assert(await evaluate("[...document.querySelectorAll('.capacity')].every(e=>e.textContent.trim()==='—')"), 'Stale data must not show capacity projection');
  receipt.checks.push({ type:'behavior', name:'Stale refresh preserves block and suppresses estimates', passed:true });
  await evaluate("document.querySelector('[data-scenario=normal]').click();document.querySelector('#theme-toggle').click()");
  assert((await evaluate("document.documentElement.dataset.theme"))==='light', 'Theme toggle failed');
  await screenshot('quota-buckets-light.png', true);
  receipt.screenshots.push('quota-buckets-light.png');
  await evaluate("document.querySelector('#mapping-button').click()");
  assert(await evaluate("document.querySelector('#mapping').open"), 'Mapping dialog should open');
  await evaluate("document.querySelector('#mapping').close();document.querySelector('#policy-button').click()");
  assert(await evaluate("document.querySelector('#policy').open"), 'Policy dialog should open');
  receipt.checks.push({ type:'behavior', name:'Theme, mapping and policy dialogs', passed:true });
  await writeFile(resolve(directory,'preview-checks.json'),JSON.stringify(receipt,null,2)+'\n');
  console.log('All layout and interaction checks passed.');
} finally {
  try { await send('Browser.close'); } catch {}
  if (browser.exitCode === null) browser.kill('SIGTERM');
}
