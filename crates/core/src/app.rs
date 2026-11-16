//! The application state behind the UI: config and servers, the caches, playback and the exit sequence.
//!
//! The UI keeps one [`App`] for the life of the process and forwards each user action to a method here; nothing in
//! this module knows about the UI framework.

use crate::config::{self, Config, Server, ServerView};
use crate::emby::{self, Emby, Error};
use crate::{danmaku, images, lock, player, rt, stream, updater};
use reqwest::Method;
use serde::Serialize;
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};

/// When the user closed the window (or the process began exiting): each close phase logs its time since, to find
/// where "closing hangs for a moment" is spent. Only the UI's close handling starts it, never [`App::shutdown`], so
/// a shutdown that does not end in exit (an update whose installer fails to start) leaves no stale start behind.
static CLOSE_T0: OnceLock<Instant> = OnceLock::new();

/// Starts the timer; later calls keep the first start.
pub fn exit_start() {
    CLOSE_T0.get_or_init(Instant::now);
}

/// Logs a close phase; does nothing when not closing (e.g. mpv closed during normal use or for an update).
pub fn exit_mark(phase: &str) {
    if let Some(t0) = CLOSE_T0.get() {
        log::info!("关闭耗时 {phase}：{} ms", t0.elapsed().as_millis());
    }
}

/// Running marker (`running` in the data dir, holding the version): written by [`App::open`], removed by
/// [`App::shutdown`] (a clean exit, auto-update included). Still there at start = the last run crashed or was killed.
const RUNNING: &str = "running";

pub fn set_running(root: &std::path::Path, on: bool) {
    let f = root.join(RUNNING);
    let _ = if on {
        std::fs::write(f, emby::VERSION)
    } else {
        std::fs::remove_file(f)
    };
}

/// The async methods use tokio (reqwest, spawned tasks): await them on [`rt`], e.g.
/// `rt::spawn(async move { app.call(..).await })`, not on the UI's own executor.
pub struct App {
    path: PathBuf,
    cfg: Mutex<Config>,
    http: reqwest::Client,
    /// Bundled mpv config in the resource dir (ModernZ etc.); also holds mpv.exe when the package ships one.
    mpv_dir: Option<PathBuf>,
    /// mpv's cache dir, `mpv-cache` under the data dir, so uninstalling with "delete app data" removes it.
    mpv_cache: Option<PathBuf>,
    player: Mutex<Option<Player>>,
    /// Disk cache for posters and backdrops.
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
    closing: Mutex<Vec<tokio::task::JoinHandle<()>>>,
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
    task: tokio::task::JoinHandle<()>,
}

/// The mpv path from settings without surrounding blanks and quotes (Explorer's "Copy as path" adds them).
fn clean_path(p: &str) -> &str {
    p.trim().trim_matches('"').trim()
}

