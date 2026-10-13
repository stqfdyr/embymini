<!-- 媒体库：海报网格，滚到底加载下一页；排序 + 只看未看（服务端过滤，总数才准）。 -->
<script lang="ts">
  import Dropdown from '../lib/Dropdown.svelte'
  import Grid from '../lib/Grid.svelte'
  import { LIST_FIELDS, get, pager, uid } from '../lib/api.svelte'
  import { restoreScrollWhen } from '../lib/router.svelte'
  import type { Item } from '../lib/types'

  let { id }: { id: string } = $props()
  const SORTS = { DateCreated: '最近加入', SortName: '名称', ProductionYear: '年份', CommunityRating: '评分' } as const

  let sort = $state<keyof typeof SORTS>('DateCreated')
  let unplayed = $state(false)
  // 换排序、切换「只看未看」、手动刷新：都从第一页重来
  const { s: pg, more } = pager(() => ({
    ParentId: id, Recursive: true, IncludeItemTypes: 'Movie,Series', Fields: LIST_FIELDS,
    SortBy: sort === 'SortName' ? 'SortName' : `${sort},SortName`, SortOrder: sort === 'SortName' ? 'Ascending' : 'Descending',
    Filters: unplayed ? 'IsUnplayed' : undefined,
  }))
  restoreScrollWhen(() => pg.loaded)

  const view = $derived(get<Item>(`/Users/${uid()}/Items/${id}`))
</script>

<div class="page">
  <div class="page-title">
    {#await view then v}<h1>{v.Name}</h1>{/await}
    {#if pg.total}<span class="muted">{pg.total} 部</span>{/if}
    <span class="grow"></span>
    <div class="seg">
      <button class:on={!unplayed} onclick={() => (unplayed = false)}>全部</button>
      <button class:on={unplayed} onclick={() => (unplayed = true)}>未看</button>
    </div>
    <Dropdown value={sort} items={Object.entries(SORTS).map(([value, label]) => ({ value: value as keyof typeof SORTS, label }))} onselect={(v) => (sort = v)} />
  </div>
  {#if pg.error}<div class="center-msg">加载失败：{pg.error}</div>{/if}
  <Grid items={pg.items} onmore={more} loading={pg.busy} />
  {#if pg.loaded && !pg.items.length}<div class="center-msg">{unplayed ? '这里都看过了' : '这个媒体库是空的'}</div>{/if}
</div>

<style>
  .page-title { align-items: center; }
  .grow { flex: 1; }
  .seg { display: flex; padding: 2px; border-radius: 8px; background: var(--fill); }
  .seg button { height: 28px; padding: 0 14px; border-radius: 6px; font-size: 13px; color: var(--text-2); transition: background 0.12s, color 0.12s; }
  .seg button.on { background: var(--bg-raised); color: var(--text); font-weight: 600; box-shadow: 0 1px 2px rgba(0, 0, 0, 0.12), 0 0 0 0.5px var(--stroke); }
</style>
