// 只在 `vite dev` 且页面不在 Tauri 里时生效：代替 Rust 后端处理 invoke，把请求转发到真服。
// 用途：在没有 Windows 的机器上用浏览器开发界面、用 Playwright 截图。
// 账号来自启动 vite 时的环境变量（EMBY_SERVER / PORT / EMBY_USER / EMBY_PASS），token 只留在 Node 进程里。
import type { Plugin } from 'vite'
import type { IncomingMessage } from 'node:http'

const UA = 'EmbyMini/0.1.0-dev'
const auth = (uid = '', token = '') =>
  `Emby UserId="${uid}", Client="EmbyMini", Device="dev-bridge", DeviceId="embymini-dev-bridge", Version="0.1.0"` +
  (token ? `, Token="${token}"` : '')

type Session = { base: string; url: string; port: number; userId: string; token: string; name: string }

async function login(): Promise<Session> {
  const env = (k: string) => process.env[k] ?? ''
  const u = new URL(env('EMBY_SERVER'))
  const port = Number(env('PORT') || u.port || (u.protocol === 'https:' ? 443 : 80))
  u.port = String(port)
  const base = `${u.origin}${u.pathname.replace(/\/$/, '')}/emby`
  const r = await fetch(`${base}/Users/AuthenticateByName`, {
    method: 'POST',
    headers: { 'User-Agent': UA, 'X-Emby-Authorization': auth(), 'Content-Type': 'application/json' },
    body: JSON.stringify({ Username: env('EMBY_USER'), Pw: env('EMBY_PASS') }),
  })
  if (!r.ok) throw new Error(`dev bridge 登录失败：${r.status}`)
  const j = (await r.json()) as { User: { Id: string }; AccessToken: string }
  const info = (await (await fetch(`${base}/System/Info/Public`, { headers: { 'User-Agent': UA } })).json()) as { ServerName: string }
  return { base, url: env('EMBY_SERVER'), port, userId: j.User.Id, token: j.AccessToken, name: info.ServerName }
}

function readJson(req: IncomingMessage): Promise<Record<string, any>> {
  return new Promise((ok, fail) => {
    let s = ''
    req.on('data', (c) => (s += c))
    req.on('end', () => {
      try { ok(s ? JSON.parse(s) : {}) } catch (e) { fail(e) }
    })
  })
}

export function devBridge(): Plugin {
  let session: Promise<Session> | null = null
  /** 诊断数据开关只记在内存里；收到的事件打印到 vite 终端，用来核对界面埋点。 */
  let telOn = false
  return {
    name: 'embymini-dev-bridge',
    apply: 'serve',
    configureServer(server) {
      server.middlewares.use('/__dev/', async (req, res) => {
        const reply = (status: number, v: unknown) => {
          res.statusCode = status
          res.setHeader('content-type', 'application/json')
          res.end(JSON.stringify(v ?? null))
        }
        try {
          // 登录失败（上游 502 之类）不缓存，下一个请求重试，免得要重启 vite
          session ??= login().catch((e) => ((session = null), Promise.reject(e)))
          const s = await session
          const args = await readJson(req)
          const cmd = (req.url ?? '').replace(/^\//, '')
          if (cmd === 'config_get') {
            return reply(200, {
              active: 'dev', mpvPath: null, preloadPercent: 80, userMpvConf: 'D:\\EmbyMini\\data\\mpv.conf', updateProxy: null, cacheWhole: true, version: 'dev', telemetryAvailable: true, telemetry: telOn, telemetryName: '',
              servers: [{ id: 'dev', name: s.name, url: s.url, port: s.port, user: process.env.EMBY_USER, userId: s.userId, base: s.base, tag: 'dev', loggedIn: true }],
            })
          }
          if (cmd === 'emby') {
            const q = new URLSearchParams()
            for (const [k, v] of Object.entries(args.query ?? {})) if (v != null) q.set(k, String(v))
            const r = await fetch(`${s.base}${args.path}${q.size ? `?${q}` : ''}`, {
              method: args.method,
              headers: { 'User-Agent': UA, 'X-Emby-Authorization': auth(s.userId, s.token), 'X-Emby-Token': s.token, 'Content-Type': 'application/json' },
              body: args.body ? JSON.stringify(args.body) : undefined,
            })
            const text = await r.text()
            if (!r.ok) return reply(500, { kind: r.status === 401 ? 'unauthorized' : 'status', message: `服务器返回 ${r.status}：${text.slice(0, 200)}` })
            return reply(200, text ? JSON.parse(text) : null)
          }
          if (cmd === 'telemetry_set') telOn = Boolean(args.on)
          // 缓存都在 Rust 里，浏览器里没有：给一组假数字，看弹窗样式用
          if (cmd === 'cache_info' || cmd === 'cache_clear') {
            const z = args.kind === 'all'
            return reply(200, { video: z ? 0 : 3.2 * 2 ** 30, images: z ? 0 : 446.3 * 2 ** 20, subs: 0, temp: z ? 0 : 483 * 1024, playing: false })
          }
          if (cmd === 'tel') {
            for (const e of args.events ?? []) console.log('[dev-bridge] tel', JSON.stringify(e))
            return reply(200, null)
          }
          // play / server_* / settings_set：浏览器里没有 mpv，也不改本地配置，只记一笔（密码不打印）
          console.log(`[dev-bridge] ${cmd}`, JSON.stringify(args, (k, v) => (k === 'password' ? '***' : v)))
          return reply(200, null)
        } catch (e) {
          return reply(500, { kind: 'other', message: String(e) })
        }
      })
    },
  }
}
