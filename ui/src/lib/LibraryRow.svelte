<!-- 首页的「某个媒体库最新内容」一行：自己取数，刷新时保留旧内容原地更新。 -->
<script lang="ts">
  import Poster from './Poster.svelte'
  import Row from './Row.svelte'
  import { LIST_FIELDS, get, loader, session, uid } from './api.svelte'
  import type { Item, Page } from './types'

  let { view }: { view: Item } = $props()

  const q = loader(() => {
    void (session.refresh + session.quiet)
    const u = uid()
    return Promise.all([
      get<Item[]>(`/Users/${u}/Items/Latest`, { ParentId: view.Id, Limit: 16, Fields: LIST_FIELDS }),
      get<Page>(`/Users/${u}/Items`, { ParentId: view.Id, Recursive: true, IncludeItemTypes: 'Movie,Series', Limit: 0 }),
    ]).then(([latest, page]) => ({ latest, total: page.TotalRecordCount }))
  })
</script>

{#if q.data?.latest.length}
  <Row title={view.Name} count={q.data.total} href={`#/library/${view.Id}`}>
    {#each q.data.latest as it (it.Id)}<Poster item={it} />{/each}
  </Row>
{/if}
