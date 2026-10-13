<!-- 海报网格；传 onmore 时滚到底自动加载下一页（IntersectionObserver）。 -->
<script lang="ts">
  import Poster from './Poster.svelte'
  import type { Item } from './types'

  let { items, onmore, loading = false }: { items: Item[]; onmore?: () => void; loading?: boolean } = $props()
  let sentinel: HTMLDivElement | undefined = $state()

  $effect(() => {
    if (!sentinel || !onmore) return
    const io = new IntersectionObserver((es) => es[0].isIntersecting && onmore(), { rootMargin: '1200px' })
    io.observe(sentinel)
    return () => io.disconnect()
  })
</script>

<div class="grid">
  {#each items as it (it.Id)}<Poster item={it} />{/each}
  {#if loading}{#each { length: 12 } as _, k (k)}<div class="skeleton cell"></div>{/each}{/if}
</div>
{#if onmore}<div bind:this={sentinel} class="sentinel"></div>{/if}

<style>
  .grid {
    display: grid; grid-template-columns: repeat(auto-fill, 160px); justify-content: space-between;
    gap: 26px 16px; padding: 0 var(--gutter);
  }
  /* 视口外的卡片跳过布局和绘制：库很大时滚动也不卡 */
  .grid > :global(*) { content-visibility: auto; contain-intrinsic-size: 160px 285px; }
  .cell { width: 160px; aspect-ratio: 2 / 3; }
  .sentinel { height: 1px; }
</style>
