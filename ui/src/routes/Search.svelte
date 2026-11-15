<!-- Search: queries once typing pauses for 250 ms; the term and results survive leaving the page. Esc clears. -->
<script module lang="ts">
  let keep = $state({ q: '' })
</script>

<script lang="ts">
  import { Search, X } from '@lucide/svelte'
  import Grid from '../lib/Grid.svelte'
  import LoadError from '../lib/LoadError.svelte'
  import { LIST_FIELDS, get, uid } from '../lib/api.svelte'
  import type { Page } from '../lib/types'

  let q = $state(keep.q)
  let term = $state(keep.q)
  let input: HTMLInputElement
  $effect(() => {
    const v = q.trim()
    keep.q = q
    const t = setTimeout(() => (term = v), 250)
    return () => clearTimeout(t)
  })
  $effect(() => input.focus())
  /** Bumped by the retry button; failed requests are not cached, so reading it queries again. */
  let attempt = $state(0)
  const result = $derived(
    (void attempt, term) ? get<Page>(`/Users/${uid()}/Items`, { Recursive: true, SearchTerm: term, IncludeItemTypes: 'Movie,Series', Limit: 60, Fields: LIST_FIELDS }) : null,
  )
</script>

<div class="page">
  <label class="box">
    <Search size={18} />
    <input bind:this={input} bind:value={q} placeholder="搜索电影、剧集" spellcheck="false" onkeydown={(e) => e.key === 'Escape' && (q = '')} />
    {#if q}<button class="clear" aria-label="清空" onclick={() => ((q = ''), input.focus())}><X size={14} /></button>{/if}
  </label>
  {#if result}
    {#await result}
      <Grid items={[]} loading />
    {:then r}
      {#if r.Items.length}<Grid items={r.Items} />{:else}<div class="center-msg">没有找到「{term}」</div>{/if}
    {:catch e}
      <LoadError error={e} what="搜索失败" retry={() => attempt++} />
    {/await}
  {:else}
    <div class="center-msg">输入片名搜索电影和剧集</div>
  {/if}
</div>

<style>
  .box {
    display: flex; align-items: center; gap: 10px; margin: 12px auto 30px; width: min(600px, 90%); height: 44px;
    padding: 0 8px 0 16px; border-radius: 22px; background: var(--field-bg); border: 1px solid var(--field-border);
    box-shadow: var(--field-shade); color: var(--text-2); transition: border-color 0.15s, box-shadow 0.15s;
  }
  .box:focus-within { border-color: var(--accent); box-shadow: 0 0 0 3.5px var(--ring); }
  input::placeholder { color: var(--text-3); }
  input { flex: 1; border: 0; outline: 0; background: none; font-size: 15px; color: var(--text); }
  .clear { width: 26px; height: 26px; border-radius: 50%; display: grid; place-items: center; background: var(--fill); }
  .clear:hover { background: var(--fill-hover); }
</style>
