/**
 * Panel open/close state owner: the sidebar entry toggles it and the
 * center-column view renders from it. External-store shaped so React binds
 * it with useSyncExternalStore without wrappers.
 *
 * The store methods are arrow-function class properties (per-instance bound
 * references): React's useSyncExternalStore and the sidebar/mount listeners
 * call them as bare functions, so a prototype method would lose `this`.
 */

/** Snapshot the view and the sidebar entry read. */
export interface PanelControllerSnapshot {
  panelOpen: boolean
}

export class PanelController {
  private open = false
  /** Cached snapshot: useSyncExternalStore requires Object.is-stable reads. */
  private snapshot: PanelControllerSnapshot = { panelOpen: false }
  private readonly listeners = new Set<() => void>()

  private set(next: boolean): void {
    if (this.open === next) return
    this.open = next
    this.snapshot = { panelOpen: next }
    for (const listener of this.listeners) listener()
  }

  toggle = (): void => {
    this.set(!this.open)
  }

  close = (): void => {
    this.set(false)
  }

  getSnapshot = (): PanelControllerSnapshot => this.snapshot

  subscribe = (listener: () => void): (() => void) => {
    this.listeners.add(listener)
    return () => { this.listeners.delete(listener) }
  }
}
