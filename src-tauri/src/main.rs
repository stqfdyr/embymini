// No console window in release builds: keep this.
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
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_log::{RotationStrategy, Target, TargetKind, TimezoneStrategy};
use tokio::sync::{mpsc, watch};

/// When closing started (close clicked or exit begun): each close phase logs its time since, to find where
/// "closing hangs for a moment" is spent.
static CLOSE_T0: Mutex<Option<Instant>> = Mutex::new(None);

/// Starts the timer; later calls keep the first start.
fn exit_start() {
    lock(&CLOSE_T0).get_or_insert_with(Instant::now);
}

/// The exit was called off (the installer did not start): the next close times from scratch.
fn exit_cancel() {
    *lock(&CLOSE_T0) = None;
}

fn exit_elapsed() -> Option<Duration> {
    lock(&CLOSE_T0).map(|t0| t0.elapsed())
}

/// Logs a close phase; does nothing when not closing (e.g. mpv closed during normal use).
pub(crate) fn exit_mark(phase: &str) {
    if let Some(t) = exit_elapsed() {
        log::info!("关闭耗时 {phase}：{} ms", t.as_millis());
    }
}

/// Running marker (`running` in the data dir, holding the version): written at start, removed on a clean exit
/// (auto-update included). Still there at start = the last run crashed or was killed.
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
    /// Bundled mpv config in the resource dir (ModernZ etc.); also holds mpv.exe when the package ships one.
    mpv_dir: Option<PathBuf>,
    /// mpv's cache dir, `mpv-cache` under the data dir, so uninstalling with "delete app data" removes it.
    mpv_cache: Option<PathBuf>,
    player: Mutex<Option<Player>>,
    /// Disk cache for posters and backdrops (the `emby-img` protocol).
    images: std::sync::Arc<images::Images>,
    /// Local caching proxy (video, subtitles); None when it failed to start, then playback goes direct.
    stream: Option<stream::Stream>,
    /// Danmaku cache dir (`cache/danmaku`), counted under subtitles in cache management.
    danmaku_dir: PathBuf,
    /// Bumped on every play click; a start that finishes after a newer click discards itself.
    play_seq: AtomicU64,
    /// The program is exiting: a starting `play` gives up (see [`App::unless_quit`]).
    quit: watch::Sender<bool>,
    /// `play` calls not yet returned ([`Starting`]); exit waits for them.
    starting: AtomicUsize,
    /// Players told to quit that are still reporting Stopped; exit waits for them (see [`App::retire`]).
    closing: Mutex<Vec<tauri::async_runtime::JoinHandle<()>>>,
}

/// Counts `play` calls in progress: up on entry, down when dropped.
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

/// The current playback: its control channel and the event-forwarding task (ends after the session).
struct Player {
    ctl: mpsc::UnboundedSender<player::Ctl>,
    task: tauri::async_runtime::JoinHandle<()>,
}

/// The mpv path from settings without surrounding blanks and quotes (Explorer's "Copy as path" adds them).
fn clean_path(p: &str) -> &str {
    p.trim().trim_matches('"').trim()
}

/// Ignores lock poisoning: the data here is replaced whole, never left half-updated by a panic.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Subtracts without going below 0: an inexact count should undercount, never wrap into a huge number
/// that would make trimming delete everything. Shared by the video and image caches.
fn sub_total(total: &AtomicU64, n: u64) {
    // fetch_update is renamed try_update in new toolchains and the new name is missing in our MSRV: hand-rolled CAS.
    let mut v = total.load(Ordering::Relaxed);
    while let Err(now) =
        total.compare_exchange_weak(v, v.saturating_sub(n), Ordering::Relaxed, Ordering::Relaxed)
    {
        v = now;
    }
}

