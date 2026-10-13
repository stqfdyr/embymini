// 列表快照（src/lib/snap.ts）：读写、容量淘汰（删最久没更新的）、单份过大不存、localStorage 不可用时不抛错。
// 跑法：npm test（Node 原生去掉类型后执行，不需要额外依赖）
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
const { readSnap, writeSnap } = await import('../src/lib/snap.ts')

beforeEach(() => {
  store.m.clear()
  store.quota = Infinity
})

test('写了能读回来，没写过是 undefined', () => {
  writeSnap('a', { x: 1 })
  assert.deepEqual(readSnap('a'), { x: 1 })
  assert.equal(readSnap('nope'), undefined)
})

test('超过总量上限时删最久没更新的，刚写的留着', async () => {
  const big = 'x'.repeat(700_000) // 上限 300 万字符：4 份放得下，第 5 份要挤掉最早的
  for (const k of ['k1', 'k2', 'k3', 'k4']) {
    writeSnap(k, big)
    await new Promise((r) => setTimeout(r, 2)) // 时间戳要分得出先后
  }
  writeSnap('k2', big) // k2 刚更新过，不该先被删
  await new Promise((r) => setTimeout(r, 2))
  writeSnap('k5', big)
  assert.equal(readSnap('k1'), undefined, '最久没更新的被删')
  for (const k of ['k2', 'k3', 'k4', 'k5']) assert.equal(readSnap(k), big, k)
  const index = JSON.parse(store.getItem('snap-index')!)
  assert.deepEqual(Object.keys(index).sort(), ['k2', 'k3', 'k4', 'k5'])
})

test('单份超过上限的 1/4 不存', () => {
  writeSnap('huge', 'x'.repeat(800_000))
  assert.equal(readSnap('huge'), undefined)
})

test('存不下、读坏数据都不抛错', () => {
  store.quota = 10
  assert.doesNotThrow(() => writeSnap('a', { x: 1 }))
  store.quota = Infinity
  store.m.set('snap:bad', '{not json')
  assert.equal(readSnap('bad'), undefined)
})
