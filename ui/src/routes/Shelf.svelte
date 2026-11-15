<!-- Walls behind the home rows' counts: all libraries (#/views) or everything in continue watching (#/resume, paged),
     as a grid of the same 16:9 cards the home rows use. `kind` is fixed per instance (App keys it by route). -->
<script lang="ts">
  import { untrack } from 'svelte'
  import Grid from '../lib/Grid.svelte'
  import LoadError from '../lib/LoadError.svelte'
  import Wide from '../lib/Wide.svelte'
  import { LIST_FIELDS, browsable, get, img, loader, pager, play, prefetch, resumeMenu, session, uid, wideImg } from '../lib/api.svelte'
  import { openMenu } from '../lib/menu.svelte'
  import { clock, episodeLabel } from '../lib/fmt'
  import { restoreScrollWhen } from '../lib/router.svelte'
  import type { Item, Page } from '../lib/types'

  let { kind }: { kind: 'views' | 'resume' } = $props()
  const fixed = untrack(() => kind) // keyed by route in App, so it never changes

  const views =
    fixed === 'views'
      ? loader(() => {
          void session.refresh
          return get<Page>(`/Users/${uid()}/Views`).then((p) => browsable(p.Items))
        }, () => 'shelf:views')
      : null
  // Quiet reloads too (pager itself follows only manual ones): removing, marking played or playing changes this list
  const resume =
    fixed === 'resume'
      ? pager(() => {
          void session.quiet
          return { MediaTypes: 'Video', Fields: LIST_FIELDS }
        }, 'Items/Resume')
      : null
  restoreScrollWhen(() => views?.data ?? resume?.s.loaded)
  const count = $derived(views?.data ? `${views.data.length} 个` : resume?.s.loaded ? `${resume.s.total}${resume.s.exact ? '' : '+'} 部` : '')
</script>

{#snippet library(v: Item)}
  <Wide src={img(v.Id, 'Primary', v.ImageTags?.Primary, 300)} title={v.Name} href={`#/library/${v.Id}`} />
{/snippet}
{#snippet watching(it: Item)}
  <Wide
    src={wideImg(it, 300)}
    title={it.SeriesName ?? it.Name}
    sub={episodeLabel(it)}
    progress={it.UserData?.PlayedPercentage ?? 0}
    time={`${clock(it.UserData?.PlaybackPositionTicks)} / ${clock(it.RunTimeTicks)}`}
    onplay={() => play(it.Id)}
    onmenu={(e) => openMenu(e, resumeMenu(it))}
    onhover={() => prefetch(it.SeriesId ?? it.Id)}
    href={`#/item/${it.SeriesId ?? it.Id}`}
  />
{/snippet}

<div class="page">
  <div class="page-title">
    <h1>{kind === 'views' ? '我的媒体' : '继续观看'}</h1>
    {#if count}<span class="muted">{count}</span>{/if}
  </div>
  {#if views}
    {#if views.data}
      <Grid wide items={views.data} card={library} />
      {#if !views.data.length}<div class="center-msg">没有可浏览的媒体库</div>{/if}
    {:else if views.error}
      <LoadError error={views.error} />
    {:else}
      <Grid wide items={[]} loading />
    {/if}
  {:else if resume}
    {@const pg = resume.s}
    <Grid wide items={pg.items} card={watching} onmore={resume.more} loading={pg.busy} />
    <!-- Below the grid: a later page failing shows where loading stopped -->
    {#if pg.error}<LoadError error={pg.error} retry={resume.retry} />{/if}
    {#if pg.loaded && !pg.items.length}<div class="center-msg">没有看到一半的内容</div>{/if}
  {/if}
</div>
