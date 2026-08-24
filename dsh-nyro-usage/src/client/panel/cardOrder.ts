/**
 * Provider-card ordering: drag-to-reorder positions persisted per browser
 * in localStorage (task-board `dsh.<plugin>.<feature>.vN` precedent).
 * Provider ids unknown to the saved order — added in nyro after the last
 * drag — keep the gateway's natural order, appended after every explicitly
 * ordered card, so new providers never shuffle existing positions.
 */
import { useCallback, useState } from 'react'

/** localStorage slot holding the saved provider order (array of ids). */
const ORDER_KEY = 'dsh.nyroUsage.cardOrder.v1'

/** Read the saved order; any corruption falls back to "no custom order". */
function loadOrder(): string[] {
  try {
    const raw = window.localStorage.getItem(ORDER_KEY)
    if (raw === null) return []
    const parsed: unknown = JSON.parse(raw)
    if (!Array.isArray(parsed)) return []
    return [...new Set(parsed.filter((id): id is string => typeof id === 'string'))]
  } catch {
    return []
  }
}

/** Persist (or clear) the order; storage failures just don't persist. */
function saveOrder(ids: string[]): void {
  try {
    if (ids.length === 0) window.localStorage.removeItem(ORDER_KEY)
    else window.localStorage.setItem(ORDER_KEY, JSON.stringify(ids))
  } catch {
    /* private mode / quota — the ordering stays session-only */
  }
}

/**
 * Sort items by the saved id order. `Array#sort` is stable, so ids missing
 * from `order` keep their incoming relative order after the ordered ones.
 */
export function bySavedOrder<T>(items: T[], order: readonly string[], idOf: (item: T) => string): T[] {
  if (order.length === 0) return items
  const rank = new Map(order.map((id, index) => [id, index]))
  return [...items].sort((a, b) => {
    const va = rank.get(idOf(a)) ?? order.length
    const vb = rank.get(idOf(b)) ?? order.length
    return va - vb
  })
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

/** State hook backing the draggable card order. */
export function useCardOrder(): CardOrder {
  const [order, setOrder] = useState<string[]>(loadOrder)
  const commit = useCallback((next: string[]): void => {
    setOrder(next)
    saveOrder(next)
  }, [])
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
