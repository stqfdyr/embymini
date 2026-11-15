<!-- Library: poster grid paged as it scrolls; sort plus unplayed-only (filtered server-side so the total is right). -->
<script lang="ts">
  import Dropdown from '../lib/Dropdown.svelte'
  import Grid from '../lib/Grid.svelte'
  import LoadError from '../lib/LoadError.svelte'
  import { LIST_FIELDS, get, loader, pager, peek, uid } from '../lib/api.svelte'
  import { restoreScrollWhen } from '../lib/router.svelte'
  import type { Item } from '../lib/types'

  let { id }: { id: string } = $props()
  const SORTS = { DateCreated: '最近加入', SortName: '名称', ProductionYear: '年份', CommunityRating: '评分' } as const

  let sort = $state<keyof typeof SORTS>('DateCreated')
  let unplayed = $state(false)
  // A new sort, the unplayed toggle or a manual refresh starts over from page one
  const { s: pg, more } = pager(() => ({
    ParentId: id, Recursive: true, IncludeItemTypes: 'Movie,Series', Fields: LIST_FIELDS,
    SortBy: sort === 'SortName' ? 'SortName' : `${sort},SortName`, SortOrder: sort === 'SortName' ? 'Ascending' : 'Descending',
    Filters: unplayed ? 'IsUnplayed' : undefined,
  }))
  restoreScrollWhen(() => pg.loaded)

  // The name is known from the home page's library list; the request only fills it in when opened some other way
  const view = loader(() => get<Item>(`/Users/${uid()}/Items/${id}`))
</script>

<div class="page">
  <div class="page-title">
    <h1>{view.data?.Name ?? peek(id)?.Name ?? ''}</h1>
    {#if pg.total}<span class="muted">{pg.total} 部</span>{/if}
    <span class="grow"></span>
    <div class="seg">
      <button class:on={!unplayed} onclick={() => (unplayed = false)}>全部</button>
      <button class:on={unplayed} onclick={() => (unplayed = true)}>未看</button>
    </div>
    <Dropdown value={sort} items={Object.entries(SORTS).map(([value, label]) => ({ value: value as keyof typeof SORTS, label }))} onselect={(v) => (sort = v)} />
  </div>
  {#if pg.error}<LoadError error={pg.error} retry={more} />{/if}
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