impl App {
    pub fn cfg(&self) -> MutexGuard<'_, Config> {
        lock(&self.cfg)
    }

    /// A player is up or a `play` is still starting: the video cache keeps this playback's files.
    fn playing(&self) -> bool {
        lock(&self.player).is_some() || self.starting.load(Ordering::SeqCst) > 0
    }

    /// Tells the running mpv to quit; false when there is none.
    fn stop_player(&self) -> bool {
        let Some(p) = lock(&self.player).take() else {
            return false;
        };
        self.retire(p);
        true
    }

    /// Saves the volume a player ended with, unless a newer `play` was clicked since `seq`: exit also waits for
    /// the switched-away player, whose late `Ended` must not overwrite the current title's volume.
    fn save_volume(&self, seq: u64, volume: f64) {
        if self.play_seq.load(Ordering::SeqCst) == seq {
            self.remember(|c| c.volume = Some(volume));
        }
    }

    /// Sends Quit and keeps the task: it still reports Stopped, and [`App::shutdown`] waits for it.
    fn retire(&self, p: Player) {
        let _ = p.ctl.send(player::Ctl::Quit);
        let mut closing = lock(&self.closing);
        closing.retain(|t| !t.is_finished());
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

    /// First exit step, run as soon as close is clicked (before window teardown): stops
    /// starting `play` calls and tells mpv to quit; [`App::shutdown`] waits for the reports. Repeatable.
    pub fn begin_shutdown(&self) {
        self.quit.send_replace(true);
        if self.stop_player() {
            log::info!("程序退出，先关闭播放器并回传进度");
        }
    }

    /// Call before exiting: mpv is a separate process that outlives us, while the report tasks die with us and the
    /// resume point is lost. So stop starting `play` calls and wait for them (or one may register a player right
    /// after), then quit mpv and wait for the Stopped reports. Waits `wait` at most in total. Ends by removing the
    /// running marker: this is the clean exit ([`App::cancel_shutdown`] puts it back).
    pub fn shutdown(&self, wait: Duration) {
        self.begin_shutdown();
        rt::block_on(async {
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
        self.mark_running(false);
        exit_mark("shutdown_done");
    }

    /// Writes or removes the running marker in the data dir (nothing for an App without one, as in tests).
    fn mark_running(&self, on: bool) {
        if let Some(root) = self.path.parent() {
            set_running(root, on);
        }
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

    /// Opens the data dir `root`: loads (and on first start writes) the config, checks and sets the running marker,
    /// starts the image cache and the caching proxy. `mpv_dir` is the bundled mpv config dir, if any.
    pub fn open(root: &Path, mpv_dir: Option<PathBuf>) -> Result<Self, Box<dyn std::error::Error>> {
        let path = root.join("config.json");
        let cfg =
            config::load(&path).map_err(|e| format!("读取配置失败（{}）：{e}", path.display()))?;
        config::save(&path, &cfg)?; // first start: persist the generated device_id
        if let Ok(v) = std::fs::read_to_string(root.join(RUNNING)) {
            log::warn!("上次没有正常退出（版本 {v}）");
        }
        set_running(root, true);
        // Where things are, for troubleshooting from a log (the mpv dir must have no \\?\ prefix, see player::bundled_dir)
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
        let images =
            images::Images::new(root.join("cache").join("images"), images::CAP, http.clone());
        images.start();
        let stream = stream::Stream::start(&root.join("cache"), cfg.cache_whole);
        Ok(Self {
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
        })
    }

    /// The data dir (holds config.json, the caches and the running marker).
    pub fn data_root(&self) -> &Path {
        self.path.parent().unwrap_or(Path::new("."))
    }

    /// Undoes [`App::begin_shutdown`] / [`App::shutdown`] when the program stays in use after all (an update that
    /// failed to start): otherwise every later play is dropped as "quitting" and a crash would go unnoticed.
    pub fn cancel_shutdown(&self) {
        self.quit.send_replace(false);
        self.mark_running(true);
    }

    /// Settings page contents.
    pub fn config_view(&self) -> ConfigView {
        let c = self.cfg();
        ConfigView {
            active: c.active.clone(),
            servers: c.servers.iter().map(Server::view).collect(),
            mpv_path: c.mpv_path.clone(),
            preload_percent: c.preload_percent,
            user_mpv_conf: self.user_mpv_conf().display().to_string(),
            update_proxy: c.update_proxy.clone(),
            cache_whole: c.cache_whole,
            danmaku_api: c.danmaku.api.clone(),
            danmaku_words: c.danmaku.words.clone(),
            version: emby::VERSION,
        }
    }

    /// Adds a server and makes it the active one: reads its name, logs in, saves.
    pub async fn server_add(
        &self,
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
        s.name = emby::server_name(&self.http, &s.api_base()).await?;
        self.login(&mut s, &password).await?;
        let view = s.view();
        self.update(|c| {
            c.active = Some(s.id.clone());
            c.servers.push(s);
        })?;
        Ok(view)
    }

    /// Edits a server (same id; stays active if it was): address, port, user, or a fresh login.
    /// Connects to the new address first (refreshing the server name) and saves nothing if that fails.
    /// An address-only change (new domain, port, reverse proxy) keeps the token, no password needed;
    /// a changed user, a typed password or a missing token logs in again with the given credentials.
    pub async fn server_edit(
        &self,
        id: String,
        url: String,
        port: Option<u16>,
        user: String,
        password: String,
    ) -> Result<ServerView, Error> {
        let (url, port) = config::normalize(&url, port).map_err(Error::Other)?;
        let mut s = self.server(&id)?;
        let relogin = !password.is_empty() || user != s.user || s.token.is_empty();
        (s.url, s.port, s.user) = (url, port, user);
        s.name = emby::server_name(&self.http, &s.api_base()).await?;
        if relogin {
            self.login(&mut s, &password).await?;
        }
        self.put_server(s)
    }

    /// Logs in again with just the password: the token expired or a new one is wanted.
    pub async fn server_login(&self, id: String, password: String) -> Result<ServerView, Error> {
        let mut s = self.server(&id)?;
        self.login(&mut s, &password).await?;
        // Only the login fields: an edit saved while logging in keeps its address.
        self.update(|c| {
            let t = c.servers.iter_mut().find(|t| t.id == id)?;
            (t.user_id, t.token) = (s.user_id, s.token);
            Some(t.view())
        })?
        .ok_or_else(no_server)
    }

    pub fn server_remove(&self, id: String) -> Result<(), Error> {
        self.update(|c| {
            c.servers.retain(|s| s.id != id);
            if c.active.as_deref() == Some(id.as_str()) {
                c.active = c.servers.first().map(|s| s.id.clone());
            }
        })
    }

    pub fn server_use(&self, id: String) -> Result<(), Error> {
        self.update(|c| {
            if c.servers.iter().any(|s| s.id == id) {
                c.active = Some(id);
            }
        })
    }

    /// The general settings: mpv path, preload point, update proxy (validated), whole-file caching.
    pub fn settings_set(
        &self,
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
        self.update(|c| {
            c.mpv_path = mpv_path
                .map(|p| clean_path(&p).to_owned())
                .filter(|p| !p.is_empty());
            c.preload_percent = preload_percent.clamp(50, 99);
            c.update_proxy = update_proxy;
            c.cache_whole = cache_whole;
        })?;
        if let Some(s) = &self.stream {
            s.set_whole(cache_whole);
        }
        Ok(())
    }

    /// The danmaku settings page: API address (`/api/v2` style, may carry a token) and blocked words (one per line,
    /// blanks dropped); applied to the running playback at once. Display settings live in the player's panel.
    pub fn danmaku_set(&self, api: String, words: Vec<String>) -> Result<(), Error> {
        let api = danmaku::normalize(&api).map_err(Error::Other)?;
        let words: Vec<String> = words
            .iter()
            .map(|w| w.trim())
            .filter(|w| !w.is_empty())
            .map(str::to_owned)
            .collect();
        if let Some(p) = lock(&self.player).as_ref() {
            let _ = p.ctl.send(player::Ctl::Danmaku {
                api: danmaku::Api::new(self.http.clone(), &api),
                words: words.clone(),
            });
        }
        self.update(|c| {
            c.danmaku.api = api;
            c.danmaku.words = words;
        })
    }

    /// Cache manager rows (walks directories, off the async threads).
    pub async fn cache_info(self: Arc<Self>) -> CacheInfo {
        rt::spawn_blocking(move || cache_info_now(&self))
            .await
            .unwrap_or_default()
    }

    /// Cache manager: clears one row (`all` = every row but the UI-side library cache).
    pub async fn cache_clear(self: Arc<Self>, kind: Clear) -> CacheInfo {
        rt::spawn_blocking(move || cache_clear_now(&self, kind))
            .await
            .unwrap_or_default()
    }

    /// A poster or backdrop by [`images::Req`] path, from [`images::Images`] (disk cache → upstream). The path carries a
    /// server id, mapped here to that server's API root; unknown servers get a 404 and nothing is fetched elsewhere.
    pub async fn image(&self, path: &str) -> images::Got {
        let Some(req) = images::Req::parse(path) else {
            return Err(400);
        };
        let base = {
            let c = self.cfg();
            c.servers
                .iter()
                .find(|s| s.id == req.server)
                .map(config::Server::api_base)
        };
        let Some(base) = base else {
            return Err(404);
        };
        self.images.get(&req, &base).await
    }

    /// Generic Emby request to the active server. On 401 that server's token is cleared and the UI asks to sign in.
    pub async fn call(
        &self,
        method: &str,
        path: &str,
        query: Option<Map<String, Value>>,
        body: Option<Value>,
    ) -> Result<Value, Error> {
        let (server_id, client) = self.active()?;
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
        let r = client.call(method, path, &query, body.as_ref()).await;
        if matches!(r, Err(Error::Unauthorized)) {
            // Clear the server that was asked: the user may have switched servers meanwhile
            self.remember(|c| {
                if let Some(s) = c.servers.iter_mut().find(|s| s.id == server_id) {
                    s.token.clear();
                }
            });
        }
        r
    }

    /// Plays `item_id` in a new mpv, closing the current player; a later click while this one starts wins.
    /// Playback events go to `emit` (the UI), except the ones that only update the config.
    pub async fn play(
        self: &Arc<Self>,
        item_id: String,
        source_id: Option<String>,
        from_start: bool,
        emit: impl Fn(&player::Event) + Send + 'static,
    ) -> Result<(), Error> {
        let _starting = Starting::enter(&self.starting);
        let (_, client) = self.active()?;
        let seq = self.play_seq.fetch_add(1, Ordering::SeqCst) + 1;
        self.stop_player(); // switching titles: the old mpv quits and reports on its own; only exit waits for it
        let (preload_percent, volume, dm) = {
            let c = self.cfg();
            (f64::from(c.preload_percent), c.volume, c.danmaku.clone())
        };
        let opts = player::Options {
            mpv: self.mpv_path(),
            config_dir: self.mpv_dir.clone(),
            preload_percent,
            volume,
            cache_dir: self.mpv_cache.clone(),
            user_conf: Some(self.user_mpv_conf()),
            stream: self.stream.clone(),
            danmaku: Some(danmaku::Ctx {
                api: danmaku::Api::new(self.http.clone(), &dm.api),
                dir: self.danmaku_dir.clone(),
                cfg: dm,
            }),
            extra_args: Vec::new(),
        };
        let Some(session) = self
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
        if *self.quit.borrow() {
            session.abort().await;
            return Ok(());
        }
        if self.play_seq.load(Ordering::SeqCst) != seq {
            log::info!("起播期间又点了别的，作废这一次：{item_id}");
            session.abort().await;
            return Ok(());
        }
        let (ctl_tx, ctl_rx) = mpsc::unbounded_channel();
        let (ev_tx, ev_rx) = mpsc::unbounded_channel();
        rt::spawn(session.run(ctl_rx, ev_tx));
        let task = rt::spawn(forward_events(self.clone(), seq, ev_rx, emit));
        // Just in case another player got registered meanwhile: close it rather than leave an orphaned mpv
        let old = lock(&self.player).replace(Player { ctl: ctl_tx, task });
        if let Some(old) = old {
            self.retire(old);
        }
        Ok(())
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigView {
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

fn no_server() -> Error {
    Error::Other("服务器不存在".into())
}

/// Bytes per cache manager row; `playing` means the film being played stays in the video cache. A UI-side library
/// cache (list snapshots), if any, is the UI's to count.
#[derive(Serialize, Default)]
pub struct CacheInfo {
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
pub enum Clear {
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

/// 0.5.x's updater (the Tauri plugin) temp dir, `<productName>-<version>-updater-<random>`: the plugin keeps it
/// forever (`tempdir().keep()`, ~40 MB per update) and it is useless once the new version is installed.
pub fn is_updater_leftover(name: &str) -> bool {
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

/// Passes playback events to the UI and saves what belongs in the config. Ends only after `Ended` (the session
/// drops its sender then), so waiting on it at exit also waits for the reports and the saved volume.
async fn forward_events(
    app: Arc<App>,
    seq: u64,
    mut events: mpsc::UnboundedReceiver<player::Event>,
    emit: impl Fn(&player::Event),
) {
    while let Some(ev) = events.recv().await {
        match ev {
            player::Event::Ended { volume: Some(v) } => app.save_volume(seq, v),
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
        emit(&ev);
    }
}

/// Background deletion of leftovers from older versions and from the last update.
fn spawn_cleanups(root: PathBuf) {
    // shortcut: staging and markers left by the old diagnostics (staging up to ~20 MB); drop once most users have upgraded a few versions
    rt::spawn_blocking(move || {
        for f in ["telemetry", "last-exit.json", "running.json"] {
            remove_path(&root.join(f));
        }
    });
    // The installer may still be running right after an update: wait before deleting its leftovers.
    rt::spawn(async {
        tokio::time::sleep(Duration::from_secs(60)).await;
        let _ = rt::spawn_blocking(|| {
            for p in temp_entries(&std::env::temp_dir(), is_updater_leftover) {
                remove_path(&p);
            }
        })
        .await;
    });
}
#[cfg(test)]
mod tests {
    use super::*;

    /// A shutdown that does not end in exit (an update whose installer fails) must not start the close timer.
    /// (No test calls `exit_start`, so the global stays unset.)
    #[test]
    fn shutdown_does_not_start_the_close_timer() {
        test_app(None).shutdown(Duration::ZERO);
        assert!(CLOSE_T0.get().is_none());
    }

    /// Only the newest play's volume is saved; a switched-away player ending late is ignored.
    #[test]
    fn only_the_newest_play_saves_its_volume() {
        let app = test_app(None);
        app.play_seq.store(2, Ordering::SeqCst);
        app.save_volume(1, 30.0);
        assert_eq!(app.cfg().volume, None);
        app.save_volume(2, 40.0);
        assert_eq!(app.cfg().volume, Some(40.0));
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

    /// The old updater named its temp dir after the product name ("EmbyMini"); only those dirs match.
    #[test]
    fn updater_leftovers_match_product_name() {
        assert!(is_updater_leftover("EmbyMini-0.5.0-updater-a1B2c3"));
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
        let task = rt::spawn(async move {
            let _g = Starting::enter(&a.starting);
            a.unless_quit(std::future::pending::<()>()).await
        });
        while app.starting.load(Ordering::SeqCst) == 0 {
            std::thread::yield_now();
        }
        assert_eq!(rt::block_on(app.unless_quit(async { 1 })), Some(1));
        let t0 = std::time::Instant::now();
        app.shutdown(Duration::from_secs(3));
        assert!(t0.elapsed() < Duration::from_secs(1));
        assert_eq!(app.starting.load(Ordering::SeqCst), 0);
        assert!(rt::block_on(task).unwrap().is_none());
        // already exiting: a new play gives up at once
        let late = rt::block_on(app.unless_quit(async { 1 }));
        assert_eq!(late, None);
    }

    /// A `play` that registers its player while shutdown waits for it: shutdown quits that player too and
    /// waits for its report, instead of leaving a running mpv behind.
    #[test]
    fn shutdown_quits_a_player_registered_while_waiting() {
        let app = std::sync::Arc::new(test_app(None));
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (a, d) = (app.clone(), done.clone());
        let _play = rt::spawn(async move {
            let _g = Starting::enter(&a.starting);
            tokio::time::sleep(Duration::from_millis(100)).await;
            let (ctl, mut rx) = mpsc::unbounded_channel();
            let task = rt::spawn(async move {
                assert!(matches!(rx.recv().await, Some(player::Ctl::Quit)));
                d.store(true, Ordering::SeqCst);
            });
            *lock(&a.player) = Some(Player { ctl, task });
        });
        while app.starting.load(Ordering::SeqCst) == 0 {
            std::thread::yield_now();
        }
        app.shutdown(Duration::from_secs(3));
        assert!(done.load(Ordering::SeqCst));
        assert!(lock(&app.player).is_none());
    }

    /// shutdown is the clean exit and removes the marker; cancel_shutdown (the program stays open) puts it back.
    #[test]
    fn shutdown_clears_the_running_marker_and_cancel_restores_it() {
        let dir = std::env::temp_dir().join(format!("embymini-marker-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let mut app = test_app(None);
        app.path = dir.join("config.json");
        set_running(&dir, true);
        app.shutdown(Duration::ZERO);
        assert!(!dir.join(RUNNING).exists(), "shutdown removes the marker");
        app.cancel_shutdown();
        assert!(
            dir.join(RUNNING).exists(),
            "cancel_shutdown writes it again"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn running_marker_holds_the_version() {
        let dir = std::env::temp_dir().join(format!("embymini-running-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        set_running(&dir, true);
        assert_eq!(
            std::fs::read_to_string(dir.join(RUNNING)).unwrap(),
            emby::VERSION
        );
        set_running(&dir, false);
        assert!(!dir.join(RUNNING).exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Exit waits for the player to finish reporting Stopped, but not forever.
    #[test]
    fn shutdown_waits_for_player_then_gives_up() {
        let app = |task, ctl| test_app(Some(Player { ctl, task }));
        // finishes 200 ms after Quit: shutdown waits for it
        let (tx, mut rx) = mpsc::unbounded_channel();
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let d = done.clone();
        let task = rt::spawn(async move {
            assert!(matches!(rx.recv().await, Some(player::Ctl::Quit)));
            tokio::time::sleep(Duration::from_millis(200)).await;
            d.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        app(task, tx).shutdown(Duration::from_secs(3));
        assert!(done.load(std::sync::atomic::Ordering::SeqCst));

        // never finishes: wait at most `wait`
        let (tx, _rx) = mpsc::unbounded_channel();
        let task = rt::spawn(std::future::pending::<()>());
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
            let task = rt::spawn(async move {
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
        app.retire(player(rt::spawn(async {})));
        while !lock(&app.closing)[0].is_finished() {
            std::thread::yield_now();
        }
        app.retire(player(rt::spawn(std::future::pending())));
        assert_eq!(lock(&app.closing).len(), 1);
        assert!(!lock(&app.closing)[0].is_finished());
    }
}
