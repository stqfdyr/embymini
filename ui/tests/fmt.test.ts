// Display formats (src/lib/fmt.ts): durations, years and dates, series badge, tech line resolution tiers, cache usage.
// Dates are shown in local time; pin the users' zone (UTC+8). Node picks up a TZ change before the first Date use.
process.env.TZ = 'Asia/Shanghai'
import assert from 'node:assert/strict'
import { test } from 'node:test'
import { bytes, clock, date, episodeLabel, seriesBadge, techLine, years } from '../src/lib/fmt.ts'
import type { Item } from '../src/lib/types.ts'

const it = (o: Partial<Item>): Item => ({ Id: '1', Name: 'x', Type: 'Series', ...o })
const TICKS = 10_000_000

test('clock: mm:ss under an hour, hh:mm:ss from an hour', () => {
  assert.equal(clock(), '00:00')
  assert.equal(clock(59.9 * TICKS), '00:59')
  assert.equal(clock(61 * TICKS), '01:01')
  assert.equal(clock(3600 * TICKS), '01:00:00')
  assert.equal(clock((2 * 3600 + 5 * 60 + 7) * TICKS), '02:05:07')
})

test('years: ended series as a range, everything else the start year', () => {
  assert.equal(years(it({ ProductionYear: 2015, EndDate: '2025-03-01T00:00:00.0000000Z' })), '2015 - 2025')
  assert.equal(years(it({ ProductionYear: 2015, EndDate: '2015-12-01T00:00:00.0000000Z' })), '2015')
  assert.equal(years(it({ ProductionYear: 2015 })), '2015')
  assert.equal(years(it({ Type: 'Movie', ProductionYear: 2015, EndDate: '2025-03-01T00:00:00.0000000Z' })), '2015')
  assert.equal(years(it({ EndDate: '2025-03-01T00:00:00.0000000Z' })), '')
})

test('date: local time, so Beijing midnight stored as T16:00Z is the next day', () => {
  assert.equal(date('2010-07-14T16:00:00.0000000Z'), '2010年7月15日')
  assert.equal(date('2010-07-15T00:00:00.0000000Z'), '2010年7月15日')
  assert.equal(date('2024-12-31T16:00:00.0000000Z'), '2025年1月1日')
  assert.equal(date(), '')
})

test('episodeLabel: season, episode and name; other types show the year', () => {
  assert.equal(episodeLabel(it({ Type: 'Episode', ParentIndexNumber: 1, IndexNumber: 2, Name: 'Pilot' })), 'S01 · E02 · Pilot')
  assert.equal(episodeLabel(it({ Type: 'Episode', ParentIndexNumber: 0, IndexNumber: 12, Name: 'SP' })), 'S00 · E12 · SP')
  assert.equal(episodeLabel(it({ Type: 'Episode', Name: 'Special' })), 'Special')
  assert.equal(episodeLabel(it({ Type: 'Movie', ProductionYear: 2001 })), '2001')
})

test('series badge', () => {
  assert.equal(seriesBadge(it({ UserData: { UnplayedItemCount: 3 }, RecursiveItemCount: 22 })), '3')
  assert.equal(seriesBadge(it({ UserData: { UnplayedItemCount: 0 }, RecursiveItemCount: 22 })), '')
  assert.equal(seriesBadge(it({ RecursiveItemCount: 22 })), '22集') // search and some lists omit the unplayed count
  assert.equal(seriesBadge(it({ UserData: { Played: true }, RecursiveItemCount: 22 })), '')
  assert.equal(seriesBadge(it({})), '')
  assert.equal(seriesBadge(it({ Type: 'Movie', RecursiveItemCount: 5 })), '')
})

test('tech line resolution tiers', () => {
  const line = (Width: number, Height: number) => techLine({ Id: 's', MediaStreams: [{ Index: 0, Type: 'Video', Width, Height }] })
  assert.equal(line(1920, 800), '1080P')
  assert.equal(line(1280, 536), '720P')
  assert.equal(line(3840, 1600), '4K')
  assert.equal(line(720, 480), '480P')
  assert.equal(line(1440, 1080), '1080P') // HDV / 4:3: the width alone only reaches 720P
  assert.equal(line(960, 720), '720P')
  assert.equal(line(2560, 1440), '2K')
})

test('tech line: range, frame rate and size', () => {
  const v = { Index: 0, Type: 'Video', Width: 1920, Height: 1080, VideoRange: 'HDR', AverageFrameRate: 23.976 }
  assert.equal(techLine({ Id: 's', Size: 5.2 * 2 ** 30, MediaStreams: [v] }), '1080P HDR 24FPS 5.20G')
  assert.equal(techLine({ Id: 's', Size: 700 * 2 ** 20, MediaStreams: [{ ...v, RealFrameRate: 25 }] }), '1080P HDR 25FPS 700M')
  assert.equal(techLine({ Id: 's', MediaStreams: [{ Index: 1, Type: 'Audio' }] }), '')
  assert.equal(techLine(), '')
})

test('cache usage', () => {
  assert.equal(bytes(0), '0 KB')
  assert.equal(bytes(495_000), '483 KB')
  assert.equal(bytes(446.3 * 2 ** 20), '446.3 MB')
  assert.equal(bytes(12.4 * 2 ** 30), '12.40 GB')
})
