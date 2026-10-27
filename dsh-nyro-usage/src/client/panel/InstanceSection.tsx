/**
 * One nyro gateway's group in the usage panel: a collapsible header (name,
 * base URL chip, per-gateway summary, last-updated stamp, reset-order and
 * refresh actions) over that gateway's drag-to-reorder provider card grid.
 * A gateway's failure renders inside its own group only; the card order is
 * persisted per gateway.
 */
import { useCallback, useId, useState, type DragEvent } from 'react'
import type { NyroInstanceStatus, NyroInstanceUsage } from '../api.ts'
import { agoLabel, tt } from '../tt.ts'
import { bySavedOrder, useCardOrder } from './cardOrder.ts'
import { ProviderCard } from './ProviderCard.tsx'
import { cardEntries, countUsage, type CardEntry } from './upstream.ts'
import css from './panel.module.css'

export { countUsage, type UsageCounts } from './upstream.ts'

/** Load state of one gateway. */
export interface InstanceSectionState {
  /** A usage request for this gateway is in flight. */
  loading: boolean
  /** Last answered slice (kept while a refresh runs); undefined before the first answer. */
  usage: NyroInstanceUsage | undefined
  /** Transport-level failure of the last request (the host could not answer). */
  error: string | null
}

/** Props one gateway section needs. */
export interface InstanceSectionProps {
  instance: NyroInstanceStatus
  state: InstanceSectionState
  /** Current time, tracked so steady-pace markers and ages re-evaluate. */
  now: number
  /** Reload this gateway, bypassing the host cache. */
  onRefresh: () => void
}