impl App {
    fn cfg(&self) -> MutexGuard<'_, Config> {
        lock(&self.cfg)
    }

    /// A player is up or a `play` is still starting: the video cache keeps this playback's files.
    fn playing(&self) -> bool {
        lock(&self.player).is_some() || self.starting.load(Ordering::SeqCst) > 0
    }

    /// Tells the running mpv to quit; false when there is none.
    fn stop_player(&self) -> bool {
        let p = lock(&self.player).take();
        p.map(|p| self.retire(p)).is_some()
    }

    /// Sends Quit and keeps the task: it still reports Stopped, and [`App::shutdown`] waits for it.
    fn retire(&self, p: Player) {
        let _ = p.ctl.send(player::Ctl::Quit);
        let mut closing = lock(&self.closing);
        closing.retain(|t| !t.inner().is_finished());
        closing.push(p.task);
    }

    /// Starting takes seconds (PlaybackInfo, IPC) with the mpv window already open. On exit the start future is
    /// dropped and mpv dies with its `Child` (`kill_on_drop`), leaving no orphaned empty window.
    async fn unless_quit<F: Future>(&self, f: F) -> Option<F::Output> {
        let mut quit = self.quit.subscribe();
        tokio::select! {
            biased; // already exiting: give up even if f is ready too
            _ = quit.wait_for(|q| *q) => None,
            r = f => Some(r),
        }
    }

    /// First exit step, run as soon as close is clicked (before window teardown and `RunEvent::Exit`): stops
    /// starting `play` calls and tells mpv to quit; [`App::shutdown`] waits for the reports. Repeatable.
    fn begin_shutdown(&self) {
        self.quit.send_replace(true);
        if self.stop_player() {
            log::info!("程序退出，先关闭播放器并回传进度");
        }
    }

    /// Call before exiting: mpv is a separate process that outlives us, while the report tasks die with us and the
    /// resume point is lost. So stop starting `play` calls and wait for them (or one may register a player right
    /// after), then quit mpv and wait for the Stopped reports. Waits `wait` at most in total.
    fn shutdown(&self, wait: Duration) {
        exit_start();
        self.begin_shutdown();
        tauri::async_runtime::block_on(async {
            let deadline = tokio::time::Instant::now() + wait;
            while self.starting.load(Ordering::SeqCst) > 0 && tokio::time::Instant::now() < deadline
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            self.begin_shutdown(); // a player registered while we waited
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

    /// The personal mpv config: next to config.json in the data dir, so updates leave it alone. Users create it
    /// themselves; it is loaded only if it exists.
    fn user_mpv_conf(&self) -> PathBuf {
        self.path.with_file_name("mpv.conf")
    }

    /// mpv to run: the path set in settings, else the bundled one, else whatever PATH finds.
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

/// The danmaku settings page: API address (`/api/v2` style, may carry a token) and blocked words (one per line,
/// blanks dropped); applied to the running playback at once. Display settings live in the player's panel.
#[tauri::command]
fn danmaku_set(app: State<'_, App>, api: String, words: Vec<String>) -> Result<(), Error> {
    let api = danmaku::normalize(&api).map_err(Error::Other)?;
    let words: Vec<String> = words
        .iter()
        .map(|w| w.trim())
        .filter(|w| !w.is_empty())
        .map(str::to_owned)
        .collect();
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

/// Bytes per cache manager row; `playing` means the film being played stays in the video cache. The library
/// cache (list snapshots) lives in the UI's localStorage and is counted there.
#[derive(Serialize, Default)]
struct CacheInfo {
    video: u64,
    images: u64,
    /// External subtitles plus danmaku.
    subs: u64,
    temp: u64,
    playing: bool,
}

/// What `cache_clear` empties (`subs` = subtitles and danmaku).
#[derive(serde::Deserialize, Debug, Clone, Copy, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Clear {
    Video,
    Images,
    Subs,
    Temp,
    All,
}

impl Clear {
    fn covers(self, kind: Clear) -> bool {
        self == Clear::All || self == kind
    }
}

/// The updater plugin's temp dir, `<productName>-<version>-updater-<random>`: the plugin keeps it forever
/// (`tempdir().keep()`, ~40 MB per update) and it is useless once the new version is installed.
fn is_updater_leftover(name: &str) -> bool {
    name.starts_with("EmbyMini-") && name.contains("-updater-")
}

/// Entries of `dir` (the system temp dir) whose name passes `keep`.
fn temp_entries(dir: &std::path::Path, keep: impl Fn(&str) -> bool) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| keep(&e.file_name().to_string_lossy()))
        .map(|e| e.path())
        .collect()
}

/// "Temporary cache": mpv's shader cache, plus updater leftovers and seek-preview images in `tmp`.
fn temp_paths(app: &App, tmp: &std::path::Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = app.mpv_cache.iter().cloned().collect();
    v.extend(temp_entries(tmp, |n| {
        is_updater_leftover(n) || n.starts_with("thumbfast.out")
    }));
    v
}

/// Deletes a file, a directory tree or a symlink. Symlinks are never followed (the shared /tmp may hold
/// anyone's links): only the link goes.
fn remove_path(p: &std::path::Path) {
    let Ok(m) = std::fs::symlink_metadata(p) else {
        return;
    };
    let _ = if m.is_dir() {
        std::fs::remove_dir_all(p)
    } else if m.is_symlink() {
        // A directory symlink on Windows only goes with remove_dir.
        std::fs::remove_file(p).or_else(|_| std::fs::remove_dir(p))
    } else {
        std::fs::remove_file(p)
    };
}

/// Bytes under `p`; a symlink counts as its own size, not its target's.
fn path_size(p: &std::path::Path) -> u64 {
    match std::fs::symlink_metadata(p) {
        Ok(m) if m.is_dir() => stream::dir_size(p),
        Ok(m) => m.len(),
        Err(_) => 0,
    }
}

fn cache_info_now(app: &App) -> CacheInfo {
    let (video, subs) = app.stream.as_ref().map_or((0, 0), stream::Stream::usage);
    CacheInfo {
        video,
        images: app.images.usage(),
        subs: subs + stream::dir_size(&app.danmaku_dir),
        temp: temp_paths(app, &std::env::temp_dir())
            .iter()
            .map(|p| path_size(p))
            .sum(),
        playing: app.playing(),
    }
}

/// Empties `kind` and returns the usage afterwards. Blocking (walks directories).
fn cache_clear_now(app: &App, kind: Clear) -> CacheInfo {
    if let Some(s) = app.stream.as_ref() {
        if kind.covers(Clear::Video) {
            s.clear_video(app.playing());
        }
        if kind.covers(Clear::Subs) {
            s.clear_subs();
        }
    }
    if kind.covers(Clear::Subs) {
        remove_path(&app.danmaku_dir);
    }
    if kind.covers(Clear::Images) {
        app.images.clear();
    }
    if kind.covers(Clear::Temp) {
        for p in temp_paths(app, &std::env::temp_dir()) {
            remove_path(&p);
        }
    }
    log::info!("清理缓存：{kind:?}");
    cache_info_now(app)
}

#[tauri::command]
async fn cache_info(handle: AppHandle) -> CacheInfo {
    tauri::async_runtime::spawn_blocking(move || cache_info_now(&handle.state::<App>()))
        .await
        .unwrap_or_default()
}

/// Cache manager: clears one row (`all` = every row but the UI-side library cache).
#[tauri::command]
async fn cache_clear(handle: AppHandle, kind: Clear) -> CacheInfo {
    tauri::async_runtime::spawn_blocking(move || cache_clear_now(&handle.state::<App>(), kind))
        .await
        .unwrap_or_default()
}

/// `emby-img` scheme: hands the image to [`images::Images`] (disk cache → upstream). The path carries a server
/// id, mapped here to that server's API root; unknown servers get a 404 and nothing is fetched elsewhere.
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

/// Plays `item_id` in a new mpv, closing the current player; a later click while this one starts wins.
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
    app.stop_player(); // switching titles: the old mpv quits and reports on its own, no need to wait
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
    let (ev_tx, ev_rx) = mpsc::unbounded_channel();
    tauri::async_runtime::spawn(session.run(ctl_rx, ev_tx));
    let task = tauri::async_runtime::spawn(forward_events(handle, ev_rx));
    // Just in case another player got registered meanwhile: close it rather than leave an orphaned mpv
    let old = lock(&app.player).replace(Player { ctl: ctl_tx, task });
    if let Some(old) = old {
        app.retire(old);
    }
    Ok(())
}

