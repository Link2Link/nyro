/**
 * Sidebar entry injection.
 *
 * dsh's sidebar shell exposes no slot an external plugin can register into,
 * so — following the task-board / dsh-ssh precedent of DOM-level extension —
 * the entry row is injected between the shell's New Session button and the
 * workspace browser. The injection self-heals: a MutationObserver watches
 * the sidebar root and re-inserts the row whenever a React re-render
 * displaces it (re-insertion happens in the same frame, before paint).
 *
 * The row is plain DOM (no React tree) so it can never disturb the shell's
 * reconciliation; the panel view it toggles is a separate React root mounted
 * in the center column (see mount.tsx).
 */
import type { PanelController } from './controller.ts'

/** Stable data attribute identifying the injected entry row. */
export const ENTRY_SELECTOR = '[data-dsh-nyro-usage-entry]'

/** Sidebar family rows injected by sibling plugins (ordering block). */
const FAMILY_SELECTOR = '[data-dsh-taskboard-entry], [data-dsh-ssh-entry], [data-dsh-nyro-usage-entry]'

/** Inline icon (matches the shell's 16px nav-icon look): a gauge glyph. */
const ICON = '<svg viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M3 11.5a5 5 0 1 1 10 0"/><path d="M8 8.4 10.6 6"/><path d="M8 11.5h.01"/><path d="M1.5 11.5h1M13.5 11.5h1M8 3.5v-1"/></svg>'

/** Entry-row styles (single source: the panel stylesheet injects them). */
import css from './panel/panel.module.css'

/** Find the sidebar shell root element, or undefined while not yet mounted. */
function sidebarRoot(): HTMLElement | undefined {
  const column = document.querySelector<HTMLElement>('[data-pane="sidebar"], [class*="sidebarCol"]')
  if (column === null) return undefined
  // Current shells wrap the sidebar UI: column > wrapper > root(logoRow owner).
  const logoOwner = column.querySelector<HTMLElement>('[class*="logoRow"]')?.parentElement
  return logoOwner ?? (column.firstElementChild as HTMLElement | undefined)
}

/** The New Session button: nested in the logo row on current shells, a direct child on legacy shells. */
function newSessionButton(root: HTMLElement): HTMLButtonElement | undefined {
  const nested = root.querySelector<HTMLButtonElement>('button[class*="newSession"]')
  if (nested !== null) return nested
  for (const child of root.children) {
    if (child.tagName === 'BUTTON') return child as HTMLButtonElement
  }
  return undefined
}

/** Build the entry row (a detached button; insert once the shell is up). */
function createEntry(controller: PanelController, label: string, tooltip: string): HTMLButtonElement {
  const entry = document.createElement('button')
  entry.type = 'button'
  entry.dataset.dshNyroUsageEntry = ''
  entry.className = css.entry
  entry.setAttribute('aria-label', label)
  entry.setAttribute('title', tooltip)
  entry.innerHTML = '<span class="' + css.entryIcon + '">' + ICON + '</span><span class="' + css.entryLabel + '">' + label + '</span>'
  entry.addEventListener('click', () => { controller.toggle() })
  return entry
}

/** Re-insert the entry after the family block (before the browser region). */
function placeEntry(root: HTMLElement, entry: HTMLButtonElement): boolean {
  const button = newSessionButton(root)
  if (button === undefined) return false
  if (entry.parentElement !== root) {
    // Position relative to the family block (entries injected by sibling
    // plugins), never relative to transient logoRow geometry: every family
    // plugin that self-heals during a re-render then lands in the same
    // relative order. No append-to-end fallback: appending would randomly
    // reorder the block after a shell re-render.
    const row = button.closest('[class*="logoRow"]')
    const base = (row !== null && row.parentElement === root) ? row : button
    const family = Array.from(root.children).filter(
      (el): el is HTMLElement => el instanceof HTMLElement && el.matches(FAMILY_SELECTOR),
    )
    const anchor = family.length > 0 ? family[family.length - 1].nextElementSibling : base.nextElementSibling
    root.insertBefore(entry, anchor)
  }
  return true
}

/**
 * Mount the sidebar entry, waiting for the shell to render and self-healing
 * on later React re-renders.
 * @param controller - the panel controller the entry toggles.
 * @param label - visible entry label.
 * @param tooltip - entry tooltip.
 * @returns disposer removing the entry and its observers.
 */
export function mountSidebarEntry(controller: PanelController, label: string, tooltip: string): () => void {
  const entry = createEntry(controller, label, tooltip)
  let root: HTMLElement | undefined
  let placed = false

  const tryPlace = (): void => {
    if (root !== undefined && !root.isConnected) {
      rootObserver.disconnect()
      root = undefined
      placed = false
    }
    if (placed) {
      if (document.body.contains(entry)) return
      rootObserver.disconnect()
      root = undefined
      placed = false
    }
    root ??= sidebarRoot()
    if (root === undefined) return
    placed = placeEntry(root, entry)
    if (placed) {
      rootObserver.observe(root, { childList: true, subtree: true })
    }
  }

  // Body-level watcher retained as the "whole rebuild" fallback.
  const waitObserver = new MutationObserver(() => { tryPlace() })
  waitObserver.observe(document.body, { childList: true, subtree: true })

  // Self-heal: if a React re-render displaces the row, re-insert it in the
  // same frame (microtask before paint -> no visible flicker).
  const rootObserver = new MutationObserver(() => {
    if (root === undefined || !root.isConnected) {
      placed = false
      tryPlace()
      return
    }
    if (!root.contains(entry)) {
      placed = placeEntry(root, entry)
    }
  })

  // Reflect the panel's open state on the row (active highlight). Assigning
  // undefined to dataset.active materializes data-active="undefined"; delete
  // the attribute instead.
  const syncActive = () => {
    if (controller.getSnapshot().panelOpen) entry.dataset.active = 'true'
    else delete entry.dataset.active
  }
  const unsubscribe = controller.subscribe(syncActive)
  syncActive()

  tryPlace()

  return () => {
    waitObserver.disconnect()
    rootObserver.disconnect()
    unsubscribe()
    entry.remove()
  }
}
