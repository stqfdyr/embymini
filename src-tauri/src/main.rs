// release 下不弹控制台窗口，不要删。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
mod danmaku;
mod emby;
mod images;
mod player;
mod status;
mod stream;
mod updater;

use config::{Config, Server, ServerView};
use emby::{Emby, Error};
use reqwest::Method;
use serde::Serialize;
use serde_json::{Map, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_log::{RotationStrategy, Target, TargetKind, TimezoneStrategy};
use tokio::sync::{mpsc, watch};

/// 从点关闭（或开始退出）算起的时刻；关闭各阶段耗时写日志，查「关闭时卡一下」卡在哪一段。
static CLOSE_T0: OnceLock<Instant> = OnceLock::new();

/// 开始计时（重复调用以第一次为准）。
fn exit_start() {
    CLOSE_T0.get_or_init(Instant::now);
}

/// 记一个关闭阶段；还没开始关闭时（例如平时关掉 mpv）什么都不做。
pub(crate) fn exit_mark(phase: &str) {
    if let Some(t0) = CLOSE_T0.get() {
        log::info!("关闭耗时 {phase}：{} ms", t0.elapsed().as_millis());
    }
}

/// 运行标记（数据目录下的 `running`，内容是版本号）：启动时写、正常退出（含自动更新）时删。
/// 启动时它还在 = 上次崩溃、被结束进程或关机时被杀。
const RUNNING: &str = "running";

pub(crate) fn set_running(root: &std::path::Path, on: bool) {
    let f = root.join(RUNNING);
    let _ = if on {
        std::fs::write(f, emby::VERSION)
    } else {
        std::fs::remove_file(f)
    };
}

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
    /// 本地缓存代理（视频、字幕）；起不来时为 None，播放直连。
    stream: Option<stream::Stream>,
    /// 弹幕缓存目录（数据目录下的 `cache/danmaku`），缓存管理里算在「字幕和弹幕」。
    danmaku_dir: PathBuf,
    /// 每次点播放加一；起播要等几秒，完成时若已不是最新一次点击，就作废自己。
    play_seq: AtomicU64,
    /// 程序要退出了：起播中的 `play` 收到后放弃起播（见 [`App::unless_quit`]）。
    quit: watch::Sender<bool>,
    /// 还没返回的 `play` 个数（[`Starting`]），退出时等它们收尾。
    starting: AtomicUsize,
    /// 已叫停、还在收尾（回传 Stopped）的播放任务，退出时等它们（见 [`App::begin_shutdown`]）。
    closing: Mutex<Vec<tauri::async_runtime::JoinHandle<()>>>,
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
        config::update(&self.path, &mut self.cfg(), f)
            .map_err(|e| Error::Other(format!("保存配置失败：{e}")))
    }

    /// Best-effort change (player state, token cleared after a 401): kept in memory even if the save fails.
    fn remember(&self, f: impl FnOnce(&mut Config)) {
        let mut c = self.cfg();
        f(&mut c);
        if let Err(e) = config::save(&self.path, &c) {
            log::warn!("保存配置失败：{e}");
        }
    }

    /// A copy of the saved server `id`.
    fn server(&self, id: &str) -> Result<Server, Error> {
        let c = self.cfg();
        c.servers
            .iter()
            .find(|s| s.id == id)
            .cloned()
            .ok_or_else(no_server)
    }

    /// Replaces the saved server with the same id (fails if it was removed meanwhile).
    fn put_server(&self, s: Server) -> Result<ServerView, Error> {
        let view = s.view();
        self.update(|c| c.servers.iter_mut().find(|t| t.id == s.id).map(|t| *t = s))?
            .ok_or_else(no_server)?;
        Ok(view)
    }

    /// Logs `s.user` in at `s`'s address and stores the new user id and token in `s` (not saved).
    async fn login(&self, s: &mut Server, password: &str) -> Result<(), Error> {
        let device_id = self.cfg().device_id.clone();
        (s.user_id, s.token) =
            emby::login(&self.http, &s.api_base(), &device_id, &s.user, password).await?;
        Ok(())
    }

    /// Id and client of the active server.
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
    /// Path of the personal mpv.conf, shown on the settings page.
    user_mpv_conf: String,
    update_proxy: Option<String>,
    cache_whole: bool,
    /// Danmaku API address (may carry a token; only this machine's UI sees it) and blocked words.
    danmaku_api: String,
    danmaku_words: Vec<String>,
    version: &'static str,
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
        cache_whole: c.cache_whole,
        danmaku_api: c.danmaku.api.clone(),
        danmaku_words: c.danmaku.words.clone(),
        version: emby::VERSION,
    }
}