/** Render one gateway section. */
export function InstanceSection(props: InstanceSectionProps) {
  const { instance, state } = props
  const [collapsed, setCollapsed] = useState(false)
  const contentId = useId()
  const { order, move, append, reset } = useCardOrder(instance.id)
  const [dragId, setDragId] = useState<string | null>(null)
  const [dropSide, setDropSide] = useState<{ id: string; before: boolean } | null>(null)

  const items = state.usage?.items ?? []
  const counts = countUsage(items)
  // A chained Nyro's forwarded providers become cards right after it.
  // Unsupported providers (no upstream usage API) are hidden from the grid.
  const visible = bySavedOrder(
    cardEntries(items).filter(entry => entry.item.status !== 'unsupported'),
    order,
    entry => entry.key,
    entry => entry.anchor,
  )
  const visibleIds = visible.map(entry => entry.key)
  // A gateway-level failure: the host could not answer, or this gateway's own slice failed.
  const failure = state.error ?? state.usage?.error ?? null
  const updated = agoLabel(state.usage?.cachedAt)

  // --- drag-to-reorder (HTML5 DnD) ------------------------------------------
  // Row-major grid: the pointer's horizontal half of the hovered card
  // decides before/after; a drop on the grid's empty area appends to the end.
  const clearDrag = useCallback((): void => {
    setDragId(null)
    setDropSide(null)
  }, [])

  const sideOf = (event: DragEvent<HTMLDivElement>): boolean => {
    const rect = event.currentTarget.getBoundingClientRect()
    return event.clientX < rect.left + rect.width / 2
  }

  const cardDragHandlers = (entry: CardEntry) => ({
    onCardDragStart: (event: DragEvent<HTMLDivElement>): void => {
      setDragId(entry.key)
      event.dataTransfer.effectAllowed = 'move'
      // Firefox only starts a drag once data is set.
      event.dataTransfer.setData('text/plain', entry.key)
    },
    onCardDragEnd: clearDrag,
    onCardDragOver: (event: DragEvent<HTMLDivElement>): void => {
      if (dragId === null) return
      event.preventDefault()
      event.dataTransfer.dropEffect = 'move'
      if (dragId === entry.key) return
      const before = sideOf(event)
      setDropSide(previous =>
        previous !== null && previous.id === entry.key && previous.before === before
          ? previous
          : { id: entry.key, before })
    },
    onCardDrop: (event: DragEvent<HTMLDivElement>): void => {
      if (dragId === null) return
      event.preventDefault()
      event.stopPropagation()
      if (dragId !== entry.key) move(visibleIds, dragId, entry.key, sideOf(event))
      clearDrag()
    },
  })

  return (
    <section className={css.section} aria-label={instance.name}>
      <div className={css.sectionHeader}>
        <h2 className={css.sectionHeading}>
          <button
            type="button"
            className={css.sectionToggle}
            aria-expanded={!collapsed}
            aria-controls={contentId}
            title={tt(collapsed ? 'section.expand' : 'section.collapse', { name: instance.name })}
            onClick={() => { setCollapsed(!collapsed) }}
          >
            <svg
              width="12"
              height="12"
              viewBox="0 0 12 12"
              aria-hidden="true"
              className={collapsed ? `${css.sectionChevron} ${css.sectionChevronCollapsed}` : css.sectionChevron}
            >
              <path d="M2.5 4.25 6 7.75l3.5-3.5" fill="none" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round" strokeLinejoin="round" />
            </svg>
            <span className={css.sectionTitle}>{instance.name}</span>
          </button>
        </h2>
        <span className={css.baseUrlChip} title={instance.baseUrl}>{instance.baseUrl}</span>
        {items.length > 0
          ? <span className={css.summary}>{tt('panel.summary', counts)}</span>
          : null}
        <span className={css.toolbarSpacer} />
        {order.length > 0
          ? (
            <button type="button" className={css.resetOrderButton} onClick={reset}>
              {tt('panel.resetOrder')}
            </button>
          )
          : null}
        <span className={css.updated} title={state.usage?.cachedAt ?? undefined}>
          {updated === '' ? '' : tt('panel.updated', { ago: updated })}
        </span>
        <button
          type="button"
          className={css.sectionRefresh}
          title={tt('section.refresh')}
          aria-label={`${tt('section.refresh')}: ${instance.name}`}
          disabled={state.loading}
          onClick={props.onRefresh}
        >
          {state.loading ? <span className={css.spinner} aria-hidden="true" /> : null}
          {state.loading ? tt('panel.refreshing') : tt('panel.refresh')}
        </button>
      </div>

      {collapsed
        ? null
        : (
          <div id={contentId} className={css.sectionBody}>
            {failure !== null
              ? (
                <div className={css.loadError} role="alert">
                  <span>{tt('panel.loadError')}：{failure}</span>
                  <button type="button" className={css.retryButton} onClick={props.onRefresh}>
                    {tt('panel.retry')}
                  </button>
                </div>
              )
              : null}

            {failure === null && !state.loading && state.usage !== undefined && visible.length === 0
              ? <div className={css.placeholder}><p className={css.placeholderHint}>{tt('panel.empty')}</p></div>
              : null}

            {state.loading && state.usage === undefined && state.error === null
              ? <div className={css.placeholder}><p className={css.placeholderHint}>{tt('panel.refreshing')}</p></div>
              : null}

            {visible.length > 0
              ? (
                <div
                  className={css.grid}
                  // The grid's empty area is a drop target too: dropping there
                  // sends the dragged card to the end of the custom order.
                  onDragOver={(event) => {
                    if (dragId === null) return
                    event.preventDefault()
                    event.dataTransfer.dropEffect = 'move'
                  }}
                  onDrop={(event) => {
                    if (dragId === null) return
                    event.preventDefault()
                    append(visibleIds, dragId)
                    clearDrag()
                  }}
                >
                  {visible.map(entry => (
                    <ProviderCard
                      key={entry.key}
                      item={entry.item}
                      via={entry.via}
                      now={props.now}
                      dragging={dragId === entry.key}
                      dropBefore={dropSide !== null && dropSide.id === entry.key && dropSide.before && dragId !== entry.key}
                      dropAfter={dropSide !== null && dropSide.id === entry.key && !dropSide.before && dragId !== entry.key}
                      {...cardDragHandlers(entry)}
                    />
                  ))}
                </div>
              )
              : null}
          </div>
        )}
    </section>
  )
}
