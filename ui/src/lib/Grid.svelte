<!-- Poster grid (or 16:9 cards with `wide` and a `card` snippet); with `onmore`, the next page loads as the end comes within
     1200 px (IntersectionObserver). -->
<script lang="ts">
  import { untrack, type Snippet } from 'svelte'
  import Poster from './Poster.svelte'
  import type { Item } from './types'

  /** `card` replaces the poster (2:3, 160 px); `wide` lays out 16:9 cards 300 px wide instead. */
  let { items, onmore, loading = false, card, wide = false }: { items: Item[]; onmore?: () => void; loading?: boolean; card?: Snippet<[Item]>; wide?: boolean } = $props()
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

<div class="grid" class:wide>
  {#each items as it (it.Id)}{#if card}{@render card(it)}{:else}<Poster item={it} />{/if}{/each}
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
  .wide { grid-template-columns: repeat(auto-fill, 300px); gap: 28px 16px; }
  .wide > :global(*) { contain-intrinsic-size: 300px 220px; }
  .wide .cell { width: 300px; aspect-ratio: 16 / 9; }
  .sentinel { height: 1px; }
</style>
