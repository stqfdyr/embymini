<!-- 整季：本季全部单集的网格（16:9 卡，点图播放、右键菜单、点简介看全文），顶部可切季。 -->
<script lang="ts">
  import Dropdown from '../lib/Dropdown.svelte'
  import Episode from '../lib/Episode.svelte'
  import { EP_FIELDS, errText, get, img, itemDetail, loader, session, uid } from '../lib/api.svelte'
  import { restoreScrollWhen } from '../lib/router.svelte'
  import type { Page } from '../lib/types'

  let { seriesId, seasonId }: { seriesId: string; seasonId: string } = $props()

  const q = loader(() => {
    void (session.refresh + session.quiet)
    const u = uid()
    return Promise.all([
      itemDetail(seriesId),
      get<Page>(`/Shows/${seriesId}/Seasons`, { UserId: u, Fields: 'UserData' }),
      get<Page>(`/Shows/${seriesId}/Episodes`, { UserId: u, SeasonId: seasonId, Fields: EP_FIELDS }),
    ]).then(([series, seasons, eps]) => ({ series, seasons: seasons.Items, eps: eps.Items }))
  }, () => `season:${seriesId}:${seasonId}`)
  restoreScrollWhen(() => q.data)

  // 切季替换当前历史记录：返回键回到详情页，而不是上一季
  const pick = (id: string) => location.replace(`#/season/${seriesId}/${id}`)
</script>

<div class="page">
  {#if q.data}
    {@const d = q.data}
    <div class="page-title">
      <h1><a href={`#/item/${seriesId}`}>{d.series.Name}</a></h1>
      {#if d.seasons.length > 1}
        <Dropdown big value={seasonId} items={d.seasons.map((s) => ({ value: s.Id, label: s.Name, hint: s.UserData?.UnplayedItemCount ? `${s.UserData.UnplayedItemCount} 集未看` : '' }))} onselect={pick} />
      {:else}
        <h2>{d.seasons[0]?.Name ?? ''}</h2>
      {/if}
      <span class="muted">{d.eps.length} 集</span>
    </div>
    <div class="grid">
      {#each d.eps as ep (ep.Id)}<Episode {ep} fallback={img(seriesId, 'Backdrop', d.series.BackdropImageTags?.[0], 300)} width={300} />{/each}
    </div>
    {#if !d.eps.length}<div class="center-msg">这一季还没有剧集</div>{/if}
  {:else if q.error}
    <div class="center-msg">加载失败：{errText(q.error)}</div>
  {:else}
    <div class="grid">{#each { length: 12 } as _, k (k)}<div class="skeleton cell"></div>{/each}</div>
  {/if}
</div>

<style>
  .page-title { align-items: center; }
  .page-title h1 a:hover { color: var(--accent); }
  .grid { display: grid; grid-template-columns: repeat(auto-fill, 300px); justify-content: space-between; gap: 28px 16px; padding: 0 var(--gutter); }
  .grid > :global(*) { content-visibility: auto; contain-intrinsic-size: 300px 260px; }
  .cell { width: 300px; aspect-ratio: 16 / 9; }
</style>
