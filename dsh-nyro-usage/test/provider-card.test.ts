import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { registerHooks } from 'node:module'
import { test } from 'node:test'
import React from 'react'
import { renderToStaticMarkup } from 'react-dom/server'
import ts from 'typescript'
import type { NyroUsage, NyroUsageItem, NyroUpstreamProvider } from '../src/nyro.ts'

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

test('ordinary provider tiers, balances and spends still render', () => {
  const html = renderCard({ item: direct })
  for (const text of ['19%', '$12.50', '$1.25']) assert.ok(html.includes(text), text)
  assert.ok(!html.includes('cardForwarded') && !html.includes('gatewayMeta'))
})
