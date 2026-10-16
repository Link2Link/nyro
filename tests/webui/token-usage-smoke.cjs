#!/usr/bin/env node
/**
 * Render the built UI against deterministic intercepted fixtures; no running server,
 * credentials, database, network requests, or additional service needed.
 * npm run build in webui first. Requires externally installed Playwright:
 * PLAYWRIGHT_MODULE=/path/to/playwright node tests/webui/token-usage-smoke.cjs
 * Optional CHROME_BIN, NYRO_SMOKE_WEBUI, NYRO_SMOKE_SCREENSHOTS.
 */
const { chromium } = require(process.env.PLAYWRIGHT_MODULE || 'playwright');
const fs = require('node:fs/promises');
const path = require('node:path');
const os = require('node:os');
const assert = require('node:assert/strict');
const build = process.env.NYRO_SMOKE_WEBUI || path.resolve(__dirname, '../../webui/dist');
const month = new Date().toISOString().slice(0, 7);
const year = Number(month.slice(0, 4));
const FX = 7.0;
const price = (overrides = {}) => ({ currency: 'USD', input_cost: 2, cache_read_cost: 1, output_cost: 3, total_cost: 6, cache_rate_fallback: false, pricing_provider: 'openai', pricing_model: 'gpt-example', input_rate: 2, output_rate: 6, cache_read_rate: 0.5, ...overrides });
const row = (overrides = {}) => ({ date: `${month}-12`, provider_id: 'a', provider_name: 'OpenAI', model: 'gpt-example', request_count: 10, input_tokens: 3000000, output_tokens: 500000, cache_read_tokens: 2000000, cache_read_reported_count: 10, missing_usage_count: 0, cost: price(), ...overrides });
const fixture = { year, time_zone: 'UTC', generated_at: Date.now(), rows: [
  ...[1, 3, 4, 8, 9, 10, 11, 12, 13, 14, 15, 16].map((day, i) => row({ date: `${month}-${String(day).padStart(2, '0')}`, cost: price({ input_cost: 2 * (i + 1), cache_read_cost: i + 1, output_cost: 3 * (i + 1), total_cost: 6 * (i + 1) }) })),
  row({ provider_id: 'b', provider_name: 'BigModel', model: 'glm-example', cost: price({ currency: 'CNY', input_cost: 8, cache_read_cost: 4.6, output_cost: 14, total_cost: 26.6, input_rate: 8, cache_read_rate: 2.3, output_rate: 28 }) }),
  row({ date: `${month}-08`, provider_id: 'unknown', provider_name: 'Custom', model: 'unpriced-model', cost: null, missing_usage_count: 1 }),
  row({ date: `${month}-17`, provider_id: 'free', provider_name: 'Free', model: 'free-model', cost: price({ input_cost: 0, cache_read_cost: 0, output_cost: 0, total_cost: 0, input_rate: 0, cache_read_rate: 0, output_rate: 0 }) }),
  row({ date: `${month}-18`, provider_id: 'fallback', provider_name: 'Proxy', model: 'cache-fallback', cost: price({ cache_rate_fallback: true }) }),
  row({ date: `${month}-19`, provider_id: 'tiny', provider_name: 'Tiny', model: 'tiny-model', cost: price({ input_cost: 0.001, cache_read_cost: 0, output_cost: 0, total_cost: 0.001 }) }),
  row({ date: `${month}-20`, provider_id: 'anthropic', provider_name: 'Anthropic', model: 'claude-sonnet-5-5', cost: null }),
  row({ date: `${month}-20`, provider_id: 'anthropic', provider_name: 'Anthropic', model: 'claude-sonnet-5-5-max', cost: null }),
] };
/** Same bounded-prefix enumeration as the backend: exact identity, then longest first. */
const prefixBases = model => {
  const bases = [model];
  for (let i = model.length - 1; i > 0; i--) {
    if (['-', '_', ':', '/'].includes(model[i])) bases.push(model.slice(0, i));
  }
  return bases;
};
(async () => {
  const screenshots = process.env.NYRO_SMOKE_SCREENSHOTS || await fs.mkdtemp(path.join(os.tmpdir(), 'nyro-cost-smoke-'));
  await fs.mkdir(screenshots, { recursive: true });
  const browser = await chromium.launch({ headless: true, ...(process.env.CHROME_BIN ? { executablePath: process.env.CHROME_BIN } : {}), args: ['--no-sandbox'] });
  try {
    for (const scenario of [{ theme: 'light', locale: 'zh-CN', width: 1440 }, { theme: 'dark', locale: 'zh-CN', width: 1440 }, { theme: 'light', locale: 'en-US', width: 390 }, { theme: 'dark', locale: 'en-US', width: 390 }]) {
      const page = await browser.newPage({ viewport: { width: scenario.width, height: 1100 } });
      const errors = [];
      page.on('pageerror', error => errors.push(error.message));
      await page.addInitScript(s => { localStorage.setItem('nyro-locale', s.locale); localStorage.setItem('nyro-theme', s.theme); }, scenario);
      let apiMode = 'data';
      let priceGetFails = false;
      let priceSaveFails = false;
      const overrides = new Map();
      const writes = [];
      const pairKey = (provider, model) => JSON.stringify([provider, model]);
      const exactOverride = (provider, model) => overrides.get(pairKey(provider, model)) ?? null;
      /** Longest same-provider manual prefix rule strictly shorter than the model. */
      const inheritedOverride = (provider, model) => {
        for (const base of prefixBases(model)) {
          if (base === model) continue;
          const found = overrides.get(pairKey(provider, base));
          if (found) return { rates: found, base };
        }
        return null;
      };
      const priceConfig = (provider_id, model) => {
        const base = fixture.rows.find(item => item.provider_id === provider_id && item.model === model)?.cost;
        const inherited = inheritedOverride(provider_id, model);
        return { provider_id, model, override_price: exactOverride(provider_id, model),
          inherited_price: inherited ? { ...inherited.rates, pricing_provider: 'manual', pricing_model: inherited.base } : null,
          default_price: base ? { currency: base.currency, input_rate: base.input_rate, output_rate: base.output_rate, cache_read_rate: base.cache_rate_fallback ? null : base.cache_read_rate, pricing_provider: base.pricing_provider, pricing_model: base.pricing_model } : null };
      };
      const manualCost = (item, rates, matchedModel) => {
        const cached = Math.min(item.cache_read_tokens, item.input_tokens);
        const input_cost = (item.input_tokens - cached) / 1e6 * rates.input_rate;
        const cache_read_cost = cached / 1e6 * (rates.cache_read_rate ?? rates.input_rate);
        const output_cost = item.output_tokens / 1e6 * rates.output_rate;
        return { ...rates, cache_read_rate: rates.cache_read_rate ?? rates.input_rate, input_cost, cache_read_cost, output_cost, total_cost: input_cost + cache_read_cost + output_cost, is_manual: true, cache_rate_fallback: rates.cache_read_rate === null && cached > 0, pricing_provider: 'manual', pricing_model: matchedModel };
      };
      const effectiveFixture = () => ({ ...fixture, rows: fixture.rows.map(item => {
        const exact = exactOverride(item.provider_id, item.model);
        if (exact) return { ...item, cost: manualCost(item, exact, item.model) };
        const inherited = inheritedOverride(item.provider_id, item.model);
        if (inherited) return { ...item, cost: manualCost(item, inherited.rates, inherited.base) };
        return item;
      }) });
      await page.route('**/*', async route => {
        const url = new URL(route.request().url());
        // The app imports Google Fonts; keep this test entirely offline.
        if (url.host !== 'nyro.test') return route.fulfill({ status: 200, body: '', contentType: 'text/css' });
        if (url.pathname.startsWith('/api/')) {
          let data = [];
          if (url.pathname.endsWith('/stats/token-price')) {
            if (route.request().method() === 'PUT') {
              const input = route.request().postDataJSON();
              writes.push(input);
              if (priceSaveFails) return route.fulfill({ status: 503, json: { error: 'Fixture save unavailable' } });
              const key = pairKey(input.provider_id, input.model);
              if (input.price === null) overrides.delete(key); else overrides.set(key, input.price);
              data = priceConfig(input.provider_id, input.model);
            } else {
              if (priceGetFails) return route.fulfill({ status: 503, json: { error: 'Fixture price unavailable' } });
              data = priceConfig(url.searchParams.get('provider_id'), url.searchParams.get('model'));
            }
          } else if (url.pathname.endsWith('/stats/token-usage')) {
            if (apiMode === 'error') return route.fulfill({ status: 503, json: { error: 'Fixture unavailable' } });
            data = apiMode === 'empty' ? { ...fixture, rows: [] } : apiMode === 'legacy' ? { ...fixture, rows: fixture.rows.map(({ cost, ...item }) => item) } : effectiveFixture();
          } else if (url.pathname.endsWith('/stats/overview')) data = { total_requests: 170, total_input_tokens: 51000000, total_output_tokens: 8500000, total_cache_read_tokens: 34000000, avg_duration_ms: 1287, error_count: 1 };
          else if (url.pathname.endsWith('/stats/timeseries')) data = { has_data: false, points: [], bucket_minutes: 60, start_at: 0, end_at: 0 };
          else if (url.pathname.endsWith('/status')) data = {};
          return route.fulfill({ json: { data } });
        }
        const file = url.pathname.startsWith('/assets/') ? path.join(build, url.pathname) : path.join(build, 'index.html');
        const ext = path.extname(file);
        return route.fulfill({ body: await fs.readFile(file), contentType: ext === '.js' ? 'application/javascript' : ext === '.css' ? 'text/css' : ext === '.png' ? 'image/png' : 'text/html' });
      });
      await page.goto('http://nyro.test/stats');
      const panel = page.locator('.token-usage');
      const zh = scenario.locale === 'zh-CN';
      await panel.locator('.token-day').first().waitFor();
      assert.equal(await panel.getAttribute('data-metric'), 'tokens');
      await panel.getByRole('button', { name: zh ? '等值费用' : 'API value', exact: true }).click();
      assert.equal(await panel.getAttribute('data-metric'), 'cost');
      // USD 474.001 native + CNY 26.6 converted at 1/7 = 477.80 after the fixed-rate merge.
      assert.equal(await panel.locator('.token-kpi').first().locator('p').first().textContent(), '$477.80');
      assert.match(await panel.innerText(), /未匹配可靠价格|no reliable price/);
      assert.match(await panel.innerText(), new RegExp(`1 USD = 7\\.0 CNY|1 USD = 7\\.0 CNY`), 'Fixed FX rate is disclosed');
      assert.equal(await panel.locator('.token-family').count(), 5, 'Only top five families appear by default');
      assert.equal(await panel.locator('.token-family[open]').count(), 0, 'Details start collapsed');
      assert.equal(await panel.locator('.token-family-list table').count(), 0, 'Compact costs do not duplicate wide tables');
      const moreModels = panel.getByRole('button', { name: zh ? /^查看全部 / : /^Show all / });
      await moreModels.click();
      assert.equal(await panel.locator('.token-family').count(), 7, 'Base and medium are one family');
      assert.equal(await panel.locator('.token-family[data-family="claude-sonnet-5-5"]').count(), 1);
      await panel.getByRole('button', { name: zh ? '收起列表' : 'Show less', exact: true }).click();
      assert.equal(await panel.locator('.token-family').count(), 5);
      const sizes = await panel.evaluate(e => ({ width: e.clientWidth, scroll: e.scrollWidth }));
      assert.ok(sizes.scroll <= sizes.width + 1, JSON.stringify({ scenario, sizes }));
      await page.setViewportSize({ width: scenario.width, height: 4000 });
      await panel.screenshot({ path: path.join(screenshots, `cost-${scenario.theme}-${scenario.width}.png`) });
      await page.setViewportSize({ width: scenario.width, height: 1100 });
      if (scenario.width === 390) assert.ok(await panel.locator('.token-amount').evaluateAll(elements => elements.every(e => e.scrollWidth <= e.clientWidth)), 'Mobile values must fit');
      await panel.locator(`.token-day[aria-label^="${month}-12:"]`).click();
      // Day 12 holds the gpt row (48 USD) plus the glm CNY row: 48 + 26.6/7 = 51.80.
      assert.equal(await panel.locator('.token-detail > p').nth(1).textContent(), '$51.80');
      // Switching the display currency converts the SAME merged total, not a currency subset.
      await panel.getByRole('combobox', { name: zh ? '显示币种' : 'Display currency' }).click();
      await page.getByRole('option', { name: '¥ CNY', exact: true }).click();
      assert.equal(await panel.locator('.token-kpi').first().locator('p').first().textContent(), '¥3,344.61');
      assert.equal(await panel.locator('.token-detail > p').nth(1).textContent(), '¥362.60');
      await panel.getByRole('button', { name: zh ? '月度趋势' : 'Monthly trends', exact: true }).click();
      assert.equal(await panel.locator('.token-month').count(), 12);
      await panel.locator('.token-month').nth(Number(month.slice(5)) - 1).click();
      assert.ok(await panel.locator('.token-day').count() >= 28);
      const selectModel = async label => {
        await panel.getByRole('combobox', { name: zh ? '筛选模型与提供商' : 'Filter model and provider' }).click();
        await page.getByRole('option', { name: label, exact: true }).click();
      };
      // Return to USD display for the remaining checks.
      await panel.getByRole('combobox', { name: zh ? '显示币种' : 'Display currency' }).click();
      await page.getByRole('option', { name: '$ USD', exact: true }).click();
      await selectModel('unpriced-model · Custom');
      assert.equal(await panel.locator('.token-kpi').first().locator('p').first().textContent(), '—');
      await selectModel('free-model · Free');
      assert.equal(await panel.locator('.token-kpi').first().locator('p').first().textContent(), '$0.00');
      await selectModel('tiny-model · Tiny');
      assert.equal(await panel.locator('.token-kpi').first().locator('p').first().textContent(), '<$0.01');
      await selectModel('cache-fallback · Proxy');
      await panel.locator('.token-family > summary').click();
      assert.match(await panel.locator('.token-family-detail').innerText(), /缓存单价缺失|Cache rate unavailable/);
      await selectModel(zh ? '全部模型' : 'All models');
      await page.reload();
      await panel.locator('.token-day').first().waitFor();
      assert.equal(await panel.getAttribute('data-metric'), 'cost', 'Metric preference persists');
      // Manual pricing uses a separate persisted configuration from the usage snapshot.
      const editPrice = async (model, provider) => {
        const more = panel.getByRole('button', { name: zh ? /^查看全部 / : /^Show all / });
        if (await more.count()) await more.click();
        const family = panel.locator('.token-family').filter({ has: page.locator(`.token-family-variant[data-model="${model}"]`).filter({ hasText: provider }) });
        if (await family.getAttribute('open') === null) await family.locator(':scope > summary').click();
        await panel.getByRole('button', { name: `${zh ? '设置价格' : 'Set prices'}: ${model} · ${provider}`, exact: true }).click();
        await page.getByRole('dialog').getByRole('heading', { name: zh ? '设置模型价格' : 'Set model prices' }).waitFor();
      };
      const dialog = page.getByRole('dialog');
      const inputRate = () => dialog.getByLabel(zh ? '普通输入单价 *' : 'Uncached input rate *', { exact: true });
      const outputRate = () => dialog.getByLabel(zh ? '输出单价 *' : 'Output rate *', { exact: true });
      const cacheRate = () => dialog.getByLabel(zh ? '缓存读取单价' : 'Cache read rate', { exact: true });
      const save = () => dialog.getByRole('button', { name: zh ? '保存价格' : 'Save prices', exact: true }).click();
      // Prefix inheritance: rates saved on claude-sonnet-5-5 also price claude-sonnet-5-5-max.
      await editPrice('claude-sonnet-5-5', 'Anthropic');
      await inputRate().waitFor();
      assert.equal(await inputRate().inputValue(), '', 'Unknown price must not prefill zero');
      await inputRate().fill('1');
      await cacheRate().fill('0.1');
      await outputRate().fill('5');
      await save();
      await dialog.waitFor({ state: 'detached' });
      // Both claude rows now cost 3.7 USD each: 474.001 + 3.8 + 7.4 = 485.20.
      await page.waitForFunction(() => document.querySelector('.token-kpi p')?.textContent === '$485.20');
      const mediumRow = panel.locator('.token-family-variant[data-model="claude-sonnet-5-5-max"]');
      assert.equal(await panel.locator('.token-family[data-family="claude-sonnet-5-5"]').count(), 1);
      assert.equal(await panel.locator('.token-family[data-family="claude-sonnet-5-5-max"]').count(), 0);
      assert.match(await panel.locator('.token-family[data-family="claude-sonnet-5-5"] > summary').innerText(), /\$7\.40/);
      assert.match(await mediumRow.innerText(), /自定义价格|Custom prices/, 'Inherited prefix rates mark the variant as manual');
      const familyList = panel.locator('.token-family-list');
      const collapsedBefore = await familyList.evaluate(e => e.clientHeight);
      await panel.locator('.token-family[data-family="claude-sonnet-5-5"] > summary').click();
      assert.ok(await familyList.evaluate(e => e.clientHeight) < collapsedBefore, 'Collapsing the family removes lengthy variant detail');
      await familyList.screenshot({ path: path.join(screenshots, `model-costs-${scenario.theme}-${scenario.width}.png`) });
      await panel.locator('.token-family[data-family="claude-sonnet-5-5"] > summary').click();
      const expandedSizes = await familyList.evaluate(e => ({ width: e.clientWidth, scroll: e.scrollWidth }));
      assert.ok(expandedSizes.scroll <= expandedSizes.width + 1, 'Expanded details fit narrow layouts');
      await editPrice('claude-sonnet-5-5-max', 'Anthropic');
      await inputRate().waitFor();
      assert.equal(await inputRate().inputValue(), '1', 'Inherited rates prefill the variant editor');
      assert.match(await dialog.innerText(), /claude-sonnet-5-5/, 'Dialog names the inherited source model');
      await dialog.getByRole('button', { name: zh ? '取消' : 'Cancel', exact: true }).click();
      await dialog.waitFor({ state: 'detached' });
      // Removing the parent rule unprices both models again.
      await editPrice('claude-sonnet-5-5', 'Anthropic');
      await inputRate().waitFor();
      await dialog.getByRole('button', { name: zh ? '恢复默认' : 'Restore defaults', exact: true }).click();
      await dialog.getByRole('button', { name: zh ? '确认恢复默认' : 'Confirm restore defaults', exact: true }).click();
      await dialog.waitFor({ state: 'detached' });
      await page.waitForFunction(() => document.querySelector('.token-kpi p')?.textContent === '$477.80');
      await editPrice('gpt-example', 'OpenAI');
      await inputRate().waitFor();
      assert.equal(await inputRate().inputValue(), '2', 'Automatic default prefills');
      assert.equal(await cacheRate().inputValue(), '0.5');
      assert.equal(await dialog.getByRole('button', { name: zh ? '恢复默认' : 'Restore defaults', exact: true }).isDisabled(), true);
      await dialog.screenshot({ path: path.join(screenshots, `price-editor-${scenario.theme}-${scenario.width}.png`) });
      const dialogSizes = await dialog.evaluate(e => ({ width: e.clientWidth, scroll: e.scrollWidth }));
      assert.ok(dialogSizes.scroll <= dialogSizes.width + 1, 'Price editor must not overflow horizontally');
      await inputRate().fill('-1');
      await save();
      assert.equal(await inputRate().getAttribute('aria-invalid'), 'true');
      assert.equal(writes.length, 2, 'Invalid price never submitted');
      await inputRate().fill('4');
      await dialog.getByRole('button', { name: zh ? '取消' : 'Cancel', exact: true }).click();
      await dialog.waitFor({ state: 'detached' });
      assert.equal(writes.length, 2, 'Cancel does not persist a draft');
      await editPrice('gpt-example', 'OpenAI');
      await inputRate().waitFor();
      assert.equal(await inputRate().inputValue(), '2', 'Cancelled draft resets to defaults');
      await inputRate().fill('4');
      await outputRate().fill('8');
      await cacheRate().fill('');
      priceSaveFails = true;
      await save();
      await dialog.getByRole('alert').filter({ hasText: /保存失败|Could not save/ }).waitFor();
      assert.equal(await inputRate().inputValue(), '4', 'Failed save preserves edits');
      priceSaveFails = false;
      await save();
      await dialog.waitFor({ state: 'detached' });
      assert.deepEqual(writes.at(-1), { provider_id: 'a', model: 'gpt-example', price: { currency: 'USD', input_rate: 4, output_rate: 8, cache_read_rate: null } });
      // 12 gpt rows revalued to 16 USD each: 192 + 6 + 0.001 + 3.8 = 201.80.
      await page.waitForFunction(() => document.querySelector('.token-kpi p')?.textContent === '$201.80');
      assert.match(await panel.locator('.token-models').innerText(), /自定义价格|Custom prices/);
      await page.reload();
      await panel.locator('.token-day').first().waitFor();
      await editPrice('gpt-example', 'OpenAI');
      await inputRate().waitFor();
      assert.equal(await inputRate().inputValue(), '4', 'Saved price survives reload');
      assert.equal(await cacheRate().inputValue(), '');
      await dialog.getByRole('button', { name: zh ? '恢复默认' : 'Restore defaults', exact: true }).click();
      await dialog.getByRole('button', { name: zh ? '确认恢复默认' : 'Confirm restore defaults', exact: true }).click();
      await dialog.waitFor({ state: 'detached' });
      assert.equal(writes.at(-1).price, null);
      await page.waitForFunction(() => document.querySelector('.token-kpi p')?.textContent === '$477.80');
      priceGetFails = true;
      await editPrice('unpriced-model', 'Custom');
      await dialog.getByRole('alert').waitFor();
      assert.equal(await dialog.getByRole('button', { name: zh ? '保存价格' : 'Save prices', exact: true }).count(), 0, 'Load failures cannot overwrite unknown state');
      priceGetFails = false;
      await dialog.getByRole('button', { name: zh ? '重试' : 'Retry', exact: true }).click();
      await inputRate().waitFor();
      assert.equal(await inputRate().inputValue(), '', 'Unknown price must not prefill zero');
      await dialog.getByRole('combobox', { name: zh ? '计价币种' : 'Pricing currency' }).click();
      await page.getByRole('option', { name: /CNY/ }).click();
      await inputRate().fill('8');
      await cacheRate().fill('0');
      await outputRate().fill('12');
      await save();
      await dialog.waitFor({ state: 'detached' });
      // Saving CNY rates switches the display to CNY: merged total 474.001*7 + 26.6 + 14.
      await page.waitForFunction(() => document.querySelector('.token-kpi p')?.textContent === '¥3,358.61');
      assert.deepEqual(writes.at(-1).price, { currency: 'CNY', input_rate: 8, output_rate: 12, cache_read_rate: 0 });
      await editPrice('unpriced-model', 'Custom');
      await inputRate().waitFor();
      await dialog.getByRole('button', { name: zh ? '恢复默认' : 'Restore defaults', exact: true }).click();
      await dialog.getByRole('button', { name: zh ? '确认恢复默认' : 'Confirm restore defaults', exact: true }).click();
      await dialog.waitFor({ state: 'detached' });
      assert.equal(overrides.size, 0);
      // Start again to reset the presentation currency before legacy/empty checks.
      await page.reload();
      await panel.locator('.token-day').first().waitFor();
      apiMode = 'legacy';
      await panel.getByRole('button', { name: zh ? '刷新统计' : 'Refresh', exact: true }).click();
      await page.waitForFunction(() => document.querySelector('.token-kpi p')?.textContent === '—');
      await panel.getByRole('button', { name: 'Token', exact: true }).click();
      assert.notEqual(await panel.locator('.token-kpi').first().locator('p').first().textContent(), '—');
      apiMode = 'empty';
      await panel.getByRole('button', { name: zh ? '刷新统计' : 'Refresh', exact: true }).click();
      await panel.locator('.token-models tbody tr').first().waitFor({ state: 'detached' });
      await panel.getByRole('button', { name: zh ? '等值费用' : 'API value', exact: true }).click();
      assert.equal(await panel.locator('.token-kpi').first().locator('p').first().textContent(), '$0.00');
      apiMode = 'error';
      await panel.getByRole('button', { name: zh ? '刷新统计' : 'Refresh', exact: true }).click();
      await panel.getByRole('alert').waitFor();
      assert.deepEqual(errors, []);
      console.log('PASS metric toggle, FX-merged totals in both display currencies, day/month drill-down, model filters, free/unknown/tiny, fallback, prefix inheritance (save/inherit/reset/editor), manual edit/validation/save/reload/CNY, load-save failure recovery, legacy, empty, error, layout:', scenario);
      await page.close();
    }
    console.log('SCREENSHOTS', screenshots);
  } finally { await browser.close(); }
})().catch(error => { console.error(error); process.exitCode = 1; });
