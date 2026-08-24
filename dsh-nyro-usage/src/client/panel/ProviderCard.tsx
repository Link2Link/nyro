/**
 * One provider card: identity row (drag grip + name + kind/level badges +
 * scheduling pill), quota-window bars, balances and spends. The card is
 * HTML5-draggable for manual reordering (order persisted by the panel).
 * Unsupported providers render muted; per-provider query failures render an
 * inline error banner — one vendor outage never fails the whole listing.
 */
import type { DragEvent } from 'react'
import type { NyroUsageItem } from '../../nyro.ts'
import { agoLabel, tt } from '../tt.ts'
import { TierBar } from './TierBar.tsx'
import css from './panel.module.css'

/** Currency → symbol (anything else keeps its ISO code). */
function currencySymbol(currency: string): string {
  if (currency === 'CNY') return '¥'
  if (currency === 'USD') return '$'
  return `${currency} `
}

/** Props the card needs. */
export interface ProviderCardProps {
  item: NyroUsageItem
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
  const { item } = props
  const usage = item.usage
  const exhausted = usage?.scheduling?.status === 'quota_exhausted'
  const balances = usage?.balances ?? []
  const spends = usage?.spends ?? []
  const tiers = usage?.tiers ?? []

  const classes = [css.card]
  if (!item.is_enabled) classes.push(css.cardMuted)
  if (exhausted) classes.push(css.cardExhausted)
  if (props.dragging === true) classes.push(css.cardDragging)
  if (props.dropBefore === true) classes.push(css.cardDropBefore)
  if (props.dropAfter === true) classes.push(css.cardDropAfter)

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
            : <span className={`${css.pill} ${css.pillEligible}`}>{tt('card.eligible')}</span>}
        </span>
      </div>

      {item.status === 'error'
        ? (
          <div className={css.errorBanner} role="alert">
            <span className={css.errorTitle}>{tt('card.error')}</span>
            <span className={css.errorText}>{item.error ?? ''}</span>
          </div>
        )
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
