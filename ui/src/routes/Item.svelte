<!-- Detail page (series / movie): backdrop + logo + actions + overview → seasons and episodes → cast → artwork → similar → links → media info.
     The top draws at once from the item already seen in a list (peek); detail, seasons and episodes load in parallel and fill in place.
     With several versions (1080p, 4K…) the tech line becomes a version dropdown; play and media info follow the chosen version. -->
<script lang="ts">
  import { openUrl } from '@tauri-apps/plugin-opener'
  import { Check, Heart, Link, Play, RotateCcw, User } from '@lucide/svelte'
  import Dropdown from '../lib/Dropdown.svelte'
  import Episode from '../lib/Episode.svelte'
  import LoadError from '../lib/LoadError.svelte'
  import MediaInfo from '../lib/MediaInfo.svelte'
  import Poster from '../lib/Poster.svelte'
  import Row from '../lib/Row.svelte'
  import SeasonPick from '../lib/SeasonPick.svelte'
  import { BACKDROP_W, BLUR_W, DETAIL_FIELDS, EP_FIELDS, LIST_FIELDS, errText, get, img, imgOriginal, itemDetail, loader, peek, play, session, setFavorite, setPlayed, snapKey, uid } from '../lib/api.svelte'
  import { readSnap, writeSnap } from '../lib/snap'
  import { fade } from '../lib/fade'
  import { clock, date, minutes, rating, techLine } from '../lib/fmt'
  import { restoreScrollWhen } from '../lib/router.svelte'
  import type { Item, Page } from '../lib/types'

  let { id }: { id: string } = $props()

  const ROLE: Record<string, string> = { Actor: '演员', Director: '导演', Writer: '编剧', Producer: '制片', GuestStar: '客串', Composer: '作曲' }

  /** The item already seen in a list; for an episode, its series. */
  function known(id: string) {
    const p = peek(id)
    return p?.Type === 'Episode' ? (p.SeriesId ? peek(p.SeriesId) : undefined) : p
  }

  /** Requests that go with a detail: seasons and next up (series only), and similar titles. */
  function related(id: string, series: boolean) {
    const u = uid()
    return {
      extras: series
        ? Promise.all([
            get<Page>(`/Shows/${id}/Seasons`, { UserId: u, Fields: 'UserData' }),
            get<Page>('/Shows/NextUp', { SeriesId: id, UserId: u, Limit: 1, Fields: DETAIL_FIELDS }),
          ])
        : undefined,
      // Similar titles are decoration: a failure (upstream 429…) leaves the row out instead of failing the page
      similar: get<Page>(`/Items/${id}/Similar`, { UserId: u, Limit: 16, Fields: LIST_FIELDS }).catch((): Page => ({ Items: [], TotalRecordCount: 0 })),
    }
  }

  async function load(id: string) {
    // When a list already says this is a series, send its extras along with the detail instead of after it
    const guess = peek(id)
    const guessSeries = guess?.Type === 'Series' ? guess.Id : guess?.Type === 'Episode' ? guess.SeriesId : undefined
    const early = guessSeries ? related(guessSeries, true) : undefined
    // Only awaited if the guess holds; a failure of a discarded or never-reached guess must not be an unhandled rejection
    early?.extras?.catch(() => {})

    let item = await itemDetail(id)
    let season: string | undefined
    if (item.Type === 'Episode' && item.SeriesId) {
      season = item.SeasonId // opened from an episode: show its series with that season selected
      item = await itemDetail(item.SeriesId)
    }
    const r = early && guessSeries === item.Id ? early : related(item.Id, item.Type === 'Series')
    const [extras, similar] = await Promise.all([r.extras, r.similar])
    const [seasons, nextUp] = extras ?? []
    const next = nextUp?.Items[0]
    return { item, seasons: seasons?.Items ?? [], next, similar: similar.Items, season: season ?? next?.SeasonId ?? seasons?.Items[0]?.Id }
  }

  // A refresh (manual or after playback) keeps the current content; new data replaces it in place
  const q = loader(() => {
    void (session.refresh + session.quiet)
    return load(id)
  }, () => `item:${id}`)
  const loaded = $derived(q.data)
  const item = $derived(q.data?.item ?? known(id))
  let pickedSeason = $state<string>()
  let expanded = $state(false)
  const logo = $derived(img(item?.Id, 'Logo', item?.ImageTags?.Logo, 400))
  // The item whose logo failed, shown as the title instead; a refresh brings a new object, so the logo is tried again (the backend remembers upstream 404s)
  let badLogo = $state.raw<Item>()
  restoreScrollWhen(() => q.data)

  // Episodes load on their own. Only a season change swaps the list (to that season's snapshot, else empty); a refetch keeps the old list until the new one arrives
  let eps = $state<Item[]>([])
  let epsLoading = $state(true)
  /** Episode fetch failed: say so rather than "no episodes yet". */
  let epsError = $state('')
  let epsKey = ''
  const seasonId = $derived(pickedSeason ?? loaded?.season)
  $effect(() => {
    const seriesId = loaded?.item.Type === 'Series' ? loaded.item.Id : undefined
    const season = seasonId
    const key = `${seriesId}|${season}`
    const snap = snapKey(`eps:${key}`)
    if (key !== epsKey) {
      epsKey = key
      const cached = readSnap<Item[]>(snap) ?? [] // not from eps: reading it here would rerun this effect whenever a fetch writes it
      eps = cached
      epsLoading = !cached.length
    }
    epsError = ''
    if (!seriesId || !season) return
    let alive = true
    get<Page>(`/Shows/${seriesId}/Episodes`, { UserId: uid(), SeasonId: season, Fields: EP_FIELDS })
      .then(
        (p) => {
          if (!alive) return
          eps = p.Items
          writeSnap(snap, p.Items)
        },
        (e) => {
          if (alive && !eps.length) epsError = errText(e)
        },
      )
      .finally(() => {
        if (alive) epsLoading = false
      })
    return () => void (alive = false)
  })

  /** What the play button plays: a movie itself; for a series the next-up episode, else the first one of the chosen season. */
  const t = $derived(loaded && (loaded.item.Type === 'Series' ? loaded.next ?? eps[0] : loaded.item))
  const sources = $derived(t?.MediaSources ?? loaded?.item.MediaSources ?? [])
  /** A picked version only holds for the target it was picked on; a new target (another episode, next up changed) falls back to the server's first. */
  let picked = $state<{ target?: string; source: string }>()
  const src = $derived(sources.find((s) => picked && picked.target === t?.Id && s.Id === picked.source) ?? sources[0])
  const pickedSource = $derived(sources.length > 1 ? src?.Id : undefined)
  const bd = $derived(item ? img(item.Id, 'Backdrop', item.BackdropImageTags?.[0], BACKDROP_W) : '')

  function playLabel(t?: Item) {
    const pos = t?.UserData?.PlaybackPositionTicks ?? 0
    if (pos > 0) return `继续播放 ${clock(pos)}`
    if (t?.Type === 'Episode') return `播放 S${t.ParentIndexNumber ?? 1} E${t.IndexNumber ?? 1}`
    return '播放'
  }

  function links(item: Item) {
    const l = [...(item.ExternalUrls ?? [])]
    const douban = item.ProviderIds?.Douban
    if (douban) l.unshift({ Name: '豆瓣', Url: `https://movie.douban.com/subject/${douban}/` })
    return l
  }

  const toggle = (kind: 'played' | 'fav', item: Item) =>
    kind === 'played' ? setPlayed(item.Id, !item.UserData?.Played) : setFavorite(item.Id, !item.UserData?.IsFavorite)
