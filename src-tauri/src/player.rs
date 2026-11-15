//! mpv 外部进程 + JSON IPC：同季连播、按进度预加载下一集、外挂字幕与默认轨、Emby 进度回传。
//!
//! 一次播放 = 一个 [`Session`]：启动 mpv 的同时建队列、准备首集，连上 IPC 后由 [`Session::run`]
//! 在一个循环里处理 mpv 事件。网络请求不进这个循环：进度回传交给回传任务（[`spawn_reporter`]），
//! 下一集的 PlaybackInfo 在后台取（[`State::prepare_later`]），上游再慢，按键和退出也照常响应。
//! 队列是整季，自然往后播靠「到预加载进度才追加下一集」，手动上一集 / 下一集 / 选集由 EmbyMini 处理
//! （见 [`State::jump`]）。换集时沿用上一集选的音轨、字幕。
//! 视频和字幕经本地缓存代理（[`crate::stream`]）读：交给 mpv 时才登记（[`State::load`]），换集时只留当前集和下一集的缓存。

use crate::danmaku::{self, Fail as DmFail};
use crate::emby::{self, Emby, Error, Item, MediaStream, NO_QUERY};
use crate::stream::Stream;
use reqwest::Method;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, WriteHalf};
use tokio::process::Child;
use tokio::sync::mpsc;

const TICKS_PER_SEC: f64 = 10_000_000.0;
const PROGRESS_EVERY: Duration = Duration::from_secs(10);
/// mpv 里「上一集 / 下一集」按钮和按键发出的消息（见 mpv/input.conf、mpv/script-opts/modernz.conf）。
const NEXT_MSG: &str = "embymini-next";
const PREV_MSG: &str = "embymini-prev";
/// 选集菜单（mpv/scripts/embymini.lua）选中某一集，参数是队列下标。
const JUMP_MSG: &str = "embymini-jump";
/// 发给 ModernZ：下一集是否已在 mpv 列表里（`yes` / `no`），是的话下一集按钮变绿带闪电（mpv/scripts/modernz.lua）。
const NEXT_READY_MSG: &str = "embymini-next-ready";
/// 发给 ModernZ：上游下载速度（字节 / 秒），顶栏网速显示它。经缓存代理时 mpv 自己的读取速度是本机磁盘的速度，不是网速。
const SPEED_MSG: &str = "embymini-speed";
/// 弹幕脚本（mpv/scripts/danmaku.lua）发来的消息前缀：`style`、`search`、`episodes`、`pick`、`reload`，见 [`State::on_danmaku`]。
const DM_MSG: &str = "embymini-danmaku-";

/// 资源目录里自带的 mpv 配置目录（`<资源>/mpv`），不存在返回 None。
///
/// Windows 上 Tauri 的资源目录来自 `canonicalize(当前 exe)`，带 `\\?\` 前缀（逐字路径）。
/// mpv 用 `/` 拼配置文件名（`…\mpv/mpv.conf`），再原样交给 CreateFileW；逐字路径里 `/` 不算分隔符，
/// 结果 mpv.conf、scripts、fonts 一个都读不到，ModernZ 不加载，只剩 mpv 自带的控制条。
/// 所以先用 dunce 去掉前缀。测试 `bundled_config_loads` 在 Windows CI 上验证这一点。
pub fn bundled_dir(resource_dir: &Path) -> Option<PathBuf> {
    Some(dunce::simplified(resource_dir).join("mpv")).filter(|d| d.is_dir())
}

pub struct Options {
    pub mpv: PathBuf,
    /// 自带配置目录（ModernZ、mpv.conf、input.conf），作为 `--config-dir`。
    pub config_dir: Option<PathBuf>,
    pub preload_percent: f64,
    /// 上次退出时的音量（每次播放都是新的 mpv 进程，不传就回到 100）。
    pub volume: Option<f64>,
    /// mpv 运行时自己写的文件（显卡着色器缓存等）放这里。不指定的话，即使给了 `--config-dir`，
    /// mpv 也会写到它自己的 `%LOCALAPPDATA%\mpv\cache`，卸载 EmbyMini 时清不掉。
    pub cache_dir: Option<PathBuf>,
    /// 个人 mpv 配置（存在才加载）：在自带配置之后读，同名选项以它为准；放在数据目录，升级不覆盖。
    pub user_conf: Option<PathBuf>,
    /// 本地缓存代理；没有（起不来）时 mpv 直连上游、token 经请求头带。
    pub stream: Option<Stream>,
    /// 弹幕（数据在这里取，渲染在 mpv 脚本里）；None = 不加载弹幕脚本的数据。
    pub danmaku: Option<danmaku::Ctx>,
    pub extra_args: Vec<String>,
}

/// mpv 的启动参数。顺序有讲究：自带配置（`--config-dir`）→ 个人配置（`--include`，覆盖自带的）→
/// EmbyMini 记住的音量、缓存目录 → EmbyMini 必须控制的项（IPC、UA、窗口、空闲退出，个人配置改不掉），
/// 最后是调用方附加的参数。
fn mpv_args(opts: &Options, ipc: &Path) -> Vec<String> {
    let mut args = Vec::new();
    if let Some(dir) = &opts.config_dir {
        args.push(format!("--config-dir={}", dir.display()));
    }
    if let Some(f) = opts.user_conf.as_deref().filter(|f| f.is_file()) {
        args.push(format!("--include={}", f.display()));
    }
    if let Some(v) = opts.volume {
        args.push(format!("--volume={v}"));
    }
    if let Some(dir) = &opts.cache_dir {
        for opt in ["gpu-shader-cache-dir", "icc-cache-dir", "watch-later-dir"] {
            args.push(format!("--{opt}={}", dir.display()));
        }
    }
    // 进度条预览（thumbfast）要另起一个 mpv，Windows 上它只会去 PATH 里找，找不到自带的这个。
    // `-append` 只加一项、不按逗号拆，路径里有逗号也没事
    args.push(format!(
        "--script-opts-append=thumbfast-mpv_path={}",
        opts.mpv.display()
    ));
    args.push(format!("--input-ipc-server={}", ipc.display()));
    args.push(format!("--user-agent={}", emby::UA));
    args.extend(["--force-window=immediate", "--idle=once"].map(String::from));
    args.extend(opts.extra_args.iter().cloned());
    args
}

pub enum Ctl {
    Quit,
    /// 设置页改了弹幕接口地址或屏蔽词：正在播放时马上生效（「重新加载」就用新地址）。
    Danmaku {
        api: Option<danmaku::Api>,
        words: Vec<String>,
    },
    #[cfg(test)]
    Mpv(Value),
}