fn no_server() -> Error {
    Error::Other("服务器不存在".into())
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
    s.name = emby::server_name(&app.http, &s.api_base()).await?;
    app.login(&mut s, &password).await?;
    let view = s.view();
    app.update(|c| {
        c.active = Some(s.id.clone());
        c.servers.push(s);
    })?;
    Ok(view)
}

/// Edits a server (same id; stays active if it was): address, port, user, or a fresh login.
/// Connects to the new address first (refreshing the server name) and saves nothing if that fails.
/// An address-only change (new domain, port, reverse proxy) keeps the token, no password needed;
/// a changed user, a typed password or a missing token logs in again with the given credentials.
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
    let mut s = app.server(&id)?;
    let relogin = !password.is_empty() || user != s.user || s.token.is_empty();
    (s.url, s.port, s.user) = (url, port, user);
    s.name = emby::server_name(&app.http, &s.api_base()).await?;
    if relogin {
        app.login(&mut s, &password).await?;
    }
    app.put_server(s)
}

/// Logs in again with just the password: the token expired or a new one is wanted.
#[tauri::command]
async fn server_login(
    app: State<'_, App>,
    id: String,
    password: String,
) -> Result<ServerView, Error> {
    let mut s = app.server(&id)?;
    app.login(&mut s, &password).await?;
    // Only the login fields: an edit saved while logging in keeps its address.
    app.update(|c| {
        let t = c.servers.iter_mut().find(|t| t.id == id)?;
        (t.user_id, t.token) = (s.user_id, s.token);
        Some(t.view())
    })?
    .ok_or_else(no_server)
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
    cache_whole: bool,
) -> Result<(), Error> {
    let update_proxy = update_proxy
        .map(|p| p.trim().to_owned())
        .filter(|p| !p.is_empty());
    if let Some(p) = &update_proxy {
        updater::proxy_url(p)?;
    }
    app.update(|c| {
        c.mpv_path = mpv_path
            .map(|p| clean_path(&p).to_owned())
            .filter(|p| !p.is_empty());
        c.preload_percent = preload_percent.clamp(50, 99);
        c.update_proxy = update_proxy;
        c.cache_whole = cache_whole;
    })?;
    if let Some(s) = &app.stream {
        s.set_whole(cache_whole);
    }
    Ok(())
}

/// 弹幕设置页的两项：接口地址（`/api/v2` 格式，可带令牌）、屏蔽词（一行一个，空行去掉）。显示设置在播放器的弹幕面板里改。
#[tauri::command]
fn danmaku_set(app: State<'_, App>, api: String, words: Vec<String>) -> Result<(), Error> {
    let api = danmaku::normalize(&api).map_err(Error::Other)?;
    let words = words
        .iter()
        .map(|w| w.trim().to_owned())
        .filter(|w| !w.is_empty())
        .collect::<Vec<String>>();
    if let Some(p) = lock(&app.player).as_ref() {
        let _ = p.ctl.send(player::Ctl::Danmaku {
            api: danmaku::Api::new(app.http.clone(), &api),
            words: words.clone(),
        });
    }
    app.update(|c| {
        c.danmaku.api = api;
        c.danmaku.words = words;
    })
}

