<!-- Settings split into tabs on a side rail, plain rows with hairline dividers (no cards): servers (add / switch / edit /
     sign in again / delete, address and port apart), playback (mpv path, next-episode preload, personal mpv.conf, keys),
     cache (whole-file cache, cache management), danmaku (API, blocked words; display options live in the player's panel),
     about (version and updates, update proxy, which GPU renders the UI). -->
<script lang="ts" module>
  /** Last tab shown, kept while the app runs so coming back to settings lands where you left. */
  let lastTab = 'server'
</script>

<script lang="ts">
  import { invoke } from '@tauri-apps/api/core'
  import { listen } from '@tauri-apps/api/event'
  import { Check, Download, Eye, EyeOff, HardDrive, Info, LogIn, MessageSquareText, MonitorPlay, Pencil, Plus, RefreshCw, Server, Trash2 } from '@lucide/svelte'
  import { errText, loadConfig, session } from '../lib/api.svelte'
  import { go } from '../lib/router.svelte'
  import { ask } from '../lib/Dialog.svelte'
  import CacheDialog from '../lib/CacheDialog.svelte'

  const tabs = [
    { id: 'server', label: '服务器', icon: Server },
    { id: 'play', label: '播放', icon: MonitorPlay },
    { id: 'cache', label: '缓存', icon: HardDrive },
    { id: 'danmaku', label: '弹幕', icon: MessageSquareText },
    { id: 'about', label: '关于', icon: Info },
  ]
  // First run has no server yet: the add form is the only thing that matters
  let tab = $state(session.cfg?.servers.length ? lastTab : 'server')
  function pick(id: string) {
    tab = lastTab = id
    msg = null // a result belongs to the tab it came from
  }
  /** Up / down (and Home / End) move between tabs, as in a native tab list; focus follows the selection. */
  function tabKey(e: KeyboardEvent) {
    const i = tabs.findIndex((t) => t.id === tab)
    const next = { ArrowDown: i + 1, ArrowUp: i - 1, Home: 0, End: tabs.length - 1 }[e.key]
    if (next === undefined) return
    e.preventDefault()
    const t = tabs[(next + tabs.length) % tabs.length]
    pick(t.id)
    document.getElementById(`tab-${t.id}`)?.focus()
  }
  const keys = [
    ['空格', '暂停 / 继续'], ['← →', '快退 / 快进 5 秒'], ['↑ ↓', '音量'], ['F', '全屏'],
    ['< >', '上一集 / 下一集'], ['F8', '选集'], ['D', '弹幕开关'], ['Q', '退出播放'],
  ]

  const blank = { url: '', port: '', user: '', password: '' }
  let form = $state({ ...blank })
  /** Id of the server being edited; shares `form` with adding. */
  let editing = $state<string | null>(null)
  let relogin = $state<{ id: string; password: string } | null>(null)
  /** Changes in flight; a count, as a save on blur can overlap a button click. */
  let busy = $state(0)
  let msg = $state<{ text: string; ok: boolean } | null>(null)
  let mpvPath = $state(session.cfg?.mpvPath ?? '')
  let preload = $state(session.cfg?.preloadPercent ?? 80)
  let updateProxy = $state(session.cfg?.updateProxy ?? '')
  let cacheWhole = $state(session.cfg?.cacheWhole ?? false)
  let cacheOpen = $state(false)
  let dmApi = $state(session.cfg?.danmakuApi ?? '')
  let dmWords = $state((session.cfg?.danmakuWords ?? []).join('\n'))
  /** The API address may carry a token: masked by default so it does not end up in screenshots. */
  let dmShow = $state(false)
  let adding = $state(!session.cfg?.servers.length)

  /** Apply a change, then reload the config (other pages are unmounted here and refetch when shown).
   *  Resolves to whether it worked; a failure shows the backend's reason and leaves the inputs as typed. */
  async function run(f: () => Promise<unknown>, ok?: string) {
    busy++
    msg = null
    try {
      await f()
      await loadConfig()
      if (ok) msg = { text: ok, ok: true }
      return true
    } catch (e) {
      msg = { text: errText(e), ok: false }
      return false
    } finally {
      busy--
    }
  }

  async function saveDm() {
    if (!(await run(() => invoke('danmaku_set', { api: dmApi, words: dmWords.split('\n') }), '已保存，下次播放生效'))) return
    // Show the backend's normalized values (trimmed, no trailing /)
    dmApi = session.cfg?.danmakuApi ?? ''
    dmWords = (session.cfg?.danmakuWords ?? []).join('\n')
  }

  function formPort() {
    const port = form.port.trim() ? Number(form.port) : null
    if (port !== null && !(port > 0 && port < 65536)) throw { message: '端口应为 1–65535' }
    return port
  }
  // Go home only once the new config is loaded, or Home would mount against the previous server first
  async function add() {
    const added = run(async () => {
      await invoke('server_add', { url: form.url, port: formPort(), user: form.user, password: form.password })
      form = { ...blank } // saved now, even if reloading the config fails: submitting again would add it twice
    })
    if (await added) go('/')
  }
  function startEdit(s: { id: string; url: string; port: number; user: string }) {
    form = { url: s.url, port: String(s.port), user: s.user, password: '' }
    editing = s.id
    adding = false
    relogin = null
  }
  function startAdd() {
    form = { ...blank }
    editing = null
    adding = true
  }
  /** Address-only edits keep the token (no password needed); a new user name or password signs in again. */
  async function save(id: string) {
    if (!(await run(() => invoke('server_edit', { id, url: form.url, port: formPort(), user: form.user, password: form.password }), '已保存'))) return
    editing = null
    form = { ...blank }
  }
  async function login() {
    const r = relogin
    if (r && (await run(() => invoke('server_login', { id: r.id, password: r.password }), '已重新登录'))) relogin = null
  }
  const use = async (id: string) => (await run(() => invoke('server_use', { id }))) && go('/')
  const remove = async (id: string, name: string) =>
    (await ask('删除服务器', `删除「${name}」？登录信息会一起删掉。`, '删除', true)) && run(() => invoke('server_remove', { id }))
  /** Which GPU renders the UI (WebGL renderer name), to tell a janky UI on integrated graphics or software rendering apart. */
  function gpuName() {
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
  const gpu = gpuName()
  async function savePlayer() {
    if (!(await run(() => invoke('settings_set', { mpvPath: mpvPath || null, preloadPercent: preload, updateProxy: updateProxy || null, cacheWhole }), '已保存'))) return
    // Show the backend's normalized values (quotes and whitespace stripped)
    mpvPath = session.cfg?.mpvPath ?? ''
    updateProxy = session.cfg?.updateProxy ?? ''
  }

  /** Update check: idle → checking → latest / found (version and notes) → installing (progress); errors fall back with the reason. */
  let upd = $state<{ step: 'idle' | 'checking' | 'latest' | 'found' | 'installing'; version?: string; notes?: string; pct?: number; error?: string }>({ step: 'idle' })
  async function checkUpdate() {
    upd = { step: 'checking' }
    try {
      const r = await invoke<{ version: string; notes: string } | null>('update_check')
      upd = r ? { step: 'found', ...r } : { step: 'latest' }
    } catch (e) {
      upd = { step: 'idle', error: errText(e) }
    }
  }
  async function install() {
    const { version, notes } = upd
    upd = { step: 'installing', version, notes, pct: 0 }
    const off = await listen<[number, number | null]>('update-progress', (e) => {
      const [got, total] = e.payload
      if (total) upd.pct = Math.min(100, Math.round((got / total) * 100))
    })
    try {
      await invoke('update_install') // on success the app exits and the installer reopens it
    } catch (e) {
      upd = { step: 'found', version, notes, error: errText(e) }
    } finally {
      off()
    }
  }
</script>

<div class="page">
  <div class="page-title"><h1>设置</h1></div>

  <div class="layout">
    <!-- svelte-ignore a11y_interactive_supports_focus -->
    <div class="rail" role="tablist" aria-orientation="vertical" onkeydown={tabKey}>
      {#each tabs as t (t.id)}
        <button id="tab-{t.id}" role="tab" class="tab" class:on={tab === t.id} aria-selected={tab === t.id} tabindex={tab === t.id ? 0 : -1} onclick={() => pick(t.id)}>
          <t.icon size={17} /> {t.label}
        </button>
      {/each}
    </div>

    <div class="panel" role="tabpanel" aria-labelledby="tab-{tab}">
      {#if tab === 'server'}
        <h2>服务器</h2>
        {#snippet serverForm(submit: () => void, label: string, pwHint: string, cancel: (() => void) | null)}
          <form class="form" onsubmit={(e) => { e.preventDefault(); submit() }}>
            <input class="url" placeholder="地址，如 https://emby.example.com" bind:value={form.url} required spellcheck="false" />
            <input placeholder="端口（默认按协议）" bind:value={form.port} inputmode="numeric" />
            <input placeholder="用户名" bind:value={form.user} required autocomplete="username" spellcheck="false" />
            <input type="password" placeholder={pwHint} bind:value={form.password} autocomplete="current-password" />
            <div class="btns">
              <button class="btn primary" disabled={busy > 0}>{label}</button>
              {#if cancel}<button type="button" class="btn ghost" onclick={cancel}>取消</button>{/if}
            </div>
          </form>
        {/snippet}
        {#each session.cfg?.servers ?? [] as s (s.id)}
          {#if editing === s.id}
            <div class="row block">{@render serverForm(() => save(s.id), '保存', '密码（只改地址可不填）', () => (editing = null))}</div>
          {:else}
            {@const current = s.id === session.cfg?.active}
            <div class="row server">
              <span class="dot" class:on={current} title={current ? '当前使用' : ''}></span>
              <div class="text">
                <div class="title">{s.name}{#if current}<span class="cur">当前使用</span>{/if}</div>
                <div class="desc selectable">{s.url}:{s.port} · {s.user} · <span class:warn={!s.loggedIn}>{s.loggedIn ? '已登录' : '需要重新登录'}</span></div>
              </div>
              <div class="ctl">
                {#if relogin?.id === s.id}
                  <!-- svelte-ignore a11y_autofocus -->
                  <input type="password" placeholder="密码" bind:value={relogin.password} autofocus onkeydown={(e) => (e.key === 'Enter' && login(), e.key === 'Escape' && (relogin = null))} />
                  <button class="btn primary" disabled={busy > 0} onclick={login}>登录</button>
                  <button class="btn ghost" onclick={() => (relogin = null)}>取消</button>
                {:else}
                  {#if !current}<button class="btn" disabled={busy > 0} onclick={() => use(s.id)}><Check size={15} /> 使用</button>{/if}
                  <button class="btn ghost" disabled={busy > 0} onclick={() => startEdit(s)}><Pencil size={15} /> 编辑</button>
                  <button class="btn ghost" disabled={busy > 0} onclick={() => ((relogin = { id: s.id, password: '' }), (editing = null))}><LogIn size={15} /> 重新登录</button>
                  <button class="btn ghost icon danger" title="删除" disabled={busy > 0} onclick={() => remove(s.id, s.name)}><Trash2 size={15} /></button>
                {/if}
              </div>
            </div>
          {/if}
        {/each}
        {#if adding}
          <div class="row block">
            <div class="title">添加服务器</div>
            {@render serverForm(add, '添加并登录', '密码', session.cfg?.servers.length ? () => (adding = false) : null)}
          </div>
        {:else}
          <div class="row"><button class="btn ghost add" onclick={startAdd}><Plus size={16} /> 添加服务器</button></div>
        {/if}

      {:else if tab === 'play'}
        <h2>播放</h2>
        <label class="row">
          <div class="text">
            <div class="title">mpv 路径</div>
            <div class="desc">换用自己装的 mpv 时填 mpv.exe 的完整路径</div>
          </div>
          <input class="ctl field" placeholder="留空使用自带的 mpv" bind:value={mpvPath} spellcheck="false" onchange={savePlayer} />
        </label>
        <label class="row">
          <div class="text">
            <div class="title">预加载下一集</div>
            <div class="desc">播放到这个进度时，在后台准备好下一集</div>
          </div>
          <div class="ctl">
            <input class="range" type="range" min="50" max="99" bind:value={preload} onchange={savePlayer} style="--p: {((preload - 50) / 49) * 100}%" />
            <span class="val">{preload}%</span>
          </div>
        </label>
        <div class="row">
          <div class="text">
            <div class="title">个人 mpv 配置</div>
            <div class="desc">自己新建这个文件即可，写法同 mpv.conf（如 <code>demuxer-max-bytes=512MiB</code> 调缓冲）。同名选项以它为准，升级不覆盖，下次播放生效</div>
            <div class="path selectable">{session.cfg?.userMpvConf}</div>
          </div>
        </div>
        <div class="row">
          <div class="text">
            <div class="title">渲染</div>
            <div class="desc">mpv 优先使用 NVIDIA 独显（D3D11 + 硬解）；播放时按 i 可查看实际使用的解码方式</div>
          </div>
        </div>
        <h3>快捷键</h3>
        <dl class="keys">
          {#each keys as [k, what] (k)}<dt><kbd>{k}</kbd></dt><dd>{what}</dd>{/each}
        </dl>

      {:else if tab === 'cache'}
        <h2>缓存</h2>
        <label class="row">
          <div class="text">
            <div class="title">缓存整个视频</div>
            <div class="desc">播放时在后台把整个文件存到本地：拖动进度、进度条预览、重看不用再等网络；只保留最近播放的那一集，最多 20 GB。会多用流量：看一半关掉时，后面没看的部分也已经下了。不开时只缓存播放位置前方一段</div>
          </div>
          <input class="ctl switch" type="checkbox" role="switch" bind:checked={cacheWhole} onchange={savePlayer} />
        </label>
        <div class="row">
          <div class="text">
            <div class="title">缓存管理</div>
            <div class="desc">视频、图片、字幕、媒体库和临时缓存的占用，可以单独清理</div>
          </div>
          <button class="ctl btn" onclick={() => (cacheOpen = true)}><HardDrive size={14} /> 管理</button>
        </div>

      {:else if tab === 'danmaku'}
        <h2>弹幕</h2>
        <div class="row block">
          <div class="text">
            <div class="title">弹幕接口</div>
            <div class="desc">支持 <code>/api/v2</code> 格式的弹幕接口，地址里可以带令牌，只保存在本机</div>
          </div>
          <div class="line">
            <input class="wide" type={dmShow ? 'text' : 'password'} placeholder="粘贴弹幕接口地址，留空不加载弹幕" bind:value={dmApi} spellcheck="false" autocomplete="off" onchange={saveDm} />
            <button class="btn ghost icon" title={dmShow ? '隐藏地址' : '显示地址'} onclick={() => (dmShow = !dmShow)}>{#if dmShow}<EyeOff size={15} />{:else}<Eye size={15} />{/if}</button>
          </div>
        </div>
        <label class="row block">
          <div class="text">
            <div class="title">屏蔽词</div>
            <div class="desc">一行一个，含有这些字的弹幕不显示（不分大小写）</div>
          </div>
          <textarea rows="5" bind:value={dmWords} spellcheck="false" onchange={saveDm}></textarea>
        </label>
        <p class="note">播放时按 D 开关弹幕，点控制栏的「弹」也可以；右键「弹」打开弹幕设置（不透明度、显示区域、字号、速度、屏蔽类型、时间偏移、手动匹配）</p>

      {:else}
        <h2>关于</h2>
        <div class="row">
          <div class="text">
            <div class="title">EmbyMini {session.cfg?.version}</div>
            {#if upd.step === 'latest'}<div class="desc">已是最新版本</div>
            {:else if upd.step === 'found' || upd.step === 'installing'}
              <div class="desc">新版本 {upd.version}{#if upd.step === 'installing'}：正在下载 {upd.pct}%，下完会自动安装并重新打开{/if}</div>
              {#if upd.notes}<div class="desc notes selectable">{upd.notes}</div>{/if}
            {:else}<div class="desc">检查有没有新版本</div>
            {/if}
            {#if upd.error}<div class="desc err">{upd.error}</div>{/if}
          </div>
          {#if upd.step === 'found'}
            <button class="ctl btn primary" onclick={install}><Download size={14} /> 立即更新</button>
          {:else}
            <button class="ctl btn" disabled={upd.step === 'checking' || upd.step === 'installing'} onclick={checkUpdate}><RefreshCw size={14} /> {upd.step === 'checking' ? '检查中…' : '检查更新'}</button>
          {/if}
        </div>
        <label class="row">
          <div class="text">
            <div class="title">更新代理</div>
            <div class="desc">只用于检查更新和下载安装包，其它请求都直连</div>
          </div>
          <input class="ctl field" placeholder="留空跟随系统代理，如 http://127.0.0.1:10808" bind:value={updateProxy} spellcheck="false" onchange={savePlayer} />
        </label>
        <div class="row">
          <div class="text">
            <div class="title">界面渲染</div>
            <div class="desc selectable">{gpu}</div>
          </div>
        </div>
      {/if}

      {#if msg}<p class="msg" class:err={!msg.ok}>{msg.text}</p>{/if}
    </div>
  </div>
</div>
{#if cacheOpen}<CacheDialog onclose={() => (cacheOpen = false)} />{/if}

<style>
  .page { max-width: 1040px; margin: 0 auto; }
  .layout { display: grid; grid-template-columns: 188px minmax(0, 1fr); gap: 36px; padding: 0 var(--gutter); }

  /* Side rail (macOS System Settings style): the selected tab is filled with the accent, text and icon white */
  .rail { display: flex; flex-direction: column; gap: 2px; position: sticky; top: calc(var(--bar-h) + 12px); align-self: start; }
  .tab {
    display: flex; align-items: center; gap: 10px; height: 34px; padding: 0 10px;
    border-radius: 7px; color: var(--text); text-align: left; transition: background 0.12s, color 0.12s;
  }
  .tab:hover { background: var(--fill); color: var(--text); }
  .tab:active { background: var(--fill-hover); }
  .tab.on { background: var(--accent); color: var(--accent-text); font-weight: 500; }

  .panel { min-width: 0; animation: panel-in 0.16s var(--ease); }
  @keyframes panel-in { from { opacity: 0; transform: translateY(4px); } }
  h2 { font-size: 20px; font-weight: 600; margin: 4px 0 6px; }
  h3 { font-size: 13px; font-weight: 600; color: var(--text-2); margin: 28px 0 10px; }

  /* Rows: text on the left, the control on the right, hairline between rows; nothing boxed */
  .row { display: flex; align-items: center; gap: 24px; min-height: 64px; padding: 14px 0; border-bottom: 1px solid var(--stroke); }
  .row.block { flex-direction: column; align-items: stretch; gap: 10px; }
  .text { flex: 1; min-width: 0; }
  .title { display: flex; align-items: center; gap: 8px; }
  .desc { font-size: 12px; color: var(--text-2); margin-top: 3px; line-height: 1.6; }
  .path { font-size: 12px; margin-top: 6px; font-family: ui-monospace, Consolas, monospace; color: var(--text); }
  .ctl { flex: none; display: flex; align-items: center; gap: 6px; }
  .field { width: 320px; }
  .range { width: 220px; }
  .val { width: 40px; text-align: right; font-variant-numeric: tabular-nums; color: var(--text-2); }

  .server { gap: 14px; }
  .dot { width: 8px; height: 8px; border-radius: 50%; flex: none; box-shadow: inset 0 0 0 1.5px var(--stroke-strong); }
  .dot.on { background: var(--accent); box-shadow: none; }
  .cur { font-size: 12px; color: var(--accent); font-weight: 500; }
  .title:has(.cur) { font-weight: 600; }
  .warn { color: var(--danger); }
  .add { color: var(--accent); margin-left: -12px; }

  .form { display: grid; grid-template-columns: repeat(3, minmax(0, 1fr)); gap: 10px; max-width: 640px; }
  .form .url, .form .btns { grid-column: 1 / -1; }
  .btns { display: flex; gap: 8px; }

  input:not([type='range']):not([type='checkbox']), textarea {
    border: 1px solid var(--field-border); border-radius: 7px; padding: 0 10px; background: var(--field-bg); min-width: 0;
    box-shadow: var(--field-shade); transition: border-color 0.15s, box-shadow 0.15s;
  }
  input:not([type='range']):not([type='checkbox']) { height: 34px; }
  input::placeholder, textarea::placeholder { color: var(--text-3); }
  input:not([type='range']):not([type='checkbox']):focus, textarea:focus { outline: none; border-color: var(--accent); box-shadow: 0 0 0 3.5px var(--ring); }
  /* macOS slider: thin track filled with the accent up to the value (--p, set inline), white knob */
  input[type='range'] {
    appearance: none; height: 4px; border-radius: 2px; margin: 0;
    background: linear-gradient(to right, var(--accent) var(--p), var(--track) var(--p));
  }
  input[type='range']::-webkit-slider-thumb {
    appearance: none; width: 20px; height: 20px; border-radius: 50%; background: #fff;
    box-shadow: 0 0 0 0.5px rgba(0, 0, 0, 0.12), 0 1px 3px rgba(0, 0, 0, 0.3); transition: transform 0.12s var(--ease);
  }
  input[type='range']:active::-webkit-slider-thumb { transform: scale(1.1); }
  input.wide { flex: 1; }
  .line { display: flex; gap: 6px; align-items: center; }
  textarea { width: 100%; resize: vertical; padding: 8px 10px; line-height: 1.6; }

  /* macOS switch drawn on the checkbox itself, so label clicks and keyboard keep working: grey track, white knob */
  .switch {
    appearance: none; position: relative; width: 38px; height: 22px; border-radius: 11px; margin: 0;
    background: var(--track); transition: background 0.18s;
  }
  .switch::after {
    content: ''; position: absolute; left: 2px; top: 2px; width: 18px; height: 18px; border-radius: 50%;
    background: #fff; box-shadow: 0 0 0 0.5px rgba(0, 0, 0, 0.08), 0 1px 2px rgba(0, 0, 0, 0.25);
    transition: transform 0.2s var(--ease);
  }
  .switch:checked { background: var(--accent); }
  .switch:checked::after { transform: translateX(16px); }
  .switch:focus-visible { border-radius: 11px; }

  .keys { display: grid; grid-template-columns: auto 1fr auto 1fr; gap: 10px 14px; align-items: center; margin: 0; max-width: 560px; }
  .keys dd { margin: 0; color: var(--text-2); font-size: 13px; }
  kbd {
    display: inline-block; min-width: 26px; padding: 2px 7px; border-radius: 5px; text-align: center;
    font: 12px ui-monospace, Consolas, monospace; background: var(--fill); box-shadow: inset 0 -1px 0 var(--stroke-strong);
  }

  code { font-family: ui-monospace, Consolas, monospace; font-size: 11px; padding: 0 4px; border-radius: 4px; background: var(--fill); }
  .note { margin: 14px 0 0; font-size: 12px; color: var(--text-3); line-height: 1.7; }
  .msg { margin: 16px 0 0; color: var(--text-2); }
  .err { color: var(--danger); }
  .notes { white-space: pre-wrap; max-height: 160px; overflow: auto; margin-top: 6px; }
</style>
