// 只在开发时的浏览器里加载：把 invoke 转给 vite 开发服务器上的 /__dev/<命令>（dev/bridge.ts）。
import { mockIPC, mockWindows } from '@tauri-apps/api/mocks'

export function installDevBridge() {
  mockWindows('main') // 顶栏的窗口按钮要用 getCurrentWindow()
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