/// 缓存管理里各项的字节数；`playing` 时视频缓存里正在播的那部片清不掉。媒体库缓存（列表快照）在界面的 localStorage 里，界面自己算。
#[derive(Serialize)]
struct CacheInfo {
    video: u64,
    images: u64,
    /// 外挂字幕和弹幕缓存。
    subs: u64,
    temp: u64,
    playing: bool,
}

/// 「临时缓存」：mpv 的着色器缓存、更新插件留在系统临时目录的安装包目录（每次更新约 40 MB，插件自己不删）、进度条预览留下的图片文件。
fn temp_paths(app: &App) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = app.mpv_cache.iter().cloned().collect();
    v.extend(updater_leftovers());
    for e in std::fs::read_dir(std::env::temp_dir())
        .into_iter()
        .flatten()
        .flatten()
    {
        if e.file_name().to_string_lossy().starts_with("thumbfast.out") {
            v.push(e.path());
        }
    }
    v
}

/// 更新插件的临时目录（`<产品名>-<版本>-updater-<随机>`）：装完新版本就没用了。
fn updater_leftovers() -> Vec<PathBuf> {
    std::fs::read_dir(std::env::temp_dir())
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            n.starts_with("EmbyMini-") && n.contains("-updater-")
        })
        .map(|e| e.path())
        .collect()
}

fn remove_path(p: &std::path::Path) {
    let _ = if p.is_dir() {
        std::fs::remove_dir_all(p)
    } else {
        std::fs::remove_file(p)
    };
}

fn cache_info_now(app: &App) -> CacheInfo {
    let (video, subs) = app.stream.as_ref().map_or((0, 0), stream::Stream::usage);
    let subs = subs + stream::dir_size(&app.danmaku_dir);
    let playing = lock(&app.player).is_some() || app.starting.load(Ordering::SeqCst) > 0;
    let temp = temp_paths(app)
        .iter()
        .map(|p| {
            if p.is_dir() {
                stream::dir_size(p)
            } else {
                p.metadata().map_or(0, |m| m.len())
            }
        })
        .sum();
    CacheInfo {
        video,
        images: app.images.usage(),
        subs,
        temp,
        playing,
    }
}

#[tauri::command]
async fn cache_info(handle: AppHandle) -> CacheInfo {
    tauri::async_runtime::spawn_blocking(move || cache_info_now(&handle.state::<App>()))
        .await
        .unwrap_or(CacheInfo {
            video: 0,
            images: 0,
            subs: 0,
            temp: 0,
            playing: false,
        })
}

/// 清理一项（`video` / `images` / `subs`（字幕和弹幕）/ `temp`，`all` = 全部），返回清理后的占用。
#[tauri::command]
async fn cache_clear(handle: AppHandle, kind: String) -> CacheInfo {
    tauri::async_runtime::spawn_blocking(move || {
        let app = handle.state::<App>();
        let all = kind == "all";
        let playing = lock(&app.player).is_some() || app.starting.load(Ordering::SeqCst) > 0;
        if let Some(s) = app.stream.as_ref() {
            if all || kind == "video" {
                s.clear_video(playing);
            }
            if all || kind == "subs" {
                s.clear_subs();
            }
        }
        if all || kind == "subs" {
            remove_path(&app.danmaku_dir);
        }
        if all || kind == "images" {
            app.images.clear();
        }
        if all || kind == "temp" {
            temp_paths(&app).iter().for_each(|p| remove_path(p));
        }
        log::info!("清理缓存：{kind}");
        cache_info_now(&app)
    })
    .await
    .unwrap_or(CacheInfo {
        video: 0,
        images: 0,
        subs: 0,
        temp: 0,
        playing: false,
    })
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
    images::response(app.images.get(&req, &base).await)
}

