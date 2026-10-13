<!-- 首页：轮播 → 我的媒体 → 继续观看 → 每个媒体库的最新内容。 -->
<script lang="ts">
  import Hero from '../lib/Hero.svelte'
  import LibraryRow from '../lib/LibraryRow.svelte'
  import Row from '../lib/Row.svelte'
  import Wide from '../lib/Wide.svelte'
  import { LIST_FIELDS, errText, get, img, loader, play, prefetch, resumeMenu, session, uid, wideImg } from '../lib/api.svelte'
  import { openMenu } from '../lib/menu.svelte'
  import { clock, episodeLabel } from '../lib/fmt'
  import { restoreScrollWhen } from '../lib/router.svelte'
  import type { Page } from '../lib/types'

  /** 只有这些类型的库能按「电影 / 剧集」浏览（音乐、直播、播放列表不在首页显示）。 */
  const BROWSABLE = new Set(['movies', 'tvshows', 'homevideos', 'boxsets', 'mixed', undefined])

  // 轮播是随机取的，只在手动刷新时换；播放结束的后台刷新只更新媒体库与继续观看
  const hero = loader(() => {
    void session.refresh
    return get<Page>(`/Users/${uid()}/Items`, { Recursive: true, IncludeItemTypes: 'Movie,Series', SortBy: 'Random', ImageTypes: 'Backdrop', Limit: 8, Fields: LIST_FIELDS })
  })
  const q = loader(() => {
    void (session.refresh + session.quiet)
    const u = uid()
    return Promise.all([
      get<Page>(`/Users/${u}/Views`),
      get<Page>(`/Users/${u}/Items/Resume`, { Limit: 16, MediaTypes: 'Video', Fields: LIST_FIELDS }),
    ]).then(([views, resume]) => ({ views: views.Items.filter((v) => BROWSABLE.has(v.CollectionType)), resume: resume.Items }))
  })
  restoreScrollWhen(() => q.data)
</script>

{#if q.data && (hero.data || hero.error)}
  {@const d = q.data}
  <Hero items={hero.data?.Items ?? []} />

  <Row title="我的媒体" count={d.views.length}>
    {#each d.views as v (v.Id)}
      <Wide src={img(v.Id, 'Primary', v.ImageTags?.Primary, 260)} title={v.Name} href={`#/library/${v.Id}`} width={260} />
    {/each}
  </Row>

  {#if d.resume.length}
    <Row title="继续观看" count={d.resume.length}>
      {#each d.resume as it (it.Id)}
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
          width={300}
        />
      {/each}
    </Row>
  {/if}

  {#each d.views as v (v.Id)}<LibraryRow view={v} />{/each}
{:else if q.error}
  <div class="center-msg">加载失败：{errText(q.error)}</div>
{:else}
  <div class="skel-hero skeleton"></div>
  {#each { length: 2 } as _, r (r)}
    <div class="skel-row">{#each { length: 7 } as _, k (k)}<div class="skeleton"></div>{/each}</div>
  {/each}
{/if}

<style>
  .skel-hero { height: min(58vh, 600px); margin-top: calc(-1 * var(--bar-h) - 8px); border-radius: 0; opacity: 0.5; }
  .skel-row { display: flex; gap: 16px; padding: 30px var(--gutter) 0; overflow: hidden; }
  .skel-row > div { flex: none; width: 300px; aspect-ratio: 16 / 9; }
</style>
