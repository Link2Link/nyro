/**
 * The nyro usage panel: header (base URL chip, scheduling summary, manual
 * refresh, auto-refresh selector, last-updated stamp) over a responsive
 * grid of provider cards the user can drag to reorder (order persisted per
 * browser). Loads through the same-origin host proxy; state machine:
 * unconfigured → loading → data | error.
 */
import { useCallback, useEffect, useRef, useState, useSyncExternalStore, type DragEvent } from 'react'
import { NyroPanelApi, NyroPanelApiError, type NyroStatusResponse, type NyroUsageItem } from '../api.ts'
import type { PanelController } from '../controller.ts'
import { agoLabel, errorMessage, tt } from '../tt.ts'
import { bySavedOrder, useCardOrder } from './cardOrder.ts'
import { ProviderCard } from './ProviderCard.tsx'
import css from './panel.module.css'

/** Auto-refresh choices offered next to the configured default. */
const AUTO_REFRESH_CHOICES = [0, 60, 300, 600] as const

/** Props the panel needs. */
export interface NyroUsagePanelProps {
  controller: PanelController
  api: NyroPanelApi
}

/** Load state of the bulk usage listing. */
interface UsageState {
  loading: boolean
  items: NyroUsageItem[]
  cachedAt: string | null
  error: string | null
  errorKind: string | null
}

