//! mpv 外部进程 + JSON IPC：同季连播、按进度预加载下一集、外挂字幕与默认轨、Emby 进度回传。
//!
//! 一次播放 = 一个 [`Session`]：启动 mpv 的同时建队列、准备首集，连上 IPC 后由 [`Session::run`]
//! 在一个循环里处理 mpv 事件。网络请求不进这个循环：进度回传交给回传任务（[`spawn_reporter`]），
//! 下一集的 PlaybackInfo 在后台取（[`State::prepare_later`]），上游再慢，按键和退出也照常响应。
//! 队列是整季，自然往后播靠「到预加载进度才追加下一集」，手动上一集 / 下一集 / 选集由 EmbyMini 处理
//! （见 [`State::jump`]）。换集时沿用上一集选的音轨、字幕。

use crate::emby::{self, Emby, Error, Item, MediaStream, NO_QUERY};
use crate::telemetry;
use reqwest::Method;
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};
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
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Track {
    /// 服务端没给，交给 mpv 按 slang / alang 自己选。
    Auto,
    Off,
    /// mpv 的轨道号（从 1 开始）。
    Id(usize),
}

/// 把 Emby 的默认轨（流的 `Index`）换成 mpv 轨道号：
/// 内嵌轨按 Index 排序后的序号；外挂字幕在内嵌之后，按 `sub-add` 顺序（同样按 Index 排序）编号。
pub fn mpv_track(streams: &[MediaStream], kind: &str, default: Option<i32>) -> Track {
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
        return Track::Auto; // 外挂音轨不挂载
    }
    sorted(true)
        .iter()
        .position(|&i| i == want)
        .map_or(Track::Auto, |p| Track::Id(internal.len() + p + 1))
}

/// mpv 里显示的轨道标题：外挂字幕用 `sub-add` 时传的标题，内嵌轨就是容器里的轨道名。
fn stream_title(s: &MediaStream) -> Option<&str> {
    if s.is_external {
        s.display_title.as_deref().or(s.title.as_deref())
    } else {
        s.title.as_deref()
    }
}

/// 用户在上一集选的轨（取自 mpv 的 `current-tracks/*`），换集时按它挑同一条。
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

/// 语言码归一：mpv 报的是容器里的 BCP 47（`en`、`zh-CN`），Emby 多是 ISO 639-2（`eng`、`chi`）。
// ponytail: 只列常见语言，其他三字母码原样比较；匹配不上会退回服务端默认轨，不会选错
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

/// 在新一集里找和偏好最像的轨：语言必须相同，再看标题、内嵌 / 外挂。找不到返回 None（用服务端默认）。
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

/// 选集菜单里的一行：剧名在窗口标题里已经有了，这里只要集号和集名。
fn episode_label(item: &Item) -> String {
    match (item.parent_index_number, item.index_number) {
        (Some(s), Some(e)) => format!("S{s:02}E{e:02} {}", item.name),
        _ => item.name.clone(),
    }
}

fn title(item: &Item) -> String {
    match (
        &item.series_name,
        item.parent_index_number,
        item.index_number,
    ) {
        (Some(series), Some(s), Some(e)) => format!("{series} S{s:02}E{e:02} {}", item.name),
        _ => item.name.clone(),
    }
}

/// `loadfile` 的选项串。标题用 mpv 的 `%字节数%` 写法，里面有逗号、等号也不会被拆开。
fn loadfile_opts(title: &str, start: f64) -> String {
    let mut o = format!("force-media-title=%{}%{title}", title.len());
    if start > 0.0 {
        o.push_str(&format!(",start={start:.3}"));
    }
    o
}

struct Sub {
    url: String,
    title: String,
    lang: String,
}

/// 一集的播放参数（来自 PlaybackInfo）。
struct Ready {
    url: String,
    source_id: String,
    session_id: Option<String>,
    subs: Vec<Sub>,
    aid: Track,
    sid: Track,
    streams: Vec<MediaStream>,
    /// 片源信息（容器、码率、大小、视频编码），诊断数据的起播 / 换集记录带上，按片子分析慢在哪。
    media: Map<String, Value>,
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
    let subs = ext
        .into_iter()
        .map(|s| Sub {
            url: emby.subtitle_url(&item.id, &src, s),
            title: stream_title(s).unwrap_or_default().to_owned(),
            lang: s.language.clone().unwrap_or_default(),
        })
        .collect();
    Ok(Ready {
        url: emby.stream_url(&item.id, &src),
        aid: mpv_track(&src.media_streams, "Audio", src.default_audio_stream_index),
        sid: mpv_track(
            &src.media_streams,
            "Subtitle",
            src.default_subtitle_stream_index,
        ),
        media: media_info(&src),
        source_id: src.id,
        session_id: info.play_session_id,
        subs,
        streams: src.media_streams,
    })
}

/// mpv 警告日志去掉会泄露或打散去重的内容：URL（含条目 id、token 之外的路径）换成 `<url>`，数字换成 `#`
/// （字节偏移、秒数每次都不一样，不换的话同一种警告去不了重），最长 200 字。
fn scrub_log(text: &str) -> String {
    let mut out = String::new();
    for (k, word) in text.split(' ').enumerate() {
        if k > 0 {
            out.push(' ');
        }
        if word.contains("://") {
            out.push_str("<url>");
            continue;
        }
        let mut last_hash = false;
        for ch in word.chars() {
            if ch.is_ascii_digit() {
                if !last_hash {
                    out.push('#');
                }
                last_hash = true;
            } else {
                out.push(ch);
                last_hash = false;
            }
        }
    }
    out.chars().take(200).collect()
}

