<!-- Poster grid; with `onmore`, the next page loads as the end comes within 1200 px (IntersectionObserver). -->
<script lang="ts">
  import { untrack } from 'svelte'
  import Poster from './Poster.svelte'
  import type { Item } from './types'

  let { items, onmore, loading = false }: { items: Item[]; onmore?: () => void; loading?: boolean } = $props()
  let sentinel: HTMLDivElement | undefined = $state()
  let near = $state(false)

  $effect(() => {
    if (!sentinel) return
    const io = new IntersectionObserver((es) => (near = es[0].isIntersecting), { rootMargin: '1200px' })
    io.observe(sentinel)
    return () => io.disconnect()
  })
  // Keep loading while the end stays within the margin: the observer only reports crossings, and on big screens a page
  // can land with the sentinel still inside. After a failure `onmore` (pager) waits for a retry, so this cannot loop.
  $effect(() => {
    if (near && !loading && onmore) untrack(onmore)
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
