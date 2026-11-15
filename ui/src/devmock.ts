// Loaded only in a dev browser: forwards invoke to /__dev/<command> on the vite dev server (dev/bridge.ts).
import { mockIPC, mockWindows } from '@tauri-apps/api/mocks'

export function installDevBridge() {
  mockWindows('main') // the title bar's window buttons call getCurrentWindow()
  mockIPC(
    async (cmd, args) => {
      if (cmd.startsWith('plugin:window|')) return cmd.endsWith('is_maximized') ? false : null
      const r = await fetch(`/__dev/${encodeURIComponent(cmd)}`, { method: 'POST', body: JSON.stringify(args ?? {}) })
      const v = await r.json()
      if (!r.ok) throw v
      return v
    },
    { shouldMockEvents: true },
  )
}
