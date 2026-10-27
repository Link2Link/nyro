/**
 * Staged editor for the `instances[]` gateway list of the nyro-usage
 * settings namespace, saved through the card's single mutation (a
 * {@link FormExtension} of the card's CardForm).
 *
 * Tokens are secrets the Host redacts from every read-back, so the list is
 * never written back wholesale (that would wipe them). A save is a sequence
 * of path-addressed edits instead:
 * 1. field sets/unsets on kept rows, at their stored indices;
 * 2. removed rows unset in descending index order (each removal shifts only
 *    the rows after it, which are already handled);
 * 3. new rows appended at the end, each written whole with its token.
 * A token field left empty keeps the stored token.
 *
 * The edits address the USER layer's array. When the list only exists in
 * the composition base (cordis.yml) the editor is read-only: an override
 * built from the redacted base would lose the base tokens.
 */

import type { SettingsScope, SettingsScopeSnapshot } from '@deepseek-ai/dsh-client-runtime/client'
import type { FormExtension, MutateOp } from './settings-form.ts'

/** One `instances[]` row as the settings read-back carries it (token redacted). */
export interface StoredInstanceRow {
  name: string
  baseUrl: string
  enabled: boolean
}

/** The namespace fields the editor reads. */
export interface InstancesSettings {
  instances?: unknown
}

/**
 * Where the list can be edited:
 * - `editable` — the user layer owns the list (or no list exists yet);
 * - `inherited` — the list comes from the composition base only; read-only;
 * - `unavailable` — the namespace is not loaded / not served.
 */
export type InstancesMode = 'editable' | 'inherited' | 'unavailable'

/** One row as the card renders it. */
export interface InstanceRowState {
  /** Stable React key: `s:<storedIndex>` or `n:<counter>`. */
  key: string
  /** Index in the stored list; undefined for a row added in this edit. */
  storedIndex: number | undefined
  name: string
  baseUrl: string
  enabled: boolean
  /** A newly typed token (never the stored one, which is redacted). */
  token: string
  /** Base URL is not an http(s) address (only reported for edited/new rows). */
  badUrl: boolean
  /** A new row without a token. */
  tokenMissing: boolean
}

/** What the card renders for the list. */
export interface InstancesState {
  mode: InstancesMode
  rows: InstanceRowState[]
}

/** Editable fields of a row. */
export type InstanceField = 'name' | 'baseUrl' | 'token' | 'enabled'

/** Staged edits of one stored row (only the fields that differ). */
interface RowEdit {
  name?: string
  baseUrl?: string
  enabled?: boolean
  token?: string
}

/** A row added in this edit. */
interface NewRow {
  key: string
  name: string
  baseUrl: string
  enabled: boolean
  token: string
}

/** Normalize one read-back row; anything that is not an object reads as an empty row. */
function normalizeRow(raw: unknown): StoredInstanceRow {
  const row = typeof raw === 'object' && raw !== null ? raw as Record<string, unknown> : {}
  return {
    name: typeof row.name === 'string' ? row.name : '',
    baseUrl: typeof row.baseUrl === 'string' ? row.baseUrl : '',
    enabled: row.enabled !== false,
  }
}

/**
 * Read the stored list and its edit mode from a scope snapshot.
 * @param snapshot - the namespace snapshot.
 * @returns the mode and the stored rows (user layer when it owns the list).
 */
export function storedInstances(snapshot: SettingsScopeSnapshot<InstancesSettings>): { mode: InstancesMode; rows: StoredInstanceRow[] } {
  if (snapshot.status !== 'ready') return { mode: 'unavailable', rows: [] }
  const user = typeof snapshot.user === 'object' && snapshot.user !== null ? snapshot.user as InstancesSettings : undefined
  if (Array.isArray(user?.instances)) return { mode: 'editable', rows: user.instances.map(normalizeRow) }
  const effective = snapshot.value?.instances
  if (Array.isArray(effective) && effective.length > 0) return { mode: 'inherited', rows: effective.map(normalizeRow) }
  return { mode: 'editable', rows: [] }
}

