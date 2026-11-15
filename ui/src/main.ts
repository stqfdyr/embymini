import { mount } from 'svelte'
import './app.css'
import App from './App.svelte'

// `vite dev` in a plain browser (not Tauri): invoke goes through the dev bridge to a real server, see dev/bridge.ts
if (import.meta.env.DEV && !('__TAURI_INTERNALS__' in window)) {
  const { installDevBridge } = await import('./devmock')
  installDevBridge()
}

mount(App, { target: document.getElementById('app')! })
