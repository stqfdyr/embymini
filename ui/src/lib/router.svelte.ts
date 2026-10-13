// hash 路由：#/、#/item/<id>、#/library/<id>、#/search、#/favorites、#/settings。
// 离开页面时记下滚动位置，返回时恢复。

export const route = $state({ name: 'home', id: '', hash: '#/' })
const scroll = new Map<string, number>()

function parse() {
  const hash = location.hash || '#/'
  const [, name = 'home', id = ''] = hash.slice(1).split('/')
  scroll.set(route.hash, window.scrollY)
  Object.assign(route, { name: name || 'home', id: decodeURIComponent(id), hash })
  // 新页面立刻回到顶部（详情页会先画出顶部，不能停在上一页的位置）；返回的页面等数据到了再恢复
  if (!scroll.has(hash)) window.scrollTo(0, 0)
}

parse()
window.addEventListener('hashchange', parse)

export const go = (path: string) => (location.hash = path)
export const back = () => (history.length > 1 ? history.back() : go('/'))

/** 页面数据就绪后调用：回到上次离开时的位置（新页面回到顶部）。 */
export function restoreScroll() {
  const y = scroll.get(route.hash) ?? 0
  requestAnimationFrame(() => requestAnimationFrame(() => window.scrollTo(0, y)))
}

/** `ready()` 第一次为真时恢复滚动位置，之后的刷新不再动。只能在组件初始化时调用。 */
export function restoreScrollWhen(ready: () => unknown) {
  let done = false
  $effect(() => {
    if (!done && ready()) {
      done = true
      restoreScroll()
    }
  })
}
