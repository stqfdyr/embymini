<!-- Home carousel: backdrop, logo (title when missing or failed), overview and dots; rotates, paused while hovered.
     A click anywhere opens the detail page; the hover arrows and the dots only switch. The next backdrop is preloaded. -->
<script lang="ts">
  import { ChevronLeft, ChevronRight } from '@lucide/svelte'
  import { BACKDROP_W, img } from './api.svelte'
  import { fade } from './fade'
  import { years } from './fmt'
  import { go } from './router.svelte'
  import type { Item } from './types'

  const ROTATE_MS = 8000

  let { items }: { items: Item[] } = $props()
  let i = $state(0)
  let hold = $state(false)
  const n = $derived(Math.max(items.length, 1))
  const cur = $derived(items[i % n])
  const bg = (it?: Item) => (it ? img(it.Id, 'Backdrop', it.BackdropImageTags?.[0], BACKDROP_W) : '')
  const logo = $derived(img(cur?.Id, 'Logo', cur?.ImageTags?.Logo, 420))
  // Logo URL that failed on this slide, remembered only until it changes: coming back retries (the backend remembers
  // upstream 404s, so that costs no upstream request; a network error can recover)
  let badLogo = $state('')
  const show = (k: number) => ((i = (k + n) % n), (badLogo = ''))
  const step = (d: number) => show(i + d)

  $effect(() => {
    if (items.length < 2 || hold) return
    const t = setInterval(() => step(1), ROTATE_MS)
    return () => clearInterval(t)
  })
  // Warm the cache with the next backdrop so switching shows it at once
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
      <!-- A new element per URL, so a late error from the previous logo cannot hit this one -->
      {#key logo}
        {#if logo && badLogo !== logo}
          <img class="logo" src={logo} alt={cur.Name} onerror={() => (badLogo = logo)} />
        {:else}
          <div class="title">{cur.Name}</div>
        {/if}
      {/key}
      <div class="meta">{[years(cur), ...(cur.Genres ?? []).slice(0, 3)].filter(Boolean).join(' · ')}</div>
      {#if cur.Overview}<p class="ov clamp-2">{cur.Overview}</p>{/if}
    </div>
    {#if items.length > 1}
      <button class="arrow l" aria-label="上一个" onclick={(e) => (e.stopPropagation(), step(-1))}><ChevronLeft size={20} /></button>
      <button class="arrow r" aria-label="下一个" onclick={(e) => (e.stopPropagation(), step(1))}><ChevronRight size={20} /></button>
      <div class="dots">
        {#each items as it, k (it.Id)}
          <button class:on={k === i % n} aria-label={it.Name} onclick={(e) => (e.stopPropagation(), show(k))}><i></i></button>
        {/each}
      </div>
    {/if}
  </section>
{/if}

<style>
  .hero { position: relative; height: min(58vh, 600px); margin-top: calc(-1 * var(--bar-h) - 8px); overflow: hidden; cursor: pointer; }
  .bg { position: absolute; inset: 0; width: 100%; height: 100%; object-fit: cover; }
  /* A gradient overlay (cheaper than mask-image) fades the image into the page background */
  .scrim { position: absolute; inset: 0; background: var(--scrim); }
  .info {
    position: absolute; left: 50%; bottom: 46px; transform: translateX(-50%); width: min(920px, 88%);
    display: flex; flex-direction: column; align-items: center; text-align: center; gap: 8px;
  }
  .logo { max-height: 100px; max-width: 420px; object-fit: contain; }
  .title { font-size: 36px; font-weight: 700; letter-spacing: -0.02em; }
  .meta { font-size: 13px; color: var(--text-2); }
  .ov { margin: 0; font-size: 14px; line-height: 1.6; }
  /* Arrows as in Row: round buttons shown on hover */
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
