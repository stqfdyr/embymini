# EmbyMini

Windows 桌面 Emby 客户端：桌面软件风格的浏览界面，播放交给 mpv。

- 多服务器：地址、端口分开填，随时切换；可编辑（换地址、端口不用重新登录）或重新登录；支持反向代理挂在子路径下的服务器
- 浏览：首页轮播、继续观看（可移除、可撤销）、各媒体库最新；媒体库分页、搜索、收藏；剧集详情（季、集、演职人员、类似作品、媒体信息）、整季单集页、演职人员作品页
- 播放：mpv + ModernZ 界面（顶栏居中标题 + 网速）；续播、整季连播、按进度预加载下一集、选集菜单（F8）、换集沿用音轨与字幕、外挂字幕
- 进度用 Emby 标准的 Sessions 接口回传，其他客户端同步可见；token 不出现在 mpv 的播放列表与统计页里

## 安装

在 [Releases](https://github.com/stqfdyr/embymini/releases) 下载最新的 `EmbyMini_<版本>_x64-setup.exe` 运行即可，覆盖安装会保留服务器配置。
安装包没有签名，Windows 可能提示「已保护你的电脑」，点「更多信息」→「仍要运行」。可用同一 Release 里的 `SHA256SUMS.txt` 校验文件。

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
| 测试包 | 手动触发 `windows` workflow；安装包在该次运行的 Artifacts 里（`EmbyMini-setup`） |
| 发版 | 把 `src-tauri/tauri.conf.json` 与 `src-tauri/Cargo.toml` 的 `version` 改成新版本并推送，再 `git tag -a vX.Y.Z -m vX.Y.Z && git push origin vX.Y.Z`；构建、冒烟测试通过后自动发 Release（安装包 + `SHA256SUMS.txt`），版本号带 `-` 的标为预发布 |

端到端测试会真实播放并回传进度，结束时把观看记录恢复成测试前的样子。

## 数据位置与卸载

所有数据都在安装目录下的 `data\`，系统盘不留东西（默认安装目录是 `%LOCALAPPDATA%\EmbyMini`，安装时可以改到别的盘）：

| 位置（相对安装目录） | 内容 |
|---|---|
| `data\config.json` | 服务器列表（含登录 token，不存密码）、播放设置 |
| `data\mpv.conf` | 个人 mpv 配置：自己新建，在自带配置之后加载，同名选项以它为准，升级不覆盖 |
| `data\logs\` | 日志 |
| `data\webview\` | 界面数据（含海报磁盘缓存） |
| `data\mpv-cache\` | mpv 着色器缓存 |

安装目录不可写时（例如手动拷到只读位置）退回 `%LOCALAPPDATA%\com.stqfdyr.embymini`。旧版本放在 `%APPDATA%` / `%LOCALAPPDATA%` 下的配置，新版首次启动时自动搬过来并删掉旧目录。

卸载走「设置 → 应用 → 已安装的应用」。勾选「删除应用程序数据」时 `data\` 一并删除；自动更新不动它。

## 第三方

- [mpv](https://mpv.io)（GPL）：安装包附带 shinchiro 构建
- [ModernZ](https://github.com/Samillion/ModernZ) v0.3.3（LGPL-2.1，见 `src-tauri/mpv/MODERNZ-LICENSE`）。`modernz.lua` 有少量改动（顶栏标题居中、加网速；下一集已就绪时下一集按钮变绿带闪电），改动处标 `EmbyMini:`
