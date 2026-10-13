<!-- 外壳：顶栏兼作标题栏（无系统标题栏，可拖动、双击最大化；右侧窗口按钮）+ 路由 + 全局快捷键与右键菜单。 -->
<script lang="ts">
  import { invoke } from '@tauri-apps/api/core'
  import { listen } from '@tauri-apps/api/event'
  import { getCurrentWindow } from '@tauri-apps/api/window'
  import { ChevronLeft, Copy, Minus, RefreshCw, Settings as Gear, Square, X } from '@lucide/svelte'
  import ContextMenu from './lib/ContextMenu.svelte'
  import Dialog from './lib/Dialog.svelte'
  import Dropdown from './lib/Dropdown.svelte'
  import { dismiss, errText, loadConfig, notify, refresh, refreshQuiet, session, setUnauthorizedHandler, toasts } from './lib/api.svelte'
  import { back, go, route } from './lib/router.svelte'
  import Favorites from './routes/Favorites.svelte'
  import Home from './routes/Home.svelte'
  import Item from './routes/Item.svelte'
  import Library from './routes/Library.svelte'
  import Person from './routes/Person.svelte'
  import Search from './routes/Search.svelte'
  import Season from './routes/Season.svelte'
  import Settings from './routes/Settings.svelte'

  const TABS = [['home', '/', '媒体'], ['search', '/search', '搜索'], ['favorites', '/favorites', '收藏']] as const
  const win = getCurrentWindow()
  let maximized = $state(false)
  let scrolled = $state(false)
  /** 有大图头部的页面：顶栏在顶部时透明，叠在图上。 */
  const overHero = $derived((route.name === 'home' || route.name === 'item') && !scrolled)

  $effect(() => {
    setUnauthorizedHandler(() => loadConfig().then(() => go('/settings')))
    loadConfig().then(() => !session.server?.loggedIn && go('/settings'))
    // 播放失败提示；整个播放会话结束才后台静默刷新一次（切集时的 stopped 不刷）
    const off = listen<{ type: string; message?: string }>('player', (e) => {
      if (e.payload.type === 'failed') notify(e.payload.message ?? '播放失败')
      if (e.payload.type === 'ended') refreshQuiet()
    })
    const sync = () => win.isMaximized().then((m) => (maximized = m), () => {})
    sync()
    const offResize = win.onResized(sync)
    return () => {
      void off.then((f) => f())
      void offResize.then((f) => f())
    }
  })

  // 桌面应用的习惯：没有网页右键菜单、F5 不整页重载、鼠标侧键 / Alt+← 返回、Ctrl+F 搜索
  const editable = (t: EventTarget | null) => t instanceof HTMLElement && (t.isContentEditable || /^(INPUT|TEXTAREA|SELECT)$/.test(t.tagName))
  function onKey(e: KeyboardEvent) {
    const ctrl = e.ctrlKey || e.metaKey
    if (e.key === 'F5' || (ctrl && e.key.toLowerCase() === 'r')) (e.preventDefault(), refresh())
    else if (ctrl && e.key.toLowerCase() === 'f') (e.preventDefault(), go('/search'))
    else if (e.altKey && e.key === 'ArrowLeft') (e.preventDefault(), back())
    else if (e.altKey && e.key === 'ArrowRight') (e.preventDefault(), history.forward())
    else if (e.key === 'Backspace' && !editable(e.target)) (e.preventDefault(), back())
  }
  function onMouseUp(e: MouseEvent) {
    if (e.button === 3) (e.preventDefault(), back())
    if (e.button === 4) (e.preventDefault(), history.forward())
  }
  const onContextMenu = (e: MouseEvent) => !editable(e.target) && !(e.target as HTMLElement)?.closest?.('.selectable') && e.preventDefault()

  async function switchTo(id: string) {
    if (id === '__manage') return go('/settings')
    try {
      await invoke('server_use', { id })
      await loadConfig()
      go('/')
    } catch (e) {
      notify(`切换服务器失败：${errText(e)}`)
    }
  }
</script>

<svelte:window onkeydown={onKey} onmouseup={onMouseUp} oncontextmenu={onContextMenu} onscroll={() => (scrolled = scrollY > 8)} />

