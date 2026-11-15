// List snapshots (stale-while-revalidate persisted to localStorage, like TanStack Query's persister):
// the last home, detail, season, person, favorites and first library page survive restarts, so a page draws from them at once
// and is replaced in place when fresh data arrives. Only a cache: failed reads / writes or a wipe never break anything.
const PREFIX = 'snap:'
const INDEX = 'snap-index'
/** Total characters across snapshots; Chromium allows about 10 MB (UTF-16) per origin, so this leaves headroom. */
const LIMIT = 3_000_000

type Index = Record<string, [at: number, size: number]>

function index(): Index {
  try {
    return JSON.parse(localStorage.getItem(INDEX) ?? '{}')
  } catch {
    return {}
  }
}

export function readSnap<T>(key: string): T | undefined {
  try {
    const raw = localStorage.getItem(PREFIX + key)
    return raw === null ? undefined : (JSON.parse(raw) as T)
  } catch {
    return undefined
  }
}

/** Bytes used (UTF-16, two per character), shown in cache management. */
export function snapSize() {
  return Object.values(index()).reduce((n, [, size]) => n + size * 2, 0)
}

/** Drop every snapshot (cache management). */
export function snapClear() {
  try {
    for (const k of Object.keys(index())) localStorage.removeItem(PREFIX + k)
    localStorage.removeItem(INDEX)
  } catch {
    /* best effort */
  }
}

/** Store a snapshot; over the total limit, evict the least recently written (never this one). */
export function writeSnap(key: string, value: unknown) {
  try {
    const raw = JSON.stringify(value)
    if (raw.length > LIMIT / 4) return // one huge entry would evict everything else
    const idx = index()
    idx[key] = [Date.now(), raw.length]
    let total = Object.values(idx).reduce((n, [, size]) => n + size, 0)
    for (const [k] of Object.entries(idx).sort((a, b) => a[1][0] - b[1][0])) {
      if (total <= LIMIT) break
      if (k === key) continue
      localStorage.removeItem(PREFIX + k)
      total -= idx[k][1]
      delete idx[k]
    }
    localStorage.setItem(PREFIX + key, raw)
    localStorage.setItem(INDEX, JSON.stringify(idx))
  } catch {
    /* quota full or storage unavailable: just skip */
  }
}
