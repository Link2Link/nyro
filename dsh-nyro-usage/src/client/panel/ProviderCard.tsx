/**
 * One provider card: identity row (drag grip + name + kind/level badges +
 * scheduling pill), quota-window bars, balances and spends. The card is
 * HTML5-draggable for manual reordering (order persisted by the panel).
 * Unsupported providers render muted; per-provider query failures render an
 * inline error banner — one vendor outage never fails the whole listing.
 *
 * Chained Nyro gateways: each forwarded upstream provider gets its own card
 * (`via` set) with an accent bar and a "via upstream Nyro" line, replacing
 * the gateway's card; the gateway card (one-line summary) only appears as a
 * fallback when it forwards nothing drawable.
 */
import type { DragEvent } from 'react'
import type { NyroUsageItem } from '../../nyro.ts'
import { agoLabel, tt } from '../tt.ts'
import { TierBar } from './TierBar.tsx'
import { PoolQuota } from './PoolQuota.tsx'
import type { CardVia } from './upstream.ts'
import css from './panel.module.css'

/** Currency → symbol (anything else keeps its ISO code). */
function currencySymbol(currency: string): string {
  if (currency === 'CNY') return '¥'
  if (currency === 'USD') return '$'
  return `${currency} `
}

/** Money fields an upstream API key can withhold. */
function hidesMoney(redacted: readonly string[] | undefined): boolean {
  return (redacted ?? []).some(field => field === 'balances' || field === 'spends')
}

/** Props the card needs. */
export interface ProviderCardProps {
  item: NyroUsageItem
  /** Set for a provider forwarded by a chained Nyro gateway. */
  via?: CardVia
  /** Current time, tracked so the steady-pace markers re-evaluate on the 30s tick. */
  now: number
  /** True while this card is the one being dragged (dimmed). */
  dragging?: boolean
  /** True while a drop targets this card's left/right edge (insertion bar). */
  dropBefore?: boolean
  dropAfter?: boolean
  /** Drag-to-reorder wiring (HTML5 DnD); the card is always draggable. */
  onCardDragStart?: (event: DragEvent<HTMLDivElement>) => void
  onCardDragEnd?: (event: DragEvent<HTMLDivElement>) => void
  onCardDragOver?: (event: DragEvent<HTMLDivElement>) => void
  onCardDrop?: (event: DragEvent<HTMLDivElement>) => void
}

