import assert from 'node:assert/strict'
import { describe, it } from 'node:test'
import { Config, resolveRouteConfig } from '../src/index.ts'
import { PRIMARY_INSTANCE_ID, resolveInstances, slugifyInstanceName, tokenFlags } from '../src/instances.ts'

describe('resolveInstances', () => {
  it('keeps the legacy single-gateway config as the primary instance', () => {
    const instances = resolveInstances({ baseUrl: 'http://192.168.31.2:19531/api/v1/', adminToken: ' t0 ' })
    assert.deepEqual(instances, [
      { id: PRIMARY_INSTANCE_ID, name: '192.168.31.2:19531', baseUrl: 'http://192.168.31.2:19531', adminToken: 't0' },
    ])
  })

  it('lists the primary first, then enabled instances[] rows with a base URL', () => {
    const instances = resolveInstances({
      name: 'Home',
      baseUrl: 'http://a:1',
      adminToken: 'ta',
      instances: [
        { name: 'Office', baseUrl: 'http://b:2', adminToken: 'tb' },
        { name: 'Off', baseUrl: 'http://c:3', adminToken: 'tc', enabled: false },
        { name: 'No URL', baseUrl: '  ', adminToken: 'td' },
        { baseUrl: 'https://e.example:4', adminToken: '' },
      ],
    })
    assert.deepEqual(instances.map(i => [i.id, i.name, i.baseUrl, i.adminToken]), [
      ['default', 'Home', 'http://a:1', 'ta'],
      ['office', 'Office', 'http://b:2', 'tb'],
      ['nyro-4', 'e.example:4', 'https://e.example:4', ''],
    ])
  })

  it('works without a primary gateway', () => {
    const instances = resolveInstances({ instances: [{ name: 'Only', baseUrl: 'http://x', adminToken: 't' }] })
    assert.deepEqual(instances.map(i => i.id), ['only'])
  })

  it('makes ids unique and never reuses the primary id', () => {
    const instances = resolveInstances({
      baseUrl: 'http://a',
      instances: [
        { name: 'Default', baseUrl: 'http://b' },
        { name: 'Lab', baseUrl: 'http://c' },
        { name: 'lab', baseUrl: 'http://d' },
      ],
    })
    assert.deepEqual(instances.map(i => i.id), ['default', 'default-2', 'lab', 'lab-2'])
  })

  it('tolerates malformed instances values', () => {
    assert.deepEqual(resolveInstances({ instances: 'nope' }), [])
    assert.deepEqual(resolveInstances({ instances: [null, 3, { baseUrl: 'http://ok' }] }).map(i => i.id), ['nyro-3'])
  })
})

describe('slugifyInstanceName', () => {
  it('keeps unicode letters and digits', () => {
    assert.equal(slugifyInstanceName('  美国 VPS #2 '), '美国-vps-2')
    assert.equal(slugifyInstanceName('---'), '')
  })
})

describe('tokenFlags', () => {
  it('reports token presence by config position, including skipped rows', () => {
    assert.deepEqual(
      tokenFlags({ adminToken: 'x', instances: [{ adminToken: 'y', enabled: false }, { baseUrl: 'http://b' }, 'bad'] }),
      { primary: true, rows: [true, false, false] },
    )
  })
})

describe('Config schema', () => {
  it('resolves every field to a volatile reference the routes read through', () => {
    const config = Config({
      baseUrl: 'http://a',
      adminToken: 'ta',
      instances: [{ name: 'B', baseUrl: 'http://b', adminToken: 'tb' }],
    })
    const baseUrl = config.baseUrl as unknown as { get(): unknown }
    assert.equal(typeof baseUrl.get, 'function', 'baseUrl is a volatile reference')
    const route = resolveRouteConfig(config)
    assert.equal(route.enabled, true)
    assert.equal(route.cacheSeconds, 30)
    assert.deepEqual(route.instances.map(i => i.id), ['default', 'b'])
    assert.deepEqual(route.tokens, { primary: true, rows: [true] })
  })

  it('keeps an old config without instances valid', () => {
    const route = resolveRouteConfig(Config({ baseUrl: 'http://a', adminToken: 't', refreshSeconds: 60 }))
    assert.equal(route.refreshSeconds, 60)
    assert.deepEqual(route.instances.map(i => i.id), ['default'])
    assert.deepEqual(route.tokens.rows, [])
  })

  it('declares the instance token as a secret so settings redact it', () => {
    const json = JSON.stringify(Config.toJSON())
    assert.equal((json.match(/"role":"secret"/g) ?? []).length, 2)
  })
})
