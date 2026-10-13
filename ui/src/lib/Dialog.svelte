<!-- 对话框：长文本查看（可选中、可复制）和确认框。任何地方调 showText / ask，App 里的 <Dialog /> 负责显示。
     Esc、点外面关闭（确认框视为取消）。 -->
<script module lang="ts">
  type State = { title: string; text: string; ok?: string; danger?: boolean; resolve?: (yes: boolean) => void }
  const dlg = $state<{ cur: State | null }>({ cur: null })

  function close(yes = false) {
    dlg.cur?.resolve?.(yes)
    dlg.cur = null
  }

  /** 只读长文本（单集简介等）：带「复制」按钮。 */
  export function showText(title: string, text: string) {
    close()
    dlg.cur = { title, text }
  }

  /** 确认框：点确定为 true；取消、Esc、点外面为 false。 */
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
  $effect(() => okBtn?.focus())

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
  <div class="backdrop" onclick={(e) => e.target === e.currentTarget && close()}>
    <div class="dialog flyout" role="dialog" aria-modal="true" aria-label={d.title}>
      <h3>{d.title}</h3>
      <p class="selectable">{d.text}</p>
      <div class="actions">
        {#if d.resolve}
          <button class="btn" onclick={() => close(false)}>取消</button>
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
  .backdrop { position: fixed; inset: 0; z-index: 45; display: grid; place-items: center; background: rgba(0, 0, 0, 0.32); animation: dim-in 0.14s; }
  @keyframes dim-in { from { opacity: 0; } }
  .dialog { width: min(560px, calc(100vw - 48px)); max-height: calc(100vh - 120px); display: flex; flex-direction: column; padding: 20px 22px 16px; transform-origin: center; }
  h3 { margin: 0 0 10px; font-size: 16px; font-weight: 600; }
  p { margin: 0; overflow: auto; font-size: 14px; line-height: 1.7; white-space: pre-wrap; }
  .actions { display: flex; justify-content: flex-end; gap: 8px; margin-top: 18px; }
  .btn.primary.danger { background: var(--danger); color: #fff; }
</style>
