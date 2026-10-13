// 诊断数据（界面这边）：设置里打开才记，关着时每个记录点只判断一个布尔。
// 事件攒在内存里，每 5 秒一批交给 Rust（`tel` 命令），由 Rust 限流、落盘、上传。
// 每条带上游标识 srv：不同上游、不同线路的表现分开统计。不记片名、搜索词、服务器地址。
import { invoke } from '@tauri-apps/api/core'
import { session } from './api.svelte'

type Ev = Record<string, unknown>
let on = false
let started = false
let queue: Ev[] = []

export function tel(k: string, data: Ev = {}) {
  if (!on) return
  queue.push({ k, srv: session.server?.tag, ...data })
}

/** 关闭程序前调用：把攒着的事件立刻交给 Rust（平时 5 秒才交一次，关窗就丢了），最多等 300 毫秒。 */
export function flushTel(): Promise<unknown> {
  if (!queue.length) return Promise.resolve()
  const events = queue
  queue = []
  return Promise.race([invoke('tel', { events }).catch(() => {}), new Promise((r) => setTimeout(r, 300))])
}

/** 用户的一次操作（播放、收藏、菜单项…），只记名字。 */
export const op = (name: string) => tel('op', { name })

/** 进程启动的墙上时间（Rust 给的），首个页面就绪时据此报启动耗时。 */
let startMs = 0
let startupSent = false

export function setTel(enabled: boolean, processStartMs = 0) {
  on = enabled
  if (processStartMs) startMs = processStartMs
  if (on && !started) {
    started = true
    install()
  }
}

/** 界面实际用的 GPU（WebView2 底下是 Chromium，ANGLE 渲染器名里带显卡型号）。设置页也用它显示。 */
export function gpuName() {
  try {
    const gl = document.createElement('canvas').getContext('webgl')
    const ext = gl?.getExtension('WEBGL_debug_renderer_info')
    const name = ext ? String(gl!.getParameter(ext.UNMASKED_RENDERER_WEBGL)) : ''
    if (!name) return '未知（没有 WebGL，可能在用软件渲染）'
    return /swiftshader|llvmpipe|software|basic render/i.test(name) ? `软件渲染（${name}）` : name.replace(/^ANGLE \((.*)\)$/, '$1')
  } catch {
    return '未知'
  }
}

// ---------- 页面加载：点击（hash 变化）→ 数据就绪（恢复滚动）→ 首屏图片加载完 ----------
// 先画的是本地快照时（snap），再等第一份新数据到达（fresh_ms）才记，看得出「快照秒开、新数据要多久」。

let page: { name: string; t0: number; ready: boolean; snap: boolean; fresh?: number } = { name: '', t0: 0, ready: false, snap: false }

export function pageStart(name: string) {
  page = { name, t0: performance.now(), ready: false, snap: false }
}

/** 这个页面先显示了本地快照（loader / pager 调用）。 */
export function pageSnap() {
  page.snap = true
}

/** 显示快照之后，新数据到了（只记第一份）。 */
export function pageFresh() {
  if (page.snap && page.fresh === undefined) page.fresh = Math.round(performance.now() - page.t0)
}

export function pageReady() {
  if (!on || page.ready) return
  page.ready = true
  const p = page
  const ready = Math.round(performance.now() - p.t0)
  const first = !startupSent && startMs > 0
  startupSent = true
  // 视口内的图片都加载完（或 15 秒放弃）算首屏完成；先显示快照的还要等新数据到
  const t = setInterval(() => {
    const inView = [...document.images].filter((i) => {
      const r = i.getBoundingClientRect()
      return r.bottom > 0 && r.top < innerHeight && r.width > 0 && i.src
    })
    const done = inView.every((i) => i.complete) && (!p.snap || p.fresh !== undefined)
    const ms = performance.now() - p.t0
    if (done || ms > 15000) {
      clearInterval(t)
      if (page === p) tel('page', { name: p.name, ready_ms: ready, imgs_ms: Math.round(ms), imgs: inView.length, timeout: !done, snap: p.snap, fresh_ms: p.fresh })
      // 程序启动后的第一个页面：从进程启动算起，WebView 开始加载页面 / 数据就绪 / 首屏完成各多久
      if (first) {
        const sinceStart = (at: number) => Math.round(performance.timeOrigin + at - startMs)
        tel('startup', { page: p.name, webview_ms: sinceStart(0), ready_ms: sinceStart(p.t0 + ready), imgs_ms: sinceStart(p.t0 + ms), snap: p.snap })
      }
    }
  }, 100)
}

// ---------- 聚合统计（每 30 秒一条） ----------