<header class="bar" class:clear={overHero} data-tauri-drag-region>
  <div class="side" data-tauri-drag-region>
    <button class="btn ghost icon" aria-label="返回" disabled={route.name === 'home'} onclick={back}><ChevronLeft size={20} /></button>
    {#if session.cfg?.servers.length}
      <Dropdown
        value={session.cfg.active ?? undefined}
        items={[...session.cfg.servers.map((s) => ({ value: s.id, label: s.name, hint: s.user })), { value: '__manage', label: '管理服务器…' }]}
        onselect={switchTo}
      />
    {/if}
  </div>
  <nav class="tabs" data-tauri-drag-region>
    {#each TABS as [name, path, label] (name)}
      <a href={`#${path}`} class:on={route.name === name}>{label}</a>
    {/each}
  </nav>
  <div class="side end" data-tauri-drag-region>
    <button class="btn ghost icon" title="刷新（F5）" onclick={refresh}><RefreshCw size={16} /></button>
    <button class="btn ghost icon" class:on={route.name === 'settings'} title="设置" onclick={() => go('/settings')}><Gear size={16} /></button>
    <div class="win">
      <button title="最小化" onclick={() => win.minimize()}><Minus size={16} strokeWidth={1.5} /></button>
      <button title={maximized ? '向下还原' : '最大化'} onclick={() => win.toggleMaximize()}>
        {#if maximized}<Copy size={13} strokeWidth={1.5} style="transform:scaleX(-1)" />{:else}<Square size={13} strokeWidth={1.5} />{/if}
      </button>
      <button class="close" title="关闭" onclick={() => win.close()}><X size={18} strokeWidth={1.5} /></button>
    </div>
  </div>
</header>

<ContextMenu />
<Dialog />

<div class="toasts">
  {#each toasts as t (t.id)}
    <!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions -->
    <div class="toast flyout" title="点击关闭" onclick={() => dismiss(t.id)}>
      <span>{t.text}</span>
      {#if t.action}{@const a = t.action}<button class="btn" onclick={(e) => (e.stopPropagation(), dismiss(t.id), a.run())}>{a.label}</button>{/if}
    </div>
  {/each}
</div>

<main>
  {#if !session.cfg}
    <div class="center-msg">启动中…</div>
  {:else if route.name === 'settings' || !session.server?.loggedIn}
    <Settings />
  {:else}
    {#key session.server.id}
      {#if route.name === 'item'}
        {#key route.id}<Item id={route.id} />{/key}
      {:else if route.name === 'person'}
        {#key route.id}<Person id={route.id} />{/key}
      {:else if route.name === 'season'}
        {#key route.hash}<Season seriesId={route.id} seasonId={route.sub} />{/key}
      {:else if route.name === 'library'}
        {#key route.id}<Library id={route.id} />{/key}
      {:else if route.name === 'search'}
        <Search />
      {:else if route.name === 'favorites'}
        <Favorites />
      {:else}
        <div class="page"><Home /></div>
      {/if}
    {/key}
  {/if}
</main>

<style>
  .bar {
    position: fixed; top: 0; left: 0; right: 0; z-index: 20; height: var(--bar-h);
    display: grid; grid-template-columns: 1fr auto 1fr; align-items: center; padding-left: 12px;
    background: var(--bar-bg); box-shadow: 0 1px 0 var(--stroke);
    transition: background 0.2s, box-shadow 0.2s;
  }
  /* 叠在大图上：透明，顶部一层淡淡的底色保证文字看得清 */
  .bar.clear { background: linear-gradient(to bottom, color-mix(in srgb, var(--bg) 88%, transparent), color-mix(in srgb, var(--bg) 0%, transparent)); box-shadow: none; }
  /* 叠在亮图上时控件也要看得清：分段控件用近实色底 */
  .bar.clear .tabs { background: color-mix(in srgb, var(--bg) 72%, transparent); }
  .side { display: flex; align-items: center; gap: 4px; height: 100%; }
  .side.end { justify-content: flex-end; }
  .btn.on { color: var(--accent); }
  .tabs { display: flex; gap: 2px; padding: 3px; border-radius: 10px; background: var(--fill); }
  .tabs a { padding: 5px 18px; border-radius: 7px; font-size: 13px; color: var(--text-2); transition: background 0.12s, color 0.12s; }
  .tabs a:hover { color: var(--text); }
  .tabs a.on { background: var(--bg-raised); color: var(--text); font-weight: 600; box-shadow: 0 1px 2px rgba(0, 0, 0, 0.12), 0 0 0 0.5px var(--stroke); }
  /* Windows 11 风格窗口按钮 */
  .win { display: flex; height: 100%; margin-left: 8px; }
  .win button { width: 46px; height: 100%; display: grid; place-items: center; color: var(--text); transition: background 0.1s; }
  .win button:hover { background: var(--fill-hover); }
  .win button:active { background: var(--fill-press); }
  .win .close:hover { background: #c42b1c; color: #fff; }
  .win .close:active { background: #b22a1d; color: #fff; }
  .toasts { position: fixed; right: 20px; bottom: 20px; z-index: 40; display: flex; flex-direction: column; gap: 10px; max-width: 420px; }
  .toast { display: flex; align-items: center; gap: 12px; padding: 12px 16px; line-height: 1.5; }
  .toast span { flex: 1; }
  .toast .btn { flex: none; color: var(--accent); }
</style>
