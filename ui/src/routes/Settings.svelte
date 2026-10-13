<!-- 设置：服务器（添加 / 切换 / 编辑 / 重新登录 / 删除，地址与端口分开填）+ 播放（mpv 路径、预加载进度、个人 mpv 配置的位置）+ 关于（界面用哪块 GPU）。分组列表样式。 -->
<script lang="ts">
  import { invoke } from '@tauri-apps/api/core'
  import { Check, LogIn, Pencil, Plus, Trash2 } from '@lucide/svelte'
  import { errText, loadConfig, refresh, session } from '../lib/api.svelte'
  import { go } from '../lib/router.svelte'

  const blank = { url: '', port: '', user: '', password: '' }
  let form = $state({ ...blank })
  /** 正在编辑的服务器 id；和「添加」共用 form。 */
  let editing = $state<string | null>(null)
  let relogin = $state<{ id: string; password: string } | null>(null)
  let busy = $state(false)
  let msg = $state<{ text: string; ok: boolean } | null>(null)
  let mpvPath = $state(session.cfg?.mpvPath ?? '')
  let preload = $state(session.cfg?.preloadPercent ?? 80)
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
  const remove = (id: string, name: string) => confirm(`删除服务器「${name}」？`) && run(() => invoke('server_remove', { id }))
  /** 界面实际用的 GPU（WebView2 底下是 Chromium，ANGLE 渲染器名里带显卡型号），方便确认是不是在用显卡加速。 */
  function gpu() {
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
  const gpuName = gpu()
  const savePlayer = () =>
    run(async () => {
      await invoke('settings_set', { mpvPath: mpvPath || null, preloadPercent: preload })
      await loadConfig()
      mpvPath = session.cfg?.mpvPath ?? '' // 显示后端规范化后的值（去掉了引号、空白）
    }, '已保存')
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
    <div class="row"><span class="label">界面渲染</span><span class="grow sub selectable">{gpuName}</span></div>
    <div class="row"><span class="label">播放器渲染</span><span class="grow sub">mpv 优先使用 NVIDIA 独显（D3D11 + 硬解）；播放时按 i 可查看实际使用的解码方式</span></div>
  </div>
  <p class="note">播放器快捷键：空格 暂停　←→ 快退 / 快进 5 秒　↑↓ 音量　F 全屏　&lt; &gt; 上一集 / 下一集　F8 选集　Q 退出</p>

  {#if msg}<p class="msg" class:err={!msg.ok}>{msg.text}</p>{/if}
</div>

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
  input[type='range'] { accent-color: var(--accent); }
  .form { display: grid; grid-template-columns: 170px 1fr 1fr; gap: 10px; }
  .form .url, .form .btns { grid-column: 1 / -1; }
  .btns { display: flex; gap: 8px; }
  code { font-family: ui-monospace, Consolas, monospace; font-size: 11px; padding: 0 4px; border-radius: 4px; background: var(--fill); }
  .note { margin: 10px var(--gutter); font-size: 12px; color: var(--text-3); line-height: 1.8; }
  .msg { margin: 14px var(--gutter); color: var(--text-2); }
  .msg.err { color: var(--danger); }
</style>
