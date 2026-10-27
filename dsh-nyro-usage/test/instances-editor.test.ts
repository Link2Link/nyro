import assert from 'node:assert/strict'
import { describe, it } from 'node:test'
import type { SettingsScope, SettingsScopeSnapshot } from '@deepseek-ai/dsh-client-runtime/client'
import { InstancesForm, isHttpUrl, type InstancesSettings } from '../src/client/instances-editor.ts'
import type { MutateOp } from '../src/client/settings-form.ts'

/** A settings scope over fixed layers; `push` replaces the snapshot. */
function fakeScope(user: unknown, value: unknown = user, status: 'ready' | 'loading' = 'ready') {
  let snapshot: SettingsScopeSnapshot<InstancesSettings> = {
    status,
    value: value as InstancesSettings,
    base: {},
    user,
    revision: 1,
    writable: true,
    mode: 'host',
  }
  const listeners = new Set<() => void>()
  const scope: SettingsScope<InstancesSettings> = {
    getSnapshot: () => snapshot,
    subscribe: (listener) => { listeners.add(listener); return () => { listeners.delete(listener) } },
    set: async () => {},
    unset: async () => {},
  }
  return {
    scope,
    push(next: Partial<SettingsScopeSnapshot<InstancesSettings>>) {
      snapshot = { ...snapshot, ...next }
      for (const listener of listeners) listener()
    },
  }
}

/** Apply path ops to a plain section the way the dsh settings service does (array index unset splices). */
function apply(section: Record<string, unknown>, ops: MutateOp[]): Record<string, unknown> {
  const edit = (input: unknown, path: string[], op: MutateOp): unknown => {
    const [head, ...rest] = path
    if (head === undefined) return op.op === 'set' ? op.value : undefined
    if (Array.isArray(input)) {
      const index = Number(head)
      assert.ok(Number.isInteger(index) && index <= input.length && !(index === input.length && (rest.length > 0 || op.op === 'unset')), `index ${head} in range`)
      const result = [...input]
      if (rest.length === 0 && op.op === 'unset') result.splice(index, 1)
      else result[index] = edit(input[index], rest, op)
      return result
    }
    const result: Record<string, unknown> = { ...(input as Record<string, unknown> ?? {}) }
    const child = edit(result[head], rest, op)
    if (child === undefined) delete result[head]
    else result[head] = child
    return result
  }
  return ops.reduce((value, op) => edit(value, op.path, op) as Record<string, unknown>, section)
}

const STORED = {
  instances: [
    { name: 'A', baseUrl: 'http://a', adminToken: 'ta', enabled: true },
    { name: 'B', baseUrl: 'http://b', adminToken: 'tb', enabled: true },
    { name: 'C', baseUrl: 'http://c', adminToken: 'tc', enabled: false },
  ],
}

/** The read-back the browser sees: tokens redacted. */
function redacted(section: { instances: Array<Record<string, unknown>> }) {
  return { instances: section.instances.map(({ adminToken: _token, ...rest }) => rest) }
}

