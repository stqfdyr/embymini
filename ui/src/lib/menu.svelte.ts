// 全局右键菜单的状态：任何地方 `openMenu(e, items)`，App 里的 <ContextMenu /> 负责显示。
import type { Component } from 'svelte'

export type MenuItem = { label: string; icon?: Component; danger?: boolean; action: () => unknown } | 'sep'

export const menu = $state<{ open: boolean; x: number; y: number; items: MenuItem[] }>({ open: false, x: 0, y: 0, items: [] })

export function openMenu(e: MouseEvent, items: MenuItem[]) {
  e.preventDefault()
  e.stopPropagation()
  Object.assign(menu, { open: true, x: e.clientX, y: e.clientY, items })
}

export const closeMenu = () => (menu.open = false)