fn media_info(src: &emby::MediaSource) -> Map<String, Value> {
    let video = src.media_streams.iter().find(|s| s.kind == "Video");
    Map::from_iter([
        ("container".to_owned(), json!(src.container)),
        (
            "mbps".to_owned(),
            json!(src.bitrate.map(|b| (b as f64 / 1e5).round() / 10.0)),
        ),
        ("mb".to_owned(), json!(src.size.map(|b| b >> 20))),
        (
            "vcodec".to_owned(),
            json!(video.and_then(|v| v.codec.clone())),
        ),
    ])
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
    /// 点播放的时刻和起播各阶段耗时（诊断数据，首帧出来时记一条 `play`）。
    timeline: (Instant, Map<String, Value>),
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
    let t0 = Instant::now();
    let ms = move || t0.elapsed().as_millis() as u64;
    let ipc = ipc_path();
    let args = mpv_args(&opts, &ipc);
    // 参数里只有路径和开关，没有 token（token 经 IPC 设置），可以整串记日志
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
    let spawn_ms = ms();

    let queue = async {
        // 条目和 PlaybackInfo 只依赖 id，并行取；单集列表要等条目里的剧 / 季 id
        let (item, info) =
            tokio::try_join!(emby.item(item_id), emby.playback_info(item_id, source))?;
        let first = ready(&emby, &item, info)?;
        let info_ms = ms();
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
        Ok::<_, Error>((queue, at, info_ms))
    };
    let ipc_done = async {
        let r = connect(&ipc, &mut child).await;
        (r, ms())
    };
    let (queue, (io, ipc_ms)) = tokio::join!(queue, ipc_done);
    let ((queue, at, info_ms), io) = match (queue, io) {
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
        timeline: (
            t0,
            Map::from_iter([
                ("spawn_ms".to_owned(), spawn_ms.into()),
                ("info_ms".to_owned(), info_ms.into()),
                ("ipc_ms".to_owned(), ipc_ms.into()),
                ("from_start".to_owned(), from_start.into()),
            ]),
        ),
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
            timeline,
        } = self;
        let (rd, wr) = tokio::io::split(io);
        let mut lines = BufReader::new(rd).lines();
        let (reports, reporter) = spawn_reporter(emby.clone());
        let (prepared, mut prepared_rx) = mpsc::unbounded_channel();
        let mut st = State::new(emby, queue, wr, events, preload, reports, prepared);
        st.tel.on = telemetry::enabled();
        st.tel.first = Some(timeline);
        let tel_on = st.tel.on;

        // token 走请求头，不进 URL（见 emby::strip_key）；要在第一次 loadfile 之前设好
        let header = st.emby.token_header();
        st.send(json!([
            "change-list",
            "http-header-fields",
            "append",
            header
        ]))
        .await;
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
        .chain(TEL_PROPS.iter().copied().filter(|_| tel_on))
        .enumerate()
        {
            st.send(json!(["observe_property", id + 1, name])).await;
        }
        if tel_on {
            // ffmpeg / 拉流的警告（HTTP 错误、重连）：查卡顿和起播慢的原因
            st.send(json!(["request_log_messages", "warn"])).await;
        }
        st.load(at, "append-play").await;

        let mut tick = tokio::time::interval(PROGRESS_EVERY);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut minute =
            tokio::time::interval_at(tokio::time::Instant::now() + TEL_EVERY, TEL_EVERY);
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
                },
                Some((i, after, r)) = prepared_rx.recv() => st.on_prepared(i, after, r).await,
                _ = tick.tick() => st.progress("TimeUpdate"),
                _ = minute.tick(), if st.tel.on => st.probe_minute().await,
            }
        }
        if let Some(i) = st.cur {
            st.stop(i); // mpv 被直接关掉、没来得及发 end-file 时补发
        }
        let status = child.wait().await;
        crate::lifecycle::exit_mark("mpv_exited");
        let code = status.as_ref().ok().and_then(|s| s.code());
        if st.tel.on {
            telemetry::rec("mpv_exit", json!({ "srv": st.emby.tag, "code": code }));
        }
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
        crate::lifecycle::exit_mark("reported");
        log::info!("播放结束");
        let _ = events.send(Event::Ended { volume });
    }
}

struct State {
    emby: Emby,
    queue: Vec<Entry>,
    wr: WriteHalf<Box<dyn Io>>,
    events: mpsc::UnboundedSender<Event>,
    preload: f64,
    /// 正在播放的队列下标（start-file 到 end-file 之间）。
    cur: Option<usize>,
    pos: f64,
    dur: f64,
    paused: bool,
    volume: Option<f64>,
    /// 已按偏好 / 默认选好轨的那一集；之后的轨道变化才算用户的选择，记进 pref_*。
    applied: Option<usize>,
    pref_audio: Option<Pref>,
    pref_sub: Option<Pref>,
    rid: u64,
    /// loadfile 的 request_id → 队列下标，用回复里的 playlist_entry_id 对上号。
    pending: HashMap<u64, usize>,
    /// 交给回传任务的 (路径, 请求体)，见 [`spawn_reporter`]。
    reports: mpsc::UnboundedSender<(String, Value)>,
    /// 后台准备结果送回 run 的循环。
    prepared: mpsc::UnboundedSender<Prepared>,
    /// 用户最近一次要切去的集（还在后台准备）；更早的切集结果到了就丢掉。
    want: Option<usize>,
    /// 上次告诉 ModernZ 的「下一集已就绪」，见 [`State::sync_next_ready`]。
    next_ready: bool,
    /// IPC 写失败过（mpv 已退出、管道断了）：只记一次日志，后面的命令照样丢弃。
    pipe_broken: bool,
    /// 已经给界面发过 Failed（起播没成、mpv 退出时就不再补发「意外退出」）。
    failed_sent: bool,
    tel: Tel,
}

