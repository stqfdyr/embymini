<!-- 人物：头像、姓名、出生日期 / 地点、简介（点击展开）+ 作品墙（电影 + 剧集，年份倒序，滚到底加载下一页）。 -->
<script lang="ts">
  import { User } from '@lucide/svelte'
  import Grid from '../lib/Grid.svelte'
  import { LIST_FIELDS, errText, get, img, loader, pager, session, uid } from '../lib/api.svelte'
  import { fade } from '../lib/fade'
  import { date } from '../lib/fmt'
  import { restoreScrollWhen } from '../lib/router.svelte'
  import type { Item } from '../lib/types'

  let { id }: { id: string } = $props()

  const p = loader(() => {
    void session.refresh
    return get<Item>(`/Users/${uid()}/Items/${id}`)
  })
  const { s: pg, more } = pager(() => ({
    PersonIds: id, Recursive: true, IncludeItemTypes: 'Movie,Series', Fields: LIST_FIELDS,
    SortBy: 'ProductionYear,PremiereDate,SortName', SortOrder: 'Descending',
  }))
  restoreScrollWhen(() => pg.loaded)
  let expanded = $state(false)

  const born = (it: Item) => [date(it.PremiereDate), it.ProductionLocations?.[0]].filter(Boolean).join(' · ')
</script>

<div class="page">
  {#if p.data}
    {@const it = p.data}
    <header class="who">
      <div class="face ph">
        {#if it.ImageTags?.Primary}<img src={img(it.Id, 'Primary', it.ImageTags.Primary, 140)} alt="" use:fade />{:else}<User size={48} />{/if}
      </div>
      <div class="text">
        <h1>{it.Name}</h1>
        {#if born(it)}<div class="muted">{born(it)}</div>{/if}
        {#if it.Overview}
          <!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_noninteractive_element_interactions -->
          <p class="ov selectable" class:clamp-3={!expanded} onclick={() => (expanded = !expanded)} title={expanded ? '' : '点击展开'}>{it.Overview}</p>
        {/if}
      </div>
    </header>
  {:else if p.error}
    <div class="center-msg">加载失败：{errText(p.error)}</div>
  {/if}

  <div class="page-title"><h2>作品</h2>{#if pg.total}<span class="muted">{pg.total} 部</span>{/if}</div>
  {#if pg.error}<div class="center-msg">加载失败：{pg.error}</div>{/if}
  <Grid items={pg.items} onmore={more} loading={pg.busy} />
  {#if pg.loaded && !pg.items.length}<div class="center-msg">这个服务器上没有相关的电影或剧集</div>{/if}
</div>

<style>
  .who { display: flex; gap: 28px; align-items: flex-start; padding: 8px var(--gutter) 28px; }
  .face { flex: none; width: 140px; height: 140px; border-radius: 50%; overflow: hidden; display: grid; place-items: center; color: var(--text-3); }
  .face img { width: 100%; height: 100%; object-fit: cover; }
  .text { min-width: 0; display: flex; flex-direction: column; gap: 6px; padding-top: 12px; }
  h1 { font-size: 30px; letter-spacing: -0.02em; }
  .ov { margin: 6px 0 0; font-size: 14px; line-height: 1.7; max-width: 1000px; }
</style>
