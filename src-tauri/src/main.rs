// release 下不弹控制台窗口，不要删。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
mod emby;
mod images;
mod player;
mod telemetry;

use config::{Config, Server, ServerView};
use emby::{Emby, Error};
use reqwest::Method;
use serde::Serialize;
use serde_json::{Map, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_log::{RotationStrategy, Target, TargetKind, TimezoneStrategy};
use tauri_plugin_updater::UpdaterExt;
use tokio::sync::{mpsc, watch};

struct App {
    path: PathBuf,
    cfg: Mutex<Config>,
    http: reqwest::Client,
    /// 资源目录里的 mpv 自带配置（ModernZ 等）；打包时若附带了 mpv.exe 也在这里。
    mpv_dir: Option<PathBuf>,
    /// mpv 的缓存目录：数据目录下的 `mpv-cache`，卸载时勾选「删除应用程序数据」会一起删掉。
    mpv_cache: Option<PathBuf>,
    player: Mutex<Option<Player>>,
    /// 海报、背景图的磁盘缓存（`emby-img` 协议）。
    images: std::sync::Arc<images::Images>,
    /// 每次点播放加一；起播要等几秒，完成时若已不是最新一次点击，就作废自己。
    play_seq: AtomicU64,
    /// 程序要退出了：起播中的 `play` 收到后放弃起播（见 [`App::unless_quit`]）。
    quit: watch::Sender<bool>,
    /// 还没返回的 `play` 个数（[`Starting`]），退出时等它们收尾。
    starting: AtomicUsize,
    /// 已叫停、还在收尾（回传 Stopped）的播放任务，退出时等它们（见 [`App::begin_shutdown`]）。
    closing: Mutex<Vec<tauri::async_runtime::JoinHandle<()>>>,
}

// ---------- 关闭耗时埋点 ----------
// 从点关闭（或开始退出）算起，各阶段的毫秒数写日志；退出前存到 data\last-exit.json，
// 下次启动作为诊断数据 `exit` 上报，用来查「关闭时卡一下」卡在哪一段。

static CLOSE_T0: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
static CLOSE_MARKS: Mutex<Vec<(&'static str, u64)>> = Mutex::new(Vec::new());

/// 开始计时（重复调用以第一次为准）。
fn exit_start() {
    CLOSE_T0.get_or_init(std::time::Instant::now);
}

/// 记一个关闭阶段；还没开始关闭时（例如平时关掉 mpv）什么都不做。
pub fn exit_mark(phase: &'static str) {
    let Some(t0) = CLOSE_T0.get() else { return };
    let ms = t0.elapsed().as_millis() as u64;
    log::info!("关闭耗时 {phase}：{ms} ms");
    lock(&CLOSE_MARKS).push((phase, ms));
}

fn exit_file(root: &std::path::Path) -> PathBuf {
    root.join("last-exit.json")
}

/// 退出前把各阶段耗时存盘（进程马上结束，诊断数据的写入任务来不及落盘）。
fn exit_save(root: &std::path::Path) {
    let marks: Map<String, Value> = lock(&CLOSE_MARKS)
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).into()))
        .collect();
    if !marks.is_empty() {
        let _ = std::fs::write(exit_file(root), Value::Object(marks).to_string());
    }
}

/// 启动时把上次退出的耗时补记成诊断数据（没开诊断就只删文件）。
fn exit_report(root: &std::path::Path) {
    let f = exit_file(root);
    if let Some(v) = std::fs::read(&f)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
    {
        telemetry::rec("exit", v);
    }
    let _ = std::fs::remove_file(f);
}

/// 进入 `play` 时加一、离开时减一。
struct Starting<'a>(&'a AtomicUsize);

impl<'a> Starting<'a> {
    fn enter(n: &'a AtomicUsize) -> Self {
        n.fetch_add(1, Ordering::SeqCst);
        Self(n)
    }
}

impl Drop for Starting<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// 正在进行的一次播放：发控制命令的通道 + 转发播放事件的任务（会话结束后才结束）。
struct Player {
    ctl: mpsc::UnboundedSender<player::Ctl>,
    task: tauri::async_runtime::JoinHandle<()>,
}

/// 设置里填的 mpv 路径：去掉首尾空白和引号（资源管理器「复制为路径」会带双引号）。
fn clean_path(p: &str) -> &str {
    p.trim().trim_matches('"').trim()
}

