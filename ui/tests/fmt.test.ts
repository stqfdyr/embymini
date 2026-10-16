// 列表与详情的显示格式（src/lib/fmt.ts）：海报剧集角标、技术参数行的分辨率档位。
import assert from 'node:assert/strict'
import { test } from 'node:test'
import { seriesBadge, techLine } from '../src/lib/fmt.ts'
import type { Item } from '../src/lib/types.ts'

const it = (o: Partial<Item>): Item => ({ Id: '1', Name: 'x', Type: 'Series', ...o })

test('剧集角标', () => {
  assert.equal(seriesBadge(it({ UserData: { UnplayedItemCount: 3 }, RecursiveItemCount: 22 })), '3')
  assert.equal(seriesBadge(it({ UserData: { UnplayedItemCount: 0 }, RecursiveItemCount: 22 })), '')
  assert.equal(seriesBadge(it({ RecursiveItemCount: 22 })), '22集') // 搜索等接口不带未看数
  assert.equal(seriesBadge(it({ UserData: { Played: true }, RecursiveItemCount: 22 })), '')
  assert.equal(seriesBadge(it({})), '')
  assert.equal(seriesBadge(it({ Type: 'Movie', RecursiveItemCount: 5 })), '')
})

test('技术参数行按宽度定档', () => {
  const line = (Width: number, Height: number) => techLine({ Id: 's', MediaStreams: [{ Index: 0, Type: 'Video', Width, Height }] })
  assert.equal(line(1920, 800), '1080P')
  assert.equal(line(1280, 536), '720P')
  assert.equal(line(3840, 1600), '4K')
  assert.equal(line(720, 480), '480P')
})
