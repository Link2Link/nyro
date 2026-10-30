import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { registerHooks } from 'node:module'
import { test } from 'node:test'
import React from 'react'
import { renderToStaticMarkup } from 'react-dom/server'
import ts from 'typescript'
import type { NyroPoolQuota, NyroPoolQuotaWindow, NyroUsage, NyroUsageItem, NyroUpstreamProvider } from '../src/nyro.ts'

// Existing fixtures exercise the explicitly selected Chinese UI, not the fallback locale.
Object.defineProperty(globalThis, 'document', { configurable: true, value: { documentElement: { lang: 'zh-CN' } } })

// Exercise the real cards and grid without a browser or a new dependency.
// Node strips .ts types; compile JSX and stub CSS module names only.
registerHooks({
  load(url, context, nextLoad) {
    if (url.endsWith('.module.css')) return {
      format: 'module', shortCircuit: true,
      source: 'export default new Proxy({}, { get: (_, key) => String(key) })',
    }
    if (url.endsWith('.tsx')) return {
      format: 'module', shortCircuit: true,
      source: ts.transpileModule(readFileSync(new URL(url), 'utf8'), {
        compilerOptions: { jsx: ts.JsxEmit.ReactJSX, module: ts.ModuleKind.ESNext, target: ts.ScriptTarget.ES2022 },
      }).outputText,
    }
    return nextLoad(url, context)
  },
})
const { ProviderCard } = await import('../src/client/panel/ProviderCard.tsx')
const { InstanceSection } = await import('../src/client/panel/InstanceSection.tsx')
const { cardEntries, countUsage } = await import('../src/client/panel/upstream.ts')
const { bySavedOrder } = await import('../src/client/panel/cardOrder.ts')

const usage = (overrides: Partial<NyroUsage> = {}): NyroUsage => ({
  provider_id: 'remote', kind: 'kiro_plan', site: 'global',
  tiers: [{ name: 'monthly', used_percent: 19.0946 }],
  scheduling: { status: 'eligible' }, ...overrides,
})
function gateway(providers: NyroUpstreamProvider[], redacted?: string[]): NyroUsageItem {
  return {
    provider_id: 'gateway', provider_name: 'US Nyro', is_enabled: true, status: 'ok',
    usage: usage({ kind: 'nyro_gateway', tiers: [], upstream: {
      object: 'nyro.usage', version: 1, providers, redacted,
    } }),
  }
}
const remote = (overrides: Partial<NyroUpstreamProvider> = {}): NyroUpstreamProvider => ({
  id: 'remote', name: 'Remote Kiro', vendor: 'kiro', routes: ['claude-sonnet'], status: 'ok', usage: usage(), ...overrides,
})
const direct: NyroUsageItem = {
  provider_id: 'direct', provider_name: 'Direct', is_enabled: true, status: 'ok',
  usage: usage({
    balances: [{ currency: 'USD', total: 12.5, granted: 0, topped_up: 12.5 }],
    spends: [{ name: 'today', amount: 1.25, currency: 'USD' }],
  }),
}
/** Number of provider cards (the `card` class itself, not cardHeader etc.). */
function cardCount(html: string): number {
  return (html.match(/class="card(?:"| )/g) ?? []).length
}
function renderCard(props: Record<string, unknown>): string {
  return renderToStaticMarkup(React.createElement(ProviderCard, { now: Date.now(), ...props } as never))
}

test('a chained Nyro row is replaced by one card per upstream provider', () => {
  const entries = cardEntries([
    gateway([remote(), remote({ id: 'other', name: 'Other', usage: usage({ upstream: gateway([remote()]).usage!.upstream }) })]),
    direct,
  ])
  assert.deepEqual(entries.map(entry => entry.key), ['gateway\u0000remote', 'gateway\u0000other', 'direct'])
  assert.equal(entries[0].via?.gateway, 'US Nyro')
  assert.equal(entries[0].anchor, 'gateway')
  assert.equal(entries[1].item.usage?.upstream, undefined, 'forwarding stays one hop deep')
  assert.equal(entries[2].via, undefined)
  // Counters describe the drawn cards: the hidden gateway card is not counted.
  assert.deepEqual(countUsage([gateway([remote({ usage: usage({ scheduling: { status: 'quota_exhausted' } }) })])]), { ok: 1, exhausted: 1 })
})

