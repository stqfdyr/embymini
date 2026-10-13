import { svelte } from '@sveltejs/vite-plugin-svelte'
import { defineConfig } from 'vite'
import { devBridge } from './dev/bridge.ts'

export default defineConfig({
  plugins: [svelte(), devBridge()],
  clearScreen: false,
  server: { port: 5173, strictPort: true },
  // WebView2 是常青 Chromium
  build: { target: 'chrome120' },
})