/**
 * Whether a base URL draft is an http(s) address. Mirrors the host's
 * normalization (whitespace trimmed); an empty draft is invalid.
 * @param text - the draft.
 * @returns validity.
 */
export function isHttpUrl(text: string): boolean {
  const trimmed = text.trim()
  if (trimmed === '') return false
  try {
    const url = new URL(trimmed)
    return url.protocol === 'http:' || url.protocol === 'https:'
  } catch {
    return false
  }
}

/** Stages edits of the gateway list and turns them into path-addressed writes. */
export class InstancesForm implements FormExtension {
  private readonly scope: SettingsScope<InstancesSettings>
  private readonly edits = new Map<number, RowEdit>()
  private readonly removed = new Set<number>()
  private added: NewRow[] = []
  private counter = 0
  private readonly listeners = new Set<() => void>()
  private fingerprint: string

  /** @param scope - the bound settings scope for the `nyro-usage` namespace. */
  constructor(scope: SettingsScope<InstancesSettings>) {
    this.scope = scope
    this.fingerprint = JSON.stringify(this.stored())
    scope.subscribe(() => {
      const next = JSON.stringify(this.stored())
      if (next === this.fingerprint) return
      this.fingerprint = next
      // The stored list changed under the drafts (a save landed, or another
      // editor wrote): staged indices no longer address the same rows.
      this.clear()
      this.notify()
    })
  }

  /** The current stored list and mode. */
  private stored(): { mode: InstancesMode; rows: StoredInstanceRow[] } {
    return storedInstances(this.scope.getSnapshot())
  }

  /** The list as the card renders it: kept stored rows with their edits, then new rows. */
  state(): InstancesState {
    const { mode, rows: stored } = this.stored()
    const rows: InstanceRowState[] = []
    stored.forEach((row, index) => {
      if (this.removed.has(index)) return
      const edit = this.edits.get(index)
      const baseUrl = edit?.baseUrl ?? row.baseUrl
      rows.push({
        key: `s:${index}`,
        storedIndex: index,
        name: edit?.name ?? row.name,
        baseUrl,
        enabled: edit?.enabled ?? row.enabled,
        token: edit?.token ?? '',
        // Untouched stored rows never block a save of unrelated edits.
        badUrl: edit !== undefined && !isHttpUrl(baseUrl),
        tokenMissing: false,
      })
    })
    for (const row of this.added) {
      rows.push({
        key: row.key,
        storedIndex: undefined,
        name: row.name,
        baseUrl: row.baseUrl,
        enabled: row.enabled,
        token: row.token,
        badUrl: !isHttpUrl(row.baseUrl),
        tokenMissing: row.token.trim() === '',
      })
    }
    return { mode, rows }
  }

  dirty(): boolean {
    if (this.stored().mode !== 'editable') return false
    return this.edits.size > 0 || this.removed.size > 0 || this.added.length > 0
  }

  invalid(): boolean {
    return this.state().rows.some(row => row.badUrl || row.tokenMissing)
  }

