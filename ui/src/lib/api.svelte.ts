// The UI's only data entry point: invoke wrappers, a per-session GET cache and image URLs. The token never leaves Rust.
import { untrack } from 'svelte'
import { convertFileSrc, invoke } from '@tauri-apps/api/core'
import { Check, Circle, EyeOff, Heart, HeartOff, Play, RotateCcw } from '@lucide/svelte'
import type { MenuItem } from './menu.svelte'
import { readSnap, writeSnap } from './snap'
import type { Item, Page } from './types'

// Mirrors of the Rust serde shapes (`config::ServerView`, `ConfigView` in main.rs, `emby::Error`); keep in sync by hand.
type ServerView = { id: string; name: string; url: string; port: number; user: string; userId: string; base: string; loggedIn: boolean }
type ConfigView = {
  active: string | null; servers: ServerView[]; mpvPath: string | null; preloadPercent: number; userMpvConf: string
  updateProxy: string | null; cacheWhole: boolean; danmakuApi: string; danmakuWords: string[]; version: string
}
type ApiError = { kind: 'unauthorized' | 'status' | 'network' | 'other'; message: string }

/** Any error (backend ApiError, JS exception, string) → one line for the user. */
export const errText = (e: unknown) => String((e as { message?: unknown } | null)?.message ?? e)

/** Fields list cards need; the detail page adds heavy ones such as People and MediaSources. */
export const LIST_FIELDS = 'ProductionYear,EndDate,CommunityRating,UserData,PrimaryImageAspectRatio,RunTimeTicks,Overview,Genres,OfficialRating,RecursiveItemCount'
export const DETAIL_FIELDS = `${LIST_FIELDS},People,ExternalUrls,ProviderIds,MediaSources,Studios,PremiereDate,Status`
/** Fields episode cards need. */
export const EP_FIELDS = 'Overview,MediaSources,UserData,CommunityRating,PremiereDate,RunTimeTicks'

/** Bottom-right notice (playback failed etc.); gone after TOAST_MS or on click, may carry one button (e.g. undo). */
type Toast = { id: number; text: string; action?: { label: string; run: () => unknown } }
export const toasts = $state<Toast[]>([])
let toastSeq = 0
const TOAST_MS = 8000
export function dismiss(id: number) {
  const i = toasts.findIndex((t) => t.id === id)
  if (i >= 0) toasts.splice(i, 1)
}
export function notify(text: string, action?: Toast['action']) {
  const id = ++toastSeq
  toasts.push({ id, text, action })
  setTimeout(() => dismiss(id), TOAST_MS)
}

/** `refresh`: manual reload (title bar button, F5); `quiet`: background reload (after playback, favorite or played changes), updated in place without flicker. */
export const session = $state<{ cfg: ConfigView | null; server: ServerView | null; refresh: number; quiet: number }>({
  cfg: null,
  server: null,
  refresh: 0,
  quiet: 0,
})

const cache = new Map<string, Promise<unknown>>()
/** Items seen in lists, by Id: the detail page draws its header from one while the full detail loads. */
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

/** Manual reload: drop the cache and refetch every page (library grids go back to page one). */
export function refresh() {
  cache.clear()
  session.refresh++
}

/** Background reload: refresh the data without resetting lists or scroll positions. */
export function refreshQuiet() {
  cache.clear()
  session.quiet++
}

/** Snapshot key scoped to the current server so servers never mix. */
export const snapKey = (k: string) => `${session.server?.id}|${k}`

/**
 * Fetch and keep the previous result on screen while refetching; new data replaces it in place.
 * Refetches when reactive values read synchronously in `fn` change (id, session.refresh…). Call during component init.
 * With `key`, the page first draws from the local snapshot (snap.ts) and the snapshot is updated on success;
 * a failed fetch keeps showing the snapshot instead of an error.
 */
