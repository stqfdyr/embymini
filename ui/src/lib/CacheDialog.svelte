<!-- 缓存管理：逐项显示占用，可单独清理或一键清理。视频、图片、字幕、临时缓存在 Rust 里（cache_info / cache_clear），
     媒体库缓存是界面的列表快照（snap.ts）。正在播放的那部片的视频缓存清不掉。Esc、点外面、「完成」关闭。 -->
<script lang="ts">
  import { invoke } from '@tauri-apps/api/core'
  import { errText, notify } from './api.svelte'
  import { bytes } from './fmt'
  import { snapClear, snapSize } from './snap'

  let { onclose }: { onclose: () => void } = $props()
  type Info = { video: number; images: number; subs: number; temp: number; playing: boolean }
  let info = $state<Info | null>(null)
  let snap = $state(snapSize())
  let busy = $state(false)

  invoke<Info>('cache_info').then(
    (i) => (info = i),
    (e) => notify(errText(e)),
  )

  async function clear(kind: string) {
    busy = true
    try {
      if (kind === 'snap' || kind === 'all') {
        snapClear()
        snap = snapSize()
      }
      if (kind !== 'snap') info = await invoke<Info>('cache_clear', { kind })
    } catch (e) {
      notify(errText(e))
    } finally {
      busy = false
    }
  }

  const rows = $derived(
    info
      ? [
          { kind: 'video', label: '视频缓存', size: info.video, note: info.playing ? '正在播放的除外' : '' },
          { kind: 'images', label: '图片缓存', size: info.images, note: '' },
          { kind: 'subs', label: '字幕缓存', size: info.subs, note: '' },
          { kind: 'snap', label: '媒体库缓存', size: snap, note: '' },
          { kind: 'temp', label: '临时缓存', size: info.temp, note: '' },
        ]
      : [],
  )
  const total = $derived(rows.reduce((n, r) => n + r.size, 0))
</script>

<svelte:window onkeydown={(e) => e.key === 'Escape' && (e.stopPropagation(), onclose())} />

<!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions -->
<div class="backdrop" onclick={(e) => e.target === e.currentTarget && onclose()}>
  <div class="dialog flyout" role="dialog" aria-modal="true" aria-label="缓存管理">
    <h3>缓存管理</h3>
    <div class="body">
      <div class="group">
        {#each rows as r (r.kind)}
          <div class="row">
            <div class="grow">
              <div>{r.label}</div>
              <div class="sub">{bytes(r.size)}{#if r.note}　{r.note}{/if}</div>
            </div>
            <button class="btn" disabled={busy} onclick={() => clear(r.kind)}>清理</button>
          </div>
        {:else}
          <div class="row sub">正在统计…</div>
        {/each}
      </div>
      <div class="group">
        <div class="row"><span class="grow">缓存大小</span><span class="sub total">{info ? bytes(total) : ''}</span></div>
        <div class="row"><button class="btn" disabled={busy || !info} onclick={() => clear('all')}>一键清理</button></div>
      </div>
    </div>
    <div class="actions"><button class="btn primary" onclick={onclose}>完成</button></div>
  </div>
</div>

<style>
  .backdrop { position: fixed; inset: 0; z-index: 45; display: grid; place-items: center; background: rgba(0, 0, 0, 0.32); animation: dim-in 0.14s; }
  @keyframes dim-in { from { opacity: 0; } }
  .dialog { width: min(520px, calc(100vw - 48px)); max-height: calc(100vh - 120px); display: flex; flex-direction: column; padding: 20px 0 16px; transform-origin: center; }
  h3 { margin: 0 22px 14px; font-size: 16px; font-weight: 600; }
  .body { overflow: auto; padding: 0 22px; display: flex; flex-direction: column; gap: 12px; }
  .group { border-radius: var(--radius-l); background: var(--fill); overflow: hidden; }
  .row { display: flex; align-items: center; gap: 10px; min-height: 52px; padding: 8px 14px; }
  .row + .row { border-top: 1px solid var(--stroke); }
  .grow { flex: 1; min-width: 0; }
  .sub { font-size: 12px; color: var(--text-2); margin-top: 2px; }
  .total { font-size: 14px; font-variant-numeric: tabular-nums; }
  .actions { display: flex; justify-content: flex-end; margin: 16px 22px 0; }
</style>
