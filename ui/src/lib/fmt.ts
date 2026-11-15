// Display formats for lists and the detail page.
import type { Item, MediaSource } from './types'

const TICKS_PER_SEC = 10_000_000

/** Ticks → `hh:mm:ss`, or `mm:ss` under an hour. */
export function clock(ticks = 0) {
  const s = Math.floor(ticks / TICKS_PER_SEC)
  const [h, m, sec] = [Math.floor(s / 3600), Math.floor((s % 3600) / 60), s % 60]
  const p = (n: number) => String(n).padStart(2, '0')
  return h ? `${p(h)}:${p(m)}:${p(sec)}` : `${p(m)}:${p(sec)}`
}

export const minutes = (ticks = 0) => `${Math.round(ticks / TICKS_PER_SEC / 60)}分钟`

/** Year; an ended series shows `2015 - 2025`.
 *  Local time on purpose (also in `date`): scrapers often store Beijing midnight as `T16:00:00Z`, so UTC would be a day early. */
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

function size(bytes?: number) {
  if (!bytes) return ''
  return bytes >= 1 << 30 ? `${(bytes / (1 << 30)).toFixed(2)}G` : `${Math.round(bytes / (1 << 20))}M`
}

/** Cache usage: 0 KB, 512 KB, 446.3 MB, 12.40 GB. */
export function bytes(n: number) {
  if (n >= 1 << 30) return `${(n / (1 << 30)).toFixed(2)} GB`
  if (n >= 1 << 20) return `${(n / (1 << 20)).toFixed(1)} MB`
  return `${Math.round(n / 1024)} KB`
}

export function bitrate(bps?: number) {
  if (!bps) return ''
  return bps >= 1e6 ? `${(bps / 1e6).toFixed(1)}mbps` : `${Math.round(bps / 1e3)}kbps`
}

/** Episode subtitle `S01 · E02 · name`; other types show the year. */
export function episodeLabel(it: Item) {
  if (it.Type !== 'Episode') return years(it)
  const s = it.ParentIndexNumber != null ? `S${String(it.ParentIndexNumber).padStart(2, '0')}` : ''
  const e = it.IndexNumber != null ? `E${String(it.IndexNumber).padStart(2, '0')}` : ''
  return [s, e, it.Name].filter(Boolean).join(' · ')
}

/** Tech line: `1080P SDR 23FPS 4.85G`. */
export function techLine(src?: MediaSource) {
  const v = src?.MediaStreams?.find((s) => s.Type === 'Video')
  if (!src || !v) return ''
  // Scope sources are cropped in height (1920×800 is 1080P), 4:3 / HDV ones in width (1440×1080 is 1080P): tier by each, keep the higher
  const [w, h] = [v.Width ?? 0, v.Height ?? 0]
  const byW = w >= 3800 ? 2160 : w >= 2500 ? 1440 : w >= 1900 ? 1080 : w >= 1260 ? 720 : 0
  const byH = h >= 2000 ? 2160 : h >= 1400 ? 1440 : h >= 1000 ? 1080 : h >= 700 ? 720 : h
  const tier = Math.max(byW, byH)
  const res = tier >= 2160 ? '4K' : tier >= 1440 ? '2K' : tier ? `${tier}P` : ''
  const fps = v.RealFrameRate ?? v.AverageFrameRate
  return [res, v.VideoRange, fps ? `${Math.round(fps)}FPS` : '', size(src.Size)].filter(Boolean).join(' ')
}

/** Series badge on a poster: unplayed count; when the server leaves it out (some only send it on certain endpoints)
 *  the episode count `N集`, so series still stand out from movies. Nothing once played (the poster shows a tick). */
export function seriesBadge(it: Item) {
  if (it.Type !== 'Series' || it.UserData?.Played) return ''
  const left = it.UserData?.UnplayedItemCount
  if (left != null) return left ? String(left) : ''
  return it.RecursiveItemCount ? `${it.RecursiveItemCount}集` : ''
}