export function loader<T>(fn: () => Promise<T>, key?: () => string) {
  const s = $state<{ data?: T; error?: unknown }>({})
  $effect(() => {
    let alive = true
    const k = key && snapKey(key())
    if (k && untrack(() => s.data) === undefined) {
      const snap = readSnap<T>(k)
      if (snap !== undefined) s.data = snap
    }
    fn().then(
      (d) => {
        if (!alive) return
        s.data = d
        s.error = undefined
        if (k) writeSnap(k, d)
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

/** `invoke` that hands an `unauthorized` error to the sign-in handler before rethrowing it. */
function invokeAuthed<T>(cmd: string, args: Record<string, unknown>): Promise<T> {
  return invoke<T>(cmd, args).catch((e: ApiError) => {
    if (e?.kind === 'unauthorized') onUnauthorized()
    throw e
  })
}

const call = <T>(method: string, path: string, query?: Record<string, unknown>) => invokeAuthed<T>('emby', { method, path, query })

export function get<T = unknown>(path: string, query: Record<string, unknown> = {}): Promise<T> {
  const key = `${session.server?.id}|${path}|${JSON.stringify(query)}`
  let p = cache.get(key) as Promise<T> | undefined
  if (!p) {
    p = call<T>('GET', path, query)
    cache.set(key, p)
    // A refresh may have cleared the cache and refetched this key meanwhile; drop only our own failed entry
    p.then(remember, () => cache.get(key) === p && cache.delete(key))
  }
  return p
}

/** Item detail; hover prefetch and the detail page share this request, so the page hits the cache. */
export const itemDetail = (id: string) => get<Item>(`/Users/${uid()}/Items/${id}`, { Fields: DETAIL_FIELDS })

/** Hover this long on a card before prefetching its detail. */
const HOVER_MS = 150
let hoverTimer: ReturnType<typeof setTimeout> | undefined
export function prefetch(id: string) {
  clearTimeout(hoverTimer)
  hoverTimer = setTimeout(() => {
    // Also the detail page's backdrop (full size + blurred base); widths must match Item.svelte
    itemDetail(id).then((d) => {
      const tag = d.BackdropImageTags?.[0]
      for (const w of [1600, 320]) {
        const src = img(d.Id, 'Backdrop', tag, w)
        if (src) new Image().src = src
      }
    }, () => {})
  }, HOVER_MS)
}
export const cancelPrefetch = () => clearTimeout(hoverTimer)

function send(method: 'POST' | 'DELETE', path: string, query?: Record<string, unknown>) {
  cache.clear()
  return call<unknown>(method, path, query)
}

/** Favorite / played: background reload on success; a failure (upstream 502…) shows a notice instead of doing nothing. */
const mark = (kind: 'FavoriteItems' | 'PlayedItems', what: string) => (id: string, on: boolean) => {
  return send(on ? 'POST' : 'DELETE', `/Users/${uid()}/${kind}/${id}`).then(refreshQuiet, (e) => notify(`${what}失败：${errText(e)}`))
}
export const setFavorite = mark('FavoriteItems', '收藏')
export const setPlayed = mark('PlayedItems', '标记已看')
/** Remove from Continue Watching (server-side HideFromResume, progress kept); the notice offers undo. */
function hideResume(it: Item, hide = true): Promise<unknown> {
  return send('POST', `/Users/${uid()}/Items/${it.Id}/HideFromResume`, { Hide: hide }).then(
    () => {
      refreshQuiet()
      if (hide) notify(`已从继续观看中移除「${it.SeriesName ?? it.Name}」`, { label: '撤销', run: () => hideResume(it, false) })
    },
    (e) => notify(`${hide ? '移除' : '撤销'}失败：${errText(e)}`),
  )
}
export const resumeMenu = (it: Item): MenuItem[] => [...itemMenu(it), 'sep', { label: '从继续观看中移除', icon: EyeOff, action: () => hideResume(it) }]

/** Start playback; a refusal (server error, mpv missing…) shows a notice, load failures arrive later as the `failed` player event.
 *  `sourceId`: the version picked on the detail page (1080p / 4K…); without it the server's first one plays. */
export function play(itemId: string, fromStart = false, sourceId?: string) {
  return invokeAuthed('play', { itemId, fromStart, sourceId: sourceId ?? null }).catch((e) => notify(`播放失败：${errText(e)}`))
}

/** Card context menu: play for playable items (movies, episodes); played and favorite for all. */
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

/**
 * Paged list (library, person's works): `more()` fetches the next page (called when the grid nears its end).
 * Starts over from page one when reactive values read in `query()` (sort, filter, id) or a manual reload change;
 * late results of older requests are dropped by generation. Call during component init.
 */
export function pager(query: () => Record<string, unknown>, size = 60) {
  const s = $state({ items: [] as Item[], total: 0, busy: false, error: '', loaded: false })
  let gen = 0
  let q: Record<string, unknown> = {}
  /** Showing the first-page snapshot (snap.ts): the next fetch starts at 0 and replaces it. */
  let fromSnap = false
  const key = () => snapKey(`pager|${JSON.stringify(q)}`)
  async function more() {
    if (s.busy || (s.loaded && !fromSnap && s.items.length >= s.total)) return
    s.busy = true
    const mine = gen
    const start = fromSnap ? 0 : s.items.length
    try {
      const p = await get<Page>(`/Users/${uid()}/Items`, { ...q, StartIndex: start, Limit: size })
      if (mine !== gen) return
      if (start === 0) {
        s.items = p.Items
        fromSnap = false
        writeSnap(key(), { items: p.Items, total: p.TotalRecordCount })
      } else s.items.push(...p.Items)
      s.total = p.TotalRecordCount
      s.loaded = true
      s.error = ''
    } catch (e) {
      // Like loader: keep showing the snapshot without an error (scrolling retries)
      if (mine === gen && !fromSnap) s.error = errText(e)
    } finally {
      if (mine === gen) s.busy = false
    }
  }
  $effect(() => {
    void session.refresh
    q = query()
    gen++
    const snap = readSnap<{ items: Item[]; total: number }>(key())
    fromSnap = !!snap
    Object.assign(s, { items: snap?.items ?? [], total: snap?.total ?? 0, busy: false, error: '', loaded: !!snap })
    untrack(more)
  })
  return { s, more }
}

/** Width tiers: DPR-scaled widths round up to one, so zoom levels and card sizes share cached images. */
const WIDTHS = [120, 160, 240, 320, 480, 640, 960, 1280, 1600, 1920, 2560, 3840]
const IN_TAURI = '__TAURI_INTERNALS__' in globalThis

/**
 * Image URL (`index`: which image of that type, e.g. which backdrop); empty without a tag.
 * The app goes through Rust's image cache (`emby-img` protocol, src-tauri/src/images.rs); a dev browser hits upstream directly.
 */
export function img(id: string | undefined, type: string, tag: string | undefined | null, width: number, index = 0) {
  if (!id || !tag || !session.server) return ''
  const want = Math.round(width * (globalThis.devicePixelRatio || 1))
  const w = WIDTHS.find((x) => x >= want) ?? WIDTHS[WIDTHS.length - 1]
  if (IN_TAURI) return convertFileSrc(`${session.server.id}/${id}/${type}/${index}/${tag}/${w}`, 'emby-img')
  return `${session.server.base}/Items/${id}/Images/${type}/${index}?tag=${tag}&maxWidth=${w}&quality=90`
}

/** Upstream URL of the original image, for opening in an external browser (no cache, no resize). */
export function imgOriginal(id: string, type: string, tag: string, index = 0) {
  return session.server ? `${session.server.base}/Items/${id}/Images/${type}/${index}?tag=${tag}` : ''
}

/** Landscape image: own backdrop → parent series backdrop → own primary (episode still). */
export function wideImg(it: Item, width: number) {
  if (it.BackdropImageTags?.length) return img(it.Id, 'Backdrop', it.BackdropImageTags[0], width)
  if (it.ParentBackdropImageTags?.length) return img(it.ParentBackdropItemId, 'Backdrop', it.ParentBackdropImageTags[0], width)
  return img(it.Id, 'Primary', it.ImageTags?.Primary, width)
}