/// Passes playback events to the UI and saves what belongs in the config. Ends only after `Ended` (the session
/// drops its sender then), so waiting on it at exit also waits for the reports and the saved volume.
async fn forward_events(
    handle: tauri::AppHandle,
    mut events: mpsc::UnboundedReceiver<player::Event>,
) {
    while let Some(ev) = events.recv().await {
        let app = handle.state::<App>();
        match ev {
            player::Event::Ended { volume: Some(v) } => {
                app.remember(|c| c.volume = Some(v));
            }
            // Danmaku panel changes only go to the config; the UI does not need them
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
}

fn main() {
    // Log panics, then hand them to the default hook
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
        .plugin(log_plugin(root.join("logs")))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .register_asynchronous_uri_scheme_protocol(images::SCHEME, |ctx, req, responder| {
            let handle = ctx.app_handle().clone();
            let path = req.uri().path().to_owned();
            tauri::async_runtime::spawn(async move {
                responder.respond(serve_image(&handle, &path).await);
            });
        })
        .setup(move |app| setup(app, &root))
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
        .run(move |handle, ev| on_run_event(handle, &ev, &exit_root));
}

/// The default 40 KB limit deletes the whole log and starts over, losing what troubleshooting needs:
/// 2 MB, keep the last 3 on rotation, local time.
fn log_plugin<R: tauri::Runtime>(dir: PathBuf) -> tauri::plugin::TauriPlugin<R> {
    tauri_plugin_log::Builder::new()
        .level(log::LevelFilter::Info)
        .max_file_size(2_000_000)
        .rotation_strategy(RotationStrategy::KeepSome(3))
        .timezone_strategy(TimezoneStrategy::UseLocal)
        .clear_targets()
        .targets([
            Target::new(TargetKind::Stdout),
            Target::new(TargetKind::Folder {
                path: dir,
                file_name: None,
            }),
        ])
        .build()
}

/// Startup: move old config, build the main window, load the config, start the caches and register [`App`].
fn setup(app: &mut tauri::App, root: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    let p = app.path();
    let old = [
        p.app_config_dir()?,
        p.app_local_data_dir()?,
        p.app_cache_dir()?,
    ];
    match config::migrate(root, &old[0], &old) {
        Ok(true) => log::info!("已把旧版本的配置搬到数据目录"),
        Ok(false) => {}
        Err(e) => log::warn!("搬旧版本的配置失败：{e}"),
    }
    // WebView2 data (including its image disk cache) defaults to %LOCALAPPDATA% and can only be moved when
    // the window is built, so the main window is built here
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
    let cfg =
        config::load(&path).map_err(|e| format!("读取配置失败（{}）：{e}", path.display()))?;
    config::save(&path, &cfg)?; // first start: persist the generated device_id
    if let Ok(v) = std::fs::read_to_string(root.join(RUNNING)) {
        log::warn!("上次没有正常退出（版本 {v}）");
    }
    set_running(root, true);
    let mpv_dir = app
        .path()
        .resource_dir()
        .ok()
        .and_then(|d| player::bundled_dir(&d));
    // The CI smoke test checks the installed paths with this line (no \\?\ prefix): rewording it means updating windows.yml
    log::info!(
        "EmbyMini {} 启动；数据目录：{}；自带 mpv 配置：{}",
        emby::VERSION,
        root.display(),
        mpv_dir
            .as_deref()
            .map_or("（无）".into(), |d| d.display().to_string())
    );
    spawn_cleanups(root.to_path_buf());
    let http = emby::http_client()?;
    let images = images::Images::new(root.join("cache").join("images"), images::CAP, http.clone());
    images.start();
    let stream = stream::Stream::start(&root.join("cache"), cfg.cache_whole);
    app.manage(App {
        path,
        cfg: Mutex::new(cfg),
        http,
        images,
        stream,
        danmaku_dir: root.join("cache").join("danmaku"),
        mpv_dir,
        mpv_cache: Some(root.join("mpv-cache")),
        player: Mutex::new(None),
        play_seq: AtomicU64::new(0),
        quit: watch::Sender::new(false),
        starting: AtomicUsize::new(0),
        closing: Mutex::new(Vec::new()),
    });
    Ok(())
}

/// Background deletion of leftovers from older versions and from the last update.
fn spawn_cleanups(root: PathBuf) {
    // shortcut: staging and markers left by the old diagnostics (staging up to ~20 MB); drop once most users have upgraded a few versions
    tauri::async_runtime::spawn_blocking(move || {
        for f in ["telemetry", "last-exit.json", "running.json"] {
            remove_path(&root.join(f));
        }
    });
    // The installer may still be running right after an update: wait before deleting its leftovers.
    tauri::async_runtime::spawn(async {
        tokio::time::sleep(Duration::from_secs(60)).await;
        let _ = tauri::async_runtime::spawn_blocking(|| {
            for p in temp_entries(&std::env::temp_dir(), is_updater_leftover) {
                remove_path(&p);
            }
        })
        .await;
    });
}

/// Close: hide the window and tell mpv to quit at once, so the user sees it close right away; the slow parts
/// (window teardown, waiting for the reports) happen after. Each phase is logged with [`exit_mark`].
fn on_run_event(handle: &AppHandle, ev: &tauri::RunEvent, root: &std::path::Path) {
    match ev {
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
            set_running(root, false);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A cancelled exit (installer failed to start) restarts the close timer instead of counting from that attempt.
    #[test]
    fn cancelled_exit_restarts_the_timer() {
        exit_start();
        std::thread::sleep(Duration::from_millis(200));
        exit_cancel();
        exit_start();
        assert!(exit_elapsed().unwrap() < Duration::from_millis(200));
    }

    #[test]
    fn clean_path_strips_quotes() {
        assert_eq!(
            clean_path(r#" "C:\Program Files\mpv\mpv.exe" "#),
            r"C:\Program Files\mpv\mpv.exe"
        );
        assert_eq!(clean_path("  "), "");
    }

    /// mpv to run: the path from settings (quotes stripped) wins, then the bundled mpv if it is there, then PATH.
    /// The personal mpv.conf sits next to config.json.
    #[test]
    fn mpv_path_order() {
        let exe = if cfg!(windows) { "mpv.exe" } else { "mpv" };
        let dir = std::env::temp_dir().join(format!("embymini-mpv-path-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut app = test_app(None);
        app.mpv_dir = Some(dir.clone());
        assert_eq!(app.mpv_path(), PathBuf::from(exe));
        std::fs::write(dir.join(exe), "").unwrap();
        assert_eq!(app.mpv_path(), dir.join(exe));
        lock(&app.cfg).mpv_path = Some(r#" "D:\mpv\mpv.exe" "#.into());
        assert_eq!(app.mpv_path(), PathBuf::from(r"D:\mpv\mpv.exe"));
        lock(&app.cfg).mpv_path = Some("  ".into());
        assert_eq!(app.mpv_path(), dir.join(exe));
        std::fs::remove_dir_all(&dir).unwrap();
        app.path = dir.join("config.json");
        assert_eq!(app.user_mpv_conf(), dir.join("mpv.conf"));
    }

    /// The updater names its temp dir after productName: renaming the product must update the matcher.
    #[test]
    fn updater_leftovers_match_product_name() {
        let conf: Value = serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        let name = conf["productName"].as_str().unwrap();
        assert!(is_updater_leftover(&format!("{name}-0.5.0-updater-a1B2c3")));
        for other in [
            "EmbyMini-0.5.0-x",
            "Other-0.5.0-updater-x",
            "embymini-0.5.0-updater-x",
        ] {
            assert!(!is_updater_leftover(other), "{other}");
        }
    }

    #[test]
    fn clear_all_covers_every_kind() {
        for k in [Clear::Video, Clear::Images, Clear::Subs, Clear::Temp] {
            assert!(Clear::All.covers(k) && k.covers(k));
            assert!(!k.covers(Clear::All));
        }
        assert!(!Clear::Subs.covers(Clear::Images));
        let k: Clear = serde_json::from_str(r#""subs""#).unwrap();
        assert_eq!(k, Clear::Subs);
        assert!(serde_json::from_str::<Clear>(r#""snap""#).is_err());
    }

    /// Each kind empties only its own row; danmaku counts under subtitles.
    #[test]
    fn cache_clear_empties_only_its_kind() {
        let dir = std::env::temp_dir().join(format!("embymini-cache-clear-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (img, dm) = (dir.join("images"), dir.join("danmaku"));
        std::fs::create_dir_all(&img).unwrap();
        std::fs::create_dir_all(&dm).unwrap();
        std::fs::write(img.join("a"), "img").unwrap();
        std::fs::write(dm.join("b.json"), "12345").unwrap();
        let mut app = test_app(None);
        app.images = images::Images::new(img.clone(), images::CAP, reqwest::Client::new());
        app.danmaku_dir = dm.clone();

        assert_eq!(cache_info_now(&app).subs, 5);
        let info = cache_clear_now(&app, Clear::Images);
        assert!(!img.join("a").exists());
        assert!(dm.join("b.json").exists());
        assert_eq!(info.subs, 5);
        std::fs::write(img.join("a"), "img").unwrap();
        let info = cache_clear_now(&app, Clear::Subs);
        assert!(!dm.exists());
        assert!(img.join("a").exists());
        assert_eq!(info.subs, 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Temp cache = mpv's cache dir + updater leftovers + thumbfast images; other temp entries are left alone.
    #[test]
    fn temp_paths_pick_our_leftovers() {
        let tmp = std::env::temp_dir().join(format!("embymini-temp-paths-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let upd = tmp.join("EmbyMini-0.0.0-updater-abc");
        let thumb = tmp.join("thumbfast.out.tmp");
        std::fs::create_dir_all(&upd).unwrap();
        std::fs::write(&thumb, "").unwrap();
        std::fs::write(tmp.join("other"), "").unwrap();
        let mut app = test_app(None);
        app.mpv_cache = Some(PathBuf::from("mpv-cache"));
        let mut v = temp_paths(&app, &tmp);
        std::fs::remove_dir_all(&tmp).unwrap();
        v[1..].sort();
        assert_eq!(v, [PathBuf::from("mpv-cache"), upd, thumb]);
    }

    /// A symlink in the shared temp dir is neither followed when sized nor when deleted.
    #[cfg(unix)]
    #[test]
    fn temp_symlinks_are_not_followed() {
        let dir = std::env::temp_dir().join(format!("embymini-temp-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let target = dir.join("target");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("big"), vec![0u8; 5000]).unwrap();
        let link = dir.join("thumbfast.out");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(path_size(&link) < 5000);
        assert_eq!(path_size(&target), 5000);
        remove_path(&link);
        assert!(!link.exists() && target.join("big").exists());
        remove_path(&target);
        assert!(!target.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn playing_while_starting() {
        let app = test_app(None);
        assert!(!app.playing());
        let _s = Starting::enter(&app.starting);
        assert!(app.playing());
    }

    fn test_app(player: Option<Player>) -> App {
        App {
            path: PathBuf::new(),
            cfg: Mutex::new(Config::default()),
            http: reqwest::Client::new(),
            mpv_dir: None,
            mpv_cache: None,
            player: Mutex::new(player),
            // these tests fetch no images: an empty path creates no dirs and leaves nothing in temp
            images: images::Images::new(PathBuf::new(), images::CAP, reqwest::Client::new()),
            stream: None,
            danmaku_dir: PathBuf::new(),
            play_seq: AtomicU64::new(0),
            quit: watch::Sender::new(false),
            starting: AtomicUsize::new(0),
            closing: Mutex::new(Vec::new()),
        }
    }

    /// Exit while starting: the start is dropped (killing mpv) and shutdown waits only until play returns.
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
        // already exiting: a new play gives up at once
        let late = tauri::async_runtime::block_on(app.unless_quit(async { 1 }));
        assert_eq!(late, None);
    }

    /// Exit waits for the player to finish reporting Stopped, but not forever.
    #[test]
    fn shutdown_waits_for_player_then_gives_up() {
        let app = |task, ctl| test_app(Some(Player { ctl, task }));
        // finishes 200 ms after Quit: shutdown waits for it
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

        // never finishes: wait at most `wait`
        let (tx, _rx) = mpsc::unbounded_channel();
        let task = tauri::async_runtime::spawn(std::future::pending::<()>());
        let t0 = std::time::Instant::now();
        app(task, tx).shutdown(Duration::from_millis(300));
        assert!(t0.elapsed() < Duration::from_secs(2));
    }

    /// Switching titles then quitting at once: the previous title's report is waited for too, not dropped.
    #[test]
    fn shutdown_waits_for_the_switched_away_player() {
        let quit_then = |delay| {
            let (tx, mut rx) = mpsc::unbounded_channel();
            let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let d = done.clone();
            let task = tauri::async_runtime::spawn(async move {
                assert!(matches!(rx.recv().await, Some(player::Ctl::Quit)));
                tokio::time::sleep(delay).await;
                d.store(true, Ordering::SeqCst);
            });
            (Player { ctl: tx, task }, done)
        };
        let (old, old_done) = quit_then(Duration::from_millis(300));
        let app = test_app(Some(old));
        let _ = app.stop_player(); // what `play` does on a new click
        let (new, _) = quit_then(Duration::ZERO);
        *lock(&app.player) = Some(new);
        app.shutdown(Duration::from_secs(3));
        assert!(old_done.load(Ordering::SeqCst));
    }

    /// Finished tasks are pruned on the next switch, so the list stays as long as the players still reporting.
    #[test]
    fn retire_prunes_finished_tasks() {
        let app = test_app(None);
        let player = |task| Player {
            ctl: mpsc::unbounded_channel().0,
            task,
        };
        app.retire(player(tauri::async_runtime::spawn(async {})));
        while !lock(&app.closing)[0].inner().is_finished() {
            std::thread::yield_now();
        }
        app.retire(player(tauri::async_runtime::spawn(std::future::pending())));
        assert_eq!(lock(&app.closing).len(), 1);
        assert!(!lock(&app.closing)[0].inner().is_finished());
    }
}