</script>

{#if item}
  {#if bd}<div class="blur" style:background-image={`url("${img(item.Id, 'Backdrop', item.BackdropImageTags?.[0], BLUR_W)}")`}></div>{/if}

  <div class="detail">
    <section class="top">
      {#if bd}<img class="backdrop" src={bd} alt="" fetchpriority="high" use:fade /><div class="scrim"></div>{/if}
      <div class="top-inner">
        <div class="left">
          {#key logo}
            {#if logo && badLogo !== item}
              <img class="logo" src={logo} alt={item.Name} onerror={() => (badLogo = item)} />
            {:else}
              <h1>{item.Name}</h1>
            {/if}
          {/key}
          <div class="actions">
            <button class="round" title="从头播放" disabled={!t} onclick={() => t && play(t.Id, true, pickedSource)}><RotateCcw size={18} /></button>
            <button class="round" class:on={item.UserData?.Played} title={item.UserData?.Played ? '标记为未看' : '标记为已看'} onclick={() => toggle('played', item)}><Check size={18} /></button>
            <button class="round" class:on={item.UserData?.IsFavorite} title={item.UserData?.IsFavorite ? '取消收藏' : '收藏'} onclick={() => toggle('fav', item)}>
              <Heart size={18} fill={item.UserData?.IsFavorite ? 'currentColor' : 'none'} />
            </button>
          </div>
          <button class="play" disabled={!t} onclick={() => t && play(t.Id, false, pickedSource)}><Play size={16} fill="currentColor" /> {loaded ? playLabel(t) : '播放'}</button>
        </div>
        <div class="right">
          <div class="meta">
            {#if item.CommunityRating}<span class="star">★ {rating(item.CommunityRating)}</span>{/if}
            {#if item.OfficialRating}<span class="rated">{item.OfficialRating}</span>{/if}
            <span>{(item.Genres ?? []).join(' · ')}</span>
          </div>
          <div class="meta tech">
            {[minutes(t?.RunTimeTicks ?? item.RunTimeTicks), date(t?.PremiereDate ?? item.PremiereDate), sources.length > 1 ? '' : techLine(src)].filter(Boolean).join('   ')}
            {#if sources.length > 1 && src}
              <Dropdown value={src.Id} items={sources.map((s) => ({ value: s.Id, label: techLine(s) || s.Name || s.Id, hint: [s.MediaStreams?.find((m) => m.Type === 'Video')?.Codec, s.Container].filter(Boolean).join(' · ').toUpperCase() }))} onselect={(v) => (picked = { target: t?.Id, source: v })} />
            {/if}
          </div>
          {#if item.Overview}
            <!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_noninteractive_element_interactions -->
            <p class="ov selectable" class:clamp-3={!expanded} onclick={() => (expanded = !expanded)} title={expanded ? '' : '点击展开'}>{item.Overview}</p>
          {/if}
        </div>
      </div>
    </section>

    {#if loaded}
      {@const d = loaded}
      {#if d.item.Type === 'Series'}
        <Row count={eps.length || undefined} href={eps.length && seasonId ? `#/season/${d.item.Id}/${seasonId}` : undefined}>
          {#snippet head()}
            <SeasonPick seasons={d.seasons} value={seasonId} onselect={(v) => (pickedSeason = v)} fallback="剧集" />
          {/snippet}
          {#each eps as ep (ep.Id)}
            <Episode {ep} fallback={bd} selected={ep.Id === t?.Id} />
          {:else}
            {#if epsLoading}{#each { length: 5 } as _, k (k)}<div class="skeleton ep"></div>{/each}{:else if epsError}<LoadError error={epsError} what="单集加载失败" inline />{:else}<span class="muted">这一季还没有剧集</span>{/if}
          {/each}
        </Row>
      {/if}

      {#if d.item.People?.length}
        <Row title="演职人员">
          {#each d.item.People as p (p.Id + p.Type)}
            <a class="person" href={`#/person/${p.Id}`} title={p.Name}>
              <div class="face ph">
                {#if p.PrimaryImageTag}<img src={img(p.Id, 'Primary', p.PrimaryImageTag, 96)} alt="" loading="lazy" use:fade />{:else}<User size={32} />{/if}
              </div>
              <div class="pn clamp-1">{p.Name}</div>
              <div class="pr clamp-1">{p.Role || ROLE[p.Type] || p.Type}</div>
            </a>
          {/each}
        </Row>
      {/if}

      {#if d.item.BackdropImageTags?.length}
        <Row title="艺术图">
          {#each d.item.BackdropImageTags as tag, k (tag)}
            <button class="art ph" title="用浏览器打开原图" onclick={() => openUrl(imgOriginal(d.item.Id, 'Backdrop', tag, k))}>
              <img src={img(d.item.Id, 'Backdrop', tag, 288, k)} alt="" loading="lazy" use:fade />
            </button>
          {/each}
        </Row>
      {/if}

      {#if d.similar.length}
        <Row title="类似作品">
          {#each d.similar as it (it.Id)}<Poster item={it} />{/each}
        </Row>
      {/if}

      {@const ls = links(d.item)}
      {#if ls.length}
        <section class="section">
          <div class="section-head"><h2>链接</h2></div>
          <div class="chips">
            {#each ls as l (l.Url)}
              <button class="btn" onclick={() => openUrl(l.Url)}><Link size={14} /> {l.Name}</button>
            {/each}
          </div>
        </section>
      {/if}

      {#if src}
        <section class="section">
          <div class="section-head"><h2>媒体信息</h2></div>
          <MediaInfo source={src} />
        </section>
      {/if}
    {:else if q.error}
      <LoadError error={q.error} />
    {/if}
  </div>
{:else if q.error}
  <LoadError error={q.error} />
{:else}
  <div class="top skeleton-top"></div>
{/if}

<style>
  .detail { padding-bottom: 56px; animation: page-in 0.18s var(--ease); }
  /* A blurred backdrop tint behind the whole page, kept while scrolling; a small source saves decoding and GPU memory */
  .blur {
    position: fixed; inset: -60px; z-index: -1; background-size: cover; background-position: center;
    filter: blur(60px) saturate(1.4); opacity: 0.4;
  }
  .top { position: relative; min-height: max(520px, calc(100vh - 200px)); display: flex; align-items: flex-end; }
  .backdrop { position: absolute; inset: 0; width: 100%; height: 100%; object-fit: cover; }
  .scrim { position: absolute; inset: 0; background: var(--scrim); }
  .top-inner { position: relative; display: flex; gap: 40px; align-items: flex-end; padding: 0 var(--gutter) 4px; width: 100%; }
  .left { flex: none; width: 340px; display: flex; flex-direction: column; gap: 14px; }
  .logo { max-width: 320px; max-height: 140px; object-fit: contain; object-position: left bottom; }
  h1 { font-size: 38px; letter-spacing: -0.02em; line-height: 1.15; }
  .actions { display: flex; gap: 12px; }
  .round {
    width: 46px; height: 46px; border-radius: 50%; display: grid; place-items: center;
    background: color-mix(in srgb, var(--bg-raised) 82%, transparent); box-shadow: inset 0 0 0 1px var(--stroke);
    transition: background 0.12s, transform 0.12s var(--ease), color 0.12s;
  }
  .round:hover { background: var(--bg-raised); }
  .round:active { transform: scale(0.94); }
  .round:disabled { opacity: 0.45; }
  .round.on { color: var(--accent); }
  .play {
    height: 44px; border-radius: 22px; background: var(--text); color: var(--bg);
    display: flex; align-items: center; justify-content: center; gap: 8px; font-size: 15px; font-weight: 600;
    transition: transform 0.12s var(--ease), opacity 0.12s;
  }
  .play:hover { opacity: 0.88; }
  .play:active { transform: scale(0.98); }
  .play:disabled { opacity: 0.4; }
  .right { flex: 1; min-width: 0; padding-bottom: 2px; }
  .meta { font-size: 13px; color: var(--text-2); margin-bottom: 6px; display: flex; gap: 10px; align-items: center; flex-wrap: wrap; }
  .meta.tech { white-space: pre; font-variant-numeric: tabular-nums; }
  .star { color: var(--star); font-weight: 600; }
  .rated { border: 1px solid currentColor; border-radius: 4px; padding: 0 5px; font-size: 11px; line-height: 16px; }
  .ov { margin: 8px 0 0; font-size: 14px; line-height: 1.7; max-width: 1100px; }
  .ov.clamp-3 { cursor: default; }
  .skeleton.ep { width: 288px; aspect-ratio: 16 / 9; }
  .person { display: block; width: 96px; font-size: 12px; line-height: 1.45; }
  .person:hover .pn { color: var(--accent); }
  .face { width: 96px; height: 96px; border-radius: 50%; overflow: hidden; display: grid; place-items: center; color: var(--text-3); margin-bottom: 8px; }
  .face img { width: 100%; height: 100%; object-fit: cover; }
  .pn { font-weight: 500; text-align: center; }
  .pr { color: var(--text-2); text-align: center; }
  .art { width: 288px; aspect-ratio: 16 / 9; border-radius: var(--radius); overflow: hidden; transition: transform 0.12s var(--ease); }
  .art:active { transform: scale(0.98); }
  .art img { width: 100%; height: 100%; object-fit: cover; }
  .chips { display: flex; gap: 8px; flex-wrap: wrap; padding: 0 var(--gutter); }
  .skeleton-top { min-height: calc(100vh - 200px); background: var(--ph); opacity: 0.4; }
</style>
