// Active only under `vite dev` with the page outside Tauri: stands in for the Rust backend and forwards invoke to a real server,
// so the UI can be developed and screenshot with Playwright on a machine without Windows.
// Credentials come from vite's environment (EMBY_SERVER / PORT / EMBY_USER / EMBY_PASS); the token stays in this Node process.
// Dev-only: error wording is simpler than the app's (`status::describe`) on purpose. Keep vite on localhost: this is an authenticated proxy.
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
  // A failed sign-in (upstream 502…) is not cached: the next request retries without restarting vite
  const signIn = () => (session ??= login().catch((e) => ((session = null), Promise.reject(e))))
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
          const mine = signIn()
          const s = await mine
          const args = await readJson(req)
          const cmd = (req.url ?? '').replace(/^\//, '')
          if (cmd === 'config_get') {
            return reply(200, {
              active: 'dev', mpvPath: null, preloadPercent: 80, userMpvConf: 'D:\\EmbyMini\\data\\mpv.conf', updateProxy: null, cacheWhole: true, danmakuApi: '', danmakuWords: [], version: 'dev',
              servers: [{ id: 'dev', name: s.name, url: s.url, port: s.port, user: process.env.EMBY_USER, userId: s.userId, base: s.base, loggedIn: true }],
            })
          }
          if (cmd === 'emby') {
            const q = new URLSearchParams()
            for (const [k, v] of Object.entries(args.query ?? {})) if (v != null) q.set(k, String(v))
            const forward = (t: Session) =>
              fetch(`${t.base}${args.path}${q.size ? `?${q}` : ''}`, {
                method: args.method,
                headers: { 'User-Agent': UA, 'X-Emby-Authorization': auth(t.userId, t.token), 'X-Emby-Token': t.token, 'Content-Type': 'application/json' },
                body: args.body ? JSON.stringify(args.body) : undefined,
              })
            let r = await forward(s)
            if (r.status === 401) {
              // Token expired: sign in again (unless another request already did) and retry once
              if (session === mine) session = null
              r = await forward(await signIn())
            }
            const text = await r.text()
            if (!r.ok) return reply(500, { kind: r.status === 401 ? 'unauthorized' : 'status', message: `服务器返回 ${r.status}：${text.slice(0, 200)}` })
            return reply(200, text ? JSON.parse(text) : null)
          }
          // The caches live in Rust: fake numbers, enough to style the dialog
          if (cmd === 'cache_info' || cmd === 'cache_clear') {
            const z = args.kind === 'all'
            return reply(200, { video: z ? 0 : 3.2 * 2 ** 30, images: z ? 0 : 446.3 * 2 ** 20, subs: 0, temp: z ? 0 : 483 * 1024, playing: false })
          }
          // play / server_* / settings_set / danmaku_set: no mpv and no local config here, just log (password and danmaku API, which may hold a token, masked)
          console.log(`[dev-bridge] ${cmd}`, JSON.stringify(args, (k, v) => (k === 'password' || k === 'api' ? '***' : v)))
          return reply(200, null)
        } catch (e) {
          return reply(500, { kind: 'other', message: String(e) })
        }
      })
    },
  }
}
