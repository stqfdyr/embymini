# EmbyMini

Windows 桌面 Emby 客户端：桌面软件风格的浏览界面，播放交给 mpv。

- 多服务器：地址、端口分开填，随时切换；支持反向代理挂在子路径下的服务器
- 浏览：首页轮播、继续观看、各媒体库最新；媒体库分页、搜索、收藏；剧集详情（季、集、演职人员、类似作品、媒体信息）
- 播放：mpv + ModernZ 界面；续播、整季连播、按进度预加载下一集、选集菜单（F8）、换集沿用音轨与字幕、外挂字幕
- 进度用 Emby 标准的 Sessions 接口回传，其他客户端同步可见；token 不出现在 mpv 的播放列表与统计页里

## 目录

```text
src-tauri/            Rust 后端（Tauri 2）
  src/main.rs         命令注册、应用状态、退出时收尾
  src/config.rs       本地配置：服务器列表、播放设置、地址规范化
  src/emby.rs         Emby HTTP：鉴权头、UA、通用请求、播放用的强类型
  src/player.rs       mpv 外部进程 + JSON IPC：队列、预加载、选轨、进度回传
  mpv/                自带 mpv 配置：mpv.conf、input.conf、ModernZ、选集菜单脚本（安装包里另附 mpv.exe）
  testdata/           单测用的回包样本（只保留测试用到的字段）
ui/                   前端（Svelte 5 + Vite）；ui/dev/bridge.ts 是浏览器里开发界面用的转发桥
tools/mpv-shot.lua    不连服务器检查自带 mpv 配置的渲染效果（CI 用它截图）
```

## 开发

| 做什么 | 命令 |
|---|---|
| 后端单测 | `cargo test --manifest-path src-tauri/Cargo.toml` |
| 前端类型检查 | `npm --prefix ui run check`（看退出码：svelte-check 与 tsc 两段） |
| 浏览器里开发界面 | 导出 `EMBY_SERVER` / `PORT` / `EMBY_USER` / `EMBY_PASS` 后 `cd ui && npx vite`，打开 http://127.0.0.1:5173 |
| 桌面版开发（Windows） | `ui\node_modules\.bin\tauri dev` |
| 真服端到端（需要上面四个环境变量和 PATH 里的 mpv） | `cargo test --manifest-path src-tauri/Cargo.toml e2e_real_server -- --ignored --nocapture` |
| 出安装包 | 手动触发 `windows` workflow，或推送 `v*` 标签；产物 `EmbyMini-setup` |

端到端测试会真实播放并回传进度，结束时把观看记录恢复成测试前的样子。

## 数据位置与卸载

| 位置 | 内容 |
|---|---|
| `%LOCALAPPDATA%\EmbyMini` | 程序与自带的 mpv |
| `%APPDATA%\com.stqfdyr.embymini\config.json` | 服务器列表（含登录 token，不存密码）、播放设置 |
| `%APPDATA%\com.stqfdyr.embymini\mpv.conf` | 个人 mpv 配置：自己新建，在自带配置之后加载，同名选项以它为准，升级不覆盖 |
| `%LOCALAPPDATA%\com.stqfdyr.embymini` | 日志（`logs\`）、界面缓存、mpv 着色器缓存 |

卸载走「设置 → 应用 → 已安装的应用」。卸载时勾选「删除应用程序数据」，上表后三项一并删除。

## 第三方

- [mpv](https://mpv.io)（GPL）：安装包附带 shinchiro 构建
- [ModernZ](https://github.com/Samillion/ModernZ) v0.3.3（LGPL-2.1，见 `src-tauri/mpv/MODERNZ-LICENSE`）
