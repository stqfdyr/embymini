// 前端唯一的数据入口：invoke 封装 + GET 会话内缓存 + 图片地址。token 永远留在 Rust 里。
import { invoke } from '@tauri-apps/api/core'
import { Check, Circle, Heart, HeartOff, Play, RotateCcw } from '@lucide/svelte'
import type { MenuItem } from './menu.svelte'
import type { Item } from './types'

export type ServerView = { id: string; name: string; url: string; port: number; user: string; userId: string; base: string; loggedIn: boolean }
export type ConfigView = { active: string | null; servers: ServerView[]; mpvPath: string | null; preloadPercent: number; userMpvConf: string }
export type ApiError = { kind: 'unauthorized' | 'status' | 'network' | 'other'; message: string }

/** 任意错误 → 给用户看的一句话（后端的 ApiError、JS 异常、字符串都能处理）。 */
export const errText = (e: unknown) => String((e as { message?: unknown } | null)?.message ?? e)

/** 列表卡片要的字段；详情另加 People / MediaSources 等重字段。 */
export const LIST_FIELDS = 'ProductionYear,EndDate,CommunityRating,UserData,PrimaryImageAspectRatio,RunTimeTicks,Overview,Genres,OfficialRating'
export const DETAIL_FIELDS = `${LIST_FIELDS},People,ExternalUrls,ProviderIds,MediaSources,Studios,PremiereDate,Status`

/** 右下角提示（播放失败等），8 秒后自动消失。 */
export const toasts = $state<{ id: number; text: string }[]>([])
let toastSeq = 0
export function notify(text: string) {
  const id = ++toastSeq
  toasts.push({ id, text })
  setTimeout(() => {
    const i = toasts.findIndex((t) => t.id === id)
    if (i >= 0) toasts.splice(i, 1)
  }, 8000)
}

/** refresh：手动刷新（顶栏按钮、F5）；quiet：后台刷新（播放结束、收藏或已看变化后），页面原地更新，不闪不跳。 */
export const session = $state<{ cfg: ConfigView | null; server: ServerView | null; refresh: number; quiet: number }>({
  cfg: null,
  server: null,
  refresh: 0,
  quiet: 0,
})

const cache = new Map<string, Promise<unknown>>()
/** 列表里见过的条目（按 Id）：点开详情时先用它把页面顶部画出来，详情到了再补全，不让用户干等。 */
const seen = new Map<string, Item>()
export const peek = (id: string) => seen.get(id)
function remember(v: unknown) {
  const list = Array.isArray(v) ? v : (v as { Items?: unknown })?.Items
  if (Array.isArray(list)) for (const it of list as Item[]) if (it?.Id && !seen.has(it.Id)) seen.set(it.Id, it)
}

export async function loadConfig() {
  const c = await invoke<ConfigView>('config_get')
  session.cfg = c
  session.server = c.servers.find((s) => s.id === c.active) ?? null
  cache.clear()
  seen.clear()
}

export const uid = () => session.server?.userId ?? ''

/** 手动刷新：丢掉缓存，各页重新取数（媒体库网格会回到第一页）。 */
export function refresh() {
  cache.clear()
  session.refresh++
}

/** 后台刷新（播放结束、收藏或已看变化后）：只更新数据，不重置列表和滚动位置。 */
export function refreshQuiet() {
  cache.clear()
  session.quiet++
}

/**
 * 取数并保留上一次的结果：重新取数期间继续显示旧数据，新数据到了原地替换。
 * `fn` 里同步读到的响应式值（id、session.refresh 等）变化时自动重取。只能在组件初始化时调用。
 */
export function loader<T>(fn: () => Promise<T>) {
  const s = $state<{ data?: T; error?: unknown }>({})
  $effect(() => {
    let alive = true
    fn().then(
      (d) => {
        if (!alive) return
        s.data = d
        s.error = undefined
      },
      (e) => {
        if (alive && s.data === undefined) s.error = e
      },
    )
    return () => void (alive = false)
  })
  return s
}

let onUnauthorized: () => void = () => {}
export const setUnauthorizedHandler = (f: () => void) => (onUnauthorized = f)

function call<T>(method: string, path: string, query?: Record<string, unknown>, body?: unknown): Promise<T> {
  return invoke<T>('emby', { method, path, query, body }).catch((e: ApiError) => {
    if (e?.kind === 'unauthorized') onUnauthorized()
    throw e
  })
}

