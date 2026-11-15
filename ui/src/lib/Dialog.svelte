<!-- Dialogs: long read-only text (selectable, copyable) and confirmations. Call showText / ask anywhere; <Dialog /> in App
     renders them. Esc or a click outside closes (a confirmation counts that as cancel). -->
<script module lang="ts">
  type State = { title: string; text: string; ok?: string; danger?: boolean; resolve?: (yes: boolean) => void }
  const dlg = $state<{ cur: State | null }>({ cur: null })

  function close(yes = false) {
    dlg.cur?.resolve?.(yes)
    dlg.cur = null
  }

  /** Read-only long text (episode overview…) with a copy button. */
  export function showText(title: string, text: string) {
    close()
    dlg.cur = { title, text }
  }

  /** Confirmation: true for OK; false for cancel, Esc or a click outside. A newer dialog cancels a pending one. */
  export function ask(title: string, text: string, ok = '确定', danger = false) {
    close()
    return new Promise<boolean>((resolve) => {
      dlg.cur = { title, text, ok, danger, resolve }
    })
  }
</script>

<script lang="ts">
  import { Copy } from '@lucide/svelte'
  import { notify } from './api.svelte'

  let okBtn: HTMLButtonElement | undefined = $state()
  let cancelBtn: HTMLButtonElement | undefined = $state()
  // A destructive confirmation focuses cancel, so a stray Enter does not delete anything
  $effect(() => (dlg.cur?.danger ? cancelBtn : okBtn)?.focus())

  const copy = (text: string) =>
    navigator.clipboard.writeText(text).then(
      () => notify('已复制'),
      () => notify('复制失败'),
    )
</script>

<svelte:window onkeydown={(e) => dlg.cur && e.key === 'Escape' && (e.stopPropagation(), close())} />

{#if dlg.cur}
  {@const d = dlg.cur}
  <!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions -->
  <div class="modal" onclick={(e) => e.target === e.currentTarget && close()}>
    <div class="dialog flyout" role="dialog" aria-modal="true" aria-label={d.title}>
      <h3>{d.title}</h3>
      <p class="selectable">{d.text}</p>
      <div class="actions">
        {#if d.resolve}
          <button class="btn" bind:this={cancelBtn} onclick={() => close(false)}>取消</button>
          <button class="btn primary" class:danger={d.danger} bind:this={okBtn} onclick={() => close(true)}>{d.ok}</button>
        {:else}
          <button class="btn" onclick={() => copy(d.text)}><Copy size={14} /> 复制</button>
          <button class="btn primary" bind:this={okBtn} onclick={() => close()}>关闭</button>
        {/if}
      </div>
    </div>
  </div>
{/if}

<style>
  .dialog { width: min(560px, calc(100vw - 48px)); max-height: calc(100vh - 120px); display: flex; flex-direction: column; padding: 20px 22px 16px; transform-origin: center; }
  h3 { margin: 0 0 10px; font-size: 16px; font-weight: 600; }
  p { margin: 0; overflow: auto; font-size: 14px; line-height: 1.7; white-space: pre-wrap; }
  .actions { display: flex; justify-content: flex-end; gap: 8px; margin-top: 18px; }
  .btn.primary.danger { background: var(--danger); color: var(--accent-text); }
</style>
