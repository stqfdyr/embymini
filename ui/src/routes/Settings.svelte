<!-- 设置：服务器（添加 / 切换 / 编辑 / 重新登录 / 删除，地址与端口分开填）+ 播放（mpv 路径、预加载进度、边播边缓存、缓存管理、个人 mpv 配置的位置）
     + 关于（版本与检查更新、更新代理、界面用哪块 GPU）。分组列表样式。 -->
<script lang="ts">
  import { invoke } from '@tauri-apps/api/core'
  import { listen } from '@tauri-apps/api/event'
  import { Check, Download, HardDrive, LogIn, Pencil, Plus, RefreshCw, Trash2 } from '@lucide/svelte'
  import { errText, loadConfig, refresh, session } from '../lib/api.svelte'
  import { go } from '../lib/router.svelte'
  import { ask } from '../lib/Dialog.svelte'
  import CacheDialog from '../lib/CacheDialog.svelte'
  import { gpuName } from '../lib/tel'

  const blank = { url: '', port: '', user: '', password: '' }
  let form = $state({ ...blank })
  /** 正在编辑的服务器 id；和「添加」共用 form。 */
  let editing = $state<string | null>(null)
  let relogin = $state<{ id: string; password: string } | null>(null)
  let busy = $state(false)
  let msg = $state<{ text: string; ok: boolean } | null>(null)
  let mpvPath = $state(session.cfg?.mpvPath ?? '')
  let preload = $state(session.cfg?.preloadPercent ?? 80)
  let updateProxy = $state(session.cfg?.updateProxy ?? '')
  let cacheWhole = $state(session.cfg?.cacheWhole ?? true)
  let cacheOpen = $state(false)
  let telOn = $state(session.cfg?.telemetry ?? false)
  let telName = $state(session.cfg?.telemetryName ?? '')
  const saveTel = () => run(() => invoke('telemetry_set', { on: telOn, name: telName }), telOn ? '已打开诊断数据' : '已关闭诊断数据')
  let adding = $state(!session.cfg?.servers.length)

  async function run(f: () => Promise<unknown>, ok?: string) {
    busy = true
    msg = null
    try {
      await f()
      await loadConfig()
      refresh()
      if (ok) msg = { text: ok, ok: true }
    } catch (e) {
      msg = { text: errText(e), ok: false }
    } finally {
      busy = false
    }
  }

  function formPort() {
    const port = form.port.trim() ? Number(form.port) : null
    if (port !== null && !(port > 0 && port < 65536)) throw { message: '端口应为 1–65535' }
    return port
  }
  const add = () =>
    run(async () => {
      await invoke('server_add', { url: form.url, port: formPort(), user: form.user, password: form.password })
      form = { ...blank }
      go('/')
    })
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
  /** 只改地址时后端沿用原 token，不用填密码；改了用户名或填了密码就重新登录。 */
  const save = (id: string) =>
    run(async () => {
      await invoke('server_edit', { id, url: form.url, port: formPort(), user: form.user, password: form.password })
      editing = null
      form = { ...blank }
    }, '已保存')
  function login() {
    const r = relogin
    if (!r) return
    run(async () => {
      await invoke('server_login', { id: r.id, password: r.password })
      relogin = null
    }, '已重新登录')
  }
  const use = (id: string) => run(() => invoke('server_use', { id }).then(() => go('/')))
  const remove = async (id: string, name: string) =>
    (await ask('删除服务器', `删除「${name}」？登录信息会一起删掉。`, '删除', true)) && run(() => invoke('server_remove', { id }))
  const gpu = gpuName()
  const savePlayer = () =>
    run(async () => {
      await invoke('settings_set', { mpvPath: mpvPath || null, preloadPercent: preload, updateProxy: updateProxy || null, cacheWhole })
      await loadConfig()
      mpvPath = session.cfg?.mpvPath ?? '' // 显示后端规范化后的值（去掉了引号、空白）
      updateProxy = session.cfg?.updateProxy ?? ''
    }, '已保存')

  /** 检查更新：idle → checking → latest / found（显示版本和说明）→ installing（进度）；出错回到 idle 并显示原因。 */
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
      await invoke('update_install') // 成功时程序会退出并由安装器重新打开
    } catch (e) {
      upd = { step: 'found', version, notes, error: errText(e) }
    } finally {
      off()
    }
  }
</script>