/// 诊断数据（开了才有）要多订阅的属性：缓冲卡顿、缓存秒数、下载速度。
const TEL_PROPS: &[&str] = &[
    "paused-for-cache",
    "demuxer-cache-duration",
    "cache-speed",
    "demuxer-cache-idle",
];
/// 每隔这么久记一条丢帧与下载速度的汇总。
const TEL_EVERY: Duration = Duration::from_secs(60);
/// 开播后取一次的解码 / 渲染信息。
const RENDER_PROPS: &[&str] = &[
    "mpv-version",
    "hwdec-current",
    "current-gpu-context",
    "current-vo",
    "video-codec",
    "video-params",
    "container-fps",
    "display-fps",
];
/// 每分钟取一次、记增量的计数器（排查拖动窗口发卡、掉帧）；`vsync-jitter` 记当时的值。
const FRAME_PROPS: &[&str] = &[
    "frame-drop-count",
    "decoder-frame-drop-count",
    "mistimed-frame-count",
    "vo-delayed-frame-count",
    "vsync-jitter",
];

/// 每分钟汇总：计数器记和上一分钟的差（`vsync-jitter` 记当时的值），下载速度记本分钟正在读数据时的最小 / 平均 / 最大（KB/s，
/// 这一分钟没读就没有 `kbps`），重连次数。
fn minute_summary(tel: &mut Tel, data: Map<String, Value>) -> Value {
    let mut out = Map::new();
    for (k, v) in &data {
        // 换集那一刻取不到值（null）：这一项这分钟不记
        let Some(now) = v.as_f64() else { continue };
        let prev = tel.last_frames.get(k).and_then(Value::as_f64);
        let d = match prev {
            // 计数器每集从 0 开始（换集时清了 last_frames）；比上次小说明中途清零了，记当前值
            Some(p) if k != "vsync-jitter" && now >= p => now - p,
            _ => now,
        };
        out.insert(k.clone(), json!(d));
    }
    tel.last_frames = data.into_iter().filter(|(_, v)| v.is_number()).collect();
    out.insert(
        "reconnects".into(),
        json!(std::mem::take(&mut tel.reconnects)),
    );
    let sp = std::mem::take(&mut tel.speeds);
    if !sp.is_empty() {
        let kb = |x: f64| (x / 1024.0).round();
        let min = sp.iter().copied().fold(f64::MAX, f64::min);
        let max = sp.iter().copied().fold(0.0, f64::max);
        let avg = sp.iter().sum::<f64>() / sp.len() as f64;
        out.insert("kbps".into(), json!([kb(min), kb(avg), kb(max)]));
    }
    out.insert("cache_s".into(), json!(tel.cache_s.round()));
    Value::Object(out)
}

