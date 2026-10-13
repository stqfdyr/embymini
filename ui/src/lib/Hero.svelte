<!-- 首页顶部轮播：背景图 + Logo（没有就显示标题）+ 简介 + 圆点，8 秒切换，鼠标停在上面时暂停。
     整块点击进详情；悬停出左右箭头，箭头和圆点只切换不跳转。切换前预加载下一张背景图。 -->
<script lang="ts">
  import { ChevronLeft, ChevronRight } from '@lucide/svelte'
  import { img } from './api.svelte'
  import { fade } from './fade'
  import { years } from './fmt'
  import { go } from './router.svelte'
  import type { Item } from './types'

  let { items }: { items: Item[] } = $props()
  let i = $state(0)
  let hold = $state(false)
  const n = $derived(Math.max(items.length, 1))
  const cur = $derived(items[i % n])
  const bg = (it?: Item) => (it ? img(it.Id, 'Backdrop', it.BackdropImageTags?.[0], 1600) : '')
  const step = (d: number) => (i = (i + d + n) % n)

  $effect(() => {
    if (items.length < 2 || hold) return
    const t = setInterval(() => step(1), 8000)
    return () => clearInterval(t)
  })
  // 下一张提前进 HTTP 缓存，切过去时直接从缓存出图
  $effect(() => {
    const next = bg(items[(i + 1) % n])
    if (next) new Image().src = next
  })
</script>

{#if cur}
  <!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions -->
  <section class="hero" onpointerenter={() => (hold = true)} onpointerleave={() => (hold = false)} onclick={() => go(`/item/${cur.Id}`)}>
    {#key cur.Id}
      <img class="bg" src={bg(cur)} alt="" fetchpriority="high" use:fade />
    {/key}
    <div class="scrim"></div>
    <div class="info">
      {#if cur.ImageTags?.Logo}
        <img class="logo" src={img(cur.Id, 'Logo', cur.ImageTags.Logo, 420)} alt={cur.Name} />
      {:else}
        <div class="title">{cur.Name}</div>
      {/if}
      <div class="meta">{[years(cur), ...(cur.Genres ?? []).slice(0, 3)].filter(Boolean).join(' · ')}</div>
      {#if cur.Overview}<p class="ov clamp-2">{cur.Overview}</p>{/if}
    </div>
    {#if items.length > 1}
      <button class="arrow l" aria-label="上一个" onclick={(e) => (e.stopPropagation(), step(-1))}><ChevronLeft size={20} /></button>
      <button class="arrow r" aria-label="下一个" onclick={(e) => (e.stopPropagation(), step(1))}><ChevronRight size={20} /></button>
      <div class="dots">
        {#each items as it, k (it.Id)}
          <button class:on={k === i % n} aria-label={it.Name} onclick={(e) => (e.stopPropagation(), (i = k))}><i></i></button>
        {/each}
      </div>
    {/if}
  </section>
{/if}

<style>
  .hero { position: relative; height: min(58vh, 600px); margin-top: calc(-1 * var(--bar-h) - 8px); overflow: hidden; cursor: pointer; }
  .bg { position: absolute; inset: 0; width: 100%; height: 100%; object-fit: cover; }
  /* 渐变遮罩叠在图上（比 mask-image 便宜），底部融进页面底色 */
  .scrim { position: absolute; inset: 0; background: var(--scrim); }
  .info {
    position: absolute; left: 50%; bottom: 46px; transform: translateX(-50%); width: min(920px, 88%);
    display: flex; flex-direction: column; align-items: center; text-align: center; gap: 8px;
  }
  .logo { max-height: 100px; max-width: 420px; object-fit: contain; }
  .title { font-size: 36px; font-weight: 700; letter-spacing: -0.02em; }
  .meta { font-size: 13px; color: var(--text-2); }
  .ov { margin: 0; font-size: 14px; line-height: 1.6; }
  /* 箭头同横滑行：悬停才出现的圆形按钮 */
  .arrow {
    position: absolute; top: 50%; margin-top: -16px; z-index: 2; color: var(--text);
    opacity: 0; transition: opacity 0.15s;
  }
  .arrow :global(svg) {
    width: 32px; height: 32px; padding: 6px; border-radius: 50%;
    background: var(--flyout-bg); box-shadow: var(--flyout-shadow); transition: transform 0.12s var(--ease);
  }
  .arrow:hover :global(svg) { transform: scale(1.08); }
  .arrow:active :global(svg) { transform: scale(0.94); }
  .hero:hover .arrow { opacity: 1; }
  .arrow.l { left: var(--gutter); }
  .arrow.r { right: var(--gutter); }
  .dots { position: absolute; bottom: 16px; left: 0; right: 0; display: flex; justify-content: center; gap: 2px; pointer-events: none; }
  .dots button { padding: 6px 4px; pointer-events: auto; }
  .dots i { display: block; width: 6px; height: 6px; border-radius: 3px; background: var(--text-3); opacity: 0.6; transition: width 0.25s var(--ease), opacity 0.2s; }
  .dots button.on i { width: 18px; opacity: 1; background: var(--text); }
</style>
