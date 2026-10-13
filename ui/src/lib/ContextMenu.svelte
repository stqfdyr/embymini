<!-- 右键菜单：位置贴着鼠标，靠近窗口边缘时向内翻；点外面、滚动、Esc、窗口失焦都会关闭。 -->
<script lang="ts">
  import { closeMenu, menu } from './menu.svelte'

  let el: HTMLDivElement | undefined = $state()
  let pos = $state({ x: 0, y: 0 })

  $effect(() => {
    if (!menu.open || !el) return
    const r = el.getBoundingClientRect()
    pos = { x: Math.min(menu.x, innerWidth - r.width - 8), y: Math.min(menu.y, innerHeight - r.height - 8) }
    const close = (e: Event) => !el?.contains(e.target as Node) && closeMenu()
    const esc = (e: KeyboardEvent) => e.key === 'Escape' && closeMenu()
    addEventListener('pointerdown', close, true)
    addEventListener('wheel', close, { capture: true, passive: true })
    addEventListener('keydown', esc)
    addEventListener('blur', closeMenu)
    return () => {
      removeEventListener('pointerdown', close, true)
      removeEventListener('wheel', close, true)
      removeEventListener('keydown', esc)
      removeEventListener('blur', closeMenu)
    }
  })
</script>

{#if menu.open}
  <div class="ctx flyout" role="menu" bind:this={el} style:left={`${pos.x}px`} style:top={`${pos.y}px`}>
    {#each menu.items as it, i (i)}
      {#if it === 'sep'}
        <div class="sep"></div>
      {:else}
        <button role="menuitem" class:danger={it.danger} onclick={() => (closeMenu(), it.action())}>
          <span class="ic">{#if it.icon}<it.icon size={15} />{/if}</span>{it.label}
        </button>
      {/if}
    {/each}
  </div>
{/if}

<style>
  .ctx { position: fixed; z-index: 50; min-width: 200px; padding: 5px; display: flex; flex-direction: column; }
  button { display: flex; align-items: center; gap: 8px; height: 32px; padding: 0 14px 0 8px; border-radius: var(--radius-s); text-align: left; }
  button:hover { background: var(--fill-hover); }
  button.danger { color: var(--danger); }
  .ic { width: 18px; display: grid; place-items: center; color: var(--text-2); }
  .sep { height: 1px; background: var(--stroke); margin: 4px 6px; }
</style>