test('the gateway card stays as a fallback when nothing forwarded would be drawn', () => {
  assert.deepEqual(cardEntries([gateway([])]).map(entry => entry.key), ['gateway'])
  assert.deepEqual(
    cardEntries([gateway([remote({ id: 'u', status: 'unsupported' })])]).map(entry => entry.key),
    ['gateway', 'gateway\u0000u'],
  )
  // A gateway whose own query failed has no report and keeps its error card.
  const failed: NyroUsageItem = { provider_id: 'gateway', provider_name: 'US Nyro', is_enabled: true, status: 'error', error: 'HTTP 404' }
  assert.deepEqual(cardEntries([failed]).map(entry => entry.key), ['gateway'])
})

test('unsaved forwarded cards take their gateway position under a custom order', () => {
  const entries = cardEntries([direct, gateway([remote()])])
  const sorted = bySavedOrder(entries, ['gateway', 'direct'], entry => entry.key, entry => entry.anchor)
  assert.deepEqual(sorted.map(entry => entry.key), ['gateway\u0000remote', 'direct'])
})

test('a forwarded card is an ordinary card clearly marked as upstream Nyro usage', () => {
  const [entry] = cardEntries([gateway([remote()], ['balances', 'spends'])])
  const html = renderCard({ item: entry.item, via: entry.via })
  assert.equal(cardCount(html), 1)
  assert.ok(html.includes('cardForwarded'))
  for (const text of ['Remote Kiro', '上游 Nyro 用量', 'US Nyro', '19%', 'claude-sonnet', 'balances, spends']) {
    assert.ok(html.includes(text), text)
  }
  assert.ok(html.includes('pillEligible'))
})

test('forwarded failures, paused quotas and withheld amounts keep their meaning', () => {
  const [failed, paused, hidden] = cardEntries([gateway([
    { id: 'e', name: 'Failed', status: 'error', error: 'quota endpoint timeout' },
    remote({ id: 'p', usage: usage({ tiers: [{ name: 'five_hour', used_percent: 100 }], scheduling: { status: 'quota_exhausted' } }) }),
    remote({ id: 'h', name: 'Balance key', usage: usage({ kind: 'deepseek_balance', tiers: [] }) }),
  ], ['balances'])])
  const failedHtml = renderCard({ item: failed.item, via: failed.via })
  assert.ok(failedHtml.includes('quota endpoint timeout'))
  assert.ok(!failedHtml.includes('pillEligible'), 'a failed upstream must not look healthy')
  const pausedHtml = renderCard({ item: paused.item, via: paused.via })
  assert.ok(pausedHtml.includes('pillExhausted') && pausedHtml.includes('cardExhausted'))
  assert.ok(renderCard({ item: hidden.item, via: hidden.via }).includes('上游未开放金额数据'))
})

test('a fallback gateway card summarizes instead of nesting the upstream providers', () => {
  const html = renderCard({ item: gateway([remote({ id: 'u', name: 'No API', status: 'unsupported' })], ['spends']) })
  assert.equal(cardCount(html), 1)
  assert.ok(html.includes('转发上游 1 个供应商') && html.includes('1 个不支持查询') && html.includes('金额未开放'))
  assert.ok(!html.includes('No API'), 'upstream providers never nest inside the gateway card')
  assert.ok(renderCard({ item: gateway([]) }).includes('当前 API Key 下没有可见的上游供应商'))
})

