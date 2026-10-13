import { mount } from 'svelte'
import './app.css'
import App from './App.svelte'

// 浏览器里 `vite dev`（不在 Tauri 里）：invoke 走开发桥转发到真服，见 dev/bridge.ts
if (import.meta.env.DEV && !('__TAURI_INTERNALS__' in window)) {
  const { installDevBridge } = await import('./devmock')
  installDevBridge()
}

mount(App, { target: document.getElementById('app')! })
