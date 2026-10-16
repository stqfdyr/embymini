// 列表与详情的显示格式（src/lib/fmt.ts）：海报剧集角标、技术参数行的分辨率档位、缓存占用。
import assert from 'node:assert/strict'
import { test } from 'node:test'
import { bytes, seriesBadge, techLine } from '../src/lib/fmt.ts'
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

test('技术参数行的分辨率档位', () => {
  const line = (Width: number, Height: number) => techLine({ Id: 's', MediaStreams: [{ Index: 0, Type: 'Video', Width, Height }] })
  assert.equal(line(1920, 800), '1080P')
  assert.equal(line(1280, 536), '720P')
  assert.equal(line(3840, 1600), '4K')
  assert.equal(line(720, 480), '480P')
  assert.equal(line(1440, 1080), '1080P') // HDV / 4:3：按宽只够 720P
  assert.equal(line(960, 720), '720P')
  assert.equal(line(2560, 1440), '2K')
})

test('缓存占用', () => {
  assert.equal(bytes(0), '0 KB')
  assert.equal(bytes(495_000), '483 KB')
  assert.equal(bytes(446.3 * 2 ** 20), '446.3 MB')
  assert.equal(bytes(12.4 * 2 ** 30), '12.40 GB')
})
