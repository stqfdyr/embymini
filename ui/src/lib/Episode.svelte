<!-- 单集卡：详情页的单集行和整季页共用。点图播放，点简介看全文，右键菜单。 -->
<script lang="ts">
  import { img, itemMenu, play } from './api.svelte'
  import { showText } from './Dialog.svelte'
  import { clock, rating } from './fmt'
  import { openMenu } from './menu.svelte'
  import type { Item } from './types'
  import Wide from './Wide.svelte'

  let { ep, fallback = '', selected = false, width = 288 }: { ep: Item; fallback?: string; selected?: boolean; width?: number } = $props()
  const title = $derived(`${ep.IndexNumber ?? ''}. ${ep.Name}`)
  const pos = $derived(ep.UserData?.PlaybackPositionTicks ?? 0)
</script>

<Wide
  src={img(ep.Id, 'Primary', ep.ImageTags?.Primary, width) || fallback}
  {title}
  desc={ep.Overview ?? ''}
  ondesc={() => showText(title, ep.Overview ?? '')}
  score={rating(ep.CommunityRating)}
  time={pos ? `${clock(pos)} / ${clock(ep.RunTimeTicks)}` : ''}
  played={ep.UserData?.Played}
  progress={ep.UserData?.PlayedPercentage ?? 0}
  {selected}
  onplay={() => play(ep.Id)}
  onmenu={(e) => openMenu(e, itemMenu(ep))}
  {width}
/>