test('the section grid shows forwarded cards in place of the gateway card', () => {
  const items = [gateway([remote(), remote({ id: 'u', name: 'No API', status: 'unsupported' })]), direct]
  const html = renderToStaticMarkup(React.createElement(InstanceSection, {
    instance: { id: 'default', name: 'Primary', baseUrl: 'http://nyro', hasToken: true, cachedAt: null },
    state: { loading: false, usage: { id: 'default', name: 'Primary', baseUrl: 'http://nyro', items, cachedAt: null, error: null, errorKind: null }, error: null },
    now: Date.now(),
    onRefresh: () => {},
  }))
  // One supported upstream + direct: the gateway summary card is gone, and the
  // unsupported upstream is hidden like top-level ones.
  assert.equal(cardCount(html), 2)
  assert.ok(!html.includes('gatewayMeta') && !html.includes('nyro_gateway'), 'no gateway summary card')
  assert.ok(html.includes('上游 Nyro 用量 · 经「US Nyro」转发'))
  assert.ok(html.indexOf('Remote Kiro') < html.indexOf('>Direct<'))
  assert.ok(!html.includes('>No API<'))
})

test('Kiro shows only its pooled monthly window, not the per-allowance feature rows', () => {
  const kiroTiers = [
    { name: 'monthly', used_percent: 19.0946 },
    { name: 'feature:kiro:base:Credit:0', used_percent: 19.0946 },
  ]
  const kiro = renderCard({ item: { ...direct, usage: usage({ kind: 'kiro_plan', tiers: kiroTiers }) } })
  assert.ok(kiro.includes('title="monthly"'))
  assert.ok(!kiro.includes('feature:kiro'), 'Kiro allowance components are hidden')
  // Other vendors' feature windows (e.g. Codex) are still shown.
  const codex = renderCard({ item: { ...direct, usage: usage({ kind: 'openai_codex', tiers: [
    { name: 'five_hour', used_percent: 10 },
    { name: 'feature:codex_bengalfox:five_hour', used_percent: 20 },
  ] }) } })
  assert.ok(codex.includes('feature:codex_bengalfox:five_hour'))
})

const poolWindow = (overrides: Partial<NyroPoolQuotaWindow> = {}): NyroPoolQuotaWindow => ({
  total_accounts: 2, capacity_complete: true, total_units: 2,
  known_accounts: 2, known_remaining_units: 1.36, used_units: 0.64, remaining_units: 1.36,
  used_percent: 32, remaining_percent: 68, stale: false,
  next_reset_at: '2030-01-01T05:00:00Z', next_reset_accounts: 1, reset_mode: 'staggered', ...overrides,
})
const poolQuota = (overrides: Partial<NyroPoolQuotaWindow> = {}): NyroPoolQuota => ({
  estimated: true, claude_windows: [
    { name: 'five_hour', quota: poolWindow(overrides) },
    { name: 'weekly_limit', quota: poolWindow({ used_percent: 36.5, remaining_percent: 63.5, ...overrides }) },
  ],
})
const claude = (overrides: Partial<NyroUsage> = {}): NyroUsageItem => ({
  ...direct, usage: usage({ kind: 'claude_code_plan', tiers: [], ...overrides }),
})

test('direct legacy Claude pools render existing Admin quota with no identity details', () => {
  const item = claude({ account_pool: { quota: poolQuota(), accounts: [{ label: 'PRIVATE@example.org' }] } as never })
  const html = renderCard({ item })
  for (const text of ['加权估算', '已用 32%', '已用 36.5%', '剩余 63.5%', '有效用量 2 / 2', '账号分批重置', '不一定是全池']) assert.ok(html.includes(text), text)
  assert.equal((html.match(/role="progressbar"/g) ?? []).length, 2)
  for (const text of ['PRIVATE', 'tierPace', '未开放金额']) assert.ok(!html.includes(text), text)
})

