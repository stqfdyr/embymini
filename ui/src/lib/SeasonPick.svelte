<!-- Season picker shared by the detail page and the season page: a dropdown, or just the name when there is only one season. -->
<script lang="ts">
  import Dropdown from './Dropdown.svelte'
  import type { Item } from './types'

  let { seasons, value, onselect, fallback = '' }: { seasons: Item[]; value: string | undefined; onselect: (id: string) => void; fallback?: string } = $props()
</script>

{#if seasons.length > 1}
  <Dropdown big {value} items={seasons.map((s) => ({ value: s.Id, label: s.Name, hint: s.UserData?.UnplayedItemCount ? `${s.UserData.UnplayedItemCount} 集未看` : '' }))} {onselect} />
{:else}
  <h2>{seasons[0]?.Name ?? fallback}</h2>
{/if}
