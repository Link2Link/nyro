/**
 * Provider-card ordering: drag-to-reorder positions persisted per browser
 * and per nyro gateway in localStorage (task-board
 * `dsh.<plugin>.<feature>.vN` precedent). Provider ids unknown to the saved
 * order — added in nyro after the last drag — keep the gateway's natural
 * order, appended after every explicitly ordered card, so new providers
 * never shuffle existing positions.
 */
import { useCallback, useEffect, useState } from 'react'

/** localStorage slot of the primary gateway (the pre-multi-gateway key, kept so saved orders survive). */
const ORDER_KEY = 'dsh.nyroUsage.cardOrder.v1'

/** Id of the primary gateway (mirrors the host's PRIMARY_INSTANCE_ID). */
const PRIMARY_INSTANCE_ID = 'default'

/** localStorage slot holding one gateway's saved provider order (array of ids). */
export function orderKey(instanceId: string): string {
  return instanceId === PRIMARY_INSTANCE_ID ? ORDER_KEY : `${ORDER_KEY}:${instanceId}`
}

/** Read the saved order; any corruption falls back to "no custom order". */
function loadOrder(key: string): string[] {
  try {
    const raw = window.localStorage.getItem(key)
    if (raw === null) return []
    const parsed: unknown = JSON.parse(raw)
    if (!Array.isArray(parsed)) return []
    return [...new Set(parsed.filter((id): id is string => typeof id === 'string'))]
  } catch {
    return []
  }
}

/** Persist (or clear) the order; storage failures just don't persist. */
function saveOrder(key: string, ids: string[]): void {
  try {
    if (ids.length === 0) window.localStorage.removeItem(key)
    else window.localStorage.setItem(key, JSON.stringify(ids))
  } catch {
    /* private mode / quota — the ordering stays session-only */
  }
}

/**
 * Sort items by the saved id order. `Array#sort` is stable, so ids missing
 * from `order` keep their incoming relative order after the ordered ones.
 * An unsaved item with an anchor (a forwarded upstream card) sits right after
 * its saved anchor instead of falling to the end.
 */
export function bySavedOrder<T>(
  items: T[],
  order: readonly string[],
  idOf: (item: T) => string,
  anchorOf?: (item: T) => string | undefined,
): T[] {
  if (order.length === 0) return items
  const rank = new Map(order.map((id, index) => [id, index]))
  const rankOf = (item: T): number => {
    const own = rank.get(idOf(item))
    if (own !== undefined) return own
    const anchor = anchorOf?.(item)
    const anchored = anchor === undefined ? undefined : rank.get(anchor)
    return anchored === undefined ? order.length : anchored + 0.5
  }
  return [...items].sort((a, b) => rankOf(a) - rankOf(b))
}

/** Move `dragId` to before/after `overId` inside `ids`; an unknown anchor
 * appends at the end (the whole displayed sequence is what gets saved). */
function moved(ids: readonly string[], dragId: string, overId: string, before: boolean): string[] {
  const next = ids.filter(id => id !== dragId)
  const index = next.indexOf(overId)
  next.splice(index === -1 ? next.length : before ? index : index + 1, 0, dragId)
  return next
}

/** Owner of the saved provider order. */
export interface CardOrder {
  /** Currently saved order (may contain stale ids; display goes through `bySavedOrder`). */
  order: string[]
  /** Apply a card-on-card drop: `dragId` placed before/after `overId`. */
  move: (displayed: readonly string[], dragId: string, overId: string, before: boolean) => void
  /** Apply a drop on the grid's empty area: `dragId` moves to the end. */
  append: (displayed: readonly string[], dragId: string) => void
  /** Drop the custom order; the gateway's natural order returns. */
  reset: () => void
}

/**
 * State hook backing one gateway's draggable card order.
 * @param instanceId - the gateway whose cards are ordered.
 * @returns the saved order and its drop actions.
 */
export function useCardOrder(instanceId: string): CardOrder {
  const key = orderKey(instanceId)
  const [order, setOrder] = useState<string[]>(() => loadOrder(key))
  // A section reused for another gateway reloads that gateway's order.
  useEffect(() => { setOrder(loadOrder(key)) }, [key])
  const commit = useCallback((next: string[]): void => {
    setOrder(next)
    saveOrder(key, next)
  }, [key])
  // The displayed sequence (already merged with the saved order) is the
  // seed, so never-dragged providers keep their visual neighbors and the
  // saved order always covers every visible card.
  const move = useCallback((displayed: readonly string[], dragId: string, overId: string, before: boolean): void => {
    commit(moved(displayed, dragId, overId, before))
  }, [commit])
  const append = useCallback((displayed: readonly string[], dragId: string): void => {
    commit([...displayed.filter(id => id !== dragId), dragId])
  }, [commit])
  const reset = useCallback((): void => { commit([]) }, [commit])
  return { order, move, append, reset }
}
