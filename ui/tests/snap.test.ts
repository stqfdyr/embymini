// List snapshots (src/lib/snap.ts): read / write, eviction of the least recently written, oversized entries skipped,
// no throw when localStorage fails, usage and clearing. Run with `npm test` (Node strips the types itself).
import assert from 'node:assert/strict'
import { beforeEach, test } from 'node:test'

class Store {
  m = new Map<string, string>()
  quota = Infinity
  getItem(k: string) { return this.m.get(k) ?? null }
  setItem(k: string, v: string) {
    const used = [...this.m.entries()].reduce((n, [kk, vv]) => n + (kk === k ? 0 : kk.length + vv.length), 0)
    if (used + k.length + v.length > this.quota) throw new Error('QuotaExceededError')
    this.m.set(k, v)
  }
  removeItem(k: string) { this.m.delete(k) }
}
const store = new Store()
;(globalThis as { localStorage?: Store }).localStorage = store
const { readSnap, snapClear, snapSize, writeSnap } = await import('../src/lib/snap.ts')

beforeEach(() => {
  store.m.clear()
  store.quota = Infinity
})

test('reads back what was written; undefined when never written', () => {
  writeSnap('a', { x: 1 })
  assert.deepEqual(readSnap('a'), { x: 1 })
  assert.equal(readSnap('nope'), undefined)
})

test('over the total limit, evicts the least recently written and keeps the new one', async () => {
  const big = 'x'.repeat(700_000) // limit is 3M characters: four fit, a fifth evicts the oldest
  for (const k of ['k1', 'k2', 'k3', 'k4']) {
    writeSnap(k, big)
    await new Promise((r) => setTimeout(r, 2)) // distinct timestamps
  }
  writeSnap('k2', big) // k2 was just rewritten, so it must not go first
  await new Promise((r) => setTimeout(r, 2))
  writeSnap('k5', big)
  assert.equal(readSnap('k1'), undefined, 'the least recently written is evicted')
  for (const k of ['k2', 'k3', 'k4', 'k5']) assert.equal(readSnap(k), big, k)
  const index = JSON.parse(store.getItem('snap-index')!)
  assert.deepEqual(Object.keys(index).sort(), ['k2', 'k3', 'k4', 'k5'])
})

test('an entry over a quarter of the limit is not stored', () => {
  writeSnap('huge', 'x'.repeat(800_000))
  assert.equal(readSnap('huge'), undefined)
})

test('a full store or corrupt data never throws', () => {
  store.quota = 10
  assert.doesNotThrow(() => writeSnap('a', { x: 1 }))
  store.quota = Infinity
  store.m.set('snap:bad', '{not json')
  assert.equal(readSnap('bad'), undefined)
})

test('usage counts UTF-16 bytes; nothing reads back after clearing', () => {
  writeSnap('a', 'xx') // the JSON is "xx", 4 characters
  writeSnap('b', { y: 2 })
  assert.equal(snapSize(), (4 + JSON.stringify({ y: 2 }).length) * 2)
  snapClear()
  assert.equal(snapSize(), 0)
  assert.equal(readSnap('a'), undefined)
  assert.equal(store.m.size, 0)
})
