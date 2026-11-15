/** Loads faster than this count as cached and show at once; fading them in would look like loading. */
const CACHED_MS = 50

/** `<img use:fade>`: decode off the main thread and fade in once loaded, only for images that came from the network. */
export function fade(img: HTMLImageElement) {
  img.decoding = 'async'
  if (img.complete && img.naturalWidth) return
  const t0 = performance.now()
  img.classList.add('fade')
  img.addEventListener('load', () => img.classList.add(performance.now() - t0 < CACHED_MS ? 'instant' : 'in'), { once: true })
}
