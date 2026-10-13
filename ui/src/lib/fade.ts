/**
 * 图片加载完再淡入（而不是逐行蹦出来），并让解码不卡主线程。用法：<img use:fade …>
 * 只有从网络来的图才淡入（同 Coil 等图片库的做法）：已经在缓存里、很快就加载完的直接显示，不然看着像在加载。
 */
const CACHED_MS = 50

export function fade(img: HTMLImageElement) {
  img.decoding = 'async'
  if (img.complete && img.naturalWidth) return
  const t0 = performance.now()
  img.classList.add('fade')
  img.addEventListener('load', () => img.classList.add(performance.now() - t0 < CACHED_MS ? 'instant' : 'in'), { once: true })
}
