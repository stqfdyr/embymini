/** 图片加载完再淡入（而不是逐行蹦出来），并让解码不卡主线程。用法：<img use:fade …> */
export function fade(img: HTMLImageElement) {
  img.decoding = 'async'
  img.classList.add('fade')
  const on = () => img.classList.add('in')
  if (img.complete && img.naturalWidth) on()
  else img.addEventListener('load', on, { once: true })
}
