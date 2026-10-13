// 列表数据快照（常规的 stale-while-revalidate 持久化，同 TanStack Query 的 localStorage persister 一类）：
// 上次看到的首页、详情、整季、人物、收藏、媒体库第一页存在本地（WebView 的 localStorage，跨重启保留），
// 再次打开时先用它把页面画出来（海报直接从磁盘缓存出），后台取到新数据再原地替换并更新快照。
// 只是缓存：读写失败、被清掉都不影响功能。
const PREFIX = 'snap:'
const INDEX = 'snap-index'
/** 快照总字符数上限。Chromium 每个源的 localStorage 约 10 MB（UTF-16），留足余量。 */
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

/** 写一份快照；总量超上限时删最久没更新的。 */
export function writeSnap(key: string, value: unknown) {
  try {
    const raw = JSON.stringify(value)
    if (raw.length > LIMIT / 4) return // 单份太大（比如翻了很多页的列表）不存
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
    /* 存不下就算了 */
  }
}
