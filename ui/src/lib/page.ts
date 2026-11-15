// Paging rules for `pager` (api.svelte.ts), kept free of Svelte so they can be tested in Node. Some servers answer
// resumable queries with TotalRecordCount = the number returned and ignore StartIndex; these rules cope with both.
import type { Item, Page } from './types'

/** A non-empty page made only of items already shown: the server ignored StartIndex. */
export const ignoredStart = (shown: Item[], p: Page) => {
  const ids = new Set(shown.map((i) => i.Id))
  return p.Items.length > 0 && p.Items.every((i) => ids.has(i.Id))
}

/**
 * Merges a page into what is shown. `skip`: leading items of the response that are not part of this page (a server
 * ignoring StartIndex is asked from 0); the total is judged against the whole response. `fresh` drops items already
 * shown (a list sorted by last played can shift between pages, and duplicate keys break `#each`). The list ends on a
 * short page, or once a total that is plainly real (more than the response holds) is reached; `total` is what to
 * show, exact once done.
 */
export function merge(shown: Item[], p: Page, size: number, skip = 0) {
  const ids = new Set(shown.map((i) => i.Id))
  const got = p.Items.slice(skip)
  const fresh = got.filter((i) => !ids.has(i.Id) && ids.add(i.Id))
  const n = shown.length + fresh.length
  const real = p.TotalRecordCount > p.Items.length
  const done = got.length < size || (real && n >= p.TotalRecordCount)
  return { fresh, done, total: done ? n : Math.max(real ? p.TotalRecordCount : 0, n) }
}
