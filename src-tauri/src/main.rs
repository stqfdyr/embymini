// release 下不弹控制台窗口，不要删。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
mod emby;
mod player;

use config::{Config, Server, ServerView};
use emby::{Emby, Error};
use reqwest::Method;
use serde::Serialize;
use serde_json::{Map, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;
use tauri::{Emitter, Manager, State};
use tokio::sync::mpsc;

struct App {
    path: PathBuf,
    cfg: Mutex<Config>,
    http: reqwest::Client,
    /// 资源目录里的 mpv 自带配置（ModernZ 等）；打包时若附带了 mpv.exe 也在这里。
    mpv_dir: Option<PathBuf>,
    /// mpv 的缓存目录：应用自己的缓存目录下（Windows 上是 `%LOCALAPPDATA%\com.stqfdyr.embymini\mpv`），
    /// 卸载时勾选「删除应用程序数据」会一起删掉。
    mpv_cache: Option<PathBuf>,
    player: Mutex<Option<Player>>,
    /// 每次点播放加一；起播要等几秒，完成时若已不是最新一次点击，就作废自己。
    play_seq: AtomicU64,
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

    /// 程序退出前调用：mpv 是独立进程，不会跟着 EmbyMini 退出，而回传进度的任务会随进程消失，
    /// 续播点就丢了。所以先让 mpv 退出、回传 Stopped，最多等 `wait`。
    fn shutdown(&self, wait: Duration) {
        if let Some(task) = self.stop_player() {
            log::info!("程序退出，先关闭播放器并回传进度");
            tauri::async_runtime::block_on(async {
                if tokio::time::timeout(wait, task).await.is_err() {
                    log::warn!("关闭播放器超时（{wait:?}），进度可能没回传");
                }
            });
        }
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

    /// 个人 mpv 配置文件：和 config.json 同目录（Windows 上是 `%APPDATA%\com.stqfdyr.embymini\mpv.conf`），
    /// 升级不覆盖。由用户自己创建，存在才加载。
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
        (s.ok_or_else(|| Error::Other("服务器不存在".into()))?, c.device_id.clone())
    };
    let relogin = !password.is_empty() || user != s.user || s.token.is_empty();
    (s.url, s.port, s.user) = (url, port, user);
    let base = s.api_base();
    s.name = emby::server_name(&app.http, &base).await?;
    if relogin {
        (s.user_id, s.token) = emby::login(&app.http, &base, &device_id, &s.user, &password).await?;
    }
    let view = s.view();
    app.update(|c| {
        let t = c.servers.iter_mut().find(|t| t.id == id)?;
        *t = s;
        Some(())
    })?
    .ok_or_else(|| Error::Other("服务器不存在".into()))?;
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
) -> Result<(), Error> {
    app.update(|c| {
        c.mpv_path = mpv_path
            .map(|p| clean_path(&p).to_owned())
            .filter(|p| !p.is_empty());
        c.preload_percent = preload_percent.clamp(50, 99);
    })
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
    let session = player::start(client, &item_id, from_start, opts).await?;
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
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _, _| {
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.unminimize();
                let _ = w.set_focus();
            }
        }))
        .plugin(
            tauri_plugin_log::Builder::new()
                .level(log::LevelFilter::Info)
                .build(),
        )
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let path = app.path().app_config_dir()?.join("config.json");
            let cfg = config::load(&path);
            config::save(&path, &cfg)?; // 首次启动把生成的 device_id 落盘
            let mpv_dir = app
                .path()
                .resource_dir()
                .ok()
                .and_then(|d| player::bundled_dir(&d));
            // CI 冒烟测试按这一行判断安装后的路径是否正确（不能带 \\?\ 前缀），改措辞要同步改 windows.yml
            log::info!(
                "EmbyMini {} 启动；自带 mpv 配置：{}",
                emby::VERSION,
                mpv_dir
                    .as_deref()
                    .map_or("（无）".into(), |d| d.display().to_string())
            );
            let mpv_cache = app.path().app_cache_dir().ok().map(|d| d.join("mpv"));
            app.manage(App {
                path,
                cfg: Mutex::new(cfg),
                http: emby::http_client()?,
                mpv_dir,
                mpv_cache,
                player: Mutex::new(None),
                play_seq: AtomicU64::new(0),
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
            play
        ])
        .build(tauri::generate_context!())
        .unwrap_or_else(|e| panic!("EmbyMini 启动失败：{e}"))
        .run(|handle, ev| {
            if let tauri::RunEvent::Exit = ev {
                handle.state::<App>().shutdown(Duration::from_secs(3));
            }
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

    /// 退出时要等播放任务收尾（回传 Stopped），但不能无限等。
    #[test]
    fn shutdown_waits_for_player_then_gives_up() {
        let app = |task, ctl| App {
            path: PathBuf::new(),
            cfg: Mutex::new(Config::default()),
            http: reqwest::Client::new(),
            mpv_dir: None,
            mpv_cache: None,
            player: Mutex::new(Some(Player { ctl, task })),
            play_seq: AtomicU64::new(0),
        };
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
