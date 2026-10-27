/**
 * The nyro usage panel: a header (gateway count, overall summary,
 * auto-refresh selector, refresh-all) over one section per configured nyro
 * gateway. Loads through the same-origin host proxy, one request per
 * gateway, so a slow or failing gateway never holds back the others.
 * State machine per panel: status loading → disabled | unconfigured |
 * sections; per section: loading → data | error.
 */
import { useCallback, useEffect, useMemo, useRef, useState, useSyncExternalStore } from 'react'
import type { NyroInstanceStatus, NyroPanelApi, NyroStatusResponse } from '../api.ts'
import type { PanelController } from '../controller.ts'
import { errorMessage, tt } from '../tt.ts'
import { countUsage, InstanceSection, type InstanceSectionState } from './InstanceSection.tsx'
import css from './panel.module.css'

/** Auto-refresh choices offered next to the configured default. */
const AUTO_REFRESH_CHOICES = [0, 60, 300, 600] as const

/** Props the panel needs. */
export interface NyroUsagePanelProps {
  controller: PanelController
  api: NyroPanelApi
}

/**
 * Section identity: a gateway whose URL or token presence changed is a new
 * section with fresh state, so it never shows the previous gateway's data.
 */
function sectionKey(instance: NyroInstanceStatus): string {
  return `${instance.id}\u0000${instance.baseUrl}\u0000${instance.hasToken ? '1' : '0'}`
}

/** Section state before its first answer. */
const INITIAL_SECTION: InstanceSectionState = { loading: true, usage: undefined, error: null }