/// 锁中毒（持锁时 panic）也照常取用：这里的数据都是整体替换，不会停在半改状态。
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl App {
    fn cfg(&self) -> MutexGuard<'_, Config> {
        lock(&self.cfg)
    }

    /// 让正在播放的 mpv 退出（没有就什么都不做）；返回播放任务，需要时可等它回传完 Stopped。
    fn stop_player(&self) -> Option<tauri::async_runtime::JoinHandle<()>> {
        let p = lock(&self.player).take()?;
        let _ = p.ctl.send(player::Ctl::Quit);
        Some(p.task)
    }

    /// 起播要等几秒（取 PlaybackInfo、连 IPC），这期间 mpv 窗口已经开着。程序退出时丢掉起播的 future，
    /// mpv 随 `Child` 一起被杀（`kill_on_drop`），不留一个没人管的空窗口。
    async fn unless_quit<F: Future>(&self, f: F) -> Option<F::Output> {
        let mut quit = self.quit.subscribe();
        tokio::select! {
            biased; // 已经在退出时，即使 f 也就绪了也放弃
            _ = quit.wait_for(|q| *q) => None,
            r = f => Some(r),
        }
    }

    /// 退出的第一步，点关闭时就做（不等窗口销毁、`RunEvent::Exit`）：叫停起播中的 `play`，
    /// 让正在播的 mpv 立刻退出；它的回传任务留给 [`App::shutdown`] 去等。可重复调用。
    fn begin_shutdown(&self) {
        self.quit.send_replace(true);
        if let Some(task) = self.stop_player() {
            log::info!("程序退出，先关闭播放器并回传进度");
            lock(&self.closing).push(task);
        }
    }

    /// 程序退出前调用：mpv 是独立进程，不会跟着 EmbyMini 退出，而回传进度的任务会随进程消失，
    /// 续播点就丢了。所以先叫停起播中的 `play` 并等它们返回（否则可能刚起播完又登记一个播放器），
    /// 再让正在播的 mpv 退出、等回传 Stopped。总共最多等 `wait`。
    fn shutdown(&self, wait: Duration) {
        exit_start();
        self.begin_shutdown();
        tauri::async_runtime::block_on(async {
            let deadline = tokio::time::Instant::now() + wait;
            while self.starting.load(Ordering::SeqCst) > 0 && tokio::time::Instant::now() < deadline
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            self.begin_shutdown(); // 等的这段时间里刚起播完登记的播放器
            let tasks = std::mem::take(&mut *lock(&self.closing));
            for task in tasks {
                if tokio::time::timeout_at(deadline, task).await.is_err() {
                    log::warn!("关闭播放器超时（{wait:?}），进度可能没回传");
                }
            }
        });
        exit_mark("shutdown_done");
    }

    fn update<T>(&self, f: impl FnOnce(&mut Config) -> T) -> Result<T, Error> {
        let mut c = self.cfg();
        let out = f(&mut c);
        config::save(&self.path, &c).map_err(|e| Error::Other(format!("保存配置失败：{e}")))?;
        Ok(out)
    }

    /// 当前服务器的 id 和客户端。
    fn active(&self) -> Result<(String, Emby), Error> {
        let c = self.cfg();
        let s = c
            .active
            .as_ref()
            .and_then(|id| c.servers.iter().find(|s| &s.id == id))
            .ok_or_else(|| Error::Other("还没有添加服务器".into()))?;
        if s.token.is_empty() {
            return Err(Error::Unauthorized);
        }
        Ok((s.id.clone(), Emby::new(self.http.clone(), s, &c.device_id)))
    }

    /// 个人 mpv 配置文件：和 config.json 同在数据目录，升级不覆盖。由用户自己创建，存在才加载。
    fn user_mpv_conf(&self) -> PathBuf {
        self.path.with_file_name("mpv.conf")
    }

    fn mpv_path(&self) -> PathBuf {
        if let Some(p) = self
            .cfg()
            .mpv_path
            .as_deref()
            .map(clean_path)
            .filter(|p| !p.is_empty())
        {
            return PathBuf::from(p);
        }
        let exe = if cfg!(windows) { "mpv.exe" } else { "mpv" };
        self.mpv_dir
            .as_ref()
            .map(|d| d.join(exe))
            .filter(|p| p.is_file())
            .unwrap_or_else(|| PathBuf::from(exe))
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ConfigView {
    active: Option<String>,
    servers: Vec<ServerView>,
    mpv_path: Option<String>,
    preload_percent: u8,
    /// 个人 mpv 配置文件的路径，设置页里显示给用户。
    user_mpv_conf: String,
    update_proxy: Option<String>,
    version: &'static str,
    /// 打包时注入了上报地址才有诊断数据开关。
    telemetry_available: bool,
    telemetry: bool,
    telemetry_name: String,
}

#[tauri::command]
fn config_get(app: State<'_, App>) -> ConfigView {
    let c = app.cfg();
    ConfigView {
        active: c.active.clone(),
        servers: c.servers.iter().map(Server::view).collect(),
        mpv_path: c.mpv_path.clone(),
        preload_percent: c.preload_percent,
        user_mpv_conf: app.user_mpv_conf().display().to_string(),
        update_proxy: c.update_proxy.clone(),
        version: emby::VERSION,
        telemetry_available: telemetry::AVAILABLE,
        telemetry: c.telemetry,
        telemetry_name: tel_name(&c),
    }
}

#[tauri::command]
async fn server_add(
    app: State<'_, App>,
    url: String,
    port: Option<u16>,
    user: String,
    password: String,
) -> Result<ServerView, Error> {
    let (url, port) = config::normalize(&url, port).map_err(Error::Other)?;
    let mut s = Server {
        id: uuid::Uuid::new_v4().to_string(),
        name: String::new(),
        url,
        port,
        user,
        user_id: String::new(),
        token: String::new(),
    };
    let base = s.api_base();
    let device_id = app.cfg().device_id.clone();
    s.name = emby::server_name(&app.http, &base).await?;
    (s.user_id, s.token) = emby::login(&app.http, &base, &device_id, &s.user, &password).await?;
    let view = s.view();
    app.update(|c| {
        c.active = Some(s.id.clone());
        c.servers.push(s);
    })?;
    record_servers(&app.cfg());
    Ok(view)
}

/// 编辑服务器（id 不变，是当前服务器的话仍是当前）：改地址、端口、用户名，或者重新登录。
/// 先用新地址连一次服务器（顺带更新服务器名），连不上就不保存。
/// 只改地址时（换域名、换端口、换反代）沿用原来的 token，不用输密码；
/// 改了用户名、填了密码或者原来就没登录时，用这里的用户名和密码重新登录。
#[tauri::command]
async fn server_edit(
    app: State<'_, App>,
    id: String,
    url: String,
    port: Option<u16>,
    user: String,
    password: String,
) -> Result<ServerView, Error> {
    let (url, port) = config::normalize(&url, port).map_err(Error::Other)?;
    let (mut s, device_id) = {
        let c = app.cfg();
        let s = c.servers.iter().find(|s| s.id == id).cloned();
        (
            s.ok_or_else(|| Error::Other("服务器不存在".into()))?,
            c.device_id.clone(),
        )
    };
    let relogin = !password.is_empty() || user != s.user || s.token.is_empty();
    (s.url, s.port, s.user) = (url, port, user);
    let base = s.api_base();
    s.name = emby::server_name(&app.http, &base).await?;
    if relogin {
        (s.user_id, s.token) =
            emby::login(&app.http, &base, &device_id, &s.user, &password).await?;
    }
    let view = s.view();
    app.update(|c| {
        let t = c.servers.iter_mut().find(|t| t.id == id)?;
        *t = s;
        Some(())
    })?
    .ok_or_else(|| Error::Other("服务器不存在".into()))?;
    record_servers(&app.cfg());
    Ok(view)
}

/// 重新登录（只要密码）：token 失效或者想换一个新 token 时用。
#[tauri::command]
async fn server_login(
    app: State<'_, App>,
    id: String,
    password: String,
) -> Result<ServerView, Error> {
    let (base, user, device_id) = {
        let c = app.cfg();
        let s = c
            .servers
            .iter()
            .find(|s| s.id == id)
            .ok_or_else(|| Error::Other("服务器不存在".into()))?;
        (s.api_base(), s.user.clone(), c.device_id.clone())
    };
    let (user_id, token) = emby::login(&app.http, &base, &device_id, &user, &password).await?;
    app.update(|c| {
        let s = c.servers.iter_mut().find(|s| s.id == id)?;
        s.user_id = user_id;
        s.token = token;
        Some(s.view())
    })?
    .ok_or_else(|| Error::Other("服务器不存在".into()))
}

#[tauri::command]
fn server_remove(app: State<'_, App>, id: String) -> Result<(), Error> {
    app.update(|c| {
        c.servers.retain(|s| s.id != id);
        if c.active.as_deref() == Some(id.as_str()) {
            c.active = c.servers.first().map(|s| s.id.clone());
        }
    })
}

#[tauri::command]
fn server_use(app: State<'_, App>, id: String) -> Result<(), Error> {
    app.update(|c| {
        if c.servers.iter().any(|s| s.id == id) {
            c.active = Some(id);
        }
    })
}

#[tauri::command]
fn settings_set(
    app: State<'_, App>,
    mpv_path: Option<String>,
    preload_percent: u8,
    update_proxy: Option<String>,
) -> Result<(), Error> {
    let update_proxy = update_proxy
        .map(|p| p.trim().to_owned())
        .filter(|p| !p.is_empty());
    if let Some(p) = &update_proxy {
        proxy_url(p)?;
    }
    app.update(|c| {
        c.mpv_path = mpv_path
            .map(|p| clean_path(&p).to_owned())
            .filter(|p| !p.is_empty());
        c.preload_percent = preload_percent.clamp(50, 99);
        c.update_proxy = update_proxy;
    })
}

fn tel_name(c: &Config) -> String {
    c.telemetry_name
        .clone()
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(emby::device_name)
}

/// 诊断数据开关和上报名称。打开时补记一条环境信息。
#[tauri::command]
fn telemetry_set(app: State<'_, App>, on: bool, name: String) -> Result<(), Error> {
    let was = telemetry::enabled();
    app.update(|c| {
        c.telemetry = on;
        c.telemetry_name = Some(name.trim().to_owned()).filter(|n| !n.is_empty());
    })?;
    telemetry::set_enabled(on);
    if was && !on {
        telemetry::purge();
    }
    if on && !was {
        record_env();
        record_servers(&app.cfg());
    }
    Ok(())
}

/// 界面攒的一批诊断事件（约 5 秒一批）；每条带类别 `k`。
#[tauri::command]
fn tel(events: Vec<Value>) {
    for mut e in events {
        let k = e
            .as_object_mut()
            .and_then(|m| m.remove("k"))
            .and_then(|k| k.as_str().map(|s| format!("ui_{s}")))
            .unwrap_or_else(|| "ui".into());
        telemetry::rec(&k, e);
    }
}

/// 上游标识 → 服务器名（不含地址），报表据此显示名字。启动、打开开关、添加 / 编辑服务器时记。
fn record_servers(c: &Config) {
    for s in &c.servers {
        telemetry::rec(
            "server",
            serde_json::json!({ "srv": s.view().tag, "name": s.name }),
        );
    }
}

/// 启动（或打开开关）时记一条环境信息：系统、CPU、内存、系统代理。屏幕、界面 GPU、WebView2 版本由界面补。
fn record_env() {
    if !telemetry::enabled() {
        return;
    }
    tauri::async_runtime::spawn_blocking(|| {
        use sysinfo::System;
        let mut s = System::new();
        s.refresh_memory();
        s.refresh_cpu_list(sysinfo::CpuRefreshKind::nothing());
        telemetry::rec(
            "env",
            serde_json::json!({
                "os": System::long_os_version(),
                "kernel": System::kernel_version(),
                "cpu": s.cpus().first().map(|c| c.brand().trim().to_owned()),
                "cores": s.cpus().len(),
                "mem_mb": s.total_memory() / 1024 / 1024,
                "proxy": system_proxy(),
            }),
        );
    });
}

/// 系统代理（只记 host:port）：查「网络错误」时要知道用户开没开代理。
fn system_proxy() -> Option<String> {
    if !cfg!(windows) {
        return std::env::var("https_proxy").ok();
    }
    let mut cmd = std::process::Command::new("reg");
    cmd.args([
        "query",
        r"HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings",
    ]);
    // reg 是控制台程序：不加 CREATE_NO_WINDOW 会闪一下黑框
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(&mut cmd, 0x0800_0000);
    let out = cmd.output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let val = |name: &str| {
        text.lines()
            .find(|l| l.split_whitespace().next() == Some(name))
            .and_then(|l| l.split_whitespace().last().map(str::to_owned))
    };
    if val("ProxyEnable").as_deref() != Some("0x1") {
        return None;
    }
    val("ProxyServer")
}

/// warn / error 日志转给诊断数据。去掉日志插件加的 `[日期][时间][模块][级别]` 前缀，`「…」`里的片名换成省略号。
fn forward_log(r: &log::Record) {
    if !telemetry::enabled() {
        return;
    }
    let mut msg = r.args().to_string();
    while msg.starts_with('[') {
        match msg.find(']') {
            Some(i) => msg.drain(..=i),
            None => break,
        };
    }
    telemetry::rec(
        "log",
        serde_json::json!({ "lvl": r.level().as_str(), "msg": scrub(msg.trim()) }),
    );
}

/// 去掉日志里夹带的片名（写在「」里）。
fn scrub(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut depth = 0;
    for ch in s.chars() {
        match ch {
            '「' => {
                depth += 1;
                if depth == 1 {
                    out.push_str("「…」");
                }
            }
            '」' if depth > 0 => depth -= 1,
            _ if depth == 0 => out.push(ch),
            _ => {}
        }
    }
    out
}

/// `emby-img` 协议：界面要的图交给 [`images::Images`]（磁盘缓存 → 上游）。路径里是服务器 id，这里换成那台服务器的 API 根；
/// 不认识的服务器回 404，不会去别的地址取。
async fn serve_image(handle: &AppHandle, path: &str) -> tauri::http::Response<Vec<u8>> {
    let Some(req) = images::Req::parse(path) else {
        return images::response(Err(400));
    };
    let app = handle.state::<App>();
    let base = {
        let c = app.cfg();
        c.servers
            .iter()
            .find(|s| s.id == req.server)
            .map(config::Server::api_base)
    };
    let Some(base) = base else {
        return images::response(Err(404));
    };
    let srv = config::server_tag(&base);
    images::response(app.images.get(&req, &base, &srv).await)
}

/// 更新代理：`http://`、`https://`、`socks5://`、`socks5h://` 加主机和端口。
fn proxy_url(p: &str) -> Result<reqwest::Url, Error> {
    let bad = || {
        Error::Other(format!(
            "更新代理地址不对：{p}（例：http://127.0.0.1:10808 或 socks5://127.0.0.1:10808）"
        ))
    };
    let u = reqwest::Url::parse(p).map_err(|_| bad())?;
    let ok = matches!(u.scheme(), "http" | "https" | "socks5" | "socks5h")
        && u.host_str().is_some_and(|h| !h.is_empty())
        && u.port_or_known_default().is_some();
    ok.then_some(u).ok_or_else(bad)
}

#[derive(Serialize)]
struct UpdateInfo {
    version: String,
    notes: String,
}

/// 检查更新和下载安装包用的 updater：设了更新代理就走它，否则跟随系统代理
/// （updater 每次都新建 HTTP 客户端，现读系统代理设置，不会像 Emby 的客户端那样读到过期的）。
fn updater(handle: &AppHandle, app: &App) -> Result<tauri_plugin_updater::Updater, Error> {
    // 40 MB 的安装包在慢代理上也要能下完；连不上时 reqwest 照常很快报错
    let mut b = handle.updater_builder().timeout(Duration::from_secs(900));
    if let Some(p) = app.cfg().update_proxy.clone() {
        b = b.proxy(proxy_url(&p)?);
    }
    b.build()
        .map_err(|e| Error::Other(format!("检查更新失败：{e}")))
}

/// 有新版本时返回版本号和更新说明（Release 里的 latest.json，签名用编进程序的公钥校验）。
#[tauri::command]
async fn update_check(handle: AppHandle, app: State<'_, App>) -> Result<Option<UpdateInfo>, Error> {
    let u = updater(&handle, &app)?.check().await.map_err(|e| {
        log::warn!("检查更新失败：{e}");
        Error::Other(format!("检查更新失败：{e}"))
    })?;
    Ok(u.map(|u| UpdateInfo {
        version: u.version,
        notes: u.body.unwrap_or_default(),
    }))
}

/// 下载并安装新版本：进度经 `update-progress` 事件发给界面（已下载字节, 总字节）。
/// 下完先关播放器、回传进度，再启动安装器（只显示进度条，不用点），安装器装完自动重新打开程序。
#[tauri::command]
async fn update_install(handle: AppHandle, app: State<'_, App>) -> Result<(), Error> {
    let fail = |e: tauri_plugin_updater::Error| {
        log::warn!("更新失败：{e}");
        Error::Other(format!("更新失败：{e}"))
    };
    let u = updater(&handle, &app)?
        .check()
        .await
        .map_err(fail)?
        .ok_or_else(|| Error::Other("已是最新版本".into()))?;
    log::info!("下载新版本 {}", u.version);
    let (mut got, mut shown) = (0u64, u64::MAX);
    let bytes = u
        .download(
            |n, total| {
                got += n as u64;
                // 每个分块都发的话 40 MB 要发几千次：百分比（不知道总大小时按 MB）变了才发
                let step = total.map_or(got >> 20, |t| got * 100 / t.max(1));
                if step != shown {
                    shown = step;
                    let _ = handle.emit("update-progress", (got, total));
                }
            },
            || {},
        )
        .await
        .map_err(fail)?;
    let h = handle.clone();
    let _ = tauri::async_runtime::spawn_blocking(move || {
        h.state::<App>().shutdown(Duration::from_secs(3));
    })
    .await;
    log::info!("启动安装器，程序退出");
    // Windows 上启动安装器后直接退出进程，不会返回
    if let Err(e) = u.install(bytes) {
        // 安装器没起来，程序照常用：撤销退出标记，否则之后点播放都会被当成「正在退出」而放弃
        app.quit.send_replace(false);
        return Err(fail(e));
    }
    handle.restart()
}

/// 通用 Emby 请求（当前服务器）。401 时清掉该服务器的 token，界面据此引导重新登录。
#[tauri::command]
async fn emby(
    app: State<'_, App>,
    method: String,
    path: String,
    query: Option<Map<String, Value>>,
    body: Option<Value>,
) -> Result<Value, Error> {
    let (server_id, client) = app.active()?;
    let method = match method.to_ascii_uppercase().as_str() {
        "GET" => Method::GET,
        "POST" => Method::POST,
        "DELETE" => Method::DELETE,
        m => return Err(Error::Other(format!("不支持的方法：{m}"))),
    };
    let query: Vec<(String, String)> = query
        .unwrap_or_default()
        .into_iter()
        .filter(|(_, v)| !v.is_null())
        .map(|(k, v)| (k, v.as_str().map_or_else(|| v.to_string(), str::to_owned)))
        .collect();
    let r = client.call(method, &path, &query, body.as_ref()).await;
    if matches!(r, Err(Error::Unauthorized)) {
        // 清发请求的那台：请求途中用户可能已经切到别的服务器
        let _ = app.update(|c| {
            if let Some(s) = c.servers.iter_mut().find(|s| s.id == server_id) {
                s.token.clear();
            }
        });
    }
    r
}

#[tauri::command]
async fn play(
    handle: tauri::AppHandle,
    app: State<'_, App>,
    item_id: String,
    from_start: bool,
) -> Result<(), Error> {
    let _starting = Starting::enter(&app.starting);
    telemetry::hold_uploads(); // 起播那几秒不上传诊断数据
    let (_, client) = app.active()?;
    let seq = app.play_seq.fetch_add(1, Ordering::SeqCst) + 1;
    let _ = app.stop_player(); // 换片：旧的 mpv 自己退出、回传，不用等
    let (preload_percent, volume) = {
        let c = app.cfg();
        (f64::from(c.preload_percent), c.volume)
    };
    let opts = player::Options {
        mpv: app.mpv_path(),
        config_dir: app.mpv_dir.clone(),
        preload_percent,
        volume,
        cache_dir: app.mpv_cache.clone(),
        user_conf: Some(app.user_mpv_conf()),
        extra_args: Vec::new(),
    };
    let Some(session) = app
        .unless_quit(player::start(client, &item_id, from_start, opts))
        .await
    else {
        log::info!("程序退出，放弃起播：{item_id}");
        return Ok(());
    };
    let session = session?;
    if *app.quit.borrow() {
        session.abort().await;
        return Ok(());
    }
    if app.play_seq.load(Ordering::SeqCst) != seq {
        log::info!("起播期间又点了别的，作废这一次：{item_id}");
        session.abort().await;
        return Ok(());
    }
    let (ctl_tx, ctl_rx) = mpsc::unbounded_channel();
    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
    tauri::async_runtime::spawn(session.run(ctl_rx, ev_tx));
    // 转发播放事件；会话结束（Ended 发出、通道关闭）后它才结束，所以退出时等它就等到了回传和保存音量
    let task = tauri::async_runtime::spawn(async move {
        while let Some(ev) = ev_rx.recv().await {
            if let player::Event::Ended { volume: Some(v) } = ev {
                let _ = handle.state::<App>().update(|c| c.volume = Some(v));
            }
            let _ = handle.emit("player", &ev);
        }
    });
    // 保险：万一这时已有别的播放器登记着，关掉它，不留一个没人管的 mpv
    if let Some(old) = lock(&app.player).replace(Player { ctl: ctl_tx, task }) {
        let _ = old.ctl.send(player::Ctl::Quit);
    }
    Ok(())
}

fn main() {
    let ctx = tauri::generate_context!();
    let root = config::data_root(&ctx.config().identifier);
    let exit_root = root.clone();
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _, _| {
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.unminimize();
                let _ = w.set_focus();
            }
        }))
        .plugin(
            // 默认 40 KB 就整个删掉重来，排查要的记录留不住：放到 2 MB，轮换时留最近 3 份；时间用本地时间
            tauri_plugin_log::Builder::new()
                .level(log::LevelFilter::Info)
                .max_file_size(2_000_000)
                .rotation_strategy(RotationStrategy::KeepSome(3))
                .timezone_strategy(TimezoneStrategy::UseLocal)
                .clear_targets()
                .targets([
                    Target::new(TargetKind::Stdout),
                    Target::new(TargetKind::Folder {
                        path: root.join("logs"),
                        file_name: None,
                    }),
                    Target::new(TargetKind::Dispatch(
                        tauri_plugin_log::fern::Dispatch::new()
                            .chain(tauri_plugin_log::fern::Output::call(forward_log)),
                    ))
                    .filter(|m| m.level() <= log::Level::Warn),
                ])
                .build(),
        )
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .register_asynchronous_uri_scheme_protocol(images::SCHEME, |ctx, req, responder| {
            let handle = ctx.app_handle().clone();
            let path = req.uri().path().to_owned();
            tauri::async_runtime::spawn(async move {
                responder.respond(serve_image(&handle, &path).await);
            });
        })
        .setup(move |app| {
            let p = app.path();
            let old = [
                p.app_config_dir()?,
                p.app_local_data_dir()?,
                p.app_cache_dir()?,
            ];
            match config::migrate(&root, &old[0], &old) {
                Ok(true) => log::info!("已把旧版本的配置搬到数据目录"),
                Ok(false) => {}
                Err(e) => log::warn!("搬旧版本的配置失败：{e}"),
            }
            // WebView2 的数据（含图片磁盘缓存）默认在 %LOCALAPPDATA%，只能在建窗口时改，所以主窗口在这里建
            let win = app
                .config()
                .app
                .windows
                .first()
                .cloned()
                .ok_or("缺少主窗口配置")?;
            tauri::WebviewWindowBuilder::from_config(app.handle(), &win)?
                .data_directory(root.join("webview"))
                .build()?;
            let path = root.join("config.json");
            let cfg = config::load(&path);
            config::save(&path, &cfg)?; // 首次启动把生成的 device_id 落盘
            telemetry::set_enabled(cfg.telemetry);
            telemetry::start(
                root.join("telemetry"),
                telemetry::Meta {
                    device: cfg.device_id.clone(),
                    name: tel_name(&cfg),
                    version: emby::VERSION,
                },
            );
            record_env();
            record_servers(&cfg);
            exit_report(&root);
            let mpv_dir = app
                .path()
                .resource_dir()
                .ok()
                .and_then(|d| player::bundled_dir(&d));
            // CI 冒烟测试按这一行判断安装后的路径是否正确（不能带 \\?\ 前缀），改措辞要同步改 windows.yml
            log::info!(
                "EmbyMini {} 启动；数据目录：{}；自带 mpv 配置：{}",
                emby::VERSION,
                root.display(),
                mpv_dir
                    .as_deref()
                    .map_or("（无）".into(), |d| d.display().to_string())
            );
            let mpv_cache = Some(root.join("mpv-cache"));
            let http = emby::http_client()?;
            let images =
                images::Images::new(root.join("cache").join("images"), images::CAP, http.clone());
            images.start();
            app.manage(App {
                path,
                cfg: Mutex::new(cfg),
                http,
                images,
                mpv_dir,
                mpv_cache,
                player: Mutex::new(None),
                play_seq: AtomicU64::new(0),
                quit: watch::Sender::new(false),
                starting: AtomicUsize::new(0),
                closing: Mutex::new(Vec::new()),
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            config_get,
            server_add,
            server_edit,
            server_login,
            server_remove,
            server_use,
            settings_set,
            emby,
            play,
            update_check,
            update_install,
            telemetry_set,
            tel
        ])
        .build(ctx)
        .unwrap_or_else(|e| panic!("EmbyMini 启动失败：{e}"))
        .run(move |handle, ev| match ev {
            // 点关闭：先藏窗口、让 mpv 退出，再走销毁窗口、等回传这些慢的步骤，用户看到的是立刻关掉
            tauri::RunEvent::WindowEvent {
                label,
                event: tauri::WindowEvent::CloseRequested { .. },
                ..
            } if label == "main" => {
                exit_start();
                exit_mark("close_requested");
                if let Some(w) = handle.get_webview_window("main") {
                    let _ = w.hide();
                }
                exit_mark("hidden");
                handle.state::<App>().begin_shutdown();
                exit_mark("mpv_quit_sent");
            }
            tauri::RunEvent::WindowEvent {
                event: tauri::WindowEvent::Destroyed,
                ..
            } => exit_mark("window_destroyed"),
            tauri::RunEvent::ExitRequested { .. } => exit_mark("exit_requested"),
            tauri::RunEvent::Exit => {
                exit_start();
                exit_mark("exit");
                handle.state::<App>().shutdown(Duration::from_secs(3));
                exit_save(&exit_root);
            }
            _ => {}
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_url_accepts_common_forms() {
        for ok in [
            "http://127.0.0.1:10808",
            "socks5://127.0.0.1:10808",
            "socks5h://localhost:1080",
        ] {
            assert!(proxy_url(ok).is_ok(), "{ok}");
        }
        for bad in ["127.0.0.1:10808", "ftp://h:1", "socks5://", ""] {
            assert!(proxy_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn scrub_hides_titles() {
        assert_eq!(scrub("无法播放「浴血黑帮」：403"), "无法播放「…」：403");
        assert_eq!(scrub("a「b「c」d」e"), "a「…」e");
        assert_eq!(scrub("没有片名"), "没有片名");
    }

    #[test]
    fn clean_path_strips_quotes() {
        assert_eq!(
            clean_path(r#" "C:\Program Files\mpv\mpv.exe" "#),
            r"C:\Program Files\mpv\mpv.exe"
        );
        assert_eq!(clean_path("  "), "");
    }

    fn test_app(player: Option<Player>) -> App {
        App {
            path: PathBuf::new(),
            cfg: Mutex::new(Config::default()),
            http: reqwest::Client::new(),
            mpv_dir: None,
            mpv_cache: None,
            player: Mutex::new(player),
            images: images::Images::new(
                std::env::temp_dir().join("embymini-test-images"),
                images::CAP,
                reqwest::Client::new(),
            ),
            play_seq: AtomicU64::new(0),
            quit: watch::Sender::new(false),
            starting: AtomicUsize::new(0),
            closing: Mutex::new(Vec::new()),
        }
    }

    /// 起播中退出：起播被放弃（mpv 随之被杀），shutdown 等到 play 返回，不白等满 wait。
    #[test]
    fn shutdown_cancels_starting_play() {
        let app = std::sync::Arc::new(test_app(None));
        let a = app.clone();
        let task = tauri::async_runtime::spawn(async move {
            let _g = Starting::enter(&a.starting);
            a.unless_quit(std::future::pending::<()>()).await
        });
        while app.starting.load(Ordering::SeqCst) == 0 {
            std::thread::yield_now();
        }
        let t0 = std::time::Instant::now();
        app.shutdown(Duration::from_secs(3));
        assert!(t0.elapsed() < Duration::from_secs(1));
        assert_eq!(app.starting.load(Ordering::SeqCst), 0);
        assert!(tauri::async_runtime::block_on(task).unwrap().is_none());
        // 已经在退出：再点播放直接放弃
        let late = tauri::async_runtime::block_on(app.unless_quit(async { 1 }));
        assert_eq!(late, None);
    }

    /// 退出时要等播放任务收尾（回传 Stopped），但不能无限等。
    #[test]
    fn shutdown_waits_for_player_then_gives_up() {
        let app = |task, ctl| test_app(Some(Player { ctl, task }));
        // 收到 Quit 后 200ms 才收尾：shutdown 应等到它结束
        let (tx, mut rx) = mpsc::unbounded_channel();
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let d = done.clone();
        let task = tauri::async_runtime::spawn(async move {
            assert!(matches!(rx.recv().await, Some(player::Ctl::Quit)));
            tokio::time::sleep(Duration::from_millis(200)).await;
            d.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        app(task, tx).shutdown(Duration::from_secs(3));
        assert!(done.load(std::sync::atomic::Ordering::SeqCst));

        // 卡住不收尾：最多等 wait
        let (tx, _rx) = mpsc::unbounded_channel();
        let task = tauri::async_runtime::spawn(std::future::pending::<()>());
        let t0 = std::time::Instant::now();
        app(task, tx).shutdown(Duration::from_millis(300));
        assert!(t0.elapsed() < Duration::from_secs(2));
    }
}