test('new anonymous forwarded summaries render even with redacted money and never duplicate tiers', () => {
  const item = claude({ pool_quota: poolQuota(), tiers: [{ name: 'five_hour', used_percent: 99 }, { name: 'feature:opus:weekly_limit', used_percent: 20 }] })
  const [entry] = cardEntries([gateway([remote({ usage: item.usage })], ['balances', 'spends'])])
  const html = renderCard({ item: entry.item, via: entry.via })
  for (const text of ['加权估算', '已用 36.5%', 'feature:opus', 'pillEligible']) assert.ok(html.includes(text), text)
  assert.ok(!html.includes('99%') && !html.includes('未开放金额数据'))
  assert.ok(!html.includes('tierPace'), 'pool estimates never use a single-account pace marker')
})

test('legacy forwarded Claude quota gaps are not mistaken for hidden money', () => {
  const html = renderCard({ item: claude(), via: { gateway: 'US Nyro', redacted: ['balances', 'spends'] } })
  assert.ok(html.includes('未返回 Claude 配额摘要') && html.includes('升级上游 Nyro'))
  assert.ok(!html.includes('未开放金额数据') && !html.includes('0%'))
  // Never use an accidentally forwarded account_pool as a privacy workaround.
  const leaked = renderCard({ item: claude({ account_pool: { quota: poolQuota() } }), via: { gateway: 'US Nyro' } })
  assert.ok(!leaked.includes('已用 32%'))
  assert.ok(renderCard({ item: claude() }).includes('暂未取得配额数据'))
})

test('unknown, invalid, partial and stale pool windows never turn into 0% or complete usage', () => {
  for (const patch of [
    { used_percent: null }, { used_percent: NaN }, { used_percent: Infinity }, { used_percent: -1 },
    { used_percent: 101 }, { stale: true }, { known_accounts: 1 }, { capacity_complete: false }, { total_accounts: 0 },
  ]) {
    const html = renderCard({ item: claude({ pool_quota: poolQuota(patch) }) })
    assert.ok(html.includes('用量未知 / 数据不完整'), JSON.stringify(patch))
    assert.ok(!html.includes('role="progressbar"') && !html.includes('剩余 68%'))
    if (patch.stale) assert.ok(html.includes('存在旧数据'))
  }
  const unknown = renderCard({ item: claude({ pool_quota: poolQuota({ next_reset_at: 'invalid', reset_mode: 'unknown' }) }) })
  assert.ok(unknown.includes('重置时间未知') && !unknown.includes('<time'))
  const expired = renderCard({ now: Date.parse('2031-01-01'), item: claude({ pool_quota: poolQuota() }) })
  assert.ok(expired.includes('重置时间已过，等待刷新'))
})

test('valid zero usage and exact singleton windows are preserved', () => {
  const zero = renderCard({ item: claude({ pool_quota: poolQuota({ used_percent: 0, remaining_percent: 100 }) }) })
  assert.ok(zero.includes('已用 0%') && zero.includes('aria-valuenow="0"'))
  const single = renderCard({ item: claude({ tiers: [{ name: 'five_hour', used_percent: 64 }] }) })
  assert.ok(single.includes('64%') && !single.includes('配额数据') && !single.includes('加权估算'))
})

test('real singleton responses prefer exact tiers when a summary is also present', () => {
  const quota = poolQuota({ total_accounts: 1, known_accounts: 1 })
  const tiers = [
    { name: 'five_hour', used_percent: 64, resets_at: '2030-01-01T05:00:00Z' },
    { name: 'weekly_limit', used_percent: 35, resets_at: '2030-01-07T00:00:00Z' },
  ]
  for (const summary of [{ pool_quota: quota }, { account_pool: { quota } }]) {
    const html = renderCard({ item: claude({ tiers, ...summary }) })
    assert.ok(html.includes('64%') && html.includes('35%') && html.includes('tierPace'))
    assert.ok(!html.includes('加权估算') && !html.includes('已用 32%'))
  }
  const partial = renderCard({ item: claude({ pool_quota: quota, tiers: tiers.slice(0, 1) }) })
  assert.ok(partial.includes('64%') && partial.includes('已用 36.5%'), 'missing exact windows may still use the summary')
})

