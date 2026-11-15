// Paging rules (src/lib/page.ts): honest servers end on the real total, servers that report the returned count as the
// total end only on a short page, an ignored StartIndex is detected, and items already shown are dropped.
import assert from 'node:assert/strict'
import { test } from 'node:test'
import { ignoredStart, merge } from '../src/lib/page.ts'
import type { Item, Page } from '../src/lib/types.ts'

const items = (from: number, n: number): Item[] => Array.from({ length: n }, (_, k) => ({ Id: String(from + k), Name: 'x', Type: 'Movie' }))
const page = (it: Item[], total: number): Page => ({ Items: it, TotalRecordCount: total })

test('honest total: shows it while paging, ends on a full last page without an extra request', () => {
  const first = merge([], page(items(0, 3), 6), 3)
  assert.deepEqual([first.done, first.total, first.exact], [false, 6, true])
  const last = merge(items(0, 3), page(items(3, 3), 6), 3)
  assert.deepEqual([last.fresh.map((i) => i.Id), last.done, last.total], [['3', '4', '5'], true, 6])
})

test('total = number returned: a full page is not the end, the count grows, a short page ends it exactly', () => {
  const first = merge([], page(items(0, 3), 3), 3)
  assert.deepEqual([first.done, first.total, first.exact], [false, 3, false])
  const second = merge(items(0, 3), page(items(3, 3), 3), 3)
  assert.deepEqual([second.done, second.total], [false, 6])
  const last = merge(items(0, 6), page(items(6, 2), 2), 3)
  assert.deepEqual([last.done, last.total, last.exact], [true, 8, true])
  assert.equal(merge(items(0, 6), page([], 0), 3).done, true)
})

test('ignored StartIndex: a page of items already shown is detected; a partly new page or an empty one is not', () => {
  assert.equal(ignoredStart(items(0, 3), page(items(0, 3), 3)), true)
  assert.equal(ignoredStart(items(0, 3), page(items(2, 3), 3)), false)
  assert.equal(ignoredStart(items(0, 3), page([], 0)), false)
})

test('asked from 0 (StartIndex ignored): only the tail is the page, and the total is judged on the whole response', () => {
  const fake = merge(items(0, 10), page(items(0, 20), 20), 10, 10)
  assert.deepEqual([fake.fresh.map((i) => i.Id), fake.done, fake.total], [items(10, 10).map((i) => i.Id), false, 20])
  const end = merge(items(0, 20), page(items(0, 25), 25), 10, 20)
  assert.deepEqual([end.fresh.length, end.done, end.total], [5, true, 25])
  const honest = merge(items(0, 10), page(items(0, 20), 50), 10, 10)
  assert.deepEqual([honest.done, honest.total], [false, 50])
})

test('items already shown, or repeated within a page, are dropped and not counted', () => {
  const r = merge(items(0, 3), page([...items(2, 3), ...items(4, 1)], 10), 4)
  assert.deepEqual(r.fresh.map((i) => i.Id), ['3', '4'])
  assert.deepEqual([r.done, r.total], [false, 10])
})
