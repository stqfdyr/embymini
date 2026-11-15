// Hash routes: #/, #/item/<id>, #/library/<id>, #/person/<id>, #/season/<series id>/<season id>, #/views, #/resume, #/search, #/favorites, #/settings.
// The scroll position is saved on leaving a page and restored on coming back.

export const route = $state({ name: 'home', id: '', sub: '', hash: '#/' })
const scroll = new Map<string, number>()

function parse() {
  const hash = location.hash || '#/'
  const [, name = 'home', id = '', sub = ''] = hash.slice(1).split('/')
  scroll.set(route.hash, window.scrollY)
  Object.assign(route, { name: name || 'home', id: decodeURIComponent(id), sub: decodeURIComponent(sub), hash })
  // A new page starts at the top at once (the detail page draws its header early); a revisited one restores once its data is in
  if (!scroll.has(hash)) window.scrollTo(0, 0)
}

parse()
window.addEventListener('hashchange', parse)

export const go = (path: string) => (location.hash = path)
export const back = () => (history.length > 1 ? history.back() : go('/'))

/** Back to where this page was left (top for a new page); two frames so the content has laid out. */
function restoreScroll() {
  const y = scroll.get(route.hash) ?? 0
  requestAnimationFrame(() => requestAnimationFrame(() => window.scrollTo(0, y)))
}

/** Restore the scroll position the first time `ready()` is truthy; later refreshes leave it alone. Call during component init. */
export function restoreScrollWhen(ready: () => unknown) {
  let done = false
  $effect(() => {
    if (!done && ready()) {
      done = true
      restoreScroll()
    }
  })
}