/// 发给前端的播放事件。
#[derive(Serialize, Debug, Clone, PartialEq)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Event {
    Started {
        item_id: String,
    },
    Preloaded {
        item_id: String,
    },
    Stopped {
        item_id: String,
        position_ticks: i64,
    },
    /// 加载失败（mpv 打不开文件）。message 是给用户看的中文原因。
    Failed {
        item_id: String,
        message: String,
    },
    /// 整个播放会话结束；volume 是退出时的音量，下次起播沿用。
    Ended {
        volume: Option<f64>,
    },
    /// 弹幕面板里改了显示设置，存进配置。
    DanmakuStyle {
        style: danmaku::Style,
    },
    /// 手动匹配了弹幕，存进配置（键见 [`danmaku::key`]）。
    DanmakuPick {
        key: String,
        pick: danmaku::Pick,
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Track {
    /// The server gave none: mpv picks by slang / alang.
    Auto,
    Off,
    /// mpv's track id (1-based).
    Id(usize),
}

/// Maps Emby's default track (a stream `Index`) to mpv's track id: embedded tracks number by sorted Index;
/// external subtitles follow them, in `sub-add` order (also sorted by Index).
fn mpv_track(streams: &[MediaStream], kind: &str, default: Option<i32>) -> Track {
    let Some(want) = default else {
        return Track::Auto;
    };
    if want < 0 {
        return Track::Off;
    }
    let sorted = |external: bool| {
        let mut v: Vec<i32> = streams
            .iter()
            .filter(|s| s.kind == kind && s.is_external == external)
            .map(|s| s.index)
            .collect();
        v.sort_unstable();
        v
    };
    let internal = sorted(false);
    if let Some(p) = internal.iter().position(|&i| i == want) {
        return Track::Id(p + 1);
    }
    if kind != "Subtitle" {
        return Track::Auto; // external audio is never added
    }
    sorted(true)
        .iter()
        .position(|&i| i == want)
        .map_or(Track::Auto, |p| Track::Id(internal.len() + p + 1))
}

/// The track title mpv shows: the title passed to `sub-add` for external subtitles, the container's name otherwise.
fn stream_title(s: &MediaStream) -> Option<&str> {
    if s.is_external {
        s.display_title.as_deref().or(s.title.as_deref())
    } else {
        s.title.as_deref()
    }
}

/// The track the user picked last episode (from mpv's `current-tracks/*`); the next episode picks the same one.
#[derive(Debug, Clone, PartialEq)]
enum Pref {
    Off,
    Pick {
        lang: String,
        title: String,
        external: bool,
    },
}

impl Pref {
    fn from_track(v: &Value) -> Self {
        let s = |k: &str| v[k].as_str().unwrap_or_default().to_owned();
        Self::Pick {
            lang: s("lang"),
            title: s("title"),
            external: v["external"].as_bool().unwrap_or(false),
        }
    }
}

/// Normalises a language code: mpv reports the container's BCP 47 (`en`, `zh-CN`), Emby mostly ISO 639-2 (`eng`, `chi`).
// shortcut: common languages only, other codes compare as-is; a miss falls back to the server default, never a wrong
// track. Add codes when one misses.
fn lang_key(s: &str) -> String {
    let p = s
        .split(['-', '_'])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    match p.as_str() {
        "eng" => "en",
        "chi" | "zho" => "zh",
        "jpn" => "ja",
        "kor" => "ko",
        "fre" | "fra" => "fr",
        "ger" | "deu" => "de",
        "spa" => "es",
        "rus" => "ru",
        "ita" => "it",
        "por" => "pt",
        "tha" => "th",
        _ => return p,
    }
    .to_owned()
}

/// The new episode's track most like the preference: same language required, then title, then embedded /
/// external. None when nothing matches (the server default applies).
fn match_pref(streams: &[MediaStream], kind: &str, pref: &Pref) -> Option<Track> {
    let Pref::Pick {
        lang,
        title,
        external,
    } = pref
    else {
        return (kind == "Subtitle").then_some(Track::Off);
    };
    streams
        .iter()
        .filter(|s| s.kind == kind)
        .filter(|s| lang_key(s.language.as_deref().unwrap_or_default()) == lang_key(lang))
        .max_by_key(|s| {
            (
                stream_title(s).unwrap_or_default() == title,
                s.language.as_deref().unwrap_or_default() == lang,
                s.is_external == *external,
                std::cmp::Reverse(s.index),
            )
        })
        .map(|s| mpv_track(streams, kind, Some(s.index)))
        .filter(|t| *t != Track::Auto)
}

/// A row in the episode menu: the series name is already in the window title.
fn episode_label(item: &Item) -> String {
    match (item.parent_index_number, item.index_number) {
        (Some(s), Some(e)) => format!("S{s:02}E{e:02} {}", item.name),
        _ => item.name.clone(),
    }
}

/// The window title: an episode also gets its series name.
fn title(item: &Item) -> String {
    match (
        &item.series_name,
        item.parent_index_number.and(item.index_number),
    ) {
        (Some(series), Some(_)) => format!("{series} {}", episode_label(item)),
        _ => item.name.clone(),
    }
}

/// Why an entry failed to load: the upstream status when there is one, else mpv's own error.
fn fail_reason(status: Option<u16>, mpv_error: &str) -> String {
    match status {
        Some(s) if !(200..300).contains(&s) => crate::status::describe(s, ""),
        _ => format!("mpv 无法打开（{mpv_error}）"),
    }
}

/// `loadfile` options. The title uses mpv's `%bytes%` form, so commas and equals signs in it stay intact.
fn loadfile_opts(title: &str, start: f64) -> String {
    let mut o = format!("force-media-title=%{}%{title}", title.len());
    if start > 0.0 {
        let _ = write!(o, ",start={start:.3}");
    }
    o
}

struct Sub {
    url: String,
    /// File name in the subtitle cache.
    file: String,
    title: String,
    lang: String,
}

/// 一集的播放参数（来自 PlaybackInfo）。
struct Ready {
    /// 上游直链（不带 token）；交给 mpv 时换成缓存代理的本地地址。
    url: String,
    /// 缓存目录名：上游 + 条目 + 版本，同一个文件每次都一样（下次打开直接用上次的缓存）。
    name: String,
    source_id: String,
    session_id: Option<String>,
    subs: Vec<Sub>,
    aid: Track,
    sid: Track,
    streams: Vec<MediaStream>,
}

async fn prepare(emby: &Emby, item: &Item) -> Result<Ready, Error> {
    ready(emby, item, emby.playback_info(&item.id, None).await?)
}

fn ready(emby: &Emby, item: &Item, info: emby::PlaybackInfo) -> Result<Ready, Error> {
    if let Some(code) = info.error_code.filter(|c| !c.is_empty()) {
        return Err(Error::Other(format!(
            "服务器拒绝播放「{}」（{code}）",
            item.name
        )));
    }
    let src = info
        .media_sources
        .into_iter()
        .next()
        .ok_or_else(|| Error::Other(format!("「{}」没有可播放的媒体源", item.name)))?;
    let mut ext: Vec<&MediaStream> = src
        .media_streams
        .iter()
        .filter(|s| s.kind == "Subtitle" && s.is_external)
        .collect();
    ext.sort_by_key(|s| s.index);
    let name = cache_name(&[&emby.tag, &item.id, &src.id]);
    let subs = ext
        .into_iter()
        .map(|s| Sub {
            url: emby.subtitle_url(&item.id, &src, s),
            // 以视频的缓存名开头：retain 掉视频时它的字幕登记一起清掉
            file: format!(
                "{name}_{}.{}",
                s.index,
                emby::subtitle_ext(s.codec.as_deref())
            ),
            title: stream_title(s).unwrap_or_default().to_owned(),
            lang: s.language.clone().unwrap_or_default(),
        })
        .collect();
    Ok(Ready {
        url: emby.stream_url(&item.id, &src),
        name,
        aid: mpv_track(&src.media_streams, "Audio", src.default_audio_stream_index),
        sid: mpv_track(
            &src.media_streams,
            "Subtitle",
            src.default_subtitle_stream_index,
        ),
        source_id: src.id,
        session_id: info.play_session_id,
        subs,
        streams: src.media_streams,
    })
}

/// 缓存里的文件名：各段只留字母、数字和 `-`（id 里别的字符换成 `-`），用 `_` 连起来。
fn cache_name(parts: &[&str]) -> String {
    parts
        .iter()
        .map(|p| {
            p.chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || c == '-' {
                        c
                    } else {
                        '-'
                    }
                })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("_")
}

struct Entry {
    item: Item,
    start: f64,
    ready: Option<Ready>,
    /// 已经 loadfile 进 mpv 播放列表。
    loaded: bool,
    /// 准备失败过（不在每次进度更新时重试，手动切下一集时会再试）。
    failed: bool,
    entry_id: Option<i64>,
    started: bool,
    stopped: bool,
    /// 正在后台预加载（避免每次进度更新都再发一次）。
    preparing: bool,
}

/// 后台准备好一集之后要做什么。
#[derive(Debug, Clone, Copy, PartialEq)]
enum After {
    /// 预加载：追加到 mpv 列表末尾（仍在播上一集时才追加）。
    Append,
    /// 用户要切过去（上一集 / 下一集 / 选集）：替换列表立即播放。
    Play,
}

/// 后台准备的结果：(队列下标, 之后做什么, PlaybackInfo 结果)。
type Prepared = (usize, After, Result<Ready, Error>);

/// 回传任务：按顺序发 Sessions 请求。放在单独的任务里，上游慢的时候（实测 API 偶尔要 15–30 秒）
/// 不会卡住 mpv 事件处理，也不会让 Stopped 跑到 Playing 前面。积压时只保留最新的一条 Progress。
fn spawn_reporter(
    emby: Emby,
) -> (
    mpsc::UnboundedSender<(String, Value)>,
    tokio::task::JoinHandle<()>,
) {
    let (tx, mut rx) = mpsc::unbounded_channel::<(String, Value)>();
    let task = tokio::spawn(async move {
        while let Some((path, body)) = rx.recv().await {
            if path.ends_with("/Progress") && !rx.is_empty() {
                continue; // 后面还有更新的进度或 Stopped，这条已经过时
            }
            let _ = emby.call(Method::POST, &path, NO_QUERY, Some(&body)).await; // 失败在 call 里记日志
        }
    });
    (tx, task)
}

trait Io: AsyncRead + AsyncWrite + Unpin + Send + Sync {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + Sync> Io for T {}

pub struct Session {
    emby: Emby,
    queue: Vec<Entry>,
    /// 起播的那一集在队列里的下标。
    at: usize,
    child: Child,
    io: Box<dyn Io>,
    ipc: PathBuf,
    preload: f64,
    stream: Option<Stream>,
    danmaku: Option<danmaku::Ctx>,
}

/// 启动 mpv（窗口立刻出来），同时建队列（剧集 = 本集所在整季）、准备首集，再连上 IPC。出错直接返回给界面。
/// `source`：首集用哪个版本（详情页选的）；之后自动切的集用服务器默认的。
pub async fn start(
    emby: Emby,
    item_id: &str,
    source: Option<&str>,
    from_start: bool,
    opts: Options,
) -> Result<Session, Error> {
    let ipc = ipc_path();
    let args = mpv_args(&opts, &ipc);
    // 参数里只有路径和开关，没有 token（token 在缓存代理里，没有代理时经 IPC 设置），可以整串记日志
    log::info!("启动 mpv：{} {}", opts.mpv.display(), args.join(" "));
    let mut cmd = tokio::process::Command::new(&opts.mpv);
    // 播放直连，不走代理：mpv（ffmpeg）只认这类环境变量，用户的终端或代理软件可能设过
    for k in [
        "http_proxy",
        "https_proxy",
        "all_proxy",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
    ] {
        cmd.env_remove(k);
    }
    // 起播途中放弃（程序退出、出错）时丢掉 Child 就杀掉 mpv，不留空窗口
    cmd.kill_on_drop(true)
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let mut child = cmd
        .spawn()
        .map_err(|e| Error::Other(format!("无法启动 mpv（{}）：{e}", opts.mpv.display())))?;

    let queue = async {
        // 条目和 PlaybackInfo 只依赖 id，并行取；单集列表要等条目里的剧 / 季 id
        let (item, info) =
            tokio::try_join!(emby.item(item_id), emby.playback_info(item_id, source))?;
        let first = ready(&emby, &item, info)?;
        let mut items = Vec::new();
        if let (true, Some(series), Some(season)) =
            (item.kind == "Episode", &item.series_id, &item.season_id)
        {
            items = emby.episodes(series, season).await?;
        }
        let at = match items.iter().position(|e| e.id == item.id) {
            Some(at) => at,
            None => {
                items = vec![item.clone()];
                0
            }
        };
        let mut queue: Vec<Entry> = items
            .into_iter()
            .map(|item| Entry {
                item,
                start: 0.0,
                ready: None,
                loaded: false,
                failed: false,
                entry_id: None,
                started: false,
                stopped: false,
                preparing: false,
            })
            .collect();
        queue[at].ready = Some(first);
        if !from_start {
            queue[at].start = item.user_data.playback_position_ticks as f64 / TICKS_PER_SEC;
        }
        Ok::<_, Error>((queue, at))
    };
    let (queue, io) = tokio::join!(queue, connect(&ipc, &mut child));
    let ((queue, at), io) = match (queue, io) {
        (Ok(q), Ok(io)) => (q, io),
        (Err(e), _) | (_, Err(e)) => {
            let _ = child.kill().await;
            return Err(e);
        }
    };
    log::info!(
        "播放：{}（整季 {} 集，第 {} 集起）",
        queue[at].item.id,
        queue.len(),
        at + 1
    );
    Ok(Session {
        emby,
        queue,
        at,
        child,
        io,
        ipc,
        preload: opts.preload_percent,
        stream: opts.stream,
        danmaku: opts.danmaku,
    })
}

impl Session {
    /// 不播了（起播期间用户又点了别的）：直接结束 mpv。还没开始播，不用回传。
    pub async fn abort(mut self) {
        let _ = self.child.kill().await;
        #[cfg(unix)]
        let _ = std::fs::remove_file(&self.ipc);
    }

    pub async fn run(
        self,
        mut ctl: mpsc::UnboundedReceiver<Ctl>,
        events: mpsc::UnboundedSender<Event>,
    ) {
        let Self {
            emby,
            queue,
            at,
            mut child,
            io,
            ipc,
            preload,
            stream,
            danmaku,
        } = self;
        let (rd, wr) = tokio::io::split(io);
        let mut lines = BufReader::new(rd).lines();
        let (reports, reporter) = spawn_reporter(emby.clone());
        let (prepared, mut prepared_rx) = mpsc::unbounded_channel();
        let (dm_tx, mut dm_rx) = mpsc::unbounded_channel();
        let mut st = State::new(emby, queue, wr, events, preload, reports, prepared);
        st.up_seen = stream.as_ref().map_or(0, Stream::up_bytes);
        st.stream = stream;
        st.dm = danmaku.map(|ctx| Dm {
            ctx,
            tx: dm_tx,
            shown: None,
            dead: None,
            told: false,
            init: false,
        });

        // 没有缓存代理时 mpv 直连上游：token 走请求头，不进 URL（见 emby::strip_key）；要在第一次 loadfile 之前设好
        if st.stream.is_none() {
            let header = st.emby.token_header();
            st.send(json!([
                "change-list",
                "http-header-fields",
                "append",
                header
            ]))
            .await;
        }
        for (id, name) in [
            "time-pos",
            "duration",
            "pause",
            "volume",
            "current-tracks/audio",
            "current-tracks/sub",
            "sid",
        ]
        .into_iter()
        .enumerate()
        {
            st.send(json!(["observe_property", id + 1, name])).await;
        }
        st.load(at, "append-play").await;

        let mut tick = tokio::time::interval(PROGRESS_EVERY);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut second = tokio::time::interval(Duration::from_secs(1));
        let mut ctl_open = true;
        let mut quit_sent = false;
        loop {
            tokio::select! {
                line = lines.next_line() => match line {
                    Ok(Some(l)) => match serde_json::from_str::<Value>(&l) {
                        Ok(v) => st.on_message(v).await,
                        Err(e) => log::warn!("mpv IPC 消息解析失败：{e}"),
                    },
                    _ => break, // mpv 退出，管道关闭
                },
                c = ctl.recv(), if ctl_open => match c {
                    Some(Ctl::Quit) => { log::info!("关闭播放器"); quit_sent = true; st.send(json!(["quit"])).await; }
                    None => { ctl_open = false; quit_sent = true; st.send(json!(["quit"])).await; }
                    #[cfg(test)]
                    Some(Ctl::Mpv(cmd)) => { st.send(cmd).await; }
                    Some(Ctl::Danmaku { api, words }) => st.dm_settings(api, words).await,
                },
                Some((i, after, r)) = prepared_rx.recv() => st.on_prepared(i, after, r).await,
                Some(m) = dm_rx.recv() => st.on_dm_result(m).await,
                _ = tick.tick() => st.progress("TimeUpdate"),
                _ = second.tick(), if st.stream.is_some() => st.send_speed().await,
            }
        }
        if let Some(i) = st.cur {
            st.stop(i); // mpv 被直接关掉、没来得及发 end-file 时补发
        }
        let status = child.wait().await;
        crate::exit_mark("mpv_exited");
        if let Some(s) = &st.stream {
            let names: Vec<String> = st
                .queue
                .iter()
                .filter_map(|e| e.ready.as_ref().map(|r| r.name.clone()))
                .collect();
            s.idle(&names);
        }
        let code = status.as_ref().ok().and_then(|s| s.code());
        // 一集都没播起来 mpv 就异常退出（显卡初始化失败、崩溃），又不是我们叫它退的：告诉界面，不能悄无声息
        if !quit_sent
            && !st.failed_sent
            && !status.as_ref().is_ok_and(|s| s.success())
            && !st.queue.iter().any(|e| e.started)
        {
            let e = &st.queue[at];
            let message = format!(
                "无法播放「{}」：播放器意外退出（退出码 {}），启动参数见日志",
                title(&e.item),
                code.map_or("无".into(), |c| c.to_string())
            );
            log::warn!("{message}（条目 {}）", e.item.id);
            let _ = st.events.send(Event::Failed {
                item_id: e.item.id.clone(),
                message,
            });
        }
        #[cfg(unix)]
        let _ = std::fs::remove_file(&ipc);
        #[cfg(not(unix))]
        let _ = ipc;
        // 等回传队列发完再报「结束」：程序退出时也靠这一步保证 Stopped 发出去
        let State {
            events,
            volume,
            reports,
            ..
        } = st;
        drop(reports);
        let _ = reporter.await;
        crate::exit_mark("reported");
        log::info!("播放结束");
        let _ = events.send(Event::Ended { volume });
    }
}

/// 弹幕的会话状态。
struct Dm {
    ctx: danmaku::Ctx,
    /// 后台取数的结果送回 run 的循环（[`State::on_dm_result`]）。
    tx: mpsc::UnboundedSender<DmMsg>,
    /// 已经为哪一集发起过加载（避免重复取）。
    shown: Option<usize>,
    /// 地址或令牌不对（[`DmFail::Setup`]）：这次播放不再请求，面板里显示原因。
    dead: Option<String>,
    /// 已经弹过一次提示（没匹配上、取不到）：一次播放只提示一次，不每集都弹。
    told: bool,
    /// 已把设置发给脚本。
    init: bool,
}

/// 后台弹幕任务的结果。
enum DmMsg {
    Loaded(usize, Result<Option<danmaku::Loaded>, DmFail>),
    /// 回给脚本的消息（名字, JSON）：搜索结果、集列表。
    Reply(&'static str, Value),
}

struct State {
    emby: Emby,
    queue: Vec<Entry>,
    wr: WriteHalf<Box<dyn Io>>,
    events: mpsc::UnboundedSender<Event>,
    preload: f64,
    /// Queue index playing now (between start-file and end-file).
    cur: Option<usize>,
    pos: f64,
    dur: f64,
    paused: bool,
    volume: Option<f64>,
    /// The episode whose tracks are set (preference or default); only later track changes are the user's choice
    /// (pref_*).
    applied: Option<usize>,
    pref_audio: Option<Pref>,
    pref_sub: Option<Pref>,
    rid: u64,
    /// loadfile request_id → queue index, matched to the reply's playlist_entry_id.
    pending: HashMap<u64, usize>,
    /// (path, body) for the report task, see [`spawn_reporter`].
    reports: mpsc::UnboundedSender<(String, Value)>,
    /// Background preparation results back to the run loop.
    prepared: mpsc::UnboundedSender<Prepared>,
    /// The episode the user last switched to (still preparing); earlier switch results are dropped.
    want: Option<usize>,
    /// What ModernZ was last told about "next episode ready", see [`State::sync_next_ready`].
    next_ready: bool,
    /// An IPC write failed (mpv gone, pipe broken): logged once, later commands are dropped all the same.
    pipe_broken: bool,
    /// Failed was already sent to the UI (no extra "unexpected exit" when mpv quits).
    failed_sent: bool,
    stream: Option<Stream>,
    dm: Option<Dm>,
    /// Upstream bytes the cache proxy had last time (the top-bar speed is the difference).
    up_seen: u64,
}

impl State {
    fn new(
        emby: Emby,
        queue: Vec<Entry>,
        wr: WriteHalf<Box<dyn Io>>,
        events: mpsc::UnboundedSender<Event>,
        preload: f64,
        reports: mpsc::UnboundedSender<(String, Value)>,
        prepared: mpsc::UnboundedSender<Prepared>,
    ) -> Self {
        Self {
            emby,
            queue,
            wr,
            events,
            preload,
            cur: None,
            pos: 0.0,
            dur: 0.0,
            paused: false,
            volume: None,
            applied: None,
            pref_audio: None,
            pref_sub: None,
            rid: 0,
            pending: HashMap::new(),
            reports,
            prepared,
            want: None,
            next_ready: false,
            pipe_broken: false,
            failed_sent: false,
            stream: None,
            dm: None,
            up_seen: 0,
        }
    }

    async fn send(&mut self, cmd: Value) -> u64 {
        self.rid += 1;
        let line = format!("{}\n", json!({ "command": cmd, "request_id": self.rid }));
        if let Err(e) = self.wr.write_all(line.as_bytes()).await
            && !std::mem::replace(&mut self.pipe_broken, true)
        {
            log::warn!("写 mpv IPC 失败：{e}");
        }
        self.rid
    }

    /// Turns ModernZ's next button green with a bolt once the next episode is in mpv's playlist (PlaybackInfo, link
    /// and tracks ready, so next switches at once); back on a switch or a refused append. Sent only on change.
    async fn sync_next_ready(&mut self) {
        let ready = self
            .cur
            .is_some_and(|i| self.queue.get(i + 1).is_some_and(|e| e.loaded));
        if ready != self.next_ready {
            self.next_ready = ready;
            let v = if ready { "yes" } else { "no" };
            self.send(json!(["script-message", NEXT_READY_MSG, v]))
                .await;
        }
    }

    /// Sends the cache proxy's upstream speed to ModernZ's top bar, once a second.
    async fn send_speed(&mut self) {
        let Some(up) = self.stream.as_ref().map(Stream::up_bytes) else {
            return;
        };
        let d = up.saturating_sub(std::mem::replace(&mut self.up_seen, up));
        self.send(json!(["script-message", SPEED_MSG, d.to_string()]))
            .await;
    }

    /// The cache keeps only this episode and the next one already in mpv's playlist ("keep the last playback's cache").
    fn keep_cache(&self) {
        let (Some(s), Some(i)) = (&self.stream, self.cur) else {
            return;
        };
        let next = Some(i + 1).filter(|&n| self.queue.get(n).is_some_and(|e| e.loaded));
        let keep: Vec<String> = [Some(i), next]
            .into_iter()
            .flatten()
            .filter_map(|k| self.queue[k].ready.as_ref().map(|r| r.name.clone()))
            .collect();
        s.retain(&keep);
    }

    async fn osd(&mut self, text: &str) {
        self.send(json!(["show-text", text, 5000])).await;
    }

    fn index_of(&self, entry_id: Option<i64>) -> Option<usize> {
        entry_id.and_then(|id| self.queue.iter().position(|e| e.entry_id == Some(id)))
    }

    /// Hands a prepared episode to mpv: `append-play` appends (plays at once when idle), `replace` clears and plays.
    /// Registered with the cache proxy only now: earlier, a retain on an episode switch could delete it as someone
    /// else's.
    async fn load(&mut self, i: usize, mode: &str) {
        let e = &self.queue[i];
        let Some(r) = &e.ready else { return };
        let url = match &self.stream {
            Some(s) => s.video(&r.name, &r.url, &self.emby.stream_auth()),
            None => r.url.clone(),
        };
        let cmd = json!([
            "loadfile",
            url,
            mode,
            -1,
            loadfile_opts(&title(&e.item), e.start)
        ]);
        let rid = self.send(cmd).await;
        self.pending.insert(rid, i);
        self.queue[i].loaded = true;
    }

    /// Fetches episode i's PlaybackInfo in the background; [`Self::on_prepared`] takes the result, mpv stays
    /// responsive meanwhile.
    fn prepare_later(&mut self, i: usize, after: After) {
        self.queue[i].preparing = true;
        let (emby, item, tx) = (
            self.emby.clone(),
            self.queue[i].item.clone(),
            self.prepared.clone(),
        );
        tokio::spawn(async move {
            let r = prepare(&emby, &item).await;
            let _ = tx.send((i, after, r));
        });
    }

    /// Background preparation done. The user may have moved on meanwhile, so check the result still applies.
    async fn on_prepared(&mut self, i: usize, after: After, r: Result<Ready, Error>) {
        self.queue[i].preparing = false;
        let id = self.queue[i].item.id.clone();
        match (after, r) {
            (After::Append, Ok(r)) => {
                // A switch replaces the whole playlist: appending after the user left the previous episode would land
                // it in the wrong place
                if self.queue[i].loaded || self.want.is_some() || self.cur != i.checked_sub(1) {
                    log::info!("下一集 {id} 准备好时已不在上一集，不追加");
                    return;
                }
                self.queue[i].ready = Some(r);
                self.load(i, "append-play").await;
                self.sync_next_ready().await;
                self.keep_cache();
                if let (Some(s), Some(r)) = (&self.stream, &self.queue[i].ready) {
                    s.prefetch(&r.name);
                }
                if self.dm_ready() {
                    self.dm_fetch(i, false);
                }
                log::info!("预加载下一集：{id}");
                let _ = self.events.send(Event::Preloaded { item_id: id });
            }
            (After::Append, Err(err)) => {
                log::warn!("下一集 {id} 准备失败：{err}");
                self.queue[i].failed = true; // no retry on every progress update; a manual switch tries again
                self.osd(&format!("下一集加载失败：{err}")).await;
            }
            (After::Play, _) if self.want != Some(i) => {
                log::info!("{id} 准备好时用户已改选别的集，忽略");
            }
            (After::Play, Ok(r)) => {
                self.want = None;
                self.queue[i].ready = Some(r);
                self.play_now(i).await;
            }
            (After::Play, Err(err)) => {
                self.want = None;
                log::warn!("切到 {id} 失败：{err}");
                self.osd(&format!("打开失败：{err}")).await;
            }
        }
    }

    /// One line from mpv's IPC: a command reply, or an event.
    async fn on_message(&mut self, v: Value) {
        match v["event"].as_str() {
            None => self.on_reply(&v).await,
            Some("property-change") => self.on_property(&v),
            Some("client-message") => self.on_client_message(&v["args"]).await,
            Some(ev) => self.on_file_event(ev, &v).await,
        }
    }

    /// A command reply; only loadfile's matter, for the playlist entry id.
    async fn on_reply(&mut self, v: &Value) {
        let Some(i) = v["request_id"]
            .as_u64()
            .and_then(|rid| self.pending.remove(&rid))
        else {
            return;
        };
        match v["data"]["playlist_entry_id"].as_i64() {
            Some(id) => self.queue[i].entry_id = Some(id),
            None => {
                log::warn!("loadfile 失败：{}", v["error"]);
                // Not in the playlist: a manual next must go through jump, not playlist-next
                self.queue[i].loaded = false;
                self.sync_next_ready().await;
            }
        }
    }

    /// start-file / file-loaded / end-file; other events are ignored.
    async fn on_file_event(&mut self, ev: &str, v: &Value) {
        if !matches!(ev, "start-file" | "file-loaded" | "end-file") {
            return;
        }
        let entry = self.index_of(v["playlist_entry_id"].as_i64());
        // These lines rebuild mpv's timeline when chasing a failed switch or load; file-loaded has no entry id
        let id = entry
            .or(self.cur)
            .map_or("?", |i| self.queue[i].item.id.as_str());
        log::info!(
            "mpv {ev}：{id} {}",
            v["reason"].as_str().unwrap_or_default()
        );
        match (ev, entry) {
            ("start-file", Some(i)) => {
                // mpv may play the same entry again (playlist loop): a new playback, or every report is skipped
                (self.queue[i].started, self.queue[i].stopped) = (false, false);
                self.cur = Some(i);
                self.applied = None;
                let labels: Vec<String> =
                    self.queue.iter().map(|e| episode_label(&e.item)).collect();
                self.send(json!([
                    "script-message-to",
                    "embymini",
                    "episodes",
                    json!(labels).to_string(),
                    i.to_string()
                ]))
                .await;
                self.pos = self.queue[i].start;
                self.dur = 0.0;
                self.sync_next_ready().await;
                self.keep_cache();
            }
            ("file-loaded", _) => {
                let Some(i) = self.cur else { return };
                // Only a loaded file counts as started; one that never loaded reports neither Playing nor Stopped
                self.queue[i].started = true;
                self.apply_tracks(i).await;
                self.report(i, "", json!({}));
                self.dm_show(i).await;
                let _ = self.events.send(Event::Started {
                    item_id: self.queue[i].item.id.clone(),
                });
            }
            ("end-file", Some(i)) => {
                if v["reason"] == "error" {
                    self.fail(i, v["file_error"].as_str().unwrap_or("未知错误"))
                        .await;
                } else {
                    self.stop(i);
                }
            }
            _ => {}
        }
    }

    /// `script-message` from our scripts (episode switching, the danmaku panel).
    async fn on_client_message(&mut self, args: &Value) {
        match args[0].as_str().unwrap_or_default() {
            NEXT_MSG => self.next().await,
            PREV_MSG => self.prev().await,
            JUMP_MSG => {
                let to = args[1].as_str().and_then(|s| s.parse::<usize>().ok());
                if let Some(i) = to.filter(|&i| i < self.queue.len() && Some(i) != self.cur) {
                    self.jump(i).await;
                }
            }
            a if a.starts_with(DM_MSG) => self.on_danmaku(args).await,
            _ => {}
        }
    }

    fn on_property(&mut self, v: &Value) {
        match (v["name"].as_str(), &v["data"]) {
            (Some("time-pos"), d) => {
                if let Some(p) = d.as_f64() {
                    self.pos = p;
                    if let Some(i) = self.cur {
                        let next = i + 1;
                        let due = self.dur > 0.0 && p / self.dur * 100.0 >= self.preload;
                        if due
                            && self
                                .queue
                                .get(next)
                                .is_some_and(|e| !e.loaded && !e.failed && !e.preparing)
                        {
                            self.prepare_later(next, After::Append);
                        }
                    }
                }
            }
            (Some("duration"), d) => self.dur = d.as_f64().unwrap_or(0.0),
            (Some("volume"), d) => self.volume = d.as_f64().or(self.volume),
            // Only changes after the tracks are set; the null while switching doesn't count (subs off shows as sid=no)
            (Some("current-tracks/audio"), d) if self.tracking() && d.is_object() => {
                self.pref_audio = Some(Pref::from_track(d));
            }
            (Some("current-tracks/sub"), d) if self.tracking() && d.is_object() => {
                self.pref_sub = Some(Pref::from_track(d));
            }
            (Some("sid"), d) if self.tracking() && (d.as_bool() == Some(false) || d == "no") => {
                self.pref_sub = Some(Pref::Off);
            }
            (Some("pause"), d) => {
                if let Some(p) = d.as_bool().filter(|p| *p != self.paused) {
                    self.paused = p;
                    self.progress(if p { "Pause" } else { "Unpause" });
                }
            }
            _ => {}
        }
    }

    fn tracking(&self) -> bool {
        self.cur.is_some() && self.applied == self.cur
    }

    /// Manual next: switch at once when preloaded (mpv may have read ahead), else [`Self::jump`] (switch once
    /// prepared).
    async fn next(&mut self) {
        let Some(i) = self.cur else { return };
        if i + 1 >= self.queue.len() {
            self.osd("没有下一集了").await;
            return;
        }
        if self.queue[i + 1].loaded {
            self.send(json!(["playlist-next"])).await;
        } else {
            self.jump(i + 1).await;
        }
    }

    async fn prev(&mut self) {
        match self.cur {
            Some(0) => self.osd("已经是第一集").await,
            Some(i) => self.jump(i - 1).await,
            None => {}
        }
    }

    /// Goes to episode i: at once when prepared, else fetches PlaybackInfo in the background and switches then
    /// ([`Self::on_prepared`]).
    async fn jump(&mut self, i: usize) {
        self.osd(&format!("正在打开：{}", title(&self.queue[i].item)))
            .await;
        let e = &self.queue[i];
        // A played episode is fetched again: one PlaySessionId per playback
        if e.ready.is_some() && !e.stopped {
            self.want = None;
            self.play_now(i).await;
        } else {
            self.want = Some(i);
            self.prepare_later(i, After::Play);
        }
    }

    /// Replaces mpv's playlist and plays episode i from the start (anything appended goes with it).
    async fn play_now(&mut self, i: usize) {
        for e in &mut self.queue {
            e.loaded = false;
        }
        let e = &mut self.queue[i];
        (e.start, e.failed, e.started, e.stopped) = (0.0, false, false, false);
        self.load(i, "replace").await;
    }

    /// Adds external subtitles and picks audio / subtitle: the user's choice from last episode (by language, title),
    /// else the server default.
    async fn apply_tracks(&mut self, i: usize) {
        let Some(r) = &self.queue[i].ready else {
            return;
        };
        let pick = |pref: &Option<Pref>, kind, default| {
            pref.as_ref()
                .and_then(|p| match_pref(&r.streams, kind, p))
                .unwrap_or(default)
        };
        let aid = pick(&self.pref_audio, "Audio", r.aid);
        let sid = pick(&self.pref_sub, "Subtitle", r.sid);
        let auth = self.emby.stream_auth();
        let cmds: Vec<Value> = r
            .subs
            .iter()
            .map(|s| {
                let url = match &self.stream {
                    Some(st) => st.sub(&s.file, &s.url, &auth),
                    None => s.url.clone(),
                };
                json!(["sub-add", url, "auto", s.title, s.lang])
            })
            .chain(
                [("aid", aid), ("sid", sid)]
                    .into_iter()
                    .filter_map(|(prop, t)| match t {
                        Track::Auto => None,
                        Track::Off => Some(json!(["set_property", prop, "no"])),
                        Track::Id(n) => Some(json!(["set_property", prop, n])),
                    }),
            )
            .collect();
        for c in cmds {
            self.send(c).await;
        }
        self.applied = Some(i);
    }

    /// Load failed: gets the upstream status (the cache proxy has it; without the proxy, probes the link once) and
    /// tells the UI why.
    async fn fail(&mut self, i: usize, mpv_error: &str) {
        let status = match (&self.queue[i].ready, &self.stream) {
            (Some(r), Some(s)) => s.fail_status(&r.name),
            (Some(r), None) => self.emby.probe(&r.url).await,
            (None, _) => None,
        };
        self.stop(i); // reports Stopped if it had started
        let e = &mut self.queue[i];
        // Its play session is spent either way: going back fetches a fresh PlaybackInfo. `ready` stays,
        // so the end of the session still stops its download.
        e.stopped = true;
        let message = format!(
            "无法播放「{}」：{}",
            title(&e.item),
            fail_reason(status, mpv_error)
        );
        log::warn!("{message}（条目 {}）", e.item.id);
        let item_id = e.item.id.clone();
        if self.cur == Some(i) {
            self.cur = None;
        }
        self.failed_sent = true;
        let _ = self.events.send(Event::Failed { item_id, message });
    }

    /// Hands to the report task (doesn't wait). `what` is `""` / `"/Progress"` / `"/Stopped"`.
    fn report(&self, i: usize, what: &str, extra: Value) {
        let e = &self.queue[i];
        let Some(r) = &e.ready else { return };
        let mut body = json!({
            "ItemId": e.item.id,
            "MediaSourceId": r.source_id,
            "PlaySessionId": r.session_id,
            "PositionTicks": (self.pos * TICKS_PER_SEC) as i64,
            "IsPaused": self.paused,
            "CanSeek": true,
            "PlayMethod": "DirectStream",
        });
        if let (Some(b), Value::Object(x)) = (body.as_object_mut(), extra) {
            b.extend(x);
        }
        let _ = self
            .reports
            .send((format!("/Sessions/Playing{what}"), body));
    }

    fn progress(&self, event: &str) {
        if let Some(i) = self
            .cur
            .filter(|&i| self.queue[i].started && !self.queue[i].stopped)
        {
            self.report(i, "/Progress", json!({ "EventName": event }));
        }
    }

    fn stop(&mut self, i: usize) {
        if !self.queue[i].started || self.queue[i].stopped {
            return;
        }
        self.report(i, "/Stopped", json!({}));
        self.queue[i].stopped = true;
        if self.cur == Some(i) {
            self.cur = None;
        }
        let _ = self.events.send(Event::Stopped {
            item_id: self.queue[i].item.id.clone(),
            position_ticks: (self.pos * TICKS_PER_SEC) as i64,
        });
    }
}

/// 弹幕：取数、和脚本（`mpv/scripts/danmaku.lua`，脚本名 `danmaku`）往来。
impl State {
    async fn dm_send(&mut self, name: &str, v: Value) {
        self.send(json!(["script-message-to", "danmaku", name, v.to_string()]))
            .await;
    }

    /// 开着弹幕、填了接口、这次播放没判定地址不对。
    fn dm_ready(&self) -> bool {
        self.dm
            .as_ref()
            .is_some_and(|d| d.ctx.cfg.style.on && d.ctx.api.is_some() && d.dead.is_none())
    }

    /// 在后台取第 i 集的弹幕；`deliver` = 取完交给脚本（当前集），否则只是预取进缓存（下一集）。
    fn dm_fetch(&mut self, i: usize, deliver: bool) {
        let Some(d) = &self.dm else { return };
        let Some(api) = d.ctx.api.clone() else { return };
        let item = &self.queue[i].item;
        let want = danmaku::Want {
            name: danmaku::match_name(item),
            index: danmaku::index(item),
            pick: d
                .ctx
                .cfg
                .picks
                .get(&danmaku::key(&self.emby.tag, item))
                .cloned(),
        };
        let (dir, tx) = (d.ctx.dir.clone(), d.tx.clone());
        tokio::spawn(async move {
            let r = danmaku::load(&api, &dir, &want).await;
            match (deliver, r) {
                (true, r) => {
                    let _ = tx.send(DmMsg::Loaded(i, r));
                }
                (false, Err(e)) => log::info!("预取下一集弹幕没成：{e}"),
                (false, Ok(_)) => {}
            }
        });
    }

    /// 第 i 集开播：第一次先把设置发给脚本，再按需取弹幕。
    async fn dm_show(&mut self, i: usize) {
        let Some(d) = &mut self.dm else { return };
        let init = !std::mem::replace(&mut d.init, true);
        let msg = json!({ "style": d.ctx.cfg.style, "words": d.ctx.cfg.words });
        if init {
            self.dm_send("init", msg).await;
        }
        self.dm_load(i).await;
    }

    /// 给脚本加载第 i 集：取数，或说明为什么没有。
    async fn dm_load(&mut self, i: usize) {
        let Some(d) = &mut self.dm else { return };
        if !d.ctx.cfg.style.on {
            return; // 关着就不取，打开时再取（见 on_danmaku 的 style）
        }
        d.shown = Some(i);
        let status = match (&d.ctx.api, &d.dead) {
            (None, _) => {
                json!({ "kind": "setup", "text": "还没有填弹幕接口：在 EmbyMini 的设置里填写" })
            }
            (_, Some(why)) => json!({ "kind": "setup", "text": why }),
            _ => json!({ "kind": "loading", "text": "正在加载弹幕…" }),
        };
        let go = status["kind"] == "loading";
        // 每集第一条消息：脚本据此认定当前是哪一集，之后 item 对不上的（换集前发出的晚到结果）丢掉
        let mut status = status;
        status["start"] = true.into();
        status["item"] = self.queue[i].item.id.clone().into();
        self.dm_send("status", status).await;
        if go {
            self.dm_fetch(i, true);
        }
    }

    async fn on_dm_result(&mut self, m: DmMsg) {
        let (i, r) = match m {
            DmMsg::Reply(name, v) => return self.dm_send(name, v).await,
            DmMsg::Loaded(i, r) => (i, r),
        };
        if self.cur != Some(i) {
            return; // 已经换集了
        }
        let item = self.queue[i].item.id.clone();
        let Some(d) = &mut self.dm else { return };
        // 没匹配上、取不到时提示一次（之后的集只在面板里显示），地址不对时也只提示一次
        let toast = !std::mem::replace(&mut d.told, true);
        let status = match r {
            Ok(Some(l)) => {
                d.told = !toast; // 成功不算提示过
                // 弹幕本身经 IPC 发给脚本，不让脚本读文件：Windows 上 mpv 内置 Lua 的 io.open 走 ANSI 代码页，
                // 数据目录带中文时打不开。`l.list` 是缓存文件的原文（JSON 数组），原样拼进去，不再解析一遍
                let head =
                    json!({ "item": item, "title": l.title, "manual": l.manual, "stale": l.stale })
                        .to_string();
                let body = format!("{},\"list\":{}}}", &head[..head.len() - 1], l.list);
                self.send(json!(["script-message-to", "danmaku", "load", body]))
                    .await;
                return;
            }
            Ok(None) => {
                json!({ "kind": "none", "text": "没有匹配到弹幕，可在弹幕设置里手动匹配", "toast": toast })
            }
            Err(DmFail::Setup(why)) => {
                log::warn!("{why}");
                d.dead = Some(why.clone());
                json!({ "kind": "setup", "text": why, "toast": toast })
            }
            Err(e @ (DmFail::Busy(_) | DmFail::Other(_))) => {
                log::warn!("{e}");
                json!({ "kind": "error", "text": format!("{e}（可在弹幕设置里重试）"), "toast": toast })
            }
        };
        let mut status = status;
        status["item"] = item.into();
        self.dm_send("status", status).await;
    }

    /// 设置页改了接口地址或屏蔽词（[`Ctl::Danmaku`]）：换掉本次播放的接口，屏蔽词重新下发给脚本。
    async fn dm_settings(&mut self, api: Option<danmaku::Api>, words: Vec<String>) {
        let Some(d) = &mut self.dm else { return };
        d.ctx.api = api;
        d.dead = None;
        let changed = d.ctx.cfg.words != words;
        d.ctx.cfg.words = words;
        if changed && d.init {
            let msg = json!({ "style": d.ctx.cfg.style, "words": d.ctx.cfg.words });
            self.dm_send("init", msg).await;
        }
    }

    /// 脚本发来的消息：`args[0]` = `embymini-danmaku-<动作>`，后面是参数。
    async fn on_danmaku(&mut self, args: &Value) {
        let (Some(cur), Some(d)) = (self.cur, &mut self.dm) else {
            return;
        };
        let what = args[0]
            .as_str()
            .unwrap_or_default()
            .trim_start_matches(DM_MSG);
        let arg = |k: usize| args[k].as_str().unwrap_or_default().to_owned();
        let (api, tx) = (d.ctx.api.clone(), d.tx.clone());
        let item = self.queue[cur].item.clone();
        let key = danmaku::key(&self.emby.tag, &item);
        match what {
            "style" => {
                let Ok(style) = serde_json::from_str::<danmaku::Style>(&arg(1)) else {
                    return;
                };
                let style = style.clamped();
                let was_on = d.ctx.cfg.style.on;
                if style != d.ctx.cfg.style {
                    d.ctx.cfg.style = style.clone();
                    let _ = self.events.send(Event::DanmakuStyle { style });
                }
                if !was_on && d.ctx.cfg.style.on && d.shown != Some(cur) {
                    self.dm_load(cur).await;
                }
            }
            "reload" => {
                d.dead = None; // 可能刚在设置里改好了地址（新地址已由 Ctl::Danmaku 换上）
                self.dm_load(cur).await;
            }
            "search" | "episodes" => {
                let Some(api) = api else { return };
                let (kw, pick) = (arg(1), d.ctx.cfg.picks.get(&key).cloned());
                let search = what == "search";
                tokio::spawn(async move {
                    let (name, v) = if search {
                        ("search", api.search(&kw).await.map(|l| json!(l)))
                    } else {
                        let anime = kw.parse().unwrap_or(0);
                        let r = api.episodes(anime).await.map(|l| {
                            // 默认选中：按已有的匹配偏移或集号算出的那一集
                            let off = pick.filter(|p| p.anime == anime).map_or(0, |p| p.offset);
                            let at = (danmaku::index(&item) + off)
                                .clamp(0, l.len().saturating_sub(1) as i32);
                            json!({ "anime": anime, "list": l, "at": at })
                        });
                        ("episodes", r)
                    };
                    let v = v.unwrap_or_else(|e| json!({ "error": e.to_string() }));
                    let _ = tx.send(DmMsg::Reply(name, v));
                });
            }
            "pick" => {
                let (Ok(anime), Ok(at)) = (arg(1).parse::<i64>(), arg(2).parse::<i32>()) else {
                    return;
                };
                let pick = danmaku::Pick {
                    anime,
                    offset: at - danmaku::index(&item),
                };
                d.ctx.cfg.picks.insert(key.clone(), pick.clone());
                d.dead = None;
                let _ = self.events.send(Event::DanmakuPick { key, pick });
                self.dm_load(cur).await;
            }
            _ => {}
        }
    }
}

fn ipc_path() -> PathBuf {
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let name = format!(
        "embymini-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    );
    if cfg!(windows) {
        PathBuf::from(format!(r"\\.\pipe\{name}"))
    } else {
        std::env::temp_dir().join(format!("{name}.sock"))
    }
}

#[cfg(windows)]
async fn open(path: &Path) -> std::io::Result<Box<dyn Io>> {
    Ok(Box::new(
        tokio::net::windows::named_pipe::ClientOptions::new().open(path)?,
    ))
}

#[cfg(unix)]
async fn open(path: &Path) -> std::io::Result<Box<dyn Io>> {
    Ok(Box::new(tokio::net::UnixStream::connect(path).await?))
}

/// mpv 启动后才创建 IPC 端点，轮询等它出现（最多 10 秒）。
async fn connect(path: &Path, child: &mut Child) -> Result<Box<dyn Io>, Error> {
    for _ in 0..100 {
        if let Ok(io) = open(path).await {
            return Ok(io);
        }
        if let Ok(Some(status)) = child.try_wait() {
            return Err(Error::Other(format!(
                "mpv 启动后立即退出（{status}），请检查 mpv 路径与版本（需要 0.38 以上）"
            )));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let _ = child.kill().await;
    Err(Error::Other("连接 mpv 超时".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emby::{MediaSource, PlaybackInfo, tests::fixture};
    use std::time::Instant;

    #[test]
    fn tracks_from_real_playbackinfo() {
        // Measured: video 0, audio 1, embedded subs 2/3/4, external Chinese ass 5; server default audio 1, subtitle 5.
        let pi: PlaybackInfo = fixture("playbackinfo.json");
        let s: &MediaSource = &pi.media_sources[0];
        assert_eq!(
            mpv_track(&s.media_streams, "Audio", s.default_audio_stream_index),
            Track::Id(1)
        );
        assert_eq!(
            mpv_track(
                &s.media_streams,
                "Subtitle",
                s.default_subtitle_stream_index
            ),
            Track::Id(4)
        );
        assert_eq!(
            mpv_track(&s.media_streams, "Subtitle", Some(3)),
            Track::Id(2)
        );
        assert_eq!(
            mpv_track(&s.media_streams, "Subtitle", Some(-1)),
            Track::Off
        );
        assert_eq!(mpv_track(&s.media_streams, "Subtitle", None), Track::Auto);
        // Index 0 is the video: not a subtitle, and not "off" either
        assert_eq!(
            mpv_track(&s.media_streams, "Subtitle", Some(0)),
            Track::Auto
        );
        assert_eq!(
            mpv_track(&s.media_streams, "Subtitle", Some(99)),
            Track::Auto
        );
    }

    #[test]
    fn track_prefs_carry_over_by_language_and_title() {
        // The same measured streams: audio 1 (eng), embedded subs 2 English / 3 English SDH / 4 SDH (PGS), external 5 zh-CN
        let pi: PlaybackInfo = fixture("playbackinfo.json");
        let st = &pi.media_sources[0].media_streams;
        let pick = |lang: &str, title: &str, external| Pref::Pick {
            lang: lang.into(),
            title: title.into(),
            external,
        };
        // mpv says en, Emby says eng; the one with the same title wins
        assert_eq!(
            match_pref(st, "Subtitle", &pick("en", "English SDH", false)),
            Some(Track::Id(2))
        );
        assert_eq!(
            match_pref(
                st,
                "Subtitle",
                &pick("zh-CN", "Chinese Simplified (ASS)", true)
            ),
            Some(Track::Id(4))
        );
        // The next episode's titles changed: within the language, prefer same embedded / external, then the lower index
        assert_eq!(
            match_pref(st, "Subtitle", &pick("eng", "别的标题", false)),
            Some(Track::Id(1))
        );
        assert_eq!(match_pref(st, "Subtitle", &pick("ja", "", false)), None);
        assert_eq!(match_pref(st, "Subtitle", &Pref::Off), Some(Track::Off));
        assert_eq!(match_pref(st, "Audio", &Pref::Off), None);
        assert_eq!(
            match_pref(st, "Audio", &pick("en", "", false)),
            Some(Track::Id(1))
        );
        assert_eq!(
            Pref::from_track(&json!({"lang": "zh-CN", "title": "简中", "external": true})),
            pick("zh-CN", "简中", true)
        );

        // No title match: the exact language string, then embedded / external, decide
        let sub = |index, lang: &str, external| MediaStream {
            index,
            kind: "Subtitle".into(),
            language: Some(lang.into()),
            title: Some(format!("t{index}")),
            is_external: external,
            ..Default::default()
        };
        let st = [
            sub(2, "chi", false),
            sub(4, "chi", true),
            sub(3, "zh-CN", true),
        ];
        assert_eq!(mpv_track(&st, "Subtitle", Some(4)), Track::Id(3));
        assert_eq!(
            match_pref(&st, "Subtitle", &pick("chi", "x", true)),
            Some(Track::Id(3))
        );
        assert_eq!(
            match_pref(&st, "Subtitle", &pick("chi", "x", false)),
            Some(Track::Id(1))
        );
    }

    #[test]
    fn language_codes() {
        for (a, b) in [
            ("eng", "en"),
            ("chi", "zh"),
            ("zho", "zh"),
            ("jpn", "ja"),
            ("kor", "ko"),
            ("fre", "fr"),
            ("fra", "fr"),
            ("ger", "de"),
            ("deu", "de"),
            ("spa", "es"),
            ("rus", "ru"),
            ("ita", "it"),
            ("por", "pt"),
            ("tha", "th"),
            ("zh-CN", "zh"),
            ("pt_BR", "pt"),
            ("VIE", "vie"),
            ("", ""),
        ] {
            assert_eq!(lang_key(a), b, "{a}");
        }
    }

    #[test]
    fn titles_and_loadfile_options() {
        let e = Item {
            name: "开端".into(),
            parent_index_number: Some(1),
            index_number: Some(2),
            ..Default::default()
        };
        assert_eq!(episode_label(&e), "S01E02 开端");
        let ep = Item {
            name: "第 1 集, 开端".into(),
            series_name: Some("庆余年".into()),
            parent_index_number: Some(2),
            index_number: Some(3),
            ..Default::default()
        };
        let t = title(&ep);
        assert_eq!(t, "庆余年 S02E03 第 1 集, 开端");
        assert_eq!(
            loadfile_opts(&t, 0.0),
            format!("force-media-title=%{}%{t}", t.len())
        );
        assert!(loadfile_opts(&t, 61.5).ends_with(",start=61.500"));
        let movie = Item {
            name: "沙丘".into(),
            ..Default::default()
        };
        assert_eq!(title(&movie), "沙丘");
    }

    /// A State not connected to mpv: the commands it writes are read from the other end of the returned pipe.
    fn offline_state(
        n: usize,
    ) -> (
        State,
        tokio::io::DuplexStream,
        mpsc::UnboundedReceiver<Event>,
    ) {
        let (ours, theirs) = tokio::io::duplex(1 << 16);
        let (_, wr) = tokio::io::split(Box::new(ours) as Box<dyn Io>);
        let (ev_tx, ev_rx) = mpsc::unbounded_channel();
        let (rep_tx, _) = mpsc::unbounded_channel();
        let (prep_tx, _) = mpsc::unbounded_channel();
        let queue = (0..n)
            .map(|k| Entry {
                item: Item {
                    id: format!("e{k}"),
                    ..Default::default()
                },
                start: 0.0,
                ready: None,
                loaded: false,
                failed: false,
                entry_id: None,
                started: false,
                stopped: false,
                preparing: false,
            })
            .collect();
        let st = State::new(
            crate::emby::tests::client(),
            queue,
            wr,
            ev_tx,
            80.0,
            rep_tx,
            prep_tx,
        );
        (st, theirs, ev_rx)
    }

    fn ready_for(id: &str) -> Ready {
        Ready {
            url: format!("https://h.example/{id}"),
            name: id.into(),
            source_id: id.into(),
            session_id: None,
            subs: Vec::new(),
            aid: Track::Auto,
            sid: Track::Auto,
            streams: Vec::new(),
        }
    }

    /// Every command the State wrote to mpv.
    async fn commands(st: State, mut mpv: tokio::io::DuplexStream) -> Vec<Value> {
        drop(st);
        let mut out = String::new();
        tokio::io::AsyncReadExt::read_to_string(&mut mpv, &mut out)
            .await
            .unwrap();
        out.lines()
            .map(|l| serde_json::from_str::<Value>(l).unwrap()["command"].clone())
            .collect()
    }

    /// 弹幕：第一集先下发设置；没填接口时说明原因、不取数；面板改设置、手动匹配会存配置；
    /// 取到的弹幕整集发给脚本（带 item，脚本丢掉对不上的）；换集后到的旧结果丢掉；地址不对之后不再请求；
    /// 设置页改了屏蔽词重新下发。
    #[tokio::test]
    async fn danmaku_messages() {
        let (mut st, mpv, mut ev) = offline_state(2);
        let dir = std::env::temp_dir().join(format!("embymini-dm-msg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        st.dm = Some(Dm {
            ctx: danmaku::Ctx {
                api: None,
                dir: dir.clone(),
                cfg: danmaku::Config::default(),
            },
            tx,
            shown: None,
            dead: None,
            told: false,
            init: false,
        });
        st.cur = Some(0);
        st.dm_show(0).await;
        st.dm_show(0).await; // init 只发一次
        let style = json!({ "on": true, "opacity": 5, "speed": 4 }).to_string();
        st.on_message(
            json!({ "event": "client-message", "args": ["embymini-danmaku-style", style] }),
        )
        .await;
        st.on_danmaku(&json!(["embymini-danmaku-pick", "77", "3"]))
            .await;
        let file = dir.join("1.json");
        std::fs::write(&file, r#"[[1.5,1,16777215,"好",1.0]]"#).unwrap();
        let loaded = danmaku::Loaded {
            list: std::fs::read_to_string(&file).unwrap(),
            title: "某剧 第1集".into(),
            manual: true,
            stale: false,
        };
        st.on_dm_result(DmMsg::Loaded(1, Ok(None))).await; // 不是当前集：丢掉
        st.on_dm_result(DmMsg::Loaded(0, Ok(Some(loaded)))).await;
        st.on_dm_result(DmMsg::Loaded(0, Err(DmFail::Setup("地址不对".into()))))
            .await;
        assert!(!st.dm_ready());
        st.dm_settings(None, vec!["剧透".into()]).await; // 设置页改了屏蔽词：重新下发
        assert!(
            st.dm.as_ref().unwrap().dead.is_none(),
            "改了设置就不再认定地址不对"
        );
        let id0 = st.queue[0].item.id.clone();

        assert!(
            matches!(ev.try_recv(), Ok(Event::DanmakuStyle { style }) if style.opacity == 1.0 && style.speed == 4)
        );
        assert!(
            matches!(ev.try_recv(), Ok(Event::DanmakuPick { pick, .. }) if pick.anime == 77 && pick.offset == 3)
        );
        let cmds: Vec<(String, Value)> = commands(st, mpv)
            .await
            .into_iter()
            .filter(|c| c[1] == "danmaku")
            .map(|c| {
                let v = serde_json::from_str(c[3].as_str().unwrap()).unwrap();
                (c[2].as_str().unwrap().to_owned(), v)
            })
            .collect();
        let names: Vec<&str> = cmds.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            [
                "init", "status", "status", "status", "load", "status", "init"
            ]
        );
        assert_eq!(cmds[1].1["kind"], "setup"); // 没填接口
        assert_eq!(
            (&cmds[1].1["start"], &cmds[1].1["item"]),
            (&json!(true), &json!(id0))
        );
        assert_eq!(
            cmds[4].1["item"], id0,
            "脚本按 item 丢掉换集前发出的晚到结果"
        );
        assert_eq!(cmds[6].1["words"], json!(["剧透"]));
        assert_eq!(cmds[4].1["list"][0][3], "好");
        assert_eq!(cmds[4].1["manual"], true);
        assert_eq!(
            (cmds[5].1["kind"].as_str(), cmds[5].1["toast"].as_bool()),
            (Some("setup"), Some(true))
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Every loadfile command the State wrote to mpv: (url, mode).
    async fn loadfiles(st: State, mpv: tokio::io::DuplexStream) -> Vec<(String, String)> {
        commands(st, mpv)
            .await
            .into_iter()
            .filter(|c| c[0] == "loadfile")
            .map(|c| {
                (
                    c[1].as_str().unwrap().to_owned(),
                    c[2].as_str().unwrap().to_owned(),
                )
            })
            .collect()
    }

    /// A background result may arrive after the user moved on: a stale one must not append or switch.
    #[tokio::test]
    async fn prepared_results_respect_where_the_user_is() {
        // Still on episode 0: episode 1's preload is appended
        let (mut st, mpv, mut ev) = offline_state(3);
        st.cur = Some(0);
        st.on_prepared(1, After::Append, Ok(ready_for("e1"))).await;
        assert_eq!(
            ev.try_recv().unwrap(),
            Event::Preloaded {
                item_id: "e1".into()
            }
        );
        assert_eq!(
            loadfiles(st, mpv).await,
            [("https://h.example/e1".into(), "append-play".into())]
        );

        // Already on episode 2: episode 1's preload is dropped; a failed preparation is only marked, not appended
        let (mut st, mpv, _ev) = offline_state(3);
        st.cur = Some(2);
        st.on_prepared(1, After::Append, Ok(ready_for("e1"))).await;
        st.cur = Some(0);
        st.on_prepared(1, After::Append, Err(Error::Other("502".into())))
            .await;
        assert!(st.queue[1].failed && !st.queue[1].loaded);
        assert!(loadfiles(st, mpv).await.is_empty());

        // Already in the playlist, or a switch is pending: not appended
        let (mut st, mpv, _ev) = offline_state(3);
        st.cur = Some(0);
        st.queue[1].loaded = true;
        st.on_prepared(1, After::Append, Ok(ready_for("e1"))).await;
        st.queue[1].loaded = false;
        st.want = Some(2);
        st.on_prepared(1, After::Append, Ok(ready_for("e1"))).await;
        assert!(loadfiles(st, mpv).await.is_empty());

        // Picked episode 1, then changed to 2: episode 1's result is dropped, only 2 is played
        let (mut st, mpv, _ev) = offline_state(3);
        st.cur = Some(0);
        st.jump(1).await;
        st.jump(2).await;
        st.on_prepared(1, After::Play, Ok(ready_for("e1"))).await;
        st.on_prepared(2, After::Play, Ok(ready_for("e2"))).await;
        assert_eq!(st.want, None);
        assert_eq!(
            loadfiles(st, mpv).await,
            [("https://h.example/e2".into(), "replace".into())]
        );
    }

    /// The next button turns green (yes) once the next episode is appended, back (no) on switching to it or a refused
    /// append; no repeats.
    #[tokio::test]
    async fn next_ready_follows_the_queue() {
        let (mut st, mpv, _ev) = offline_state(3);
        st.cur = Some(0);
        st.on_prepared(1, After::Append, Ok(ready_for("e1"))).await;
        let rid = st.rid - 1; // script-message follows loadfile
        st.on_message(json!({ "request_id": rid, "data": { "playlist_entry_id": 7 } }))
            .await;
        st.on_message(json!({ "event": "start-file", "playlist_entry_id": 7 }))
            .await;
        assert_eq!(st.cur, Some(1));
        // mpv refuses episode 2's append: still no, and it no longer counts as in the playlist
        st.on_prepared(2, After::Append, Ok(ready_for("e2"))).await;
        let rid = st.rid - 1;
        st.on_message(json!({ "request_id": rid, "error": "x" }))
            .await;
        assert!(!st.queue[2].loaded);
        let ready: Vec<String> = commands(st, mpv)
            .await
            .into_iter()
            .filter(|c| c[0] == "script-message" && c[1] == NEXT_READY_MSG)
            .map(|c| c[2].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(ready, ["yes", "no", "yes", "no"]);
    }

    /// A stuck upstream (connects, never answers) must not block mpv's event handling in the background preload:
    /// closing the player reaches mpv at once. Preparing synchronously in the loop would wait the 30-second timeout.
    #[cfg(unix)]
    #[tokio::test]
    async fn slow_upstream_does_not_block_controls() {
        // An "upstream" that accepts connections and never answers
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((sock, _)) = listener.accept().await {
                held.push(sock);
            }
        });
        let server = emby::tests::server_at("http://127.0.0.1", port);
        let emby = Emby::new(emby::http_client().unwrap(), &server, "dev");
        let (st, _, _) = offline_state(2);
        let mut queue = st.queue;
        queue[0].ready = Some(ready_for("e0"));
        let (ours, theirs) = tokio::io::duplex(1 << 16);
        let session = Session {
            emby,
            queue,
            at: 0,
            child: tokio::process::Command::new("true").spawn().unwrap(),
            io: Box::new(ours),
            ipc: std::env::temp_dir().join("embymini-slow-test.sock"),
            preload: 80.0,
            stream: None,
            danmaku: None,
        };
        let (ctl, ctl_rx) = mpsc::unbounded_channel();
        let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
        let run = tokio::spawn(session.run(ctl_rx, ev_tx));

        // Fake mpv: answers loadfile, reports start, and progress at 95% triggers a preload (to the stuck upstream)
        let (rd, mut wr) = tokio::io::split(theirs);
        let mut lines = BufReader::new(rd).lines();
        let mut say = async |v: Value| wr.write_all(format!("{v}\n").as_bytes()).await.unwrap();
        loop {
            let v: Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            if v["command"][0] == "loadfile" {
                say(json!({ "request_id": v["request_id"], "error": "success", "data": { "playlist_entry_id": 1 } })).await;
                break;
            }
        }
        say(json!({ "event": "start-file", "playlist_entry_id": 1 })).await;
        say(json!({ "event": "property-change", "name": "duration", "data": 100.0 })).await;
        say(json!({ "event": "property-change", "name": "time-pos", "data": 95.0 })).await;
        tokio::time::sleep(Duration::from_millis(300)).await; // let the preload request go out and hang

        let t0 = std::time::Instant::now();
        ctl.send(Ctl::Quit).unwrap();
        let got_quit = tokio::time::timeout(Duration::from_secs(2), async {
            while let Ok(Some(l)) = lines.next_line().await {
                if serde_json::from_str::<Value>(&l).unwrap()["command"] == json!(["quit"]) {
                    return true;
                }
            }
            false
        })
        .await;
        assert_eq!(got_quit, Ok(true), "上游卡住时，关闭播放器没有及时送到 mpv");
        eprintln!("quit 送达用时 {:?}", t0.elapsed());

        drop(lines); // the fake mpv quits, the pipe closes
        drop(wr);
        tokio::time::timeout(Duration::from_secs(5), run)
            .await
            .expect("会话没有结束")
            .unwrap();
        assert!(matches!(ev_rx.recv().await, Some(Event::Ended { .. })));
    }

    /// When mpv plays a finished episode again (playlist loop), the second time reports start and stop too.
    #[tokio::test]
    async fn replayed_entry_reports_again() {
        let (mut st, _mpv, mut ev) = offline_state(1);
        st.queue[0].ready = Some(ready_for("e0"));
        st.queue[0].entry_id = Some(7);
        for _ in 0..2 {
            st.on_message(json!({ "event": "start-file", "playlist_entry_id": 7 }))
                .await;
            st.on_message(json!({ "event": "file-loaded" })).await;
            st.on_message(json!({ "event": "end-file", "playlist_entry_id": 7, "reason": "eof" }))
                .await;
        }
        let mut got = Vec::new();
        while let Ok(e) = ev.try_recv() {
            got.push(e);
        }
        let started = got
            .iter()
            .filter(|e| matches!(e, Event::Started { .. }))
            .count();
        let stopped = got
            .iter()
            .filter(|e| matches!(e, Event::Stopped { .. }))
            .count();
        assert_eq!((started, stopped), (2, 2), "{got:?}");
    }

    fn prop(name: &str, data: Value) -> Value {
        json!({ "event": "property-change", "name": name, "data": data })
    }

    /// Script messages: next / prev / jump switch episodes, with an OSD note at either end; bad jumps are ignored.
    #[tokio::test]
    async fn script_messages_switch_episodes() {
        let msg = |args: Value| json!({ "event": "client-message", "args": args });
        let (mut st, mpv, _ev) = offline_state(3);
        for (k, e) in st.queue.iter_mut().enumerate() {
            e.ready = Some(ready_for(&format!("e{k}")));
        }
        st.cur = Some(1);
        st.queue[2].loaded = true;
        st.on_message(msg(json!([NEXT_MSG]))).await; // in the playlist: playlist-next
        for bad in ["1", "3", "x"] {
            st.on_message(msg(json!([JUMP_MSG, bad]))).await; // the current one, out of range, garbage
        }
        st.on_message(msg(json!(["embymini-other"]))).await;
        st.on_message(msg(json!([PREV_MSG]))).await; // prepared: replace at once
        st.on_message(msg(json!([NEXT_MSG]))).await; // no longer in the playlist: replace
        st.cur = Some(2);
        st.on_message(msg(json!([NEXT_MSG]))).await;
        st.cur = Some(0);
        st.on_message(msg(json!([PREV_MSG]))).await;
        st.on_message(msg(json!([JUMP_MSG, "2"]))).await;
        let got: Vec<String> = commands(st, mpv)
            .await
            .iter()
            .map(|c| match c[0].as_str().unwrap() {
                "loadfile" => format!("{} {}", c[1].as_str().unwrap(), c[2].as_str().unwrap()),
                "show-text" => c[1].as_str().unwrap().to_owned(),
                other => other.to_owned(),
            })
            .collect();
        assert_eq!(
            got,
            [
                "playlist-next",
                "正在打开：",
                "https://h.example/e0 replace",
                "正在打开：",
                "https://h.example/e2 replace",
                "没有下一集了",
                "已经是第一集",
                "正在打开：",
                "https://h.example/e2 replace",
            ]
        );
    }

    /// Preload: the next episode is prepared once playback passes the preload point (80% here), not before, not
    /// without a duration, not when it is already in the playlist or failed before.
    #[tokio::test]
    async fn progress_triggers_preload() {
        for (dur, pos, setup, want) in [
            (100.0, 79.0, "", false),
            (100.0, 80.0, "", true),
            (0.0, 50.0, "", false),
            (100.0, 90.0, "loaded", false),
            (100.0, 90.0, "failed", false),
        ] {
            let (mut st, _mpv, _ev) = offline_state(2);
            st.cur = Some(0);
            st.queue[1].loaded = setup == "loaded";
            st.queue[1].failed = setup == "failed";
            st.on_message(prop("duration", json!(dur))).await;
            st.on_message(prop("time-pos", json!(pos))).await;
            assert_eq!(st.pos, pos);
            assert_eq!(st.queue[1].preparing, want, "{dur} {pos} {setup}");
            assert!(!st.queue[0].preparing);
        }
    }

    /// Playing / Progress (pause, resume) / Stopped go to the report task with the position; nothing before the
    /// file loads, nothing twice for the same pause state.
    #[tokio::test]
    async fn playback_reports_carry_the_position() {
        let (mut st, _mpv, mut ev) = offline_state(1);
        let (tx, mut reports) = mpsc::unbounded_channel();
        st.reports = tx;
        st.queue[0].ready = Some(ready_for("e0"));
        st.queue[0].entry_id = Some(7);
        st.on_message(json!({ "event": "start-file", "playlist_entry_id": 7 }))
            .await;
        st.on_message(prop("pause", json!(true))).await; // not loaded yet: no report
        st.on_message(prop("pause", json!(false))).await;
        st.on_message(json!({ "event": "file-loaded" })).await;
        st.on_message(prop("time-pos", json!(12.5))).await;
        st.on_message(prop("pause", json!(true))).await;
        st.on_message(prop("pause", json!(true))).await;
        st.on_message(prop("pause", json!(false))).await;
        st.on_message(prop("volume", json!(55.0))).await;
        st.on_message(json!({ "event": "end-file", "playlist_entry_id": 7, "reason": "eof" }))
            .await;
        assert_eq!((st.cur, st.volume), (None, Some(55.0)));
        let mut got = Vec::new();
        while let Ok((path, b)) = reports.try_recv() {
            got.push((
                path,
                b["EventName"].as_str().unwrap_or_default().to_owned(),
                b["PositionTicks"].as_i64().unwrap(),
                b["IsPaused"].as_bool().unwrap(),
            ));
        }
        let r = |p: &str, e: &str, t, paused| (p.to_owned(), e.to_owned(), t, paused);
        assert_eq!(
            got,
            [
                r("/Sessions/Playing", "", 0, false),
                r("/Sessions/Playing/Progress", "Pause", 125_000_000, true),
                r("/Sessions/Playing/Progress", "Unpause", 125_000_000, false),
                r("/Sessions/Playing/Stopped", "", 125_000_000, false),
            ]
        );
        let mut stopped = None;
        while let Ok(e) = ev.try_recv() {
            if let Event::Stopped { position_ticks, .. } = e {
                stopped = Some(position_ticks);
            }
        }
        assert_eq!(stopped, Some(125_000_000));
    }

    /// Track changes count as the user's choice only once this episode's tracks are set (file-loaded); the next
    /// episode gets the same choice, its external subtitles added first.
    #[tokio::test]
    async fn track_choices_carry_to_the_next_episode() {
        let pi: PlaybackInfo = fixture("playbackinfo.json");
        let streams = &pi.media_sources[0].media_streams;
        let (mut st, mpv, _ev) = offline_state(2);
        for (k, e) in st.queue.iter_mut().enumerate() {
            let mut r = ready_for(&format!("e{k}"));
            r.streams = streams.clone();
            e.ready = Some(r);
            e.entry_id = Some(7 + k as i64);
        }
        st.queue[1].ready.as_mut().unwrap().subs.push(Sub {
            url: "https://h.example/sub".into(),
            file: "e1_5.ass".into(),
            title: "简中".into(),
            lang: "chi".into(),
        });
        let sdh = json!({ "lang": "en", "title": "English SDH", "external": false });
        st.on_message(prop("current-tracks/sub", sdh.clone())).await; // nothing playing
        st.on_message(json!({ "event": "start-file", "playlist_entry_id": 7 }))
            .await;
        st.on_message(prop("current-tracks/sub", sdh.clone())).await; // tracks not set yet
        st.on_message(prop("sid", json!("no"))).await;
        assert_eq!(st.pref_sub, None);
        st.on_message(json!({ "event": "file-loaded" })).await;
        st.on_message(prop("current-tracks/sub", Value::Null)).await; // unloading
        st.on_message(prop("current-tracks/audio", Value::Null))
            .await;
        assert_eq!((&st.pref_sub, &st.pref_audio), (&None, &None));
        st.on_message(prop("current-tracks/audio", json!({ "lang": "eng" })))
            .await;
        st.on_message(prop("current-tracks/sub", sdh)).await;
        st.on_message(prop("sid", json!("auto"))).await; // not "off"
        assert!(matches!(&st.pref_sub, Some(Pref::Pick { title, .. }) if title == "English SDH"));
        assert!(matches!(&st.pref_audio, Some(Pref::Pick { lang, .. }) if lang == "eng"));
        st.on_message(json!({ "event": "end-file", "playlist_entry_id": 7, "reason": "eof" }))
            .await;
        st.on_message(json!({ "event": "start-file", "playlist_entry_id": 8 }))
            .await;
        st.on_message(json!({ "event": "file-loaded" })).await;
        st.on_message(prop("sid", json!(false))).await;
        assert_eq!(st.pref_sub, Some(Pref::Off));
        let set: Vec<Value> = commands(st, mpv)
            .await
            .into_iter()
            .filter(|c| c[0] == "sub-add" || c[0] == "set_property")
            .collect();
        assert_eq!(
            set,
            [
                json!(["sub-add", "https://h.example/sub", "auto", "简中", "chi"]),
                json!(["set_property", "aid", 1]),
                json!(["set_property", "sid", 2]),
            ]
        );
    }

    /// The cache keeps this episode and the next one already in mpv's playlist, nothing else; the upstream speed
    /// goes to the top bar.
    #[tokio::test(flavor = "multi_thread")]
    async fn cache_keeps_current_and_next() {
        let dir = std::env::temp_dir().join(format!("embymini-keep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (mut st, mpv, _ev) = offline_state(3);
        st.stream = Stream::start(&dir, false);
        let v = dir.join("video");
        for (k, e) in st.queue.iter_mut().enumerate() {
            e.ready = Some(ready_for(&format!("e{k}")));
            std::fs::create_dir_all(v.join(format!("e{k}"))).unwrap();
        }
        st.cur = Some(1);
        st.queue[2].loaded = true;
        st.keep_cache();
        for _ in 0..60 {
            if !v.join("e0").exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(!v.join("e0").exists());
        assert!(v.join("e1").exists() && v.join("e2").exists());
        st.send_speed().await;
        assert_eq!(
            commands(st, mpv).await,
            [json!(["script-message", SPEED_MSG, "0"])]
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// An entry that errors out after it started still reports Stopped; going back to a failed entry
    /// fetches PlaybackInfo afresh instead of reusing the old link and play session.
    #[tokio::test]
    async fn failed_entry_stops_and_refetches() {
        let (mut st, _mpv, mut ev) = offline_state(2);
        let mut r = ready_for("e0");
        r.url = "http://127.0.0.1:1/e0".into(); // probe is refused: no status
        st.queue[0].ready = Some(r);
        st.queue[0].entry_id = Some(7);
        st.on_message(json!({ "event": "start-file", "playlist_entry_id": 7 }))
            .await;
        st.on_message(json!({ "event": "file-loaded" })).await;
        st.on_message(json!({ "event": "end-file", "playlist_entry_id": 7, "reason": "error", "file_error": "boom" }))
            .await;
        let mut got = Vec::new();
        while let Ok(e) = ev.try_recv() {
            got.push(e);
        }
        assert!(
            got.iter().any(|e| matches!(e, Event::Stopped { .. })),
            "{got:?}"
        );
        assert!(
            got.iter().any(|e| matches!(e, Event::Failed { message, .. } if message.contains("mpv 无法打开（boom）"))),
            "{got:?}"
        );
        assert_eq!(st.cur, None);
        st.jump(0).await;
        assert_eq!(st.want, Some(0), "a failed entry must be prepared again");

        // Never loaded at all: nothing to report, but its session is spent too
        let mut r = ready_for("e1");
        r.url = "http://127.0.0.1:1/e1".into();
        st.queue[1].ready = Some(r);
        st.queue[1].entry_id = Some(8);
        st.on_message(json!({ "event": "start-file", "playlist_entry_id": 8 }))
            .await;
        st.on_message(json!({ "event": "end-file", "playlist_entry_id": 8, "reason": "error", "file_error": "boom" }))
            .await;
        assert!(
            st.queue[1].ready.is_some(),
            "kept: the session end stops its download"
        );
        assert_eq!(st.cur, None);
        st.jump(1).await;
        assert_eq!(st.want, Some(1));
    }

    #[test]
    fn failure_reasons() {
        assert_eq!(fail_reason(Some(403), "x"), "服务器拒绝访问（403）");
        assert_eq!(fail_reason(Some(404), "x"), "服务器上找不到（404）");
        assert_eq!(
            fail_reason(Some(502), "x"),
            "服务器暂时不可用，稍后再试（502）"
        );
        assert_eq!(fail_reason(Some(206), "x"), "mpv 无法打开（x）");
        assert_eq!(fail_reason(None, "x"), "mpv 无法打开（x）");
    }

    #[test]
    fn event_json_shape() {
        let v = serde_json::to_value(Event::Stopped {
            item_id: "1".into(),
            position_ticks: 5,
        })
        .unwrap();
        assert_eq!(
            v,
            json!({ "type": "stopped", "itemId": "1", "positionTicks": 5 })
        );
    }

    /// 跑一次自带 mpv 的检查结果。
    #[derive(Debug, PartialEq)]
    struct MpvRun {
        config: bool,
        modernz: bool,
        thumbfast: bool,
        builtin_osc: bool,
        /// mpv 日志里的错误行（`[e]`），Windows 上应为空（配置写错、选项不存在都会出现在这里）。
        errors: Vec<String>,
    }

    fn bundled_mpv_run(config_dir: &Path) -> MpvRun {
        let exe = if cfg!(windows) {
            config_dir.join("mpv.exe") // CI 在 cargo test 之前已把 mpv.exe 下载到 src-tauri/mpv/
        } else {
            PathBuf::from("mpv")
        };
        let log = std::env::temp_dir().join(format!("embymini-cfg-{}.log", std::process::id()));
        let status = std::process::Command::new(&exe)
            .arg(format!("--config-dir={}", config_dir.display()))
            .arg(format!("--log-file={}", log.display()))
            .args(["--vo=null", "--ao=null", "--force-window=no", "--idle=no"])
            .arg("av://lavfi:testsrc=duration=1")
            .status()
            .unwrap_or_else(|e| panic!("启动 {} 失败：{e}", exe.display()));
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        let _ = std::fs::remove_file(&log);
        assert!(status.success(), "mpv 退出码 {status}；日志：\n{text}");
        MpvRun {
            config: text.contains("Reading config file") && text.contains("mpv.conf"),
            modernz: text.contains("[modernz] Loading lua script"),
            thumbfast: text.contains("[thumbfast] Loading lua script"),
            builtin_osc: text.contains("@osc.lua"),
            errors: text
                .lines()
                .filter(|l| l.contains("][e]["))
                .map(str::to_owned)
                .collect(),
        }
    }

    /// 个人配置要覆盖自带配置，EmbyMini 必须控制的项（如 UA）又不能被它改掉。走真实的 [`mpv_args`]。
    #[test]
    #[cfg_attr(not(windows), ignore = "需要 PATH 里的 mpv；Windows CI 上默认运行")]
    fn user_conf_overrides_bundled() {
        let dir = std::env::temp_dir().join(format!("embymini-user-conf-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let conf = dir.join("mpv.conf");
        // 自带配置是 1GiB；个人配置改成 123MiB，同时试图改 UA
        std::fs::write(&conf, "demuxer-max-bytes=123MiB\nuser-agent=Other/1.0\n").unwrap();
        let bundled =
            dunce::simplified(&std::fs::canonicalize(env!("CARGO_MANIFEST_DIR")).unwrap())
                .join("mpv");
        let log = dir.join("mpv.log");
        let opts = Options {
            mpv: if cfg!(windows) {
                bundled.join("mpv.exe")
            } else {
                "mpv".into()
            },
            user_conf: Some(conf),
            ..test_opts(vec![
                "--vo=null".into(),
                "--ao=null".into(),
                "--force-window=no".into(),
                "--idle=no".into(),
                format!("--log-file={}", log.display()),
                "--term-playing-msg=EMBYMINI-CHECK ${demuxer-max-bytes} ${user-agent}".into(),
                "av://lavfi:testsrc=duration=1".into(),
            ])
        };
        let opts = Options {
            config_dir: Some(bundled),
            ..opts
        };
        let args = mpv_args(&opts, &dir.join("ipc"));
        let status = std::process::Command::new(&opts.mpv)
            .args(&args)
            .status()
            .unwrap();
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(status.success(), "{text}");
        // 取展开后的那行（日志开头还会原样记一遍命令行，那行里是 `${…}`）
        let line = text
            .lines()
            .find(|l| l.contains("EMBYMINI-CHECK") && !l.contains("${"))
            .expect("没有输出检查行");
        eprintln!("{line}");
        assert!(
            line.contains("123"),
            "个人配置没有覆盖自带的缓冲大小：{line}"
        );
        assert!(line.contains(emby::UA), "UA 被个人配置改掉了：{line}");
    }

    /// 把自带配置目录整个复制到 `to`（模拟装在别的路径下）。
    fn copy_dir(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).unwrap();
        for e in std::fs::read_dir(from).unwrap() {
            let p = e.unwrap().path();
            let dst = to.join(p.file_name().unwrap());
            if p.is_dir() {
                copy_dir(&p, &dst);
            } else {
                std::fs::copy(&p, &dst).unwrap();
            }
        }
    }

    /// 安装后的真实路径链：资源目录（Windows 上带 `\\?\`）→ bundled_dir → mpv 读到自带配置、加载 ModernZ、
    /// 不加载自带 OSC、配置没有报错（d3d11-adapter 在没有 N 卡的机器上必报错，除外）。另测一次装在「中文 + 空格」目录下（中文用户名时 %LOCALAPPDATA% 就是这样）。
    #[test]
    #[cfg_attr(not(windows), ignore = "需要 PATH 里的 mpv；Windows CI 上默认运行")]
    fn bundled_config_loads() {
        let check = |resource: &Path| {
            eprintln!("资源目录：{}", resource.display());
            if cfg!(windows) {
                assert!(
                    resource.to_string_lossy().starts_with(r"\\?\"),
                    r"前提不成立：canonicalize 没加 \\?\ 前缀"
                );
                // 对照组：不去前缀时 mpv 的表现。只打印，不作为通过条件
                eprintln!(
                    r"  对照（带 \\?\）：{:?}",
                    bundled_mpv_run(&resource.join("mpv"))
                );
            }
            let dir = bundled_dir(resource).expect("资源目录里没有 mpv/");
            assert!(
                !dir.to_string_lossy().starts_with(r"\\?\"),
                "{}",
                dir.display()
            );
            let got = bundled_mpv_run(&dir);
            eprintln!("  修复后（{}）：{got:?}", dir.display());
            assert!(
                got.config && got.modernz && got.thumbfast && !got.builtin_osc,
                "自带配置没生效：{got:?}"
            );
            if cfg!(windows) {
                // d3d11-adapter=NVIDIA 在没有 N 卡的机器上（CI）必报错，取决于机器而不是配置写错，放过；
                // Linux 的 mpv 没有这个选项，所以只在 Windows 上要求其余零错误
                let errors: Vec<_> = got
                    .errors
                    .iter()
                    .filter(|l| !l.contains("d3d11-adapter") && !l.contains("No adapter matching"))
                    .collect();
                assert!(errors.is_empty(), "mpv 报错：{errors:#?}");
            }
        };
        // 和 Tauri 一样用 canonicalize 得到资源目录（Windows 上会带 \\?\ 前缀）
        check(&std::fs::canonicalize(env!("CARGO_MANIFEST_DIR")).unwrap());

        let odd = std::env::temp_dir().join(format!("EmbyMini 测试 {}", std::process::id()));
        copy_dir(
            &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("mpv"),
            &odd.join("mpv"),
        );
        check(&std::fs::canonicalize(&odd).unwrap());
        std::fs::remove_dir_all(&odd).unwrap();
    }

    // ---- 真服端到端：无头 mpv 跑完「起播 → 到进度预加载 → 自动切下一集 → 退出」，并核对、恢复服务端观看记录 ----

    fn env(k: &str) -> String {
        std::env::var(k).unwrap_or_else(|_| panic!("缺环境变量 {k}"))
    }

    async fn playable(c: &Emby, item: &Item) -> bool {
        match prepare(c, item).await {
            Ok(r) => c.probe(&r.url).await == Some(206), // probe 带 token 请求头
            Err(_) => false,
        }
    }

    /// 选一集「本集与下一集都能拉流」的剧集（服务器上个别条目 PlaybackInfo 正常、直链却 403）。可用 EMBY_TEST_ITEM 指定。
    async fn pick_pair(c: &Emby) -> (Item, Item) {
        let ids: Vec<String> = match std::env::var("EMBY_TEST_ITEM") {
            Ok(id) => vec![id],
            Err(_) => {
                let q = [("MediaTypes", "Video"), ("Limit", "30")];
                let v = c
                    .call(
                        Method::GET,
                        &format!("/Users/{}/Items/Resume", c.user_id),
                        &q,
                        None,
                    )
                    .await
                    .unwrap();
                v["Items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|i| i["Type"] == "Episode")
                    .filter_map(|i| i["Id"].as_str().map(str::to_owned))
                    .collect()
            }
        };
        for id in ids {
            let it = c.item(&id).await.unwrap();
            let (Some(series), Some(season)) = (&it.series_id, &it.season_id) else {
                continue;
            };
            let eps = c.episodes(series, season).await.unwrap();
            let Some(next) = eps
                .iter()
                .position(|e| e.id == it.id)
                .and_then(|i| eps.get(i + 1))
                .cloned()
            else {
                continue;
            };
            if playable(c, &it).await && playable(c, &next).await {
                return (it, next);
            }
        }
        panic!("没有找到「本集与下一集都能播放」的剧集，用 EMBY_TEST_ITEM 指定");
    }

    /// 把观看记录恢复成测试前的样子（只用标准接口：PlayedItems + Sessions/Playing/Stopped）。
    /// 返回没恢复成功的原因（不在这里 panic，免得后面的条目来不及恢复）。
    async fn restore(c: &Emby, before: &Item) -> Option<String> {
        let path = format!("/Users/{}/PlayedItems/{}", c.user_id, before.id);
        if before.user_data.played {
            c.call(Method::POST, &path, NO_QUERY, None).await.unwrap();
        } else {
            c.call(Method::DELETE, &path, NO_QUERY, None).await.unwrap(); // 同时清零进度
            let p = before.user_data.playback_position_ticks;
            if p > 0 {
                // 只发 Stopped 服务端不记进度（2026-10-13 实测），要走一遍带 PlaySessionId 的 Playing → Stopped
                let r = prepare(c, before).await.unwrap();
                let body = json!({
                    "ItemId": before.id,
                    "MediaSourceId": r.source_id,
                    "PlaySessionId": r.session_id,
                    "PositionTicks": p,
                });
                for what in ["", "/Stopped"] {
                    c.call(
                        Method::POST,
                        &format!("/Sessions/Playing{what}"),
                        NO_QUERY,
                        Some(&body),
                    )
                    .await
                    .unwrap();
                }
            }
        }
        let now = c.item(&before.id).await.unwrap().user_data;
        let diff = (now.playback_position_ticks - before.user_data.playback_position_ticks).abs();
        (now.played != before.user_data.played || diff >= 20 * 10_000_000).then(|| {
            format!(
                "{} 观看记录没恢复：已看 {} → {}，进度 {} → {}",
                before.id,
                before.user_data.played,
                now.played,
                before.user_data.playback_position_ticks,
                now.playback_position_ticks
            )
        })
    }

    /// 用环境变量里的账号登录真服。
    async fn login_from_env() -> Emby {
        use crate::config::{Server, normalize};
        let port = std::env::var("PORT").ok().and_then(|p| p.parse().ok());
        let (url, port) = normalize(&env("EMBY_SERVER"), port).unwrap();
        let mut s = Server {
            id: "e2e".into(),
            name: String::new(),
            url,
            port,
            user: env("EMBY_USER"),
            user_id: String::new(),
            token: String::new(),
        };
        let http = emby::http_client().unwrap();
        (s.user_id, s.token) = emby::login(
            &http,
            &s.api_base(),
            "embymini-e2e",
            &s.user,
            &env("EMBY_PASS"),
        )
        .await
        .unwrap();
        Emby::new(http, &s, "embymini-e2e")
    }

    /// PATH 里的 mpv + 仓库里的自带配置。
    fn test_opts(extra_args: Vec<String>) -> Options {
        Options {
            mpv: "mpv".into(),
            config_dir: Some(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("mpv")),
            preload_percent: 90.0,
            volume: None,
            cache_dir: None,
            user_conf: None,
            stream: None,
            danmaku: None,
            extra_args,
        }
    }

    fn headless(extra: &[String]) -> Options {
        let base = ["--vo=null", "--ao=null", "--force-window=no"].map(String::from);
        test_opts(base.into_iter().chain(extra.iter().cloned()).collect())
    }

    /// 和正式程序一样经本地缓存代理播放（缓存放临时目录，测完删）。只缓存前方：整片模式会在测试的一两分钟里
    /// 把整集从中继拉下来（每次几 GB 流量），整片的逻辑由 stream.rs 的单测覆盖。
    fn proxied(opts: Options, tag: &str) -> (Options, PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("embymini-e2e-cache-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let stream = Some(Stream::start(&dir, false).expect("缓存代理起不来"));
        (Options { stream, ..opts }, dir)
    }

    /// 本进程起的、还活着的 mpv（按命令行里本进程专属的 IPC 名字找）。
    #[cfg(target_os = "linux")]
    fn our_mpvs() -> usize {
        let tag = format!("embymini-{}-", std::process::id());
        std::fs::read_dir("/proc")
            .unwrap()
            .flatten()
            .filter_map(|e| std::fs::read(e.path().join("cmdline")).ok())
            .filter(|c| String::from_utf8_lossy(c).contains(&tag))
            .count()
    }

    /// 起播途中放弃（程序退出时丢掉起播的 future）：mpv 窗口已经开着，必须随之被杀掉，不能留下空窗口。
    /// 上游只收连接不回话，起播一直卡在取 PlaybackInfo。
    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "需要 PATH 里的 mpv"]
    async fn abandoned_start_kills_mpv() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((sock, _)) = listener.accept().await {
                held.push(sock);
            }
        });
        let server = emby::tests::server_at("http://127.0.0.1", port);
        let emby = Emby::new(emby::http_client().unwrap(), &server, "dev");
        let starting = start(emby, "1", None, false, headless(&[]));
        // 1.5 秒后还在起播（上游不回话），这时 mpv 已经起来了；然后放弃
        let r = tokio::time::timeout(Duration::from_millis(1500), async {
            let mut starting = Box::pin(starting);
            tokio::select! {
                _ = &mut starting => panic!("上游不回话，起播不该完成"),
                _ = tokio::time::sleep(Duration::from_millis(1200)) => {}
            }
            assert_eq!(our_mpvs(), 1, "起播途中 mpv 应该已经在运行");
            drop(starting);
        })
        .await;
        assert!(r.is_ok());
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(our_mpvs(), 0, "放弃起播后 mpv 应该被杀掉");
    }

    /// 测试自己另开一个 IPC 连接给 mpv 发命令，返回 data。
    async fn mpv_cmd(ipc: &Path, cmd: Value) -> Value {
        let io = open(ipc).await.unwrap();
        let (rd, mut wr) = tokio::io::split(io);
        let line = json!({ "command": cmd, "request_id": 777 });
        wr.write_all(format!("{line}\n").as_bytes()).await.unwrap();
        let mut lines = BufReader::new(rd).lines();
        while let Some(l) = lines.next_line().await.unwrap() {
            let v: Value = serde_json::from_str(&l).unwrap();
            if v["request_id"] == 777 {
                return v["data"].clone();
            }
        }
        panic!("mpv 没回 {line}");
    }

    async fn get_property(ipc: &Path, name: &str) -> Value {
        mpv_cmd(ipc, json!(["get_property", name])).await
    }

    /// 上游坏条目（PlaybackInfo 正常、直链 403）：应收到带 403 原因的 Failed，且不回传 Stopped。
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "需要真服账号、PATH 里的 mpv，以及 EMBY_BROKEN_ITEM（一个直链 403 的条目）"]
    async fn e2e_broken_item() {
        let c = login_from_env().await;
        let id = env("EMBY_BROKEN_ITEM");
        // 上游修好的条目一播就会改观看记录，先探测，能播就停
        assert!(
            !playable(&c, &c.item(&id).await.unwrap()).await,
            "{id} 已经能播放了（上游修好了），换一个直链 403 的条目"
        );
        let (opts, cache) = proxied(headless(&[]), "broken");
        let session = start(c, &id, None, true, opts).await.unwrap();
        let (_ctl, ctl_rx) = mpsc::unbounded_channel();
        let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
        tokio::spawn(session.run(ctl_rx, ev_tx));
        let mut seen = Vec::new();
        while let Ok(Some(ev)) = tokio::time::timeout(Duration::from_secs(60), ev_rx.recv()).await {
            let end = matches!(ev, Event::Ended { .. });
            seen.push(ev);
            if end {
                break;
            }
        }
        eprintln!("事件序列：{seen:?}");
        assert!(
            matches!(&seen[..], [Event::Failed { item_id, message }, .., Event::Ended { .. }] if *item_id == id && message.contains("403")),
            "期望 Failed(403) … Ended"
        );
        assert!(
            !seen
                .iter()
                .any(|e| matches!(e, Event::Started { .. } | Event::Stopped { .. })),
            "坏条目不应回传开播/停止"
        );
        let _ = std::fs::remove_dir_all(cache);
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "需要真服账号（EMBY_SERVER / PORT / EMBY_USER / EMBY_PASS）和 PATH 里的 mpv"]
    async fn e2e_real_server() {
        let c = login_from_env().await;

        let (ep1, ep2) = pick_pair(&c).await;
        let before = [
            c.item(&ep1.id).await.unwrap(),
            c.item(&ep2.id).await.unwrap(),
        ];
        let log = std::env::temp_dir().join(format!("embymini-e2e-{}.log", std::process::id()));
        let (opts, cache) = proxied(headless(&[format!("--log-file={}", log.display())]), "real");
        let session = start(c.clone(), &ep1.id, None, true, opts).await.unwrap();
        let side = session.ipc.clone(); // 测试自己另开一个 IPC 连接读 mpv 属性
        let (ctl, ctl_rx) = mpsc::unbounded_channel();
        let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
        let run = tokio::spawn(session.run(ctl_rx, ev_tx));
        let mut seen = Vec::new();
        let mut next = async || {
            let ev = tokio::time::timeout(Duration::from_secs(120), ev_rx.recv())
                .await
                .expect("等待播放事件超时")
                .expect("事件通道关闭");
            seen.push(ev.clone());
            ev
        };

        assert_eq!(
            next().await,
            Event::Started {
                item_id: ep1.id.clone()
            }
        );
        // 用户在第 1 集关掉字幕：第 2 集应沿用
        ctl.send(Ctl::Mpv(json!(["set_property", "sid", "no"])))
            .unwrap();
        tokio::time::sleep(Duration::from_secs(1)).await;
        ctl.send(Ctl::Mpv(json!(["seek", 99, "absolute-percent"])))
            .unwrap();
        assert_eq!(
            next().await,
            Event::Preloaded {
                item_id: ep2.id.clone()
            }
        );
        match next().await {
            Event::Stopped {
                item_id,
                position_ticks,
            } => {
                assert_eq!(item_id, ep1.id);
                assert!(
                    position_ticks as f64 >= ep1.run_time_ticks.unwrap_or(0) as f64 * 0.9,
                    "停止位置太早：{position_ticks}"
                );
            }
            e => panic!("期望 ep1 自然播完，实际 {e:?}"),
        }
        // 播完就查（回传在单独的任务里发，等它一下）：后面「上一集」会从头再播 ep1，有的服务器会因此清掉已看
        tokio::time::sleep(Duration::from_secs(2)).await;
        let ep1_played = c.item(&ep1.id).await.unwrap().user_data.played;
        assert_eq!(
            next().await,
            Event::Started {
                item_id: ep2.id.clone()
            },
            "没有自动切到下一集"
        );
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert_eq!(
            get_property(&side, "sid").await,
            json!(false),
            "第 2 集没沿用「关字幕」"
        );

        // 上一集：停掉第 2 集，从头重新播第 1 集
        ctl.send(Ctl::Mpv(json!(["script-message", PREV_MSG])))
            .unwrap();
        assert!(matches!(next().await, Event::Stopped { ref item_id, .. } if *item_id == ep2.id));
        assert_eq!(
            next().await,
            Event::Started {
                item_id: ep1.id.clone()
            },
            "上一集没切回第 1 集"
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
        ctl.send(Ctl::Quit).unwrap();
        assert!(matches!(next().await, Event::Stopped { ref item_id, .. } if *item_id == ep1.id));
        assert!(
            matches!(next().await, Event::Ended { volume: Some(_) }),
            "退出时没带回音量"
        );
        run.await.unwrap();

        let mpv_log = std::fs::read_to_string(&log).unwrap_or_default();
        let _ = std::fs::remove_file(&log);
        eprintln!("缓存：{} MB", crate::stream::dir_size(&cache) >> 20);
        let _ = std::fs::remove_dir_all(&cache);
        let mut bad = Vec::new();
        for b in &before {
            bad.extend(restore(&c, b).await);
        }
        assert!(bad.is_empty(), "{bad:?}");
        assert!(ep1_played, "ep1 播完后服务端没有标记已看");
        assert!(
            !mpv_log.contains("X-Emby-Token"),
            "经缓存代理时 mpv 不该拿到 token"
        );
        eprintln!("事件序列：{seen:?}");
        eprintln!(
            "mpv 预取下一集：{}",
            if mpv_log.contains("Prefetching") {
                "是"
            } else {
                "否（当前集未读到末尾前不会预取）"
            }
        );
    }

    /// 弹幕端到端：真服 + 真弹幕接口（`DANMU_API`），起播后弹幕自动匹配、加载进脚本；给了 SHOT_DIR 就截弹幕和设置面板两张图。
    /// 选片用 `EMBY_TEST_ITEM`（最好是国产剧 / 动画，能匹配上），不给就随便挑一集（可能匹配不上，只验证流程不出错）。
    /// 无显示器时同 [`e2e_ui_shots`]。
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "需要真服账号、弹幕接口、DISPLAY 和 PATH 里的 mpv"]
    async fn e2e_danmaku() {
        let c = login_from_env().await;
        let id = match std::env::var("EMBY_TEST_ITEM") {
            Ok(id) => id,
            Err(_) => pick_pair(&c).await.0.id,
        };
        let before = c.item(&id).await.unwrap();
        let dir = std::env::temp_dir().join(format!("embymini-e2e-dm-{}", std::process::id()));
        let mut opts = test_opts(
            std::env::var("MPV_EXTRA")
                .map(|a| a.split_whitespace().map(String::from).collect())
                .unwrap_or_default(),
        );
        opts.danmaku = Some(danmaku::Ctx {
            api: danmaku::Api::new(crate::emby::http_client().unwrap(), &env("DANMU_API")),
            dir: dir.clone(),
            cfg: danmaku::Config::default(),
        });
        let (opts, cache) = proxied(opts, "danmaku");
        let session = start(c.clone(), &id, None, false, opts).await.unwrap();
        let ipc = session.ipc.clone();
        let (ctl, ctl_rx) = mpsc::unbounded_channel();
        let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
        let run = tokio::spawn(session.run(ctl_rx, ev_tx));
        let ev = tokio::time::timeout(Duration::from_secs(120), ev_rx.recv()).await;
        assert!(matches!(ev, Ok(Some(Event::Started { .. }))), "{ev:?}");
        let mut state = Value::Null;
        for _ in 0..120 {
            tokio::time::sleep(Duration::from_millis(500)).await;
            state = get_property(&ipc, "user-data/embymini/danmaku").await;
            if matches!(
                state["kind"].as_str(),
                Some("ok" | "none" | "error" | "setup")
            ) {
                break;
            }
        }
        println!("弹幕：{state}");
        assert!(
            matches!(state["kind"].as_str(), Some("ok" | "none")),
            "{state}"
        );
        if let Ok(shots) = std::env::var("SHOT_DIR") {
            let shot = async |name: &str| {
                let f = PathBuf::from(&shots).join(format!("{name}.png"));
                mpv_cmd(
                    &ipc,
                    json!(["screenshot-to-file", f.to_string_lossy(), "window"]),
                )
                .await;
            };
            tokio::time::sleep(Duration::from_secs(6)).await;
            shot("dm-1-play").await;
            mpv_cmd(&ipc, json!(["keypress", "D"])).await;
            tokio::time::sleep(Duration::from_millis(800)).await;
            shot("dm-2-panel").await;
        }
        ctl.send(Ctl::Quit).unwrap();
        run.await.unwrap();
        let _ = std::fs::remove_dir_all(cache);
        let _ = std::fs::remove_dir_all(dir);
        if let Some(e) = restore(&c, &before).await {
            panic!("{e}");
        }
    }

    /// 播放界面截图 + 防泄露检查：真流程起播（有窗口），模拟常用操作并截图到 SHOT_DIR，
    /// 同时断言 mpv 能显示出来的地址（path、播放列表、外挂字幕）里都没有 token。
    /// 无显示器时：`Xvfb :98 -screen 0 2560x1600x24 &`，`DISPLAY=:98 MPV_EXTRA=--gpu-sw`。
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "需要真服账号、DISPLAY、SHOT_DIR 和 PATH 里的 mpv"]
    async fn e2e_ui_shots() {
        let c = login_from_env().await;
        let (ep1, ep2) = pick_pair(&c).await;
        let before = [
            c.item(&ep1.id).await.unwrap(),
            c.item(&ep2.id).await.unwrap(),
        ];
        let dir = PathBuf::from(env("SHOT_DIR"));
        let (opts, cache) = proxied(
            test_opts(
                std::env::var("MPV_EXTRA")
                    .map(|a| a.split_whitespace().map(String::from).collect())
                    .unwrap_or_default(),
            ),
            "shots",
        );
        let session = start(c.clone(), &ep1.id, None, true, opts).await.unwrap();
        let ipc = session.ipc.clone();
        let (ctl, ctl_rx) = mpsc::unbounded_channel();
        let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
        let run = tokio::spawn(session.run(ctl_rx, ev_tx));
        let mut next = async || {
            tokio::time::timeout(Duration::from_secs(120), ev_rx.recv())
                .await
                .expect("等待播放事件超时")
                .expect("事件通道关闭")
        };
        let pause = |ms| tokio::time::sleep(Duration::from_millis(ms));
        let shot = async |name: &str| {
            let f = dir.join(format!("{name}.png"));
            mpv_cmd(
                &ipc,
                json!(["screenshot-to-file", f.to_string_lossy(), "window"]),
            )
            .await;
        };
        let key = async |k: &str| {
            mpv_cmd(&ipc, json!(["keypress", k])).await;
        };

        assert_eq!(
            next().await,
            Event::Started {
                item_id: ep1.id.clone()
            }
        );
        pause(1500).await;
        shot("01-start").await;
        // 第 1 集的外挂字幕地址（换集后轨道列表就是第 2 集的了）
        let ep1_tracks = get_property(&ipc, "track-list").await;
        pause(3000).await;
        key("RIGHT").await; // 加载后的第一次 seek：也要出进度条
        pause(500).await;
        shot("02-first-seek").await;
        key("UP").await;
        pause(300).await;
        shot("03-volume").await;
        key("F8").await;
        pause(1000).await;
        shot("04-episode-menu").await;
        key("DOWN").await;
        key("ENTER").await; // 选集菜单选下一集
        assert!(matches!(next().await, Event::Stopped { ref item_id, .. } if *item_id == ep1.id));
        assert_eq!(
            next().await,
            Event::Started {
                item_id: ep2.id.clone()
            },
            "选集菜单没切到下一集"
        );
        pause(2500).await;
        shot("05-after-jump").await;
        key("i").await;
        pause(800).await;
        shot("06-stats").await;

        let mut shown = vec![get_property(&ipc, "path").await];
        shown.extend(
            get_property(&ipc, "playlist")
                .await
                .as_array()
                .unwrap()
                .iter()
                .map(|e| e["filename"].clone()),
        );
        for tracks in [ep1_tracks, get_property(&ipc, "track-list").await] {
            shown.extend(
                tracks
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|t| t["external-filename"].clone()),
            );
        }
        ctl.send(Ctl::Quit).unwrap();
        while !matches!(next().await, Event::Ended { .. }) {}
        run.await.unwrap();
        let mut bad = Vec::new();
        for b in &before {
            bad.extend(restore(&c, b).await);
        }
        assert!(bad.is_empty(), "{bad:?}");
        // token 本身和 api_key 参数都不能出现（断言信息里不打印 token）
        let header = c.token_header();
        let token = header.trim_start_matches("X-Emby-Token: ");
        assert!(!token.is_empty());
        let leaked = shown
            .iter()
            .map(Value::to_string)
            .filter(|v| v.contains(token) || v.contains("api_key"))
            .count();
        assert_eq!(leaked, 0, "mpv 能显示出来的地址里有 {leaked} 处带 token");
        // 视频和外挂字幕都经缓存代理（path 在第一个；内嵌轨的 external-filename 是 null）
        let direct = shown
            .iter()
            .filter_map(Value::as_str)
            .filter(|u| !u.starts_with("http://127.0.0.1:"))
            .count();
        assert!(
            shown[0].is_string() && direct == 0,
            "有 {direct} 个地址没经缓存代理"
        );
        let subs = shown
            .iter()
            .filter(|v| v.as_str().is_some_and(|u| u.contains("/s/")))
            .count();
        if subs == 0 {
            // Only the episode's own subtitles count: with none, this check proved nothing
            eprintln!(
                "注意：这一集没有外挂字幕，「字幕经缓存代理」没验到；换一集有外挂字幕的（EMBY_TEST_ITEM）"
            );
        } else {
            eprintln!("外挂字幕 {subs} 条经缓存代理");
        }
        let _ = std::fs::remove_dir_all(cache);
        eprintln!("截图在 {}；检查过的地址 {} 个", dir.display(), shown.len());
    }

    /// 缓存代理验收（整片模式，真服 + 无头 mpv）：后台整片下载、拖到没缓存 / 已缓存的位置各要多久、
    /// 关掉播放器后不再下载、重开同一集从缓存起播。会经中继下几百 MB。
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "需要真服账号和 PATH 里的 mpv；整片模式会下几百 MB"]
    async fn e2e_cache_proxy() {
        let c = login_from_env().await;
        let (ep1, ep2) = pick_pair(&c).await;
        let before = [
            c.item(&ep1.id).await.unwrap(),
            c.item(&ep2.id).await.unwrap(),
        ];
        let dir =
            std::env::temp_dir().join(format!("embymini-e2e-cache-whole-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let stream = Stream::start(&dir, true).unwrap();
        let blocks = || {
            std::fs::read_dir(dir.join("video"))
                .into_iter()
                .flatten()
                .flatten()
                .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
                .flat_map(|e| std::fs::read_dir(e.path()).into_iter().flatten().flatten())
                .filter(|e| e.file_name().to_string_lossy().parse::<u64>().is_ok())
                .count()
        };
        let play = async || {
            let opts = Options {
                stream: Some(stream.clone()),
                ..headless(&[])
            };
            let t0 = Instant::now();
            let session = start(c.clone(), &ep1.id, None, true, opts).await.unwrap();
            let ipc = session.ipc.clone();
            let (ctl, ctl_rx) = mpsc::unbounded_channel();
            let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
            let run = tokio::spawn(session.run(ctl_rx, ev_tx));
            assert!(matches!(ev_rx.recv().await, Some(Event::Started { .. })));
            (t0.elapsed(), ipc, ctl, ev_rx, run)
        };
        // 拖到 at 秒，等 time-pos 走过 at 开始前进
        let seek = async |ipc: &Path, at: f64| {
            let t0 = Instant::now();
            mpv_cmd(ipc, json!(["seek", at, "absolute"])).await;
            loop {
                let pos = get_property(ipc, "time-pos").await.as_f64().unwrap_or(0.0);
                if pos >= at + 0.5 {
                    return t0.elapsed();
                }
                assert!(
                    t0.elapsed() < Duration::from_secs(60),
                    "拖到 {at} 秒一分钟没出画面"
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        };

        let (first, ipc, ctl, mut ev_rx, run) = play().await;
        let dur = get_property(&ipc, "duration").await.as_f64().unwrap();
        let up0 = stream.up_bytes();
        tokio::time::sleep(Duration::from_secs(8)).await;
        let rate = (stream.up_bytes() - up0) as f64 / 8.0 / 1048576.0;
        eprintln!(
            "起播 {first:?}；后台下载 {rate:.1} MB/s，已缓存 {} 块",
            blocks()
        );
        assert!(rate > 0.5, "整片模式没在后台下载");
        let far = seek(&ipc, dur * 0.6).await;
        let near = seek(&ipc, 10.0).await;
        eprintln!("拖到没缓存的 60%：{far:?}；拖回已缓存的开头：{near:?}");
        assert!(
            near < Duration::from_secs(2),
            "已缓存的位置拖动太慢：{near:?}"
        );
        ctl.send(Ctl::Quit).unwrap();
        while !matches!(ev_rx.recv().await, Some(Event::Ended { .. }) | None) {}
        run.await.unwrap();
        tokio::time::sleep(Duration::from_secs(1)).await;
        let up1 = stream.up_bytes();
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert_eq!(stream.up_bytes(), up1, "关掉播放器后还在下载");
        let cached = blocks();

        let (again, _ipc, ctl, mut ev_rx, run) = play().await;
        eprintln!("重开同一集起播 {again:?}（第一次 {first:?}）；缓存 {cached} 块");
        ctl.send(Ctl::Quit).unwrap();
        while !matches!(ev_rx.recv().await, Some(Event::Ended { .. }) | None) {}
        run.await.unwrap();
        let mut bad = Vec::new();
        for b in &before {
            bad.extend(restore(&c, b).await);
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert!(bad.is_empty(), "{bad:?}");
        assert!(cached > 0);
    }
}
