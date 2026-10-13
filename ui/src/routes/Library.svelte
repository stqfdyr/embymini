<!-- 媒体库：海报网格，滚到底加载下一页；排序 + 只看未看（服务端过滤，总数才准）。 -->
<script lang="ts">
  import { untrack } from 'svelte'
  import Dropdown from '../lib/Dropdown.svelte'
  import Grid from '../lib/Grid.svelte'
  import { LIST_FIELDS, errText, get, session, uid } from '../lib/api.svelte'
  import { restoreScroll } from '../lib/router.svelte'
  import type { Item, Page } from '../lib/types'

  let { id }: { id: string } = $props()
  const SORTS = { DateCreated: '最近加入', SortName: '名称', ProductionYear: '年份', CommunityRating: '评分' } as const
  const PAGE = 60

  let sort = $state<keyof typeof SORTS>('DateCreated')
  let unplayed = $state(false)
  let items = $state<Item[]>([])
  let total = $state(0)
  let busy = $state(false)
  let error = $state('')
  /** 每次重置加一；旧请求晚到的结果按代次丢弃，不会混进新列表。 */
  let gen = 0
  let restored = false

  const view = $derived(get<Item>(`/Users/${uid()}/Items/${id}`))

  async function more() {
    if (busy || (items.length && items.length >= total)) return
    busy = true
    const mine = gen
    try {
      const p = await get<Page>(`/Users/${uid()}/Items`, {
        ParentId: id, Recursive: true, IncludeItemTypes: 'Movie,Series', Fields: LIST_FIELDS,
        SortBy: sort === 'SortName' ? 'SortName' : `${sort},SortName`, SortOrder: sort === 'SortName' ? 'Ascending' : 'Descending',
        Filters: unplayed ? 'IsUnplayed' : undefined, StartIndex: items.length, Limit: PAGE,
      })
      if (mine !== gen) return
      items.push(...p.Items)
      total = p.TotalRecordCount
      if (!restored) ((restored = true), restoreScroll())
    } catch (e) {
      if (mine === gen) error = errText(e)
    } finally {
      if (mine === gen) busy = false
    }
  }

  // 换排序、切换「只看未看」、手动刷新：都从第一页重来
  $effect(() => {
    void [sort, unplayed, session.refresh]
    gen++
    items = []
    total = 0
    error = ''
    busy = false
    untrack(more)
  })
</script>

<div class="page">
  <div class="page-title">
    {#await view then v}<h1>{v.Name}</h1>{/await}
    {#if total}<span class="muted">{total} 部</span>{/if}
    <span class="grow"></span>
    <div class="seg">
      <button class:on={!unplayed} onclick={() => (unplayed = false)}>全部</button>
      <button class:on={unplayed} onclick={() => (unplayed = true)}>未看</button>
    </div>
    <Dropdown value={sort} items={Object.entries(SORTS).map(([value, label]) => ({ value: value as keyof typeof SORTS, label }))} onselect={(v) => (sort = v)} />
  </div>
  {#if error}<div class="center-msg">加载失败：{error}</div>{/if}
  <Grid {items} onmore={more} loading={busy} />
  {#if !busy && !error && !items.length}<div class="center-msg">{unplayed ? '这里都看过了' : '这个媒体库是空的'}</div>{/if}
</div>

<style>
  .page-title { align-items: center; }
  .grow { flex: 1; }
  .seg { display: flex; padding: 2px; border-radius: 8px; background: var(--fill); }
  .seg button { height: 28px; padding: 0 14px; border-radius: 6px; font-size: 13px; color: var(--text-2); transition: background 0.12s, color 0.12s; }
  .seg button.on { background: var(--bg-raised); color: var(--text); font-weight: 600; box-shadow: 0 1px 2px rgba(0, 0, 0, 0.12), 0 0 0 0.5px var(--stroke); }
</style>