function install() {
  // 按页面分开汇总（慢在哪个页面），慢交互另记最慢那次的目标元素
  type Perf = { longN: number; longMax: number; jankN: number; jankMax: number; slowN: number; slowMax: number; names: Set<string>; target: string }
  const perf = new Map<string, Perf>()
  const cur = () => {
    let x = perf.get(page.name)
    if (!x) perf.set(page.name, (x = { longN: 0, longMax: 0, jankN: 0, jankMax: 0, slowN: 0, slowMax: 0, names: new Set(), target: '' }))
    return x
  }
  const describe = (el: unknown) => (el instanceof Element ? [el.tagName.toLowerCase(), el.classList[0]].filter(Boolean).join('.') : '')

  const observe = (type: string, f: (e: PerformanceEntry) => void, extra: object = {}) => {
    try {
      new PerformanceObserver((l) => l.getEntries().forEach(f)).observe({ type, buffered: true, ...extra } as PerformanceObserverInit)
    } catch {
      /* 不支持这类条目就不记 */
    }
  }
  // 图片耗时、命中率在 Rust 的图片缓存里按上游统计（src-tauri/src/images.rs），这里不再重复
  // 主线程长任务（>50 ms）
  observe('longtask', (e) => {
    const x = cur()
    x.longN++
    x.longMax = Math.max(x.longMax, e.duration)
  })
  // 交互延迟（点击到画面更新 >100 ms）
  observe('event', (e) => {
    if (e.duration < 100) return
    const x = cur()
    x.slowN++
    x.names.add(e.name)
    if (e.duration > x.slowMax) {
      x.slowMax = e.duration
      x.target = describe((e as PerformanceEntry & { target?: unknown }).target)
    }
  }, { durationThreshold: 104 })
  // 滚动掉帧：滚动期间帧间隔 >50 ms
  let scrolling = 0, last = 0
  const frame = (t: number) => {
    if (last && t - last > 50) {
      const x = cur()
      x.jankN++
      x.jankMax = Math.max(x.jankMax, t - last)
    }
    last = t
    if (performance.now() < scrolling) requestAnimationFrame(frame)
    else last = 0
  }
  addEventListener('scroll', () => {
    const idle = performance.now() >= scrolling
    scrolling = performance.now() + 200
    if (idle) requestAnimationFrame(frame)
  }, { capture: true, passive: true })
  // JS 错误（同文案由 Rust 每分钟去重）
  addEventListener('error', (e) => !(e.target instanceof HTMLElement) && tel('js_error', { msg: String(e.message).slice(0, 300) }))
  addEventListener('unhandledrejection', (e) => tel('js_error', { msg: String((e.reason as Error)?.message ?? e.reason).slice(0, 300) }))

  setInterval(() => {
    for (const [name, x] of perf) {
      tel('perf', {
        page: name, long_n: x.longN, long_max: Math.round(x.longMax), jank_n: x.jankN, jank_max: Math.round(x.jankMax),
        slow_n: x.slowN, slow_max: Math.round(x.slowMax), slow_events: [...x.names], slow_target: x.target,
      })
    }
    perf.clear()
  }, 30000)

  setInterval(() => {
    if (!queue.length) return
    const events = queue
    queue = []
    invoke('tel', { events }).catch(() => {})
  }, 5000)

  void env()
}

/** 环境信息（界面这边能拿到的）：屏幕、缩放、刷新率、WebView2 版本、Windows 版本、界面 GPU。
 *  刷新率在启动 10 秒后、主线程空闲时测 60 帧取中位数：启动时首页正在加载，帧间隔会被拉长。 */
async function env() {
  await new Promise((r) => setTimeout(r, 10000))
  await new Promise((r) => ('requestIdleCallback' in globalThis ? requestIdleCallback(r, { timeout: 5000 }) : setTimeout(r, 0)))
  const gaps: number[] = []
  await new Promise<void>((done) => {
    let prev = 0
    const f = (t: number) => {
      if (prev) gaps.push(t - prev)
      prev = t
      if (gaps.length < 60) requestAnimationFrame(f)
      else done()
    }
    requestAnimationFrame(f)
  })
  gaps.sort((a, b) => a - b)
  type UAData = { getHighEntropyValues(h: string[]): Promise<{ platformVersion?: string }> }
  const ua = (navigator as Navigator & { userAgentData?: UAData }).userAgentData
  const platform = await ua?.getHighEntropyValues(['platformVersion']).then((v) => v.platformVersion, () => undefined)
  tel('env', {
    screen: `${screen.width}x${screen.height}`,
    dpr: devicePixelRatio,
    hz: Math.round(1000 / gaps[gaps.length >> 1]),
    webview: /Edg\/([\d.]+)/.exec(navigator.userAgent)?.[1] ?? navigator.userAgent.slice(0, 120),
    // Windows 11 的 platformVersion 主版本 ≥ 13
    platform,
    gpu: gpuName(),
    cores: navigator.hardwareConcurrency,
  })
}
