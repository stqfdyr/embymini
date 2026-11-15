<!-- Dropdown in place of a native <select> (season, sort, server). Keys ↑↓ / Enter / Esc; closes on a click outside. -->
<script lang="ts" generics="T extends string">
  import { Check, ChevronDown } from '@lucide/svelte'

  let {
    items, value, onselect, big = false, placeholder = '',
  }: { items: { value: T; label: string; hint?: string }[]; value: T | undefined; onselect: (v: T) => void; big?: boolean; placeholder?: string } = $props()

  let open = $state(false)
  let active = $state(0)
  let root: HTMLDivElement
  let list: HTMLDivElement | undefined = $state()
  const cur = $derived(items.find((i) => i.value === value))

  function toggle() {
    open = !open
    if (open) active = Math.max(0, items.findIndex((i) => i.value === value))
  }
  function pick(v: T) {
    open = false
    if (v !== value) onselect(v)
  }
  function key(e: KeyboardEvent) {
    if (!open) {
      if (e.key === 'ArrowDown' || e.key === 'Enter' || e.key === ' ') (e.preventDefault(), toggle())
      return
    }
    if (e.key === 'Escape') open = false
    else if (e.key === 'ArrowDown') active = (active + 1) % items.length
    else if (e.key === 'ArrowUp') active = (active - 1 + items.length) % items.length
    else if (e.key === 'Enter' || e.key === ' ') pick(items[active].value)
    else return
    e.preventDefault()
  }
  // Close on a click outside, a page scroll or the window losing focus
  $effect(() => {
    if (!open) return
    const close = (e: Event) => !root.contains(e.target as Node) && (open = false)
    const blur = () => (open = false)
    addEventListener('pointerdown', close, true)
    addEventListener('wheel', close, { capture: true, passive: true })
    addEventListener('blur', blur)
    return () => {
      removeEventListener('pointerdown', close, true)
      removeEventListener('wheel', close, true)
      removeEventListener('blur', blur)
    }
  })
  // Keep the active item in view in long lists
  $effect(() => {
    if (open && list) (list.children[active] as HTMLElement | undefined)?.scrollIntoView({ block: 'nearest' })
  })
</script>

<!-- svelte-ignore a11y_no_static_element_interactions -->
<div class="dd" bind:this={root} onkeydown={key}>
  <button class="trigger" class:big class:open aria-haspopup="listbox" aria-expanded={open} onclick={toggle}>
    <span>{cur?.label ?? placeholder}</span><ChevronDown size={big ? 18 : 15} />
  </button>
  {#if open}
    <div class="list flyout" role="listbox" bind:this={list}>
      {#each items as it, i (it.value)}
        <button role="option" aria-selected={it.value === value} class:active={i === active} onpointerenter={() => (active = i)} onclick={() => pick(it.value)}>
          <span class="check">{#if it.value === value}<Check size={15} />{/if}</span>
          <span class="label">{it.label}</span>
          {#if it.hint}<span class="hint">{it.hint}</span>{/if}
        </button>
      {/each}
    </div>
  {/if}
</div>

<style>
  .dd { position: relative; display: inline-block; }
  .trigger {
    display: inline-flex; align-items: center; gap: 6px; height: 32px; padding: 0 10px; border-radius: var(--radius-s);
    background: var(--fill); transition: background 0.12s;
  }
  .trigger:hover, .trigger.open { background: var(--fill-hover); }
  .trigger :global(svg) { color: var(--text-2); transition: transform 0.15s var(--ease); }
  .trigger.open :global(svg) { transform: rotate(180deg); }
  .trigger.big { background: none; padding: 0 8px 0 0; font-size: 20px; font-weight: 600; height: 36px; }
  .trigger.big:hover, .trigger.big.open { background: none; color: var(--accent); }
  .list {
    position: absolute; top: calc(100% + 6px); left: 0; z-index: 30; min-width: max(100%, 180px); max-height: 360px; overflow-y: auto;
    padding: 5px; display: flex; flex-direction: column;
  }
  .list button { display: flex; align-items: center; gap: 6px; height: 34px; padding: 0 12px 0 6px; border-radius: var(--radius-s); text-align: left; white-space: nowrap; }
  .list button.active { background: var(--fill-hover); }
  .check { width: 20px; display: grid; place-items: center; color: var(--accent); flex: none; }
  .label { flex: 1; }
  .hint { color: var(--text-3); font-size: 12px; margin-left: 16px; }
</style>