/** Render the nyro provider usage panel. */
export function NyroUsagePanel(props: NyroUsagePanelProps) {
  const { api } = props
  const snapshot = useSyncExternalStore(props.controller.subscribe, props.controller.getSnapshot)
  const [status, setStatus] = useState<NyroStatusResponse | null>(null)
  const [state, setState] = useState<UsageState>({ loading: false, items: [], cachedAt: null, error: null, errorKind: null })
  const [autoSeconds, setAutoSeconds] = useState<number | null>(null)
  // Drag-to-reorder state: the dragged provider id + the hovered drop slot.
  const { order, move, append, reset } = useCardOrder()
  const [dragId, setDragId] = useState<string | null>(null)
  const [dropSide, setDropSide] = useState<{ id: string; before: boolean } | null>(null)
  // Re-evaluates the steady-pace markers and countdowns on a 30s tick.
  const [now, setNow] = useState(() => Date.now())
  const inFlight = useRef(false)
  const loadSeq = useRef(0)

  useEffect(() => {
    const timer = window.setInterval(() => { setNow(Date.now()) }, 30_000)
    return () => { window.clearInterval(timer) }
  }, [])

  const load = useCallback(async (refresh: boolean): Promise<void> => {
    if (inFlight.current) return
    inFlight.current = true
    const seq = ++loadSeq.current
    setState(previous => ({ ...previous, loading: true }))
    try {
      const [nextStatus, usage] = await Promise.all([
        api.status().catch(() => null),
        api.usage(refresh),
      ])
      if (seq !== loadSeq.current) return
      if (nextStatus !== null) setStatus(nextStatus)
      setState({ loading: false, items: usage.data, cachedAt: usage.cachedAt, error: null, errorKind: null })
    } catch (error) {
      if (seq !== loadSeq.current) return
      const message = errorMessage(error)
      const kind = error instanceof NyroPanelApiError ? (error.kind ?? null) : null
      setState(previous => ({ ...previous, loading: false, error: message, errorKind: kind }))
    } finally {
      inFlight.current = false
    }
  }, [api])

  // Panel becomes visible (or mounts): refresh the status immediately, and
  // load usage when configured. Re-checks whenever the panel reopens so a
  // settings change made in between is picked up.
  useEffect(() => {
    if (!snapshot.panelOpen) return
    void api.status().then(next => { setStatus(next) }).catch(() => {})
  }, [api, snapshot.panelOpen])

  const configured = status?.configured ?? false
  useEffect(() => {
    if (!snapshot.panelOpen || !configured) return
    void load(false)
  }, [snapshot.panelOpen, configured, load])

  // Auto-refresh while the panel is open and configured.
  const interval = autoSeconds ?? status?.refreshSeconds ?? 300
  useEffect(() => {
    if (!snapshot.panelOpen || !configured || interval <= 0) return
    const timer = window.setInterval(() => { void load(false) }, interval * 1000)
    return () => { window.clearInterval(timer) }
  }, [snapshot.panelOpen, configured, interval, load])

  const exhausted = state.items.filter(item => item.usage?.scheduling?.status === 'quota_exhausted').length
  const ok = state.items.filter(item => item.status === 'ok').length
  // Unsupported providers (no upstream usage API) are hidden from the grid.
  const items = state.items.filter(item => item.status !== 'unsupported')
  const visible = bySavedOrder(items, order, item => item.provider_id)
  const visibleIds = visible.map(item => item.provider_id)
  const updated = agoLabel(state.cachedAt)

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

  const cardDragHandlers = (item: NyroUsageItem) => ({
    onCardDragStart: (event: DragEvent<HTMLDivElement>): void => {
      setDragId(item.provider_id)
      event.dataTransfer.effectAllowed = 'move'
      // Firefox only starts a drag once data is set.
      event.dataTransfer.setData('text/plain', item.provider_id)
    },
    onCardDragEnd: clearDrag,
    onCardDragOver: (event: DragEvent<HTMLDivElement>): void => {
      if (dragId === null) return
      event.preventDefault()
      event.dataTransfer.dropEffect = 'move'
      if (dragId === item.provider_id) return
      const before = sideOf(event)
      setDropSide(previous =>
        previous !== null && previous.id === item.provider_id && previous.before === before
          ? previous
          : { id: item.provider_id, before })
    },
    onCardDrop: (event: DragEvent<HTMLDivElement>): void => {
      if (dragId === null) return
      event.preventDefault()
      event.stopPropagation()
      if (dragId !== item.provider_id) move(visibleIds, dragId, item.provider_id, sideOf(event))
      clearDrag()
    },
  })

  return (
    <div className={css.panel}>
      <div className={css.panelHeader}>
        <h1 className={css.panelTitle}>{tt('panel.title')}</h1>
        {status !== null && status.baseUrl !== ''
          ? <span className={css.baseUrlChip} title={status.baseUrl}>{status.baseUrl}</span>
          : null}
        {state.items.length > 0
          ? <span className={css.summary}>{tt('panel.summary', { ok, exhausted })}</span>
          : null}
        <span className={css.toolbarSpacer} />
        {order.length > 0
          ? (
            <button type="button" className={css.resetOrderButton} onClick={reset}>
              {tt('panel.resetOrder')}
            </button>
          )
          : null}
        <label className={css.autoRefreshLabel}>
          {tt('panel.autoRefresh')}
          <select
            className={css.autoRefreshSelect}
            value={String(interval)}
            onChange={(event) => { setAutoSeconds(Number(event.target.value)) }}
          >
            {AUTO_REFRESH_CHOICES.map(choice => (
              <option key={choice} value={String(choice)}>
                {choice === 0 ? tt('panel.autoRefresh.off') : choice >= 60 ? `${choice / 60}m` : `${choice}s`}
              </option>
            ))}
          </select>
        </label>
        <span className={css.updated} title={state.cachedAt ?? undefined}>
          {updated === '' ? '' : tt('panel.updated', { ago: updated })}
        </span>
        <button
          type="button"
          className={css.refreshButton}
          disabled={state.loading || !configured}
          onClick={() => { void load(true) }}
        >
          {state.loading ? <span className={css.spinner} aria-hidden="true" /> : null}
          {state.loading ? tt('panel.refreshing') : tt('panel.refresh')}
        </button>
      </div>

      <div className={css.panelContent}>
        {status !== null && !configured
          ? (
            <div className={css.placeholder}>
              <p className={css.placeholderTitle}>{tt('panel.notConfigured')}</p>
              <p className={css.placeholderHint}>{tt('panel.notConfiguredHint')}</p>
            </div>
          )
          : null}

        {configured && state.error !== null
          ? (
            <div className={css.loadError} role="alert">
              <span>{tt('panel.loadError')}：{state.error}</span>
              <button type="button" className={css.retryButton} onClick={() => { void load(true) }}>
                {tt('panel.retry')}
              </button>
            </div>
          )
          : null}

        {configured && state.error === null && !state.loading && visible.length === 0
          ? <div className={css.placeholder}><p className={css.placeholderHint}>{tt('panel.empty')}</p></div>
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
              {visible.map(item => (
                <ProviderCard
                  key={item.provider_id}
                  item={item}
                  now={now}
                  dragging={dragId === item.provider_id}
                  dropBefore={dropSide !== null && dropSide.id === item.provider_id && dropSide.before && dragId !== item.provider_id}
                  dropAfter={dropSide !== null && dropSide.id === item.provider_id && !dropSide.before && dragId !== item.provider_id}
                  {...cardDragHandlers(item)}
                />
              ))}
            </div>
          )
          : null}

        {configured && state.loading && state.items.length === 0 && state.error === null
          ? <div className={css.placeholder}><p className={css.placeholderHint}>{tt('panel.refreshing')}</p></div>
          : null}
      </div>
    </div>
  )
}