/** Render one provider card. */
export function ProviderCard(props: ProviderCardProps) {
  const { item, via } = props
  const usage = item.usage
  const scheduling = item.status === 'ok' ? usage?.scheduling?.status : undefined
  const exhausted = scheduling === 'quota_exhausted'
  const balances = usage?.balances ?? []
  const spends = usage?.spends ?? []
  // Older direct Admin responses expose the same safe quota under account_pool.
  // Never consult account identities or infer a pool from a different gateway.
  const pool = usage?.kind === 'claude_code_plan'
    ? usage.pool_quota ?? (via === undefined ? usage.account_pool?.quota : undefined)
    : undefined
  const poolWindows = (pool?.claude_windows ?? [])
    .filter(window => ['five_hour', 'weekly_limit'].includes(window.name) && window.quota != null)
    // Singleton responses also include a summary; retain their exact tier/reset/pace.
    .filter(window => window.quota.total_accounts !== 1
      || !(usage?.tiers ?? []).some(tier => tier.name === window.name))
  const poolNames = new Set(poolWindows.map(window => window.name))
  // Kiro reports its pooled quota as `monthly` plus one `feature:kiro:*` row
  // per allowance component (base / bonus / free trial) that sums into it.
  // Show only the pooled window, like the Nyro WebUI does.
  const tiers = (usage?.tiers ?? [])
    .filter(tier => usage?.kind !== 'kiro_plan' || !tier.name.startsWith('feature:kiro:'))
    .filter(tier => !poolNames.has(tier.name))
  const upstream = usage?.upstream

  const classes = [css.card]
  if (!item.is_enabled) classes.push(css.cardMuted)
  if (via !== undefined) classes.push(css.cardForwarded)
  if (exhausted) classes.push(css.cardExhausted)
  if (props.dragging === true) classes.push(css.cardDragging)
  if (props.dropBefore === true) classes.push(css.cardDropBefore)
  if (props.dropAfter === true) classes.push(css.cardDropAfter)

  // Gateway card: a one-line pointer at the forwarded cards that follow it.
  let gatewayMeta: string | null = null
  if (upstream != null) {
    const unsupported = upstream.providers.filter(provider => provider.status === 'unsupported').length
    const parts = [upstream.providers.length === 0
      ? tt('card.upstreamEmpty')
      : tt('card.upstreamCount', { count: upstream.providers.length })]
    if (unsupported > 0) parts.push(tt('card.upstreamUnsupported', { count: unsupported }))
    if (hidesMoney(upstream.redacted)) parts.push(tt('card.upstreamRedacted'))
    gatewayMeta = parts.join(' · ')
  }

  // Forwarded card: say where the numbers come from, with the detail in a tooltip.
  const viaTitle = via === undefined
    ? undefined
    : [
        via.vendor ? `${item.provider_name} · ${via.vendor}` : item.provider_name,
        via.routes && via.routes.length > 0 ? tt('card.routes', { routes: via.routes.join(', ') }) : '',
        via.redacted && via.redacted.length > 0 ? tt('card.redacted', { fields: via.redacted.join(', ') }) : '',
      ].filter(line => line !== '').join('\n')
  const noUsage = item.status === 'ok' && upstream == null && poolWindows.length === 0
    && tiers.length === 0 && balances.length === 0 && spends.length === 0
  // Report-level money redaction says nothing about missing subscription quotas.
  const moneyHidden = noUsage && via !== undefined && hidesMoney(via.redacted)
    && ['deepseek_balance', 'mimo_balance'].includes(usage?.kind ?? '')
  const quotaMissing = noUsage && !moneyHidden
  const missingMessage = usage?.kind === 'claude_code_plan' && via !== undefined
    ? 'card.poolNotForwarded' : 'card.quotaUnknown'

  return (
    <div
      className={classes.join(' ')}
      draggable
      onDragStart={props.onCardDragStart}
      onDragEnd={props.onCardDragEnd}
      onDragOver={props.onCardDragOver}
      onDrop={props.onCardDrop}
    >
      <div className={css.cardHeader}>
        <span className={css.dragHandle} title={tt('card.dragHandle')} aria-hidden="true">⠿</span>
        <span className={css.cardTitle} title={item.provider_id}>{item.provider_name}</span>
        <span className={css.cardBadges}>
          {usage !== null && usage !== undefined && usage.kind !== '' ? <span className={css.badge}>{usage.kind}</span> : null}
          {usage?.level != null && usage.level !== '' ? <span className={`${css.badge} ${css.badgeLevel}`}>{usage.level}</span> : null}
          {!item.is_enabled ? <span className={css.badge}>{tt('card.disabled')}</span> : null}
          {exhausted
            ? <span className={`${css.pill} ${css.pillExhausted}`}>{tt('card.quotaExhausted')}</span>
            : scheduling === 'eligible'
              ? <span className={`${css.pill} ${css.pillEligible}`}>{tt('card.eligible')}</span>
              : null}
        </span>
      </div>

      {via !== undefined
        ? <div className={css.viaLine} title={viaTitle}>{tt('card.via', { gateway: via.gateway })}</div>
        : null}

      {item.status === 'error'
        ? (
          <div className={css.errorBanner} role="alert">
            <span className={css.errorTitle}>{tt('card.error')}</span>
            <span className={css.errorText}>{item.error ?? ''}</span>
          </div>
        )
        : null}

      {gatewayMeta !== null
        ? (
          <div
            className={css.gatewayMeta}
            title={upstream?.redacted?.length ? tt('card.redacted', { fields: upstream.redacted.join(', ') }) : undefined}
          >
            {gatewayMeta}
          </div>
        )
        : null}

      {moneyHidden ? <div className={css.gatewayMeta}>{tt('card.upstreamHidden')}</div> : null}
      {quotaMissing ? <div className={css.poolMeta}>{tt(missingMessage)}</div> : null}

      {poolWindows.length > 0
        ? <PoolQuota windows={poolWindows} estimated={pool?.estimated !== false} now={props.now} />
        : null}

      {tiers.length > 0
        ? (
          <div className={css.tierList}>
            {tiers.map(tier => <TierBar key={tier.name} tier={tier} now={props.now} />)}
          </div>
        )
        : null}

      {balances.length > 0
        ? (
          <div className={css.balanceList}>
            {balances.map(balance => (
              <div key={balance.currency} className={css.balanceRow}>
                <span className={css.balanceLabel}>{tt('card.balance')}</span>
                <span className={css.balanceValue}>
                  {currencySymbol(balance.currency)}{balance.total.toFixed(2)}
                </span>
                <span className={css.balanceDetail}>
                  {balance.granted > 0
                    ? ` (+${balance.topped_up.toFixed(2)} / ${balance.granted.toFixed(2)})`
                    : ''}
                </span>
                {usage?.is_available === false ? <span className={css.unavailableBadge}>✗</span> : null}
              </div>
            ))}
          </div>
        )
        : null}

      {spends.length > 0
        ? (
          <div className={css.spendRow}>
            {spends.map(spend => (
              <span key={spend.name} className={css.spendItem} title={spend.name}>
                {spend.name === 'today' ? tt('card.spendToday') : spend.name === 'month' ? tt('card.spendMonth') : spend.name}
                {' '}
                <span className={css.spendValue}>
                  {currencySymbol(spend.currency)}{spend.amount.toFixed(2)}
                </span>
              </span>
            ))}
          </div>
        )
        : null}

      {usage?.queried_at != null && usage.queried_at !== ''
        ? (
          <div className={css.cardFooter}>{tt('card.queriedAt', { ago: agoLabel(usage.queried_at) })}</div>
        )
        : null}
    </div>
  )
}
