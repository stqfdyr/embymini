<!-- Poster grid; with `onmore`, the next page loads as the end comes within 1200 px (IntersectionObserver). -->
<script lang="ts">
  import Poster from './Poster.svelte'
  import type { Item } from './types'

  let { items, onmore, loading = false }: { items: Item[]; onmore?: () => void; loading?: boolean } = $props()
  let sentinel: HTMLDivElement | undefined = $state()

  $effect(() => {
    // Re-armed whenever the list changes: a fresh observer reports at once, so a sentinel still inside the margin after a
    // page lands (big screens, short pages) loads the next one; an observer only fires on entering. A failed load changes
    // nothing, so it is not retried in a loop.
    void items.length
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
  /* Off-screen cards skip layout and paint, so huge libraries still scroll smoothly; the size matches a Poster */
  .grid > :global(*) { content-visibility: auto; contain-intrinsic-size: 160px 285px; }
  .cell { width: 160px; aspect-ratio: 2 / 3; }
  .sentinel { height: 1px; }
</style>
