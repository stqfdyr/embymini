<!-- A failed load: the reason plus a retry button (a manual refresh unless the page passes its own retry). -->
<script lang="ts">
  import { errText, refresh } from './api.svelte'

  let { error, what = '加载失败', retry = refresh }: { error: unknown; what?: string; retry?: () => unknown } = $props()
</script>

<div class="center-msg">{what}：{errText(error)}<button class="btn" onclick={() => retry()}>重试</button></div>

<style>
  .btn { margin-left: 12px; }
</style>
