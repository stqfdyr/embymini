<!-- A failed load: the reason plus a retry button (a manual refresh unless the page passes its own retry).
     `inline` fits it into a row (e.g. the detail page's episode row) instead of a centered page message. -->
<script lang="ts">
  import { errText, refresh } from './api.svelte'

  let { error, what = '加载失败', retry = refresh, inline = false }: { error: unknown; what?: string; retry?: () => unknown; inline?: boolean } = $props()
</script>

<div class={inline ? 'muted inline' : 'center-msg'}>{what}：{errText(error)}<button class="btn" onclick={() => retry()}>重试</button></div>

<style>
  .btn { margin-left: 12px; }
  .inline { display: flex; align-items: center; }
</style>
