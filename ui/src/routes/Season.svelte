<!-- Whole season: a grid of its episodes (16:9 cards; click to play, right-click menu, click the overview for the full text), season switch on top. -->
<script lang="ts">
  import Episode from '../lib/Episode.svelte'
  import LoadError from '../lib/LoadError.svelte'
  import SeasonPick from '../lib/SeasonPick.svelte'
  import { EP_FIELDS, get, img, itemDetail, loader, session, uid } from '../lib/api.svelte'
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

  // Switching seasons replaces the history entry, so Back returns to the detail page rather than the previous season
  const pick = (id: string) => location.replace(`#/season/${seriesId}/${id}`)
</script>

<div class="page">
  {#if q.data}
    {@const d = q.data}
    <div class="page-title">
      <h1><a href={`#/item/${seriesId}`}>{d.series.Name}</a></h1>
      <SeasonPick seasons={d.seasons} value={seasonId} onselect={pick} />
      <span class="muted">{d.eps.length} 集</span>
    </div>
    <div class="grid">
      {#each d.eps as ep (ep.Id)}<Episode {ep} fallback={img(seriesId, 'Backdrop', d.series.BackdropImageTags?.[0], 300)} width={300} />{/each}
    </div>
    {#if !d.eps.length}<div class="center-msg">这一季还没有剧集</div>{/if}
  {:else if q.error}
    <LoadError error={q.error} />
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