/** Render the nyro provider usage panel. */
export function NyroUsagePanel(props: NyroUsagePanelProps) {
  const { api } = props
  const snapshot = useSyncExternalStore(props.controller.subscribe, props.controller.getSnapshot)
  const [status, setStatus] = useState<NyroStatusResponse | null>(null)
  const [statusError, setStatusError] = useState<string | null>(null)
  const [sections, setSections] = useState<Record<string, InstanceSectionState>>({})
  const [autoSeconds, setAutoSeconds] = useState<number | null>(null)
  // Re-evaluates the steady-pace markers and countdowns on a 30s tick.
  const [now, setNow] = useState(() => Date.now())
  const inFlight = useRef(new Set<string>())
  const loadSeq = useRef(new Map<string, number>())

  useEffect(() => {
    const timer = window.setInterval(() => { setNow(Date.now()) }, 30_000)
    return () => { window.clearInterval(timer) }
  }, [])

  const refreshStatus = useCallback(async (): Promise<void> => {
    try {
      setStatus(await api.status())
      setStatusError(null)
    } catch (error) {
      setStatusError(errorMessage(error))
    }
  }, [api])

  /** Load one gateway; a request already in flight for it is not repeated. */
  const loadOne = useCallback(async (instance: NyroInstanceStatus, refresh: boolean): Promise<void> => {
    const key = sectionKey(instance)
    if (inFlight.current.has(key)) return
    inFlight.current.add(key)
    const seq = (loadSeq.current.get(key) ?? 0) + 1
    loadSeq.current.set(key, seq)
    setSections(previous => ({ ...previous, [key]: { ...(previous[key] ?? INITIAL_SECTION), loading: true } }))
    try {
      const usage = await api.usage(instance.id, refresh)
      if (loadSeq.current.get(key) !== seq) return
      setSections(previous => ({ ...previous, [key]: { loading: false, usage, error: null } }))
    } catch (error) {
      if (loadSeq.current.get(key) !== seq) return
      const message = errorMessage(error)
      setSections(previous => ({ ...previous, [key]: { ...(previous[key] ?? INITIAL_SECTION), loading: false, error: message } }))
    } finally {
      inFlight.current.delete(key)
    }
  }, [api])

  const configured = status?.configured ?? false
  // Keyed on the gateway list's identity rather than the status object (a
  // fresh one arrives on every panel open), so the effects and the
  // auto-refresh timer below only restart when the list actually changes.
  const listKey = configured ? (status?.instances ?? []).map(instance => `${sectionKey(instance)}\u0000${instance.name}`).join('\u0001') : ''
  const statusRef = useRef(status)
  statusRef.current = status
  const instances = useMemo(
    () => (statusRef.current?.configured === true ? statusRef.current.instances : []),
    [listKey],
  )

  const loadAll = useCallback((refresh: boolean): void => {
    for (const instance of instances) void loadOne(instance, refresh)
  }, [instances, loadOne])

  // Panel becomes visible (or mounts): refresh the status immediately.
  // Re-checks whenever the panel reopens so a settings change made in
  // between (gateways added/removed) is picked up.
  useEffect(() => {
    if (!snapshot.panelOpen) return
    void refreshStatus()
  }, [snapshot.panelOpen, refreshStatus])

  // A new gateway list loads every section (the host cache keeps repeats
  // cheap) and drops the state of gateways that are gone.
  useEffect(() => {
    const live = new Set(instances.map(sectionKey))
    setSections((previous) => {
      const stale = Object.keys(previous).filter(key => !live.has(key))
      if (stale.length === 0) return previous
      const next = { ...previous }
      for (const key of stale) delete next[key]
      return next
    })
    if (snapshot.panelOpen) loadAll(false)
  }, [instances, snapshot.panelOpen, loadAll])

  // Auto-refresh while the panel is open and configured.
  const interval = autoSeconds ?? status?.refreshSeconds ?? 300
  useEffect(() => {
    if (!snapshot.panelOpen || !configured || interval <= 0) return
    const timer = window.setInterval(() => { loadAll(false) }, interval * 1000)
    return () => { window.clearInterval(timer) }
  }, [snapshot.panelOpen, configured, interval, loadAll])

  const states = instances.map(instance => sections[sectionKey(instance)] ?? INITIAL_SECTION)
  const anyLoading = states.some(state => state.loading)
  const total = states.reduce((sum, state) => {
    const counts = countUsage(state.usage?.items ?? [])
    return { ok: sum.ok + counts.ok, exhausted: sum.exhausted + counts.exhausted }
  }, { ok: 0, exhausted: 0 })
  const anyItems = states.some(state => (state.usage?.items.length ?? 0) > 0)

  const refreshAll = (): void => {
    void refreshStatus()
    loadAll(true)
  }

  return (
    <div className={css.panel}>
      <div className={css.panelHeader}>
        <h1 className={css.panelTitle}>{tt('panel.title')}</h1>
        {instances.length > 1
          ? <span className={css.instanceCount}>{tt('panel.instances', { count: instances.length })}</span>
          : null}
        {anyItems
          ? <span className={css.summary}>{tt('panel.summary', total)}</span>
          : null}
        <span className={css.toolbarSpacer} />
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
        <button
          type="button"
          className={css.refreshButton}
          disabled={anyLoading || !configured}
          onClick={refreshAll}
        >
          {anyLoading ? <span className={css.spinner} aria-hidden="true" /> : null}
          {anyLoading ? tt('panel.refreshing') : instances.length > 1 ? tt('panel.refreshAll') : tt('panel.refresh')}
        </button>
      </div>

      <div className={css.panelContent}>
        {statusError !== null && status === null
          ? (
            <div className={css.loadError} role="alert">
              <span>{tt('panel.loadError')}：{statusError}</span>
              <button type="button" className={css.retryButton} onClick={() => { void refreshStatus() }}>
                {tt('panel.retry')}
              </button>
            </div>
          )
          : null}

        {status !== null && status.disabled
          ? (
            <div className={css.placeholder}>
              <p className={css.placeholderTitle}>{tt('panel.disabled')}</p>
              <p className={css.placeholderHint}>{tt('panel.disabledHint')}</p>
            </div>
          )
          : null}

        {status !== null && !status.disabled && !configured
          ? (
            <div className={css.placeholder}>
              <p className={css.placeholderTitle}>{tt('panel.notConfigured')}</p>
              <p className={css.placeholderHint}>{tt('panel.notConfiguredHint')}</p>
            </div>
          )
          : null}

        {instances.map((instance, index) => (
          <InstanceSection
            key={sectionKey(instance)}
            instance={instance}
            state={states[index]}
            now={now}
            onRefresh={() => { void loadOne(instance, true) }}
          />
        ))}
      </div>
    </div>
  )
}