/// 播放相关的诊断数据状态。每条记录都带上游标识 `srv`：不同上游、不同线路的表现分开统计。
#[derive(Default)]
struct Tel {
    on: bool,
    /// 首次起播：点播放的时刻 + 已有阶段；首帧（第一次 playback-restart）时记 `play`。
    first: Option<(Instant, Map<String, Value>)>,
    /// 换集：发起时刻 + 已有字段（cause = next / prev / menu / auto，api_ready = 不用等接口，queued = 已在 mpv 列表里，
    /// file-loaded 时补 loaded_ms）；首帧时记 `switch`。
    switch: Option<(Instant, Map<String, Value>)>,
    /// seek：第一次 seek 的时刻、当时的位置和缓存秒数；之后的首帧（playback-restart）记 `seek`。连续拖动只算一次。
    seek: Option<(Instant, f64, f64)>,
    stall: Option<Instant>,
    /// mpv 正在读数据（`demuxer-cache-idle` = no）。不读时 `cache-speed` 停在旧值，那时的采样不算。
    reading: bool,
    /// 本分钟 ffmpeg 报的重连次数（mpv 警告日志里的 reconnect）。
    reconnects: u32,
    /// 这一集加载完了、还没取解码 / 渲染信息：等首帧再取（`file-loaded` 时解码器还没建，`hwdec-current` 是空的）。
    render_due: bool,
    cache_s: f64,
    /// 本分钟的下载速度采样（字节 / 秒）。
    speeds: Vec<f64>,
    /// get_property 的 request_id → (哪一组, 属性名)；一组凑齐后一起记。
    probes: HashMap<u64, (&'static str, &'static str)>,
    probe_data: HashMap<&'static str, (usize, Map<String, Value>)>,
    last_frames: Map<String, Value>,
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
            tel: Tel::default(),
        }
    }

    /// 记一条播放诊断数据，补上上游标识和当前条目。
    fn tel_rec(&self, k: &str, mut v: Value) {
        if let Some(m) = v.as_object_mut() {
            m.insert("srv".into(), self.emby.tag.clone().into());
            if let Some(i) = self.cur {
                m.insert("item".into(), self.queue[i].item.id.clone().into());
            }
        }
        telemetry::rec(k, v);
    }

    /// 发一组 get_property，回包凑齐后由 [`Self::on_probe`] 记一条。
    async fn probe(&mut self, tag: &'static str, props: &'static [&'static str]) {
        self.tel.probe_data.insert(tag, (props.len(), Map::new()));
        for p in props {
            let rid = self.send(json!(["get_property", p])).await;
            self.tel.probes.insert(rid, (tag, p));
        }
    }

    async fn probe_minute(&mut self) {
        if self.cur.is_some() && !self.paused {
            self.probe("minute", FRAME_PROPS).await;
        }
    }

    fn on_probe(&mut self, tag: &'static str, prop: &'static str, v: Value) {
        let Some((left, data)) = self.tel.probe_data.get_mut(tag) else {
            return;
        };
        data.insert(prop.into(), v);
        *left -= 1;
        if *left > 0 {
            return;
        }
        let Some((_, data)) = self.tel.probe_data.remove(tag) else {
            return;
        };
        match tag {
            "render" => {
                let mut data = data;
                // video-params 有 30 多项，只留分辨率、像素格式和看 HDR 用的几项
                if let Some(Value::Object(vp)) = data.get_mut("video-params") {
                    vp.retain(|k, _| {
                        ["w", "h", "pixelformat", "gamma", "primaries", "sig-peak"]
                            .contains(&k.as_str())
                    });
                }
                self.tel_rec("render", Value::Object(data))
            }
            _ => {
                let out = minute_summary(&mut self.tel, data);
                self.tel_rec("pmin", out);
            }
        }
    }

    /// 换集（next / prev / menu / auto）：记下发起时刻和目标集的状态，首帧出来时记耗时。
    fn tel_switch(&mut self, to: usize, cause: &'static str) {
        if self.tel.on {
            let e = &self.queue[to];
            let m = Map::from_iter([
                ("cause".to_owned(), json!(cause)),
                (
                    "api_ready".to_owned(),
                    json!(e.loaded || (e.ready.is_some() && !e.stopped)),
                ),
                ("queued".to_owned(), json!(e.loaded)),
            ]);
            self.tel.switch = Some((Instant::now(), m));
        }
    }

    /// 当前这一集的片源信息（还没准备好时为空）。
    fn media(&self) -> Map<String, Value> {
        self.cur
            .and_then(|i| self.queue[i].ready.as_ref())
            .map(|r| r.media.clone())
            .unwrap_or_default()
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

    /// 下一集已追加进 mpv 列表（PlaybackInfo、直链、轨道都已就绪，点下一集立即切换）时让 ModernZ 的
    /// 下一集按钮变绿带闪电；换集、追加失败时恢复。状态变了才发。
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

    async fn osd(&mut self, text: &str) {
        self.send(json!(["show-text", text, 5000])).await;
    }

    fn index_of(&self, entry_id: Option<i64>) -> Option<usize> {
        entry_id.and_then(|id| self.queue.iter().position(|e| e.entry_id == Some(id)))
    }

    /// 把已准备好的一集交给 mpv：`append-play` 追加到列表（空闲时立即播放），`replace` 清空列表立即播放。
    async fn load(&mut self, i: usize, mode: &str) {
        let e = &self.queue[i];
        let Some(r) = &e.ready else { return };
        let cmd = json!([
            "loadfile",
            r.url,
            mode,
            -1,
            loadfile_opts(&title(&e.item), e.start)
        ]);
        let rid = self.send(cmd).await;
        self.pending.insert(rid, i);
        self.queue[i].loaded = true;
    }

    /// 在后台取第 i 集的 PlaybackInfo，结果回到 run 的循环里由 [`Self::on_prepared`] 处理，等待期间 mpv 照常响应。
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

    /// 后台准备完成。准备期间用户可能已经切走，所以先确认结果还用得上。
    async fn on_prepared(&mut self, i: usize, after: After, r: Result<Ready, Error>) {
        self.queue[i].preparing = false;
        let id = self.queue[i].item.id.clone();
        match (after, r) {
            (After::Append, Ok(r)) => {
                // 切集会替换整个列表：已不在上一集时再追加，就排到错误的位置了
                if self.queue[i].loaded || self.want.is_some() || self.cur != i.checked_sub(1) {
                    log::info!("下一集 {id} 准备好时已不在上一集，不追加");
                    return;
                }
                self.queue[i].ready = Some(r);
                self.load(i, "append-play").await;
                self.sync_next_ready().await;
                log::info!("预加载下一集：{id}");
                let _ = self.events.send(Event::Preloaded { item_id: id });
            }
            (After::Append, Err(err)) => {
                log::warn!("下一集 {id} 准备失败：{err}");
                self.queue[i].failed = true; // 不在每次进度更新时重试，手动切过去时会再试
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

    async fn on_message(&mut self, v: Value) {
        let Some(ev) = v["event"].as_str() else {
            if let Some((tag, prop)) = v["request_id"]
                .as_u64()
                .and_then(|rid| self.tel.probes.remove(&rid))
            {
                self.on_probe(tag, prop, v["data"].clone());
                return;
            }
            if let Some(i) = v["request_id"]
                .as_u64()
                .and_then(|rid| self.pending.remove(&rid))
            {
                match v["data"]["playlist_entry_id"].as_i64() {
                    Some(id) => self.queue[i].entry_id = Some(id),
                    None => {
                        log::warn!("loadfile 失败：{}", v["error"]);
                        // 没进列表：手动下一集要走 jump，不能 playlist-next
                        self.queue[i].loaded = false;
                        self.sync_next_ready().await;
                    }
                }
            }
            return;
        };
        let entry = self.index_of(v["playlist_entry_id"].as_i64());
        if matches!(ev, "start-file" | "file-loaded" | "end-file") {
            // 排查切集 / 加载失败时，对着这几行就能还原 mpv 的时间线
            // file-loaded 不带 playlist_entry_id，用当前集
            let id = entry
                .or(self.cur)
                .map_or("?", |i| self.queue[i].item.id.as_str());
            log::info!(
                "mpv {ev}：{id} {}",
                v["reason"].as_str().unwrap_or_default()
            );
        }
        match ev {
            "start-file" => {
                if let Some(i) = entry {
                    // 同一集可能被 mpv 再播一遍（ModernZ 中键开了列表循环）：当作新的一次播放，否则回传全被跳过
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
                    // 播完自动进下一集（不是用户点的）也算一次换集
                    if self.tel.on && self.tel.first.is_none() && self.tel.switch.is_none() {
                        self.tel_switch(i, "auto");
                    }
                    self.tel.last_frames.clear(); // 丢帧类计数器每集从 0 开始
                    self.tel.seek = None;
                }
            }
            "file-loaded" => {
                if let Some(i) = self.cur {
                    // 真正加载出来才算开播；没加载出来的不回传 Playing / Stopped
                    self.queue[i].started = true;
                    self.apply_tracks(i).await;
                    if self.tel.on {
                        if let Some((t0, m)) = &mut self.tel.first {
                            m.insert("loaded_ms".into(), (t0.elapsed().as_millis() as u64).into());
                        }
                        if let Some((t0, m)) = &mut self.tel.switch {
                            m.insert("loaded_ms".into(), (t0.elapsed().as_millis() as u64).into());
                        }
                        self.tel.render_due = true;
                    }
                    self.report(i, "", json!({}));
                    let _ = self.events.send(Event::Started {
                        item_id: self.queue[i].item.id.clone(),
                    });
                }
            }
            // 首帧（也会在 seek 后触发，只在等首帧时处理）
            // 用户拖进度条 / 快进快退（换集、起播时的跳转不算，由 play / switch 记）
            "seek"
                if self.tel.on
                    && self.tel.first.is_none()
                    && self.tel.switch.is_none()
                    && self.tel.seek.is_none() =>
            {
                self.tel.seek = Some((Instant::now(), self.pos, self.tel.cache_s));
            }
            "log-message" if self.tel.on => {
                let text = v["text"].as_str().unwrap_or("").trim();
                if text.to_ascii_lowercase().contains("reconnect") {
                    self.tel.reconnects += 1;
                }
                let msg = scrub_log(text);
                self.tel_rec(
                    "mpv_log",
                    json!({ "lvl": v["level"], "mod": v["prefix"], "msg": msg }),
                );
            }
            "playback-restart" if self.tel.on => {
                if std::mem::take(&mut self.tel.render_due) {
                    self.probe("render", RENDER_PROPS).await;
                }
                if let Some((t0, mut m)) = self.tel.first.take() {
                    m.insert("frame_ms".into(), (t0.elapsed().as_millis() as u64).into());
                    m.extend(self.media());
                    self.tel_rec("play", Value::Object(m));
                } else if let Some((t0, mut m)) = self.tel.switch.take() {
                    m.insert("ms".into(), (t0.elapsed().as_millis() as u64).into());
                    m.extend(self.media());
                    self.tel_rec("switch", Value::Object(m));
                } else if let Some((t0, from, cache_s)) = self.tel.seek.take() {
                    let ms = t0.elapsed().as_millis() as u64;
                    let dist = (self.pos - from).round();
                    self.tel_rec(
                        "seek",
                        json!({ "ms": ms, "dist": dist, "cache_s": cache_s.round() }),
                    );
                }
            }
            "end-file" => {
                if self.tel.on {
                    self.tel_rec(
                        "end",
                        json!({ "reason": v["reason"], "err": v["file_error"], "pos": self.pos.round() }),
                    );
                }
                if let Some(i) = entry {
                    if v["reason"] == "error" {
                        self.fail(i, v["file_error"].as_str().unwrap_or("未知错误"))
                            .await;
                    } else {
                        self.stop(i);
                    }
                }
            }
            "property-change" => self.on_property(&v).await,
            "client-message" if v["args"][0] == NEXT_MSG => self.next().await,
            "client-message" if v["args"][0] == PREV_MSG => self.prev().await,
            "client-message" if v["args"][0] == JUMP_MSG => {
                let to = v["args"][1].as_str().and_then(|s| s.parse::<usize>().ok());
                if let Some(i) = to.filter(|&i| i < self.queue.len() && Some(i) != self.cur) {
                    self.tel_switch(i, "menu");
                    self.jump(i).await;
                }
            }
            _ => {}
        }
    }

    async fn on_property(&mut self, v: &Value) {
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
            (Some("paused-for-cache"), d) => match (d.as_bool(), self.tel.stall) {
                (Some(true), None) => self.tel.stall = Some(Instant::now()),
                (Some(false), Some(t0)) => {
                    self.tel.stall = None;
                    let ms = t0.elapsed().as_millis() as u64;
                    let kbps = self.tel.speeds.last().map_or(0.0, |s| (s / 1024.0).round());
                    self.tel_rec("stall", json!({ "ms": ms, "cache_s": self.tel.cache_s, "kbps": kbps, "pos": self.pos.round() }));
                }
                _ => {}
            },
            (Some("demuxer-cache-duration"), d) => self.tel.cache_s = d.as_f64().unwrap_or(0.0),
            (Some("cache-speed"), d) => {
                if let Some(x) = d.as_f64().filter(|_| self.tel.reading) {
                    self.tel.speeds.push(x);
                }
            }
            (Some("demuxer-cache-idle"), d) => self.tel.reading = *d == json!(false),
            (Some("volume"), d) => self.volume = d.as_f64().or(self.volume),
            // 只记「选好轨之后」的变化；换集卸载时的 null 不算（关字幕看 sid=no）
            (Some("current-tracks/audio"), d) if self.tracking() && d.is_object() => {
                self.pref_audio = Some(Pref::from_track(d));
            }
            (Some("current-tracks/sub"), d) if self.tracking() && d.is_object() => {
                self.pref_sub = Some(Pref::from_track(d));
            }
            (Some("sid"), d) if self.tracking() && (*d == json!(false) || *d == "no") => {
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

    /// 手动下一集：已预加载就直接切（mpv 可能已预读好），否则交给 [`Self::jump`]（后台准备好再切）。
    async fn next(&mut self) {
        let Some(i) = self.cur else { return };
        if i + 1 >= self.queue.len() {
            self.osd("没有下一集了").await;
            return;
        }
        self.tel_switch(i + 1, "next");
        if self.queue[i + 1].loaded {
            self.send(json!(["playlist-next"])).await;
        } else {
            self.jump(i + 1).await;
        }
    }

    async fn prev(&mut self) {
        match self.cur {
            Some(0) => self.osd("已经是第一集").await,
            Some(i) => {
                self.tel_switch(i - 1, "prev");
                self.jump(i - 1).await;
            }
            None => {}
        }
    }

    /// 跳到第 i 集：已准备好就立刻切，否则后台取 PlaybackInfo，到了再切（见 [`Self::on_prepared`]）。
    async fn jump(&mut self, i: usize) {
        self.osd(&format!("正在打开：{}", title(&self.queue[i].item)))
            .await;
        let e = &self.queue[i];
        // 播过的集要重新取：一次播放对应一个 PlaySessionId
        if e.ready.is_some() && !e.stopped {
            self.want = None;
            self.play_now(i).await;
        } else {
            self.want = Some(i);
            self.prepare_later(i, After::Play);
        }
    }

    /// 替换 mpv 列表，从头播第 i 集（之前追加进列表的集随之清掉）。
    async fn play_now(&mut self, i: usize) {
        for e in &mut self.queue {
            e.loaded = false;
        }
        let e = &mut self.queue[i];
        (e.start, e.failed, e.started, e.stopped) = (0.0, false, false, false);
        self.load(i, "replace").await;
    }

    /// 挂外挂字幕，选音轨、字幕：优先沿用上一集用户选的（按语言、标题匹配），否则用服务端默认轨。
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
        let cmds: Vec<Value> = r
            .subs
            .iter()
            .map(|s| json!(["sub-add", s.url, "auto", s.title, s.lang]))
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

    /// 加载失败：探测一次直链拿到状态码，换成能看懂的原因发给界面。
    async fn fail(&mut self, i: usize, mpv_error: &str) {
        let status = match &self.queue[i].ready {
            Some(r) => self.emby.probe(&r.url).await,
            None => None,
        };
        let why = match status {
            Some(403) => {
                "服务器拒绝访问文件（403），上游可能正在迁移这一集，换一集或稍后再试".to_owned()
            }
            Some(404) => "服务器上找不到文件（404）".to_owned(),
            Some(s) if !(200..300).contains(&s) => format!("拉流返回 HTTP {s}"),
            _ => format!("mpv 无法打开（{mpv_error}）"),
        };
        let e = &self.queue[i];
        let message = format!("无法播放「{}」：{why}", title(&e.item));
        log::warn!("{message}（条目 {}）", e.item.id);
        let item_id = e.item.id.clone();
        if self.cur == Some(i) {
            self.cur = None;
        }
        self.failed_sent = true;
        let _ = self.events.send(Event::Failed { item_id, message });
    }

    /// 交给回传任务（不等结果）。`what` 是 `""` / `"/Progress"` / `"/Stopped"`。
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
        if let (Some(b), Some(x)) = (body.as_object_mut(), extra.as_object()) {
            b.extend(x.clone());
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

    #[test]
    fn tracks_from_real_playbackinfo() {
        // 实测：视频 0、音频 1、内嵌字幕 2/3/4、外挂中文 ass 5；服务端默认音轨 1、字幕 5。
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
        assert_eq!(
            mpv_track(&s.media_streams, "Subtitle", Some(99)),
            Track::Auto
        );
    }

    #[test]
    fn track_prefs_carry_over_by_language_and_title() {
        // 同一份实测流：音频 1(eng)、内嵌字幕 2 English / 3 English SDH / 4 SDH(PGS)、外挂 5 zh-CN
        let pi: PlaybackInfo = fixture("playbackinfo.json");
        let st = &pi.media_sources[0].media_streams;
        let pick = |lang: &str, title: &str, external| Pref::Pick {
            lang: lang.into(),
            title: title.into(),
            external,
        };
        // mpv 报 en，Emby 记 eng；标题相同的那条胜出
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
        // 下一集标题变了：同语言里优先内嵌 / 外挂一致、序号小的
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

    /// 一个不连 mpv 的 State：写给 mpv 的命令从返回的管道另一端读出来。
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
            source_id: id.into(),
            session_id: None,
            subs: Vec::new(),
            aid: Track::Auto,
            sid: Track::Auto,
            streams: Vec::new(),
            media: Map::from_iter([("container".to_owned(), json!("mkv"))]),
        }
    }

    /// 读出 State 写给 mpv 的全部命令。
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

    /// 读出 State 写给 mpv 的全部 loadfile 命令：(url, 模式)。
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

    /// 后台准备的结果回来时，用户可能已经切走：过时的结果不能追加 / 切过去。
    #[tokio::test]
    async fn prepared_results_respect_where_the_user_is() {
        // 仍在第 0 集：第 1 集的预加载结果追加到列表
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

        // 已经切到第 2 集：第 1 集的预加载结果作废；准备失败只标记，不追加
        let (mut st, mpv, _ev) = offline_state(3);
        st.cur = Some(2);
        st.on_prepared(1, After::Append, Ok(ready_for("e1"))).await;
        st.cur = Some(0);
        st.on_prepared(1, After::Append, Err(Error::Other("502".into())))
            .await;
        assert!(st.queue[1].failed && !st.queue[1].loaded);
        assert!(loadfiles(st, mpv).await.is_empty());

        // 先选第 1 集、又改选第 2 集：第 1 集的结果丢掉，只切到第 2 集
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

    #[test]
    fn minute_summary_reports_deltas_and_speed() {
        let mut tel = Tel::default();
        let m = |drop: f64, jitter: f64| {
            Map::from_iter([
                ("frame-drop-count".to_owned(), json!(drop)),
                ("vsync-jitter".to_owned(), json!(jitter)),
            ])
        };
        tel.speeds = vec![1024.0, 3072.0];
        let first = minute_summary(&mut tel, m(5.0, 0.1));
        assert_eq!(first["frame-drop-count"], json!(5.0));
        assert_eq!(first["kbps"], json!([1.0, 2.0, 3.0]));
        let second = minute_summary(&mut tel, m(12.0, 0.2));
        assert_eq!(second["frame-drop-count"], json!(7.0));
        assert_eq!(second["vsync-jitter"], json!(0.2));
        assert!(second.get("kbps").is_none());
    }

    /// 每分钟汇总：计数器求差；换集清零后不出负数；取不到的值不记；下载速度只收正在读数据时的采样。
    #[tokio::test]
    async fn minute_summary_handles_resets_and_idle() {
        let (mut st, _mpv, _ev) = offline_state(2);
        st.tel.on = true;
        let props = |drop: Value| {
            Map::from_iter([
                ("frame-drop-count".to_owned(), drop),
                ("vsync-jitter".to_owned(), json!(0.05)),
            ])
        };
        let v = minute_summary(&mut st.tel, props(json!(10)));
        assert_eq!(v["frame-drop-count"], 10.0);
        let v = minute_summary(&mut st.tel, props(json!(15)));
        assert_eq!(v["frame-drop-count"], 5.0);
        assert_eq!(v["vsync-jitter"], 0.05);
        // 换集那一刻取不到：这一项不记，也不覆盖上次的值
        let v = minute_summary(&mut st.tel, props(Value::Null));
        assert!(v.get("frame-drop-count").is_none());
        // 新的一集计数从 0 开始（start-file 时清 last_frames）
        st.cur = Some(0);
        st.on_message(json!({ "event": "start-file", "playlist_entry_id": 1 }))
            .await;
        st.queue[1].entry_id = Some(2);
        st.on_message(json!({ "event": "start-file", "playlist_entry_id": 2 }))
            .await;
        let v = minute_summary(&mut st.tel, props(json!(3)));
        assert_eq!(v["frame-drop-count"], 3.0);
        // 下载速度：不在读的时候（cache-speed 停在旧值）不收
        st.on_property(&json!({ "name": "demuxer-cache-idle", "data": true }))
            .await;
        st.on_property(&json!({ "name": "cache-speed", "data": 999_999 }))
            .await;
        st.on_property(&json!({ "name": "demuxer-cache-idle", "data": false }))
            .await;
        st.on_property(&json!({ "name": "cache-speed", "data": 2048 }))
            .await;
        st.on_message(json!({ "event": "log-message", "level": "warn", "prefix": "ffmpeg", "text": "Will reconnect at 123 in 0 second(s)" })).await;
        let v = minute_summary(&mut st.tel, props(json!(3)));
        assert_eq!(v["kbps"], json!([2.0, 2.0, 2.0]));
        assert_eq!(v["reconnects"], 1);
        let v = minute_summary(&mut st.tel, props(json!(3)));
        assert!(v.get("kbps").is_none(), "这一分钟没读数据就没有 kbps");
    }

    /// mpv 警告去掉 URL、数字归一（同一种警告才能去重）。
    #[test]
    fn mpv_log_is_scrubbed() {
        assert_eq!(
            scrub_log("Will reconnect at 1048576 in 0 second(s), error=Connection reset."),
            "Will reconnect at # in # second(s), error=Connection reset."
        );
        assert_eq!(
            scrub_log("http: HTTP error 403 Forbidden https://h.example/emby/videos/1/stream?x=1"),
            "http: HTTP error # Forbidden <url>"
        );
    }

    /// seek：从 seek 事件到下一个首帧记一条；起播、换集期间的跳转不算；连续拖动只算一次。
    #[tokio::test]
    async fn seek_is_timed_until_playback_restart() {
        let (mut st, mpv, _ev) = offline_state(2);
        st.tel.on = true;
        st.cur = Some(0);
        st.tel.first = Some((Instant::now(), Map::new()));
        st.on_message(json!({ "event": "seek" })).await; // 起播时的跳转
        assert!(st.tel.seek.is_none());
        st.on_message(json!({ "event": "playback-restart" })).await; // 记 play
        st.pos = 100.0;
        st.on_message(json!({ "event": "seek" })).await;
        st.pos = 50.0;
        st.on_message(json!({ "event": "seek" })).await; // 拖动中又一次：不重置起点
        assert_eq!(st.tel.seek.as_ref().map(|s| s.1), Some(100.0));
        st.pos = 400.0;
        st.on_message(json!({ "event": "playback-restart" })).await;
        assert!(st.tel.seek.is_none());
        // 换集：带上 api_ready / queued 和片源信息
        st.queue[1].ready = Some(ready_for("e1"));
        st.tel_switch(1, "next");
        let m = &st.tel.switch.as_ref().unwrap().1;
        assert_eq!(
            (
                m["cause"].clone(),
                m["api_ready"].clone(),
                m["queued"].clone()
            ),
            (json!("next"), json!(true), json!(false))
        );
        st.cur = Some(1);
        assert_eq!(st.media()["container"], "mkv");
        st.on_message(json!({ "event": "playback-restart" })).await;
        assert!(st.tel.switch.is_none());
        drop(commands(st, mpv).await);
    }

    /// 解码 / 渲染信息等首帧（playback-restart）才取：file-loaded 时解码器还没建，hwdec-current 是空的。
    #[tokio::test]
    async fn render_probe_waits_for_first_frame() {
        let (mut st, mpv, _ev) = offline_state(1);
        st.tel.on = true;
        st.cur = Some(0);
        st.on_message(json!({ "event": "file-loaded" })).await;
        let after_load = st.tel.probes.len();
        st.on_message(json!({ "event": "playback-restart" })).await;
        st.on_message(json!({ "event": "playback-restart" })).await; // seek 后再触发：不重复取
        assert_eq!(after_load, 0);
        assert_eq!(st.tel.probes.len(), RENDER_PROPS.len());
        let asked: Vec<Value> = commands(st, mpv)
            .await
            .into_iter()
            .filter(|c| c[0] == "get_property")
            .map(|c| c[1].clone())
            .collect();
        assert_eq!(asked, RENDER_PROPS);
    }

    /// 下一集追加进列表后按钮变绿（yes），换到下一集或追加失败后恢复（no），状态没变不重复发。
    #[tokio::test]
    async fn next_ready_follows_the_queue() {
        let (mut st, mpv, _ev) = offline_state(3);
        st.cur = Some(0);
        st.on_prepared(1, After::Append, Ok(ready_for("e1"))).await;
        let rid = st.rid - 1; // loadfile 之后紧跟 script-message
        st.on_message(json!({ "request_id": rid, "data": { "playlist_entry_id": 7 } }))
            .await;
        st.on_message(json!({ "event": "start-file", "playlist_entry_id": 7 }))
            .await;
        assert_eq!(st.cur, Some(1));
        // 第 2 集的追加被 mpv 拒绝：仍是 no，且不再当作在列表里
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

    /// 上游卡死（连上了但不回数据）时，后台预加载不能卡住 mpv 事件处理：关闭播放器要立刻送到 mpv。
    /// 如果预加载在事件循环里同步等 PlaybackInfo，这里要等满 30 秒的请求超时。
    #[cfg(unix)]
    #[tokio::test]
    async fn slow_upstream_does_not_block_controls() {
        // 只接受连接、永不回应的「上游」
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((sock, _)) = listener.accept().await {
                held.push(sock);
            }
        });
        let server = crate::config::Server {
            id: "s".into(),
            name: String::new(),
            url: "http://127.0.0.1".into(),
            port,
            user: "u".into(),
            user_id: "uid".into(),
            token: "tok".into(),
        };
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
            timeline: (Instant::now(), Map::new()),
        };
        let (ctl, ctl_rx) = mpsc::unbounded_channel();
        let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
        let run = tokio::spawn(session.run(ctl_rx, ev_tx));

        // 假 mpv：回 loadfile、报开播，进度到 95% 触发预加载（打到卡死的上游）
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
        tokio::time::sleep(Duration::from_millis(300)).await; // 让预加载请求发出去、挂住

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

        drop(lines); // 假 mpv 退出、管道关闭
        drop(wr);
        tokio::time::timeout(Duration::from_secs(5), run)
            .await
            .expect("会话没有结束")
            .unwrap();
        assert!(matches!(ev_rx.recv().await, Some(Event::Ended { .. })));
    }

    /// mpv 把播完的一集再播一遍（列表循环）时，第二遍也要回传开播和停止。
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
            extra_args,
        }
    }

    fn headless(extra: &[String]) -> Options {
        let base = ["--vo=null", "--ao=null", "--force-window=no"].map(String::from);
        test_opts(base.into_iter().chain(extra.iter().cloned()).collect())
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
        let server = crate::config::Server {
            id: "s".into(),
            name: String::new(),
            url: "http://127.0.0.1".into(),
            port,
            user: "u".into(),
            user_id: "uid".into(),
            token: "tok".into(),
        };
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
        let session = start(c, &id, None, true, headless(&[])).await.unwrap();
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
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "需要真服账号（EMBY_SERVER / PORT / EMBY_USER / EMBY_PASS）和 PATH 里的 mpv"]
    async fn e2e_real_server() {
        // 设了 TEL_DIR（且编译时注入了上报地址）就把诊断数据也打开，播完到这个目录看记了什么
        let tel_dir = std::env::var("TEL_DIR").ok();
        if let Some(d) = &tel_dir {
            // 编译期常量：没注入上报地址时 rec 什么都不记，测了也白测
            if !telemetry::AVAILABLE {
                panic!("TEL_DIR 要配合编译时的 EMBYMINI_TEL_URL / EMBYMINI_TEL_TOKEN");
            }
            telemetry::set_enabled(true);
            telemetry::start(
                d.into(),
                telemetry::Meta {
                    device: "e2e".into(),
                    name: "e2e".into(),
                    version: emby::VERSION,
                },
            );
        }
        let c = login_from_env().await;

        let (ep1, ep2) = pick_pair(&c).await;
        let before = [
            c.item(&ep1.id).await.unwrap(),
            c.item(&ep2.id).await.unwrap(),
        ];
        let log = std::env::temp_dir().join(format!("embymini-e2e-{}.log", std::process::id()));
        let opts = headless(&[format!("--log-file={}", log.display())]);
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

        // 服务端：ep1 播到 99% 以上，应已标记为已看
        let ep1_after = c.item(&ep1.id).await.unwrap().user_data;
        let mpv_log = std::fs::read_to_string(&log).unwrap_or_default();
        let _ = std::fs::remove_file(&log); // 日志里记着设置请求头的命令（含 token），用完即删
        let mut bad = Vec::new();
        for b in &before {
            bad.extend(restore(&c, b).await);
        }
        assert!(bad.is_empty(), "{bad:?}");
        assert!(ep1_after.played, "ep1 播完后服务端没有标记已看");
        eprintln!("事件序列：{seen:?}");
        if tel_dir.is_some() {
            tokio::time::sleep(Duration::from_secs(6)).await; // 等写入任务落盘（每 5 秒）
        }
        eprintln!(
            "mpv 预取下一集：{}",
            if mpv_log.contains("Prefetching") {
                "是"
            } else {
                "否（当前集未读到末尾前不会预取）"
            }
        );
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
        let opts = test_opts(
            std::env::var("MPV_EXTRA")
                .map(|a| a.split_whitespace().map(String::from).collect())
                .unwrap_or_default(),
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
        shown.extend(
            get_property(&ipc, "track-list")
                .await
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t["external-filename"].clone()),
        );
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
        eprintln!("截图在 {}；检查过的地址 {} 个", dir.display(), shown.len());
    }
}
