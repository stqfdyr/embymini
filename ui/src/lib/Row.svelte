<!-- 横滑行：标题 + 右侧数量 / 更多；悬停时左右出箭头，一次滚一屏。 -->
<script lang="ts">
  import type { Snippet } from 'svelte'
  import { ChevronLeft, ChevronRight } from '@lucide/svelte'

  let { title = '', count, href, head, children }: { title?: string; count?: number; href?: string; head?: Snippet; children: Snippet } = $props()
  let el: HTMLDivElement
  let atStart = $state(true)
  let atEnd = $state(false)

  function update() {
    atStart = el.scrollLeft < 4
    atEnd = el.scrollLeft + el.clientWidth >= el.scrollWidth - 4
  }
  const page = (dir: number) => el.scrollBy({ left: dir * (el.clientWidth - 120), behavior: 'smooth' })
  $effect(() => {
    update()
    const ro = new ResizeObserver(update)
    ro.observe(el)
    // 整行快进入视口时，行里屏幕右边看不到的海报也开始加载（懒加载只管视口内的，往右滑才开始取会看到空卡）
    const io = new IntersectionObserver((es) => {
      if (!es[0].isIntersecting) return
      el.querySelectorAll<HTMLImageElement>('img[loading="lazy"]').forEach((i) => (i.loading = 'eager'))
    }, { rootMargin: '600px 0px' })
    io.observe(el)
    return () => (ro.disconnect(), io.disconnect())
  })
</script>

<section class="section">
  {#if title || head}
    <div class="section-head">
      {#if head}{@render head()}{:else}<h2>{title}</h2>{/if}
      {#if href}
        <a class="more" {href}>{count ?? ''}<ChevronRight size={16} /></a>
      {:else if count != null}
        <span class="more">{count}</span>
      {/if}
    </div>
  {/if}
  <div class="wrap">
    <button class="arrow l" class:off={atStart} aria-label="向左" tabindex="-1" onclick={() => page(-1)}><ChevronLeft size={20} /></button>
    <div class="track" bind:this={el} onscroll={update}>
      {@render children()}
    </div>
    <button class="arrow r" class:off={atEnd} aria-label="向右" tabindex="-1" onclick={() => page(1)}><ChevronRight size={20} /></button>
  </div>
</section>

<style>
  .wrap { position: relative; }
  .track {
    display: flex; gap: 16px; overflow-x: auto; overscroll-behavior-x: contain;
    padding: 0 var(--gutter) 4px; scroll-padding: 0 var(--gutter); scrollbar-width: none;
  }
  .track::-webkit-scrollbar { display: none; }
  .track > :global(*) { flex: none; }
  .arrow {
    position: absolute; top: 0; bottom: 0; z-index: 2; width: calc(var(--gutter) + 12px);
    display: grid; place-items: center; color: var(--text);
    opacity: 0; transition: opacity 0.15s;
  }
  .arrow :global(svg) {
    width: 32px; height: 32px; padding: 6px; border-radius: 50%;
    background: var(--flyout-bg); box-shadow: var(--flyout-shadow); transition: transform 0.12s var(--ease);
  }
  .arrow:hover :global(svg) { transform: scale(1.08); }
  .arrow:active :global(svg) { transform: scale(0.94); }
  .wrap:hover .arrow:not(.off) { opacity: 1; }
  .arrow.off { pointer-events: none; }
  .arrow.l { left: 0; }
  .arrow.r { right: 0; }
</style>