  ops(): MutateOp[] {
    const { mode, rows: stored } = this.stored()
    if (mode !== 'editable') return []
    const ops: MutateOp[] = []
    // 1. Field edits on kept rows, at their stored indices (before any removal shifts them).
    stored.forEach((row, index) => {
      const edit = this.edits.get(index)
      if (edit === undefined || this.removed.has(index)) return
      const path = (field: string): string[] => ['instances', String(index), field]
      for (const field of ['name', 'baseUrl'] as const) {
        const value = edit[field]
        if (value === undefined) continue
        const trimmed = value.trim()
        ops.push(trimmed === '' ? { op: 'unset', path: path(field) } : { op: 'set', path: path(field), value: trimmed })
      }
      if (edit.enabled !== undefined) ops.push({ op: 'set', path: path('enabled'), value: edit.enabled })
      const token = edit.token?.trim() ?? ''
      if (token !== '') ops.push({ op: 'set', path: path('adminToken'), value: token })
    })
    // 2. Removals, highest index first.
    for (const index of [...this.removed].sort((a, b) => b - a)) {
      if (index < stored.length) ops.push({ op: 'unset', path: ['instances', String(index)] })
    }
    // 3. Appends after the kept rows.
    let next = stored.length - [...this.removed].filter(index => index < stored.length).length
    for (const row of this.added) {
      const value: Record<string, unknown> = { baseUrl: row.baseUrl.trim(), adminToken: row.token.trim() }
      const name = row.name.trim()
      if (name !== '') value.name = name
      if (!row.enabled) value.enabled = false
      ops.push({ op: 'set', path: ['instances', String(next)], value })
      next += 1
    }
    return ops
  }

  landed(): void {
    this.clear()
    this.fingerprint = JSON.stringify(this.stored())
    this.notify()
  }

  discard(): void {
    if (!this.hasDrafts()) return
    this.clear()
    this.notify()
  }

  subscribe(listener: () => void): () => void {
    this.listeners.add(listener)
    return () => { this.listeners.delete(listener) }
  }

  /**
   * Stage one field of a row.
   * @param key - the row key from {@link state}.
   * @param field - the field.
   * @param value - text for name/baseUrl/token, a boolean for enabled.
   */
  edit(key: string, field: InstanceField, value: string | boolean): void {
    if (this.stored().mode !== 'editable') return
    const added = this.added.find(row => row.key === key)
    if (added !== undefined) {
      if ((field === 'enabled') !== (typeof value === 'boolean')) return
      this.added = this.added.map(row => (row.key === key ? { ...row, [field]: value } : row))
      this.notify()
      return
    }
    const index = storedIndexOf(key)
    const stored = index === undefined ? undefined : this.stored().rows[index]
    if (index === undefined || stored === undefined || this.removed.has(index)) return
    const edit: RowEdit = { ...this.edits.get(index) }
    if (field === 'enabled') {
      if (typeof value !== 'boolean') return
      if (value === stored.enabled) delete edit.enabled
      else edit.enabled = value
    } else {
      if (typeof value !== 'string') return
      const unchanged = field === 'token' ? value === '' : value === stored[field]
      if (unchanged) delete edit[field]
      else edit[field] = value
    }
    if (Object.keys(edit).length === 0) this.edits.delete(index)
    else this.edits.set(index, edit)
    this.notify()
  }

  /** Stage a new, empty, enabled row at the end. */
  add(): void {
    if (this.stored().mode !== 'editable') return
    this.counter += 1
    this.added = [...this.added, { key: `n:${this.counter}`, name: '', baseUrl: '', enabled: true, token: '' }]
    this.notify()
  }

  /**
   * Stage a row's removal (a new row is simply dropped).
   * @param key - the row key from {@link state}.
   */
  remove(key: string): void {
    if (this.stored().mode !== 'editable') return
    if (this.added.some(row => row.key === key)) {
      this.added = this.added.filter(row => row.key !== key)
      this.notify()
      return
    }
    const index = storedIndexOf(key)
    if (index === undefined || index >= this.stored().rows.length) return
    this.removed.add(index)
    this.edits.delete(index)
    this.notify()
  }

  private hasDrafts(): boolean {
    return this.edits.size > 0 || this.removed.size > 0 || this.added.length > 0
  }

  private clear(): void {
    this.edits.clear()
    this.removed.clear()
    this.added = []
  }

  private notify(): void {
    for (const listener of this.listeners) listener()
  }
}

/** Parse a stored-row key (`s:<index>`). */
function storedIndexOf(key: string): number | undefined {
  const match = /^s:(\d+)$/.exec(key)
  return match === null ? undefined : Number(match[1])
}