test('pool copy defaults to English unless Chinese is selected', () => {
  try {
    document.documentElement.lang = 'en'
    const html = renderCard({ item: claude({ pool_quota: poolQuota() }) })
    assert.ok(html.includes('Weighted estimate') && html.includes('Used 36.5%') && html.includes('not necessarily the whole pool'))
    document.documentElement.lang = ''
    assert.ok(renderCard({ item: claude() }).includes('Quota data is unavailable'))
  } finally { document.documentElement.lang = 'zh-CN' }
})

test('pool windows use compact tier rows with details collapsed by default', () => {
  const html = renderCard({ now: Date.parse('2030-01-01T04:30:00Z'), item: claude({ pool_quota: poolQuota() }) })
  const compact = html.slice(0, html.indexOf('<details'))
  assert.equal((compact.match(/class="tierRow"/g) ?? []).length, 2)
  assert.ok(compact.includes('class="tierPercent tierPercentOk">36.5%</span>'))
  assert.ok(compact.includes('⏳ 30m*'), 'countdown uses the supplied clock and marks partial resets')
  assert.ok(html.includes('<details class="poolDetails"><summary') && !html.includes(' open='))
  assert.ok(html.includes('* 部分账号重置') && html.includes('汇总仅供参考'))
  assert.ok(!compact.includes('<time') && !compact.includes('poolWindowHeader'), 'verbose rows stay inside details')
  assert.ok(!html.includes('tierPace'), 'aggregate quota is not a single scheduling window')
})

test('pool reset countdown distinguishes aligned, unknown, invalid and expired resets', () => {
  const now = Date.parse('2030-01-01T04:30:00Z')
  const render = (patch: Partial<NyroPoolQuotaWindow>) => renderCard({ now, item: claude({ pool_quota: poolQuota(patch) }) })
  const aligned = render({ next_reset_accounts: 2, reset_mode: 'aligned' })
  assert.ok(aligned.includes('⏳ 30m') && !aligned.includes('⏳ 30m*') && !aligned.includes('* 部分账号重置'))
  for (const patch of [{ next_reset_at: null }, { next_reset_at: 'invalid' }, { next_reset_accounts: 0 }, { next_reset_accounts: 3 }]) {
    const html = render(patch)
    assert.ok(!html.includes('⏳') && !html.includes('<time') && html.includes('重置时间未知'))
  }
  const expired = render({ next_reset_at: '2030-01-01T04:00:00Z' })
  assert.ok(!expired.includes('⏳') && expired.includes('>待刷新</span>') && expired.includes('重置时间已过'))
})

test('compact pools retain precision, thresholds and visible unknown states', () => {
  for (const [used, state] of [[0, 'Ok'], [69.9, 'Ok'], [70, 'Warn'], [87.5, 'Warn'], [90, 'Danger'], [100, 'Danger']] as const) {
    const html = renderCard({ item: claude({ pool_quota: poolQuota({ used_percent: used }) }) })
    assert.ok(html.includes(`class="tierPercent tierPercent${state}">${used}%</span>`))
    assert.ok(html.includes(`aria-valuenow="${used}"`))
  }
  const html = renderCard({ item: claude({ pool_quota: poolQuota({ stale: true }) }) })
  const compact = html.slice(0, html.indexOf('<details'))
  assert.ok(compact.includes('poolTrackUnknown') && compact.includes('>未知</span>'))
  assert.ok(!compact.includes('role="progressbar"') && !compact.includes('tierPercentOk'))
})

test('ordinary provider tiers, balances and spends still render', () => {
  const html = renderCard({ item: direct })
  for (const text of ['19%', '$12.50', '$1.25']) assert.ok(html.includes(text), text)
  assert.ok(!html.includes('cardForwarded') && !html.includes('gatewayMeta'))
})
