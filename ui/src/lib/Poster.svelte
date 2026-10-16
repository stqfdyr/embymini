<!-- 竖版海报卡：评分（右下）、剧集角标（未看集数或总集数）或已看对勾（右上）、标题 + 年份。
     右键出菜单；悬停预取详情，点进去不用等。 -->
<script lang="ts">
  import { Check } from '@lucide/svelte'
  import { cancelPrefetch, img, itemMenu, prefetch } from './api.svelte'
  import { fade } from './fade'
  import { rating, seriesBadge, years } from './fmt'
  import { openMenu } from './menu.svelte'
  import { go } from './router.svelte'
  import type { Item } from './types'

  let { item, width = 160 }: { item: Item; width?: number } = $props()
  const src = $derived(img(item.Id, 'Primary', item.ImageTags?.Primary, width))
  const badge = $derived(seriesBadge(item))
  const href = $derived(`#/item/${item.Id}`)
</script>

<a
  class="poster" {href} style:width={`${width}px`} title={item.Name}
  onpointerenter={() => prefetch(item.Id)} onpointerleave={cancelPrefetch}
  oncontextmenu={(e) => openMenu(e, itemMenu(item, () => go(href.slice(1))))}
>
  <div class="art ph">
    {#if src}<img {src} alt="" loading="lazy" use:fade />{/if}
    {#if badge}<span class="badge">{badge}</span>
    {:else if item.UserData?.Played}<span class="badge"><Check size={12} strokeWidth={3} /></span>{/if}
    {#if item.CommunityRating}<span class="score">{rating(item.CommunityRating)}</span>{/if}
  </div>
  <div class="name clamp-1">{item.Name}</div>
  <div class="sub">{years(item)}</div>
</a>

<style>
  .poster { display: block; }
  .art {
    position: relative; aspect-ratio: 2 / 3; border-radius: var(--radius); overflow: hidden;
    transition: transform 0.15s var(--ease);
  }
  /* 细描边代替投影：浅色海报边缘也清楚，滚动时不用重画阴影 */
  .art::after { content: ''; position: absolute; inset: 0; border-radius: inherit; box-shadow: inset 0 0 0 1px var(--stroke); pointer-events: none; }
  img { width: 100%; height: 100%; object-fit: cover; transition: opacity 0.25s var(--ease), transform 0.35s var(--ease); }
  .poster:hover img { transform: scale(1.04); }
  .poster:active .art { transform: scale(0.97); }
  .badge {
    position: absolute; top: 6px; right: 6px; min-width: 20px; height: 20px; padding: 0 6px;
    border-radius: 10px; background: var(--accent); color: #fff; font-size: 11px; font-weight: 600;
    display: grid; place-items: center;
  }
  .score {
    position: absolute; right: 0; bottom: 0; left: 0; padding: 14px 8px 5px; text-align: right;
    color: #fff; font-weight: 600; font-size: 12px; background: linear-gradient(transparent, rgba(0, 0, 0, 0.55));
  }
  .name { margin-top: 8px; font-size: 13px; font-weight: 500; }
  .sub { font-size: 12px; color: var(--text-2); margin-top: 1px; min-height: 1.45em; }
</style>