export function get<T = unknown>(path: string, query: Record<string, unknown> = {}): Promise<T> {
  const key = `${session.server?.id}|${path}|${JSON.stringify(query)}`
  let p = cache.get(key) as Promise<T> | undefined
  if (!p) {
    p = call<T>('GET', path, query)
    cache.set(key, p)
    p.then(remember, () => cache.delete(key))
  }
  return p
}

/** 详情页要的条目详情；悬停预取和详情页用同一个请求，命中缓存。 */
export const itemDetail = (id: string) => get<Item>(`/Users/${uid()}/Items/${id}`, { Fields: DETAIL_FIELDS })

/** 鼠标在卡片上停一会儿就预取详情，点进去时多半已经到了。 */
let hoverTimer: ReturnType<typeof setTimeout> | undefined
export function prefetch(id: string) {
  clearTimeout(hoverTimer)
  hoverTimer = setTimeout(() => void itemDetail(id).catch(() => {}), 150)
}
export const cancelPrefetch = () => clearTimeout(hoverTimer)

export function send(method: 'POST' | 'DELETE', path: string, body?: unknown) {
  cache.clear()
  return call<unknown>(method, path, undefined, body)
}

/** 收藏 / 已看：成功后后台刷新；失败（上游 502 之类）弹提示，不能点了没反应。 */
const mark = (kind: 'FavoriteItems' | 'PlayedItems', what: string) => (id: string, on: boolean) =>
  send(on ? 'POST' : 'DELETE', `/Users/${uid()}/${kind}/${id}`).then(refreshQuiet, (e) => notify(`${what}失败：${errText(e)}`))
export const setFavorite = mark('FavoriteItems', '收藏')
export const setPlayed = mark('PlayedItems', '标记已看')
/** 起播。出错（服务器拒绝、找不到 mpv 等）直接弹提示；加载失败由播放事件 failed 另行提示。 */
export const play = (itemId: string, fromStart = false) =>
  invoke('play', { itemId, fromStart }).catch((e: ApiError) => {
    if (e?.kind === 'unauthorized') onUnauthorized()
    notify(`播放失败：${errText(e)}`)
  })

/** 卡片右键菜单：能直接播的（电影、单集）给播放；都能标记已看、收藏。 */
export function itemMenu(it: Item, open?: () => void): MenuItem[] {
  const playable = it.Type !== 'Series' && it.Type !== 'BoxSet' && it.Type !== 'CollectionFolder'
  const items: MenuItem[] = []
  if (playable) {
    const pos = it.UserData?.PlaybackPositionTicks ?? 0
    items.push({ label: pos > 0 ? '继续播放' : '播放', icon: Play, action: () => play(it.Id) })
    if (pos > 0) items.push({ label: '从头播放', icon: RotateCcw, action: () => play(it.Id, true) })
  }
  if (open) items.push({ label: '查看详情', action: open })
  items.push('sep')
  const played = it.UserData?.Played
  items.push({ label: played ? '标记为未看' : '标记为已看', icon: played ? Circle : Check, action: () => setPlayed(it.Id, !played) })
  const fav = it.UserData?.IsFavorite
  items.push({ label: fav ? '取消收藏' : '收藏', icon: fav ? HeartOff : Heart, action: () => setFavorite(it.Id, !fav) })
  return items
}

/** 图片地址：带 tag 的 URL 不会变，上游给 1 年缓存，交给 WebView 的 HTTP 缓存。没有 tag 返回空串。 */
export function img(id: string | undefined, type: string, tag: string | undefined | null, width: number) {
  if (!id || !tag || !session.server) return ''
  const w = Math.round(width * (globalThis.devicePixelRatio || 1))
  return `${session.server.base}/Items/${id}/Images/${type}?tag=${tag}&maxWidth=${w}&quality=90`
}

/** 横图：自身背景图 → 所属剧集的背景图 → 自身主图（单集截图）。 */
export function wideImg(it: Item, width: number) {
  if (it.BackdropImageTags?.length) return img(it.Id, 'Backdrop', it.BackdropImageTags[0], width)
  if (it.ParentBackdropImageTags?.length) return img(it.ParentBackdropItemId, 'Backdrop', it.ParentBackdropImageTags[0], width)
  return img(it.Id, 'Primary', it.ImageTags?.Primary, width)
}
