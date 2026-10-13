import type { Item, MediaSource } from './types'

const TICKS_PER_SEC = 10_000_000

/** 刻度 → `h:mm:ss` 或 `mm:ss`。 */
export function clock(ticks = 0) {
  const s = Math.floor(ticks / TICKS_PER_SEC)
  const [h, m, sec] = [Math.floor(s / 3600), Math.floor((s % 3600) / 60), s % 60]
  const p = (n: number) => String(n).padStart(2, '0')
  return h ? `${p(h)}:${p(m)}:${p(sec)}` : `${p(m)}:${p(sec)}`
}

export const minutes = (ticks = 0) => `${Math.round(ticks / TICKS_PER_SEC / 60)}分钟`

/** 年份：剧集已完结显示 `2015 - 2025`。 */
export function years(it: Item) {
  const end = it.EndDate ? new Date(it.EndDate).getFullYear() : undefined
  if (!it.ProductionYear) return ''
  return it.Type === 'Series' && end && end !== it.ProductionYear ? `${it.ProductionYear} - ${end}` : String(it.ProductionYear)
}

export const rating = (r?: number | null) => (r ? r.toFixed(1) : '')

export function date(iso?: string) {
  if (!iso) return ''
  const d = new Date(iso)
  return `${d.getFullYear()}年${d.getMonth() + 1}月${d.getDate()}日`
}

export function size(bytes?: number) {
  if (!bytes) return ''
  return bytes >= 1 << 30 ? `${(bytes / (1 << 30)).toFixed(2)}G` : `${Math.round(bytes / (1 << 20))}M`
}

export function bitrate(bps?: number) {
  if (!bps) return ''
  return bps >= 1e6 ? `${(bps / 1e6).toFixed(1)}mbps` : `${Math.round(bps / 1e3)}kbps`
}

/** `S01 · E02` / `第 2 集` 一类的副标题。 */
export function episodeLabel(it: Item) {
  if (it.Type !== 'Episode') return years(it)
  const s = it.ParentIndexNumber != null ? `S${String(it.ParentIndexNumber).padStart(2, '0')}` : ''
  const e = it.IndexNumber != null ? `E${String(it.IndexNumber).padStart(2, '0')}` : ''
  return [s, e, it.Name].filter(Boolean).join(' · ')
}

/** 技术参数行：`1080P SDR 23FPS 4.85G`。 */
export function techLine(src?: MediaSource) {
  const v = src?.MediaStreams?.find((s) => s.Type === 'Video')
  if (!src || !v) return ''
  const w = v.Width ?? 0
  const res = w >= 3800 ? '4K' : w >= 2500 ? '2K' : v.Height ? `${v.Height}P` : ''
  const fps = v.RealFrameRate ?? v.AverageFrameRate
  return [res, v.VideoRange, fps ? `${Math.round(fps)}FPS` : '', size(src.Size)].filter(Boolean).join(' ')
}

/** 剧集未看集数 / 已看对勾，供海报角标用。 */
export const unplayed = (it: Item) => (it.Type === 'Series' ? it.UserData?.UnplayedItemCount ?? 0 : 0)