<div class="page">
  <div class="page-title"><h1>设置</h1></div>

  <h3>服务器</h3>
  <div class="group">
    {#snippet serverForm(submit: () => void, label: string, pwHint: string, cancel: (() => void) | null)}
      <form class="row form" onsubmit={(e) => { e.preventDefault(); submit() }}>
        <input class="url" placeholder="地址，如 https://emby.example.com" bind:value={form.url} required spellcheck="false" />
        <input class="port" placeholder="端口（默认按协议）" bind:value={form.port} inputmode="numeric" />
        <input placeholder="用户名" bind:value={form.user} required autocomplete="username" spellcheck="false" />
        <input type="password" placeholder={pwHint} bind:value={form.password} autocomplete="current-password" />
        <div class="btns">
          <button class="btn primary" disabled={busy}>{label}</button>
          {#if cancel}<button type="button" class="btn ghost" onclick={cancel}>取消</button>{/if}
        </div>
      </form>
    {/snippet}
    {#each session.cfg?.servers ?? [] as s (s.id)}
      {#if editing === s.id}
        {@render serverForm(() => save(s.id), '保存', '密码（只改地址可不填）', () => (editing = null))}
      {:else}
      <div class="row">
        <div class="grow">
          <div class="name">{s.name}{#if s.id === session.cfg?.active}<span class="tag">当前</span>{/if}</div>
          <div class="sub selectable">{s.url}:{s.port} · {s.user} · {s.loggedIn ? '已登录' : '需要重新登录'}</div>
        </div>
        {#if relogin?.id === s.id}
          <!-- svelte-ignore a11y_autofocus -->
          <input type="password" placeholder="密码" bind:value={relogin.password} autofocus onkeydown={(e) => (e.key === 'Enter' && login(), e.key === 'Escape' && (relogin = null))} />
          <button class="btn primary" disabled={busy} onclick={login}>登录</button>
          <button class="btn ghost" onclick={() => (relogin = null)}>取消</button>
        {:else}
          {#if s.id !== session.cfg?.active}<button class="btn" disabled={busy} onclick={() => use(s.id)}><Check size={15} /> 使用</button>{/if}
          <button class="btn" disabled={busy} onclick={() => startEdit(s)}><Pencil size={15} /> 编辑</button>
          <button class="btn" disabled={busy} onclick={() => ((relogin = { id: s.id, password: '' }), (editing = null))}><LogIn size={15} /> 重新登录</button>
          <button class="btn icon danger" title="删除" disabled={busy} onclick={() => remove(s.id, s.name)}><Trash2 size={15} /></button>
        {/if}
      </div>
      {/if}
    {/each}
    {#if adding}
      {@render serverForm(add, '添加并登录', '密码', session.cfg?.servers.length ? () => (adding = false) : null)}
    {:else}
      <button class="row link" onclick={startAdd}><Plus size={16} /> 添加服务器</button>
    {/if}
  </div>

  <h3>播放</h3>
  <div class="group">
    <label class="row">
      <span class="label">mpv 路径</span>
      <input class="grow" placeholder="留空使用自带的 mpv" bind:value={mpvPath} spellcheck="false" onchange={savePlayer} />
    </label>
    <label class="row">
      <span class="label">预加载下一集</span>
      <input class="grow" type="range" min="50" max="99" bind:value={preload} onchange={savePlayer} />
      <span class="val">{preload}%</span>
    </label>
    <label class="row">
      <span class="label">边播边缓存</span>
      <div class="grow">
        <div>缓存整个视频</div>
        <div class="sub">播放时在后台把整个文件存到本地：拖动进度、进度条预览、重看不用再等网络；只保留最近播放的那一集，最多 20 GB。关掉时只缓存播放位置前方一段</div>
      </div>
      <input type="checkbox" bind:checked={cacheWhole} onchange={savePlayer} />
    </label>
    <div class="row">
      <span class="label">缓存管理</span>
      <span class="grow sub">视频、图片、字幕、媒体库和临时缓存的占用，可以单独清理</span>
      <button class="btn" onclick={() => (cacheOpen = true)}><HardDrive size={14} /> 管理</button>
    </div>
    <div class="row">
      <span class="label">个人 mpv 配置</span>
      <div class="grow">
        <div class="sub selectable">{session.cfg?.userMpvConf}</div>
        <div class="sub">自己新建这个文件即可，写法同 mpv.conf（如 <code>demuxer-max-bytes=512MiB</code> 调缓冲）。同名选项以它为准，升级不覆盖，下次播放生效</div>
      </div>
    </div>
  </div>
  <h3>关于</h3>
  <div class="group">
    <div class="row">
      <span class="label">版本</span>
      <div class="grow">
        <div>EmbyMini {session.cfg?.version}</div>
        {#if upd.step === 'latest'}<div class="sub">已是最新版本</div>
        {:else if upd.step === 'found' || upd.step === 'installing'}
          <div class="sub">新版本 {upd.version}{#if upd.step === 'installing'}：正在下载 {upd.pct}%，下完会自动安装并重新打开{/if}</div>
          {#if upd.notes}<div class="sub notes selectable">{upd.notes}</div>{/if}
        {/if}
        {#if upd.error}<div class="sub err">{upd.error}</div>{/if}
      </div>
      {#if upd.step === 'found'}
        <button class="btn primary" onclick={install}><Download size={14} /> 立即更新</button>
      {:else}
        <button class="btn" disabled={upd.step === 'checking' || upd.step === 'installing'} onclick={checkUpdate}><RefreshCw size={14} /> {upd.step === 'checking' ? '检查中…' : '检查更新'}</button>
      {/if}
    </div>
    <label class="row">
      <span class="label">更新代理</span>
      <div class="grow">
        <input class="wide" placeholder="留空跟随系统代理，如 http://127.0.0.1:10808" bind:value={updateProxy} spellcheck="false" onchange={savePlayer} />
        <div class="sub">只用于检查更新和下载安装包，其它请求都直连</div>
      </div>
    </label>
    {#if session.cfg?.telemetryAvailable}
      <label class="row">
        <span class="label">诊断数据</span>
        <div class="grow">
          <div>发送诊断数据</div>
          <div class="sub">接口、图片、页面、播放的耗时与卡顿，用来改进加载速度和流畅度；不含账号、密码、服务器地址、搜索词和片名</div>
        </div>
        <input type="checkbox" bind:checked={telOn} onchange={saveTel} />
      </label>
      {#if telOn}
        <label class="row">
          <span class="label">上报名称</span>
          <input class="grow" placeholder="默认用计算机名" bind:value={telName} spellcheck="false" onchange={saveTel} />
        </label>
      {/if}
    {/if}
    <div class="row"><span class="label">界面渲染</span><span class="grow sub selectable">{gpu}</span></div>
    <div class="row"><span class="label">播放器渲染</span><span class="grow sub">mpv 优先使用 NVIDIA 独显（D3D11 + 硬解）；播放时按 i 可查看实际使用的解码方式</span></div>
  </div>
  <p class="note">播放器快捷键：空格 暂停　←→ 快退 / 快进 5 秒　↑↓ 音量　F 全屏　&lt; &gt; 上一集 / 下一集　F8 选集　Q 退出</p>

  {#if msg}<p class="msg" class:err={!msg.ok}>{msg.text}</p>{/if}
</div>
{#if cacheOpen}<CacheDialog onclose={() => (cacheOpen = false)} />{/if}

<style>
  .page { max-width: 860px; margin: 0 auto; }
  h3 { font-size: 13px; font-weight: 600; color: var(--text-2); margin: 18px var(--gutter) 8px; }
  .group { margin: 0 var(--gutter); border-radius: var(--radius-l); background: var(--bg-raised); box-shadow: inset 0 0 0 1px var(--stroke); overflow: hidden; }
  .row { display: flex; align-items: center; gap: 10px; min-height: 56px; padding: 10px 16px; width: 100%; }
  .row + .row { border-top: 1px solid var(--stroke); }
  .grow { flex: 1; min-width: 0; }
  .name { font-weight: 600; display: flex; align-items: center; gap: 8px; }
  .sub { font-size: 12px; color: var(--text-2); margin-top: 2px; }
  .tag { font-size: 11px; color: #fff; background: var(--accent); border-radius: 4px; padding: 0 6px; font-weight: 500; line-height: 18px; }
  .label { width: 110px; flex: none; }
  .val { width: 44px; text-align: right; font-variant-numeric: tabular-nums; color: var(--text-2); }
  .link { color: var(--accent); gap: 8px; text-align: left; }
  .link:hover { background: var(--fill); }
  input:not([type='range']) {
    height: 32px; border: 0; border-radius: var(--radius-s); padding: 0 10px; background: var(--fill); min-width: 0;
    box-shadow: inset 0 -1px 0 var(--stroke-strong); transition: box-shadow 0.12s, background 0.12s;
  }
  input:not([type='range']):focus { background: var(--bg); box-shadow: inset 0 -2px 0 var(--accent); }
  input[type='range'], input[type='checkbox'] { accent-color: var(--accent); }
  input[type='checkbox'] { width: 18px; height: 18px; }
  .form { display: grid; grid-template-columns: 170px 1fr 1fr; gap: 10px; }
  .form .url, .form .btns { grid-column: 1 / -1; }
  .btns { display: flex; gap: 8px; }
  code { font-family: ui-monospace, Consolas, monospace; font-size: 11px; padding: 0 4px; border-radius: 4px; background: var(--fill); }
  .note { margin: 10px var(--gutter); font-size: 12px; color: var(--text-3); line-height: 1.8; }
  .msg { margin: 14px var(--gutter); color: var(--text-2); }
  .msg.err { color: var(--danger); }
  .sub.err { color: var(--danger); }
  .notes { white-space: pre-wrap; max-height: 160px; overflow: auto; margin-top: 6px; }
  input.wide { width: 100%; }
</style>
