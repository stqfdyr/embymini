<!-- 收藏：电影与剧集。 -->
<script lang="ts">
  import Grid from '../lib/Grid.svelte'
  import { LIST_FIELDS, errText, get, loader, session, uid } from '../lib/api.svelte'
  import { restoreScrollWhen } from '../lib/router.svelte'
  import type { Page } from '../lib/types'

  const q = loader(() => {
    void (session.refresh + session.quiet)
    return get<Page>(`/Users/${uid()}/Items`, { Recursive: true, Filters: 'IsFavorite', IncludeItemTypes: 'Movie,Series', SortBy: 'SortName', Fields: LIST_FIELDS })
  })
  restoreScrollWhen(() => q.data)
</script>

<div class="page">
  <div class="page-title"><h1>收藏</h1>{#if q.data?.Items.length}<span class="muted">{q.data.Items.length} 部</span>{/if}</div>
  {#if q.data}
    {#if q.data.Items.length}<Grid items={q.data.Items} />{:else}<div class="center-msg">还没有收藏。在详情页点 ♡，或在海报上右键 →「收藏」</div>{/if}
  {:else if q.error}
    <div class="center-msg">加载失败：{errText(q.error)}</div>
  {:else}
    <Grid items={[]} loading />
  {/if}
</div>