describe('InstancesForm', () => {
  it('keeps tokens when editing, removing and adding rows in one save', () => {
    const { scope } = fakeScope(redacted(STORED))
    const form = new InstancesForm(scope)
    assert.deepEqual(form.state().rows.map(row => row.key), ['s:0', 's:1', 's:2'])
    form.edit('s:2', 'name', 'C2')
    form.edit('s:2', 'enabled', true)
    form.remove('s:0')
    form.add()
    const key = form.state().rows.at(-1)!.key
    form.edit(key, 'name', ' D ')
    form.edit(key, 'baseUrl', 'http://d/api/v1')
    form.edit(key, 'token', 'td')
    assert.equal(form.dirty(), true)
    assert.equal(form.invalid(), false)
    const result = apply(structuredClone(STORED), form.ops())
    assert.deepEqual(result.instances, [
      { name: 'B', baseUrl: 'http://b', adminToken: 'tb', enabled: true },
      { name: 'C2', baseUrl: 'http://c', adminToken: 'tc', enabled: true },
      { name: 'D', baseUrl: 'http://d/api/v1', adminToken: 'td' },
    ])
  })

  it('removes several rows highest index first', () => {
    const { scope } = fakeScope(redacted(STORED))
    const form = new InstancesForm(scope)
    form.remove('s:0')
    form.remove('s:2')
    assert.deepEqual(form.ops(), [
      { op: 'unset', path: ['instances', '2'] },
      { op: 'unset', path: ['instances', '0'] },
    ])
    assert.deepEqual(apply(structuredClone(STORED), form.ops()).instances, [STORED.instances[1]])
  })

  it('sets a replacement token and unsets a cleared name', () => {
    const { scope } = fakeScope(redacted(STORED))
    const form = new InstancesForm(scope)
    form.edit('s:1', 'token', 'tb2')
    form.edit('s:1', 'name', '')
    assert.deepEqual(form.ops(), [
      { op: 'unset', path: ['instances', '1', 'name'] },
      { op: 'set', path: ['instances', '1', 'adminToken'], value: 'tb2' },
    ])
  })

  it('drops an edit that returns to the stored value', () => {
    const { scope } = fakeScope(redacted(STORED))
    const form = new InstancesForm(scope)
    form.edit('s:0', 'name', 'A!')
    form.edit('s:0', 'name', 'A')
    form.edit('s:0', 'token', 'x')
    form.edit('s:0', 'token', '')
    assert.equal(form.dirty(), false)
    assert.deepEqual(form.ops(), [])
  })

  it('appends into an absent user list from index 0', () => {
    const { scope } = fakeScope({})
    const form = new InstancesForm(scope)
    assert.equal(form.state().mode, 'editable')
    form.add()
    const key = form.state().rows[0].key
    form.edit(key, 'baseUrl', 'https://x.example')
    form.edit(key, 'token', 't')
    form.edit(key, 'enabled', false)
    assert.deepEqual(form.ops(), [
      { op: 'set', path: ['instances', '0'], value: { baseUrl: 'https://x.example', adminToken: 't', enabled: false } },
    ])
  })

  it('validates new rows and edited rows only', () => {
    const { scope } = fakeScope({ instances: [{ name: 'legacy', baseUrl: 'not a url' }] })
    const form = new InstancesForm(scope)
    assert.equal(form.invalid(), false, 'an untouched stored row does not block saves')
    form.edit('s:0', 'name', 'renamed')
    assert.equal(form.invalid(), true)
    form.edit('s:0', 'baseUrl', 'http://ok')
    assert.equal(form.invalid(), false)
    form.add()
    const row = form.state().rows[1]
    assert.equal(row.badUrl, true)
    assert.equal(row.tokenMissing, true)
    assert.equal(form.invalid(), true)
    form.remove(row.key)
    assert.equal(form.invalid(), false)
  })

  it('is read-only while the list only exists in the composition base', () => {
    const { scope } = fakeScope({ refreshSeconds: 60 }, redacted(STORED))
    const form = new InstancesForm(scope)
    assert.equal(form.state().mode, 'inherited')
    assert.equal(form.state().rows.length, 3)
    form.add()
    form.remove('s:0')
    assert.equal(form.dirty(), false)
    assert.deepEqual(form.ops(), [])
  })

  it('drops stale drafts when the stored list changes', () => {
    const fake = fakeScope(redacted(STORED))
    const form = new InstancesForm(fake.scope)
    let notified = 0
    form.subscribe(() => { notified += 1 })
    form.remove('s:1')
    fake.push({ user: { instances: [{ name: 'Z', baseUrl: 'http://z' }] } })
    assert.equal(form.dirty(), false)
    assert.deepEqual(form.state().rows.map(row => row.name), ['Z'])
    assert.equal(notified, 2)
  })
})

describe('isHttpUrl', () => {
  it('accepts http(s) only', () => {
    assert.equal(isHttpUrl(' http://192.168.31.2:19531 '), true)
    assert.equal(isHttpUrl('https://x'), true)
    assert.equal(isHttpUrl('file:///etc/passwd'), false)
    assert.equal(isHttpUrl(''), false)
    assert.equal(isHttpUrl('192.168.1.1:80'), false)
  })
})
