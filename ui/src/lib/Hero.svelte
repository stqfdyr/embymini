<!-- 首页顶部轮播：背景图 + Logo（没有就显示标题）+ 简介 + 圆点，8 秒切换，鼠标停在上面时暂停。 -->
<script lang="ts">
  import { img } from './api.svelte'
  import { fade } from './fade'
  import { years } from './fmt'
  import type { Item } from './types'

  let { items }: { items: Item[] } = $props()
  let i = $state(0)
  let hold = $state(false)
  const cur = $derived(items[i % Math.max(items.length, 1)])

  $effect(() => {
    if (items.length < 2 || hold) return
    const t = setInterval(() => (i = (i + 1) % items.length), 8000)
    return () => clearInterval(t)
  })
</script>

{#if cur}
  <!-- svelte-ignore a11y_no_static_element_interactions -->
  <section class="hero" onpointerenter={() => (hold = true)} onpointerleave={() => (hold = false)}>
    {#key cur.Id}
      <img class="bg" src={img(cur.Id, 'Backdrop', cur.BackdropImageTags?.[0], 1600)} alt="" fetchpriority="high" use:fade />
    {/key}
    <div class="scrim"></div>
    <a class="info" href={`#/item/${cur.Id}`}>
      {#if cur.ImageTags?.Logo}
        <img class="logo" src={img(cur.Id, 'Logo', cur.ImageTags.Logo, 420)} alt={cur.Name} />
      {:else}
        <div class="title">{cur.Name}</div>
      {/if}
      <div class="meta">{[years(cur), ...(cur.Genres ?? []).slice(0, 3)].filter(Boolean).join(' · ')}</div>
      {#if cur.Overview}<p class="ov clamp-2">{cur.Overview}</p>{/if}
    </a>
    <div class="dots">
      {#each items as it, k (it.Id)}
        <button class:on={k === i % items.length} aria-label={it.Name} onclick={() => (i = k)}><i></i></button>
      {/each}
    </div>
  </section>
{/if}

<style>
  .hero { position: relative; height: min(58vh, 600px); margin-top: calc(-1 * var(--bar-h) - 8px); overflow: hidden; }
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
  .dots { position: absolute; bottom: 16px; left: 0; right: 0; display: flex; justify-content: center; gap: 2px; }
  .dots button { padding: 6px 4px; }
  .dots i { display: block; width: 6px; height: 6px; border-radius: 3px; background: var(--text-3); opacity: 0.6; transition: width 0.25s var(--ease), opacity 0.2s; }
  .dots button.on i { width: 18px; opacity: 1; background: var(--text); }
</style>
