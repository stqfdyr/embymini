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

/** 用户的一次操作（播放、收藏、菜单项…），只记名字。 */
export const op = (name: string) => tel('op', { name })

export function setTel(enabled: boolean) {
  on = enabled
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

let page = { name: '', t0: 0, ready: false }

export function pageStart(name: string) {
  page = { name, t0: performance.now(), ready: false }
}

export function pageReady() {
  if (!on || page.ready) return
  page.ready = true
  const p = page
  const ready = Math.round(performance.now() - p.t0)
  // 视口内的图片都加载完（或 15 秒放弃）算首屏完成
  const t = setInterval(() => {
    const inView = [...document.images].filter((i) => {
      const r = i.getBoundingClientRect()
      return r.bottom > 0 && r.top < innerHeight && r.width > 0 && i.src
    })
    const done = inView.every((i) => i.complete)
    const ms = performance.now() - p.t0
    if (done || ms > 15000) {
      clearInterval(t)
      if (page === p) tel('page', { name: p.name, ready_ms: ready, imgs_ms: Math.round(ms), imgs: inView.length, timeout: !done })
    }
  }, 100)
}

// ---------- 聚合统计（每 30 秒一条） ----------

const pct = (sorted: number[], p: number) => (sorted.length ? Math.round(sorted[Math.min(sorted.length - 1, Math.floor(sorted.length * p))]) : 0)

function install() {
  const imgs: { ms: number; type: string; w: number }[] = []
  let imgFail = 0
  let longN = 0, longMax = 0
  let jankN = 0, jankMax = 0
  let slowN = 0, slowMax = 0
  const slowNames = new Set<string>()

  const observe = (type: string, f: (e: PerformanceEntry) => void, extra: object = {}) => {
    try {
      new PerformanceObserver((l) => l.getEntries().forEach(f)).observe({ type, buffered: true, ...extra } as PerformanceObserverInit)
    } catch {
      /* 不支持这类条目就不记 */
    }
  }
  // 图片耗时。海报是跨域加载的，浏览器不给协议和传输字节数（没有 Timing-Allow-Origin），只有总耗时；
  // 所以「命中磁盘缓存」按耗时判断（< 10 ms），统计分位数时去掉这些
  observe('resource', (e) => {
    const r = e as PerformanceResourceTiming
    if (r.initiatorType !== 'img') return
    const m = /\/Images\/(\w+)\?.*maxWidth=(\d+)/.exec(r.name)
    imgs.push({ ms: r.duration, type: m?.[1] ?? '', w: Number(m?.[2] ?? 0) })
  })
  addEventListener('error', (e) => e.target instanceof HTMLImageElement && imgFail++, true)
  // 主线程长任务（>50 ms）
  observe('longtask', (e) => ((longN++), (longMax = Math.max(longMax, e.duration))))
  // 交互延迟（点击到画面更新 >100 ms）
  observe('event', (e) => {
    if (e.duration < 100) return
    slowN++
    slowMax = Math.max(slowMax, e.duration)
    slowNames.add(e.name)
  }, { durationThreshold: 104 })
  // 滚动掉帧：滚动期间帧间隔 >50 ms
  let scrolling = 0, last = 0
  const frame = (t: number) => {
    if (last && t - last > 50) ((jankN++), (jankMax = Math.max(jankMax, t - last)))
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
    if (imgs.length || imgFail) {
      const fresh = imgs.filter((i) => i.ms >= 10)
      const ms = fresh.map((i) => i.ms).sort((a, b) => a - b)
      const slow = [...fresh].sort((a, b) => b.ms - a.ms).slice(0, 5).map((i) => ({ type: i.type, w: i.w, ms: Math.round(i.ms) }))
      tel('img', { n: imgs.length, cached: imgs.length - fresh.length, fail: imgFail, p50: pct(ms, 0.5), p90: pct(ms, 0.9), p99: pct(ms, 0.99), max: pct(ms, 1), slow })
      imgs.length = 0
      imgFail = 0
    }
    if (longN || jankN || slowN) {
      tel('perf', { long_n: longN, long_max: Math.round(longMax), jank_n: jankN, jank_max: Math.round(jankMax), slow_n: slowN, slow_max: Math.round(slowMax), slow_events: [...slowNames] })
      longN = longMax = jankN = jankMax = slowN = slowMax = 0
      slowNames.clear()
    }
  }, 30000)

  setInterval(() => {
    if (!queue.length) return
    const events = queue
    queue = []
    invoke('tel', { events }).catch(() => {})
  }, 5000)

  void env()
}

/** 环境信息（界面这边能拿到的）：屏幕、缩放、刷新率、WebView2 版本、Windows 版本、界面 GPU。 */
async function env() {
  const gaps: number[] = []
  await new Promise<void>((done) => {
    let prev = 0
    const f = (t: number) => {
      if (prev) gaps.push(t - prev)
      prev = t
      if (gaps.length < 30) requestAnimationFrame(f)
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
