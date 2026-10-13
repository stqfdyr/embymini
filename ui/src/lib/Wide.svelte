<!-- 横版卡（16:9）：继续观看、单集、媒体库共用。点图片 onplay（悬停出播放键），点文字 href。 -->
<script lang="ts">
  import { Check, Play } from '@lucide/svelte'
  import { fade } from './fade'

  let {
    src, title, sub = '', desc = '', href, onplay, onmenu, onhover, progress = 0, time = '', played = false, score = '',
    selected = false, width = 300,
  }: {
    src: string; title: string; sub?: string; desc?: string; href?: string; onplay?: () => void
    onmenu?: (e: MouseEvent) => void; onhover?: () => void
    progress?: number; time?: string; played?: boolean; score?: string; selected?: boolean; width?: number
  } = $props()
</script>

<!-- svelte-ignore a11y_no_static_element_interactions -->
<div class="wide" style:width={`${width}px`} oncontextmenu={onmenu} onpointerenter={onhover}>
  {#snippet art()}
    {#if src}<img {src} alt="" loading="lazy" use:fade />{/if}
    {#if played}<span class="badge"><Check size={12} strokeWidth={3} /></span>{/if}
    {#if time || score}<span class="corner">{#if time}{time}{:else}{score}{/if}</span>{/if}
    {#if progress > 0}<span class="bar"><i style:width={`${Math.min(progress, 100)}%`}></i></span>{/if}
    {#if onplay}<span class="play"><Play size={20} fill="currentColor" /></span>{/if}
  {/snippet}
  {#if onplay}
    <button class="art ph" class:selected onclick={onplay} aria-label={`播放 ${title}`}>{@render art()}</button>
  {:else}
    <a class="art ph" class:selected {href} aria-label={title}>{@render art()}</a>
  {/if}
  <a class="text" href={href}>
    <div class="name clamp-1">{title}</div>
    {#if sub}<div class="sub clamp-1">{sub}</div>{/if}
    {#if desc}<div class="desc clamp-3">{desc}</div>{/if}
  </a>
</div>

<style>
  .wide { display: block; }
  .art {
    position: relative; display: block; width: 100%; aspect-ratio: 16 / 9; border-radius: var(--radius);
    overflow: hidden; transition: transform 0.15s var(--ease);
  }
  .art::after { content: ''; position: absolute; inset: 0; border-radius: inherit; box-shadow: inset 0 0 0 1px var(--stroke); pointer-events: none; }
  .art.selected::after { box-shadow: inset 0 0 0 2px var(--accent); }
  img { width: 100%; height: 100%; object-fit: cover; transition: opacity 0.25s var(--ease), transform 0.35s var(--ease); }
  .art:hover img { transform: scale(1.03); }
  .art:active { transform: scale(0.98); }
  .badge {
    position: absolute; top: 6px; right: 6px; width: 20px; height: 20px; border-radius: 50%;
    background: var(--accent); color: #fff; display: grid; place-items: center;
  }
  .corner {
    position: absolute; right: 6px; bottom: 8px; padding: 1px 6px; border-radius: 4px;
    background: rgba(0, 0, 0, 0.55); color: #fff; font-size: 11px; font-weight: 600; font-variant-numeric: tabular-nums;
  }
  .bar { position: absolute; left: 0; right: 0; bottom: 0; height: 3px; background: rgba(255, 255, 255, 0.3); }
  .bar i { display: block; height: 100%; background: var(--accent); }
  /* 悬停时中间出现播放键 */
  .play {
    position: absolute; left: 50%; top: 50%; width: 44px; height: 44px; margin: -22px 0 0 -22px; border-radius: 50%;
    display: grid; place-items: center; color: #fff; background: rgba(0, 0, 0, 0.5);
    opacity: 0; transform: scale(0.85); transition: opacity 0.15s, transform 0.15s var(--ease);
  }
  .play :global(svg) { margin-left: 2px; }
  .art:hover .play { opacity: 1; transform: none; }
  .text { display: block; padding: 8px 2px 0; }
  .name { font-size: 13px; font-weight: 500; }
  .sub { font-size: 12px; color: var(--text-2); margin-top: 1px; }
  .desc { font-size: 12px; color: var(--text-2); margin-top: 4px; line-height: 1.5; }
</style>