/// Generic Emby request to the active server. On 401 that server's token is cleared and the UI asks to sign in.
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
        // Clear the server that was asked: the user may have switched servers meanwhile
        app.remember(|c| {
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
    source_id: Option<String>,
    from_start: bool,
) -> Result<(), Error> {
    let _starting = Starting::enter(&app.starting);
    let (_, client) = app.active()?;
    let seq = app.play_seq.fetch_add(1, Ordering::SeqCst) + 1;
    let _ = app.stop_player(); // 换片：旧的 mpv 自己退出、回传，不用等
    let (preload_percent, volume, dm) = {
        let c = app.cfg();
        (f64::from(c.preload_percent), c.volume, c.danmaku.clone())
    };
    let opts = player::Options {
        mpv: app.mpv_path(),
        config_dir: app.mpv_dir.clone(),
        preload_percent,
        volume,
        cache_dir: app.mpv_cache.clone(),
        user_conf: Some(app.user_mpv_conf()),
        stream: app.stream.clone(),
        danmaku: Some(danmaku::Ctx {
            api: danmaku::Api::new(app.http.clone(), &dm.api),
            dir: app.danmaku_dir.clone(),
            cfg: dm,
        }),
        extra_args: Vec::new(),
    };
    let Some(session) = app
        .unless_quit(player::start(
            client,
            &item_id,
            source_id.as_deref(),
            from_start,
            opts,
        ))
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
            let app = handle.state::<App>();
            match ev {
                player::Event::Ended { volume: Some(v) } => {
                    app.remember(|c| c.volume = Some(v));
                }
                // 弹幕面板里的改动：存进配置，界面不用知道
                player::Event::DanmakuStyle { style } => {
                    app.remember(|c| c.danmaku.style = style);
                    continue;
                }
                player::Event::DanmakuPick { key, pick } => {
                    app.remember(|c| {
                        c.danmaku.picks.insert(key, pick);
                    });
                    continue;
                }
                _ => {}
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
    // panic 写进日志，再交给默认处理
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log::error!("程序出错（panic）：{info}");
        default_hook(info);
    }));
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
            let cfg = config::load(&path)
                .map_err(|e| format!("读取配置失败（{}）：{e}", path.display()))?;
            config::save(&path, &cfg)?; // 首次启动把生成的 device_id 落盘
            if let Ok(v) = std::fs::read_to_string(root.join(RUNNING)) {
                log::warn!("上次没有正常退出（版本 {v}）");
            }
            set_running(&root, true);
            // shortcut: 旧版本诊断数据留下的暂存与标记（暂存最多约 20 MB），用户大多升过几版后去掉
            let old = root.clone();
            tauri::async_runtime::spawn_blocking(move || {
                for f in ["telemetry", "last-exit.json", "running.json"] {
                    remove_path(&old.join(f));
                }
            });
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
            let stream = stream::Stream::start(&root.join("cache"), cfg.cache_whole);
            // 装完新版本，更新插件留下的安装包就没用了；等一会儿再删（安装器可能还没退出）
            tauri::async_runtime::spawn(async {
                tokio::time::sleep(Duration::from_secs(60)).await;
                for p in updater_leftovers() {
                    remove_path(&p);
                }
            });
            app.manage(App {
                path,
                cfg: Mutex::new(cfg),
                http,
                images,
                stream,
                danmaku_dir: root.join("cache").join("danmaku"),
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
            danmaku_set,
            cache_info,
            cache_clear,
            emby,
            play,
            updater::update_check,
            updater::update_install,
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
                set_running(&exit_root, false);
            }
            _ => {}
        });
}

#[cfg(test)]
mod tests {
    use super::*;

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
            // 这些测试不取图：空路径不建目录，不在临时目录留东西
            images: images::Images::new(PathBuf::new(), images::CAP, reqwest::Client::new()),
            stream: None,
            danmaku_dir: PathBuf::new(),
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
