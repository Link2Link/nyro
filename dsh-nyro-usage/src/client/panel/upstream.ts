/**
 * Chained Nyro gateways: flatten a gateway provider's forwarded usage report
 * (`usage.upstream.providers[]`) into ordinary provider cards that take the
 * gateway's place in the grid; the gateway's own summary card is shown only
 * when it forwards nothing drawable. Each forwarded card carries a `via`
 * marker so it is labelled as upstream Nyro usage. The same upstream provider
 * may also appear in that gateway's own panel section; the duplicate is
 * intentional.
 *
 * Nyro forwards one hop only, so forwarded usage never expands further.
 */
import type { NyroUsageItem } from '../../nyro.ts'

/** Where a forwarded card's usage comes from. */
export interface CardVia {
  /** Display name of the local gateway provider that forwarded the report. */
  gateway: string
  vendor?: string | null
  routes?: string[]
  /** Fields the upstream API key's permissions withhold (e.g. balances). */
  redacted?: string[]
}

/** One card in a gateway section's grid. */
export interface CardEntry {
  /** Stable card identity, also the saved drag-order id. */
  key: string
  item: NyroUsageItem
  via?: CardVia
  /** Key of the card this one follows when it has no saved position yet. */
  anchor?: string
}

/**
 * Expand a section's provider rows into cards: an ordinary row keeps its
 * card; a chained Nyro row becomes one card per forwarded provider (its own
 * card is kept only as a fallback when none of them would be drawn).
 * @param items - the gateway section's provider rows, in natural order.
 * @returns cards in natural order.
 */
export function cardEntries(items: readonly NyroUsageItem[]): CardEntry[] {
  const entries: CardEntry[] = []
  for (const item of items) {
    const upstream = item.usage?.upstream
    if (upstream == null) {
      entries.push({ key: item.provider_id, item })
      continue
    }
    const forwarded: CardEntry[] = []
    for (const provider of upstream.providers) {
      // Defensive one-hop limit, matching the Nyro wire contract.
      const usage = provider.usage == null ? provider.usage : { ...provider.usage, upstream: undefined }
      forwarded.push({
        key: `${item.provider_id}\u0000${provider.id}`,
        anchor: item.provider_id,
        via: {
          gateway: item.provider_name,
          vendor: provider.vendor,
          routes: provider.routes,
          redacted: upstream.redacted,
        },
        item: {
          provider_id: provider.id,
          provider_name: provider.name,
          // The forwarding endpoint only reports enabled providers.
          is_enabled: true,
          status: provider.status,
          error: provider.error,
          usage,
        },
      })
    }
    // The forwarded cards replace the gateway's own summary card. Keep it only
    // when nothing forwarded would be drawn (empty report or every upstream
    // lacks a usage API), so the gateway never silently vanishes.
    if (!forwarded.some(entry => entry.item.status !== 'unsupported')) {
      entries.push({ key: item.provider_id, item })
    }
    entries.push(...forwarded)
  }
  return entries
}

/** Usage counters shown in the panel and section headers (a type alias so it reads as template values). */
export type UsageCounts = {
  ok: number
  exhausted: number
}

/**
 * Count healthy and quota-exhausted cards, forwarded upstream cards included
 * (the counters describe what the grid shows).
 * @param items - one gateway's provider rows.
 * @returns the counters.
 */
export function countUsage(items: readonly NyroUsageItem[]): UsageCounts {
  const cards = cardEntries(items).map(entry => entry.item)
  return {
    ok: cards.filter(item => item.status === 'ok').length,
    exhausted: cards.filter(item => item.usage?.scheduling?.status === 'quota_exhausted').length,
  }
}
