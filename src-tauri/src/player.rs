//! mpv as an external process driven over JSON IPC: whole-season playback, preloading the next episode, external
//! subtitles and default tracks, and Emby progress reports.
//!
//! One playback is one [`Session`]: mpv starts while the queue is built and the first episode prepared; once IPC is
//! connected, [`Session::run`] handles mpv events in a single loop. Network requests stay out of that loop: reports
//! go through the report task ([`spawn_reporter`]) and other episodes' PlaybackInfo is fetched in the background
//! ([`State::prepare_later`]), so keys and quit respond however slow the upstream is.
//! The queue is the whole season. Playing on appends the next episode once the preload progress is reached;
//! previous / next / episode picks are handled here ([`State::jump`]). Audio and subtitle choices carry over.
//! Video and subtitles are read through the local cache proxy ([`crate::stream`]): registered when handed to mpv
//! ([`State::load`]); on an episode change only the current and next episode keep their cache.

use crate::danmaku::{self, Fail as DmFail};
use crate::emby::{self, Emby, Error, Item, MediaStream, NO_QUERY};
use crate::stream::Stream;
use reqwest::Method;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, WriteHalf};
use tokio::process::Child;
use tokio::sync::mpsc;

const TICKS_PER_SEC: f64 = 10_000_000.0;
/// Progress report interval (Emby's own web client also reports about every 10 s).
const PROGRESS_EVERY: Duration = Duration::from_secs(10);
/// How often the upstream download speed goes to the OSC top bar ([`SPEED_MSG`]).
const SPEED_EVERY: Duration = Duration::from_secs(1);
/// Sent by mpv's previous / next buttons and keys (see mpv/input.conf, mpv/script-opts/modernz.conf).
const NEXT_MSG: &str = "embymini-next";
const PREV_MSG: &str = "embymini-prev";
/// The episode menu (mpv/scripts/embymini.lua) picked an episode; the argument is its queue index.
const JUMP_MSG: &str = "embymini-jump";
/// To ModernZ: whether the next episode is already in mpv's playlist (`yes` / `no`); if so the next button turns
/// green with a bolt (mpv/scripts/modernz.lua).
const NEXT_READY_MSG: &str = "embymini-next-ready";
/// To ModernZ: upstream download speed in bytes/s for the top bar. Through the cache proxy mpv's own read rate is
/// the local disk's, not the network's.
const SPEED_MSG: &str = "embymini-speed";
/// Prefix of messages from the danmaku script (mpv/scripts/danmaku.lua): `style`, `search`, `episodes`, `pick`,
/// `reload`; see [`State::on_danmaku`].
const DM_MSG: &str = "embymini-danmaku-";

/// The bundled mpv config directory (`<resources>/mpv`), or None if it is missing.
///
/// On Windows Tauri's resource dir comes from `canonicalize(current exe)` and carries the `\\?\` verbatim prefix.
/// mpv joins config file names with `/` (`…\mpv/mpv.conf`) and hands that to CreateFileW as is; in a verbatim path
/// `/` is not a separator, so mpv.conf, scripts and fonts all fail to load and only mpv's own OSC is left.
/// dunce strips the prefix; `bundled_config_loads` checks this on Windows CI.
pub fn bundled_dir(resource_dir: &Path) -> Option<PathBuf> {
    Some(dunce::simplified(resource_dir).join("mpv")).filter(|d| d.is_dir())
}

pub struct Options {
    pub mpv: PathBuf,
    /// Bundled config directory (ModernZ, mpv.conf, input.conf), passed as `--config-dir`.
    pub config_dir: Option<PathBuf>,
    pub preload_percent: f64,
    /// Volume at the last exit (each playback is a new mpv process, which would otherwise start at 100).
    pub volume: Option<f64>,
    /// Where mpv writes its own files (GPU shader cache etc.). Without it mpv writes to its own
    /// `%LOCALAPPDATA%\mpv\cache` even with `--config-dir`, which uninstalling EmbyMini cannot clean up.
    pub cache_dir: Option<PathBuf>,
    /// Personal mpv config, loaded if it exists: read after the bundled one so its options win; it lives in the
    /// data directory so updates leave it alone.
    pub user_conf: Option<PathBuf>,
    /// Local cache proxy; without it (it failed to start) mpv reads upstream directly with the token in a header.
    pub stream: Option<Stream>,
    /// Danmaku data source (rendering is the mpv script's job); None = no danmaku data for the script.
    pub danmaku: Option<danmaku::Ctx>,
    /// Appended to mpv's arguments last (tests use it to run headless).
    pub extra_args: Vec<String>,
}

/// mpv's command line, in this order: bundled config (`--config-dir`) → personal config (`--include`, overrides the
/// bundled one) → remembered volume and cache dirs → what EmbyMini must control (IPC, UA, window, idle exit; the
/// personal config cannot change these) → the caller's extra arguments.
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
    // Seek bar previews (thumbfast) start a second mpv, which on Windows is only looked up in PATH.
    // `-append` adds one entry without splitting on commas, so a comma in the path is fine
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

/// Commands from the app to a running session.
pub enum Ctl {
    Quit,
    /// The danmaku API address or block words changed in settings: applies at once ("reload" uses the new address).
    Danmaku {
        api: Option<danmaku::Api>,
        words: Vec<String>,
    },
    /// Tests: a raw command for mpv.
    #[cfg(test)]
    Mpv(Value),
}

/// Playback events for the frontend. The UI acts only on `failed` and `ended`; `started`, `preloaded` and
/// `stopped` are the milestones tests wait on (they mirror the Sessions reports and cost one small message each).
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
    /// An episode failed to load or broke off; `message` is the reason shown to the user.
    Failed {
        item_id: String,
        message: String,
    },
    /// The whole playback session is over; `volume` is the volume at exit, reused next time.
    Ended {
        volume: Option<f64>,
    },
    /// Danmaku display settings changed in the panel; saved to the config.
    DanmakuStyle {
        style: danmaku::Style,
    },
    /// A manual danmaku match; saved to the config (key: [`danmaku::key`]).
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
        Some(s) if crate::status::classify(s) != crate::status::Class::Ok => {
            crate::status::describe(s, "")
        }
        // A 3xx the proxy could not follow: describe() would call it a success
        Some(s) if !(200..300).contains(&s) => format!("拉流返回 HTTP {s}"),
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

/// Playback parameters of one episode (from PlaybackInfo).
struct Ready {
    /// Upstream direct URL (no token); swapped for the cache proxy's local URL when handed to mpv.
    url: String,
    /// Cache directory name: upstream + item + version, the same every time (the next playback reuses the cache).
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
            // Prefixed with the video's cache name: retaining away the video drops its subtitles' registrations too
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

/// Cache file name: each part keeps ASCII letters, digits and `-` (anything else becomes `-`), joined with `_`.
fn cache_name(parts: &[&str]) -> String {
    parts
        .iter()
        .map(|p| p.replace(|c: char| !c.is_ascii_alphanumeric() && c != '-', "-"))
        .collect::<Vec<_>>()
        .join("_")
}

/// One episode in the queue.
struct Entry {
    item: Item,
    /// Where to start, in seconds (the resume point; only the episode playback started from has one).
    start: f64,
    ready: Option<Ready>,
    /// Already loadfile'd into mpv's playlist.
    loaded: bool,
    /// Preparing it failed (not retried on every progress update; switching to it by hand retries).
    failed: bool,
    entry_id: Option<i64>,
    started: bool,
    stopped: bool,
    /// Being prepared in the background (so progress updates do not ask again).
    preparing: bool,
}

impl Entry {
    fn new(item: Item) -> Self {
        Self {
            item,
            start: 0.0,
            ready: None,
            loaded: false,
            failed: false,
            entry_id: None,
            started: false,
            stopped: false,
            preparing: false,
        }
    }
}

/// What to do once an episode has been prepared in the background.
#[derive(Debug, Clone, Copy, PartialEq)]
enum After {
    /// Preload: append it to mpv's playlist (only if the previous episode is still playing).
    Append,
    /// The user switched to it (previous / next / episode menu): replace the playlist and play it now.
    Play,
}

/// A background preparation's result: (queue index, what to do next, PlaybackInfo result).
type Prepared = (usize, After, Result<Ready, Error>);

/// The report task: sends Sessions requests in order. In its own task so a slow upstream (the API sometimes takes
/// 15–30 s) never stalls mpv event handling, and Stopped never overtakes Playing. When backed up, only the newest
/// Progress is kept.
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
                continue; // a newer Progress or a Stopped is queued behind it, so this one is stale
            }
            let _ = emby.call(Method::POST, &path, NO_QUERY, Some(&body)).await; // `call` logs failures
        }
    });
    (tx, task)
}

trait Io: AsyncRead + AsyncWrite + Unpin + Send + Sync {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + Sync> Io for T {}

/// The IPC endpoint's path. On Unix mpv creates a socket file there that stays behind when mpv is killed, so it is
/// removed once the session (or an abandoned start) lets go of it; Windows named pipes leave nothing behind.
struct IpcPath(PathBuf);

impl Drop for IpcPath {
    fn drop(&mut self) {
        #[cfg(unix)]
        let _ = std::fs::remove_file(&self.0);
    }
}

pub struct Session {
    emby: Emby,
    queue: Vec<Entry>,
    /// Queue index of the episode playback started from.
    at: usize,
    child: Child,
    io: Box<dyn Io>,
    ipc: IpcPath,
    preload: f64,
    stream: Option<Stream>,
    danmaku: Option<danmaku::Ctx>,
}

/// Starts mpv (its window shows at once) while building the queue and preparing the first episode, then connects
/// to its IPC. Errors go straight back to the UI. `source`: the version picked on the detail page, for the first
/// episode only; later episodes play the server's default.
pub async fn start(
    emby: Emby,
    item_id: &str,
    source: Option<&str>,
    from_start: bool,
    opts: Options,
) -> Result<Session, Error> {
    let ipc = ipc_path();
    let mut child = spawn_mpv(&opts, &ipc.0)?;
    // try_join: the first failure ends the start (mpv gone need not wait on a slow upstream, nor the reverse)
    let r = tokio::try_join!(
        build_queue(&emby, item_id, source, from_start),
        connect(&ipc.0, &mut child)
    );
    let ((queue, at), io) = match r {
        Ok(r) => r,
        Err(e) => {
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

fn spawn_mpv(opts: &Options, ipc: &Path) -> Result<Child, Error> {
    let args = mpv_args(opts, ipc);
    // Only paths and switches, never the token (it lives in the cache proxy, or goes over IPC), so log it whole
    log::info!("启动 mpv：{} {}", opts.mpv.display(), args.join(" "));
    let mut cmd = tokio::process::Command::new(&opts.mpv);
    // Playback never goes through a proxy: mpv (ffmpeg) only honours these variables, which a terminal or proxy
    // tool may have set
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
    // Dropping the Child (start abandoned on exit or error) kills mpv, leaving no empty window behind
    cmd.kill_on_drop(true)
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    cmd.spawn()
        .map_err(|e| Error::Other(format!("无法启动 mpv（{}）：{e}", opts.mpv.display())))
}

/// The play queue and the index of `item_id` in it: an episode brings its whole season (alone if the season list
/// fails), anything else plays alone. The first entry is prepared here (with `source`) and resumes unless
/// `from_start`.
async fn build_queue(
    emby: &Emby,
    item_id: &str,
    source: Option<&str>,
    from_start: bool,
) -> Result<(Vec<Entry>, usize), Error> {
    // The item and its PlaybackInfo need only the id, so fetch both at once; the season list needs the item
    let (item, info) = tokio::try_join!(emby.item(item_id), emby.playback_info(item_id, source))?;
    let first = ready(emby, &item, info)?;
    let mut items = Vec::new();
    if let (true, Some(series), Some(season)) =
        (item.kind == "Episode", &item.series_id, &item.season_id)
    {
        // Without the list only the rest of the season is lost, so play this episode anyway
        items = emby.episodes(series, season).await.unwrap_or_else(|e| {
            log::warn!("取整季单集失败，只播这一集：{e}");
            Vec::new()
        });
    }
    let at = items
        .iter()
        .position(|e| e.id == item.id)
        .unwrap_or_else(|| {
            items = vec![item.clone()];
            0
        });
    let mut queue: Vec<Entry> = items.into_iter().map(Entry::new).collect();
    queue[at].ready = Some(first);
    if !from_start {
        queue[at].start = item.user_data.playback_position_ticks as f64 / TICKS_PER_SEC;
    }
    Ok((queue, at))
}

impl Session {
    /// Not playing after all (the user picked something else while this started): just end mpv. Nothing played,
    /// so nothing to report.
    pub async fn abort(mut self) {
        let _ = self.child.kill().await;
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
            child,
            io,
            ipc: _ipc, // removed when the session ends
            preload,
            stream,
            danmaku,
        } = self;
        let (rd, wr) = tokio::io::split(io);
        let mut rd = BufReader::new(rd);
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
            seq: 0,
        });
        set_up(&mut st, at).await;

        let mut tick = tokio::time::interval(PROGRESS_EVERY);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut speed = tokio::time::interval(SPEED_EVERY);
        speed.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut line = Vec::new();
        let mut ctl_open = true;
        let mut quit_sent = false;
        loop {
            tokio::select! {
                // read_until is cancel safe: a partly read line stays in `line` for the next round
                n = rd.read_until(b'\n', &mut line) => {
                    if !matches!(n, Ok(1..)) {
                        break; // mpv exited and closed the pipe
                    }
                    // mpv may send invalid UTF-8 (odd file tags, file names); one bad string must not end the session
                    match serde_json::from_str::<Value>(&String::from_utf8_lossy(&line)) {
                        Ok(v) => st.on_message(v).await,
                        Err(e) => log::warn!("mpv IPC 消息解析失败：{e}"),
                    }
                    line.clear();
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
                _ = speed.tick(), if st.stream.is_some() => st.send_speed().await,
            }
        }
        finish(st, child, reporter, at, quit_sent).await;
    }
}

/// First commands to a fresh mpv: the auth header when there is no cache proxy, the observed properties, then the
/// first episode.
async fn set_up(st: &mut State, at: usize) {
    // Without the cache proxy mpv reads upstream directly: the token goes in a header, never in the URL
    // (see emby::strip_key), and must be set before the first loadfile
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
}

/// After mpv's pipe closed: report the last stop, wait for mpv, release the cache, tell the UI about a crash, then
/// drain the reports and send `Ended` last (closing the app waits on that to know the reports went out).
async fn finish(
    mut st: State,
    mut child: Child,
    reporter: tokio::task::JoinHandle<()>,
    at: usize,
    quit_sent: bool,
) {
    if let Some(i) = st.cur {
        st.stop(i); // mpv was closed before it could send end-file
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
    // mpv died before anything played (GPU init failure, crash) and we did not ask it to quit: never fail silently
    if !quit_sent && !st.failed_sent && !st.queue.iter().any(|e| e.started) {
        let e = &st.queue[at];
        if let Some(message) = crash_message(&e.item, &status) {
            log::warn!("{message}（条目 {}）", e.item.id);
            let _ = st.events.send(Event::Failed {
                item_id: e.item.id.clone(),
                message,
            });
        }
    }
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

/// The user-facing message for mpv exiting on its own with `status`; None for a clean exit.
fn crash_message(item: &Item, status: &std::io::Result<ExitStatus>) -> Option<String> {
    if status.as_ref().is_ok_and(ExitStatus::success) {
        return None;
    }
    let code = status.as_ref().ok().and_then(ExitStatus::code);
    Some(format!(
        "无法播放「{}」：播放器意外退出（退出码 {}），启动参数见日志",
        title(item),
        code.map_or("无".into(), |c| c.to_string())
    ))
}

/// Danmaku state for one playback.
struct Dm {
    ctx: danmaku::Ctx,
    /// Background results go back to the run loop ([`State::on_dm_result`]).
    tx: mpsc::UnboundedSender<DmMsg>,
    /// Episode last loaded for the script; switching danmaku on loads the current one only if it is not this.
    shown: Option<usize>,
    /// Wrong address or token ([`DmFail::Setup`]): no more requests this playback; the panel shows why.
    dead: Option<String>,
    /// A problem (no match, unavailable) has popped up once; later ones only show in the panel.
    told: bool,
    /// Settings already sent to the script.
    init: bool,
    /// Bumped by every [`State::dm_load`]; a result from an older one is dropped, so a slow auto match finishing
    /// after a manual pick (or after a "set up the API" status) cannot replace it (the script only checks the item).
    seq: u64,
}

/// Results of background danmaku tasks.
enum DmMsg {
    /// (episode index, [`Dm::seq`] of the load, result).
    Loaded(usize, u64, Result<Option<danmaku::Loaded>, DmFail>),
    /// A reply for the script (name, JSON): search results, an episode list.
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
            Some(ev @ ("start-file" | "file-loaded" | "end-file")) => {
                self.on_file_event(ev, &v).await
            }
            Some(_) => {}
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

    /// start-file / file-loaded / end-file.
    async fn on_file_event(&mut self, ev: &str, v: &Value) {
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
                    self.fail(i, v["file_error"].as_str().unwrap_or("未知错误"));
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

    /// Load failed: reports Stopped if it had started, and tells the UI why. The upstream status comes from the cache
    /// proxy; without one, from a probe of the link, done off the event loop (a stuck upstream must not stall mpv).
    fn fail(&mut self, i: usize, mpv_error: &str) {
        let e = &self.queue[i];
        let (known, probe) = match (&e.ready, &self.stream) {
            (Some(r), Some(s)) => (s.fail_status(&r.name), None),
            (Some(r), None) => (None, Some(r.url.clone())),
            (None, _) => (None, None),
        };
        let (name, item_id) = (title(&e.item), e.item.id.clone());
        self.stop(i);
        // Its play session is spent: going back fetches a fresh PlaybackInfo, never replays it from the playlist.
        // `ready` stays, so the end of the session still stops its download.
        self.queue[i].loaded = false;
        self.failed_sent = true;
        let (emby, events, mpv_error) =
            (self.emby.clone(), self.events.clone(), mpv_error.to_owned());
        tokio::spawn(async move {
            let status = match probe {
                Some(url) => emby.probe(&url).await,
                None => known,
            };
            let message = format!("无法播放「{name}」：{}", fail_reason(status, &mpv_error));
            log::warn!("{message}（条目 {item_id}）");
            let _ = events.send(Event::Failed { item_id, message });
        });
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

    /// This playback of entry i is over: Stopped is reported once, and only if it had started.
    fn stop(&mut self, i: usize) {
        if self.cur == Some(i) {
            self.cur = None;
        }
        let was_stopped = std::mem::replace(&mut self.queue[i].stopped, true);
        if !self.queue[i].started || was_stopped {
            return;
        }
        self.report(i, "/Stopped", json!({}));
        let _ = self.events.send(Event::Stopped {
            item_id: self.queue[i].item.id.clone(),
            position_ticks: (self.pos * TICKS_PER_SEC) as i64,
        });
    }
}

/// Danmaku: fetching, and messages to and from the script (`mpv/scripts/danmaku.lua`, script name `danmaku`).
impl State {
    async fn dm_send(&mut self, name: &str, v: Value) {
        self.send(json!(["script-message-to", "danmaku", name, v.to_string()]))
            .await;
    }

    /// Sends the display settings and blocked words to the script.
    async fn dm_init(&mut self) {
        let Some(d) = &self.dm else { return };
        let msg = json!({ "style": d.ctx.cfg.style, "words": d.ctx.cfg.words });
        self.dm_send("init", msg).await;
    }

    /// Danmaku on, an API configured, and not judged misconfigured during this playback.
    fn dm_ready(&self) -> bool {
        self.dm
            .as_ref()
            .is_some_and(|d| d.ctx.cfg.style.on && d.ctx.api.is_some() && d.dead.is_none())
    }

    /// Fetches episode `i` in the background; `deliver` hands the result to the script (current episode),
    /// otherwise it only warms the cache (next episode).
    fn dm_fetch(&self, i: usize, deliver: bool) {
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
        let (dir, tx, seq) = (d.ctx.dir.clone(), d.tx.clone(), d.seq);
        tokio::spawn(async move {
            let r = danmaku::load(&api, &dir, &want).await;
            match (deliver, r) {
                (true, r) => {
                    let _ = tx.send(DmMsg::Loaded(i, seq, r));
                }
                (false, Err(e)) => log::info!("预取下一集弹幕没成：{e}"),
                (false, Ok(_)) => {}
            }
        });
    }

    /// Episode `i` started: settings go to the script once per playback, then its danmaku.
    async fn dm_show(&mut self, i: usize) {
        let Some(d) = &mut self.dm else { return };
        if !std::mem::replace(&mut d.init, true) {
            self.dm_init().await;
        }
        self.dm_load(i).await;
    }

    /// Loads episode `i` for the script, or tells it why there is none.
    async fn dm_load(&mut self, i: usize) {
        let Some(d) = &mut self.dm else { return };
        if !d.ctx.cfg.style.on {
            return; // fetched when switched on (the "style" message)
        }
        d.shown = Some(i);
        d.seq += 1;
        let mut status = match (&d.ctx.api, &d.dead) {
            (None, _) => {
                json!({ "kind": "setup", "text": "还没有填弹幕接口：在 EmbyMini 的设置里填写" })
            }
            (_, Some(why)) => json!({ "kind": "setup", "text": why }),
            _ => json!({ "kind": "loading", "text": "正在加载弹幕…" }),
        };
        let go = status["kind"] == "loading";
        // The episode's first message: the script adopts this item and drops later ones for any other
        status["start"] = true.into();
        status["item"] = self.queue[i].item.id.clone().into();
        self.dm_send("status", status).await;
        if go {
            self.dm_fetch(i, true);
        }
    }

    async fn on_dm_result(&mut self, m: DmMsg) {
        let (i, seq, r) = match m {
            DmMsg::Reply(name, v) => return self.dm_send(name, v).await,
            DmMsg::Loaded(i, seq, r) => (i, seq, r),
        };
        let Some(d) = &mut self.dm else { return };
        if self.cur != Some(i) || seq != d.seq {
            return; // moved to another episode, or a newer load for this one is under way
        }
        let item = self.queue[i].item.id.clone();
        // Problems pop up once per playback; later episodes only show them in the panel
        let toast = !d.told;
        let mut status = match r {
            Ok(Some(l)) => {
                // Sent over IPC rather than read by the script: mpv's Lua `io.open` uses the ANSI code page on
                // Windows and fails on non-ASCII data dirs. `l.list` is already a JSON array; spliced in unparsed.
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
        d.told = true;
        status["item"] = item.into();
        self.dm_send("status", status).await;
    }

    /// The settings page changed the API or blocked words ([`Ctl::Danmaku`]): swap the API for this playback,
    /// resend changed words to the script, and reload the current episode from a new address (which also drops
    /// loads still running against the old one, so their failures cannot mark the new address bad).
    async fn dm_settings(&mut self, api: Option<danmaku::Api>, words: Vec<String>) {
        let Some(d) = &mut self.dm else { return };
        let moved =
            d.ctx.api.as_ref().map(danmaku::Api::base) != api.as_ref().map(danmaku::Api::base);
        d.ctx.api = api;
        d.dead = None;
        let changed = d.ctx.cfg.words != words;
        d.ctx.cfg.words = words;
        let reload = self.cur.filter(|&c| moved && d.shown == Some(c));
        if changed && d.init {
            self.dm_init().await;
        }
        if let Some(cur) = reload {
            self.dm_load(cur).await;
        }
    }

    /// A message from the script: `args[0]` = `embymini-danmaku-<action>`, then its arguments.
    async fn on_danmaku(&mut self, args: &Value) {
        let (Some(cur), Some(d)) = (self.cur, &mut self.dm) else {
            return;
        };
        let what = args[0]
            .as_str()
            .unwrap_or_default()
            .trim_start_matches(DM_MSG);
        let arg = |k: usize| args[k].as_str().unwrap_or_default().to_owned();
        let item = &self.queue[cur].item;
        let (key, index) = (danmaku::key(&self.emby.tag, item), danmaku::index(item));
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
                d.dead = None; // the address may just have been fixed (already swapped in by Ctl::Danmaku)
                self.dm_load(cur).await;
            }
            "search" => self.dm_lookup(arg(1), None),
            "episodes" => {
                let pick = d.ctx.cfg.picks.get(&key).cloned();
                self.dm_lookup(arg(1), Some((index, pick)));
            }
            "pick" => {
                let (Ok(anime), Ok(at)) = (arg(1).parse::<i64>(), arg(2).parse::<i32>()) else {
                    return;
                };
                let pick = danmaku::Pick {
                    anime,
                    offset: at.saturating_sub(index),
                };
                d.ctx.cfg.picks.insert(key.clone(), pick.clone());
                d.dead = None;
                let _ = self.events.send(Event::DanmakuPick { key, pick });
                self.dm_load(cur).await;
            }
            _ => {}
        }
    }

    /// Manual match lookups for the panel, answered via [`DmMsg::Reply`]: `episodes` = None searches shows by
    /// keyword `arg`; Some((Emby index, saved pick)) lists show `arg`'s episodes with the expected one preselected.
    fn dm_lookup(&self, arg: String, episodes: Option<(i32, Option<danmaku::Pick>)>) {
        let Some(d) = &self.dm else { return };
        let Some(api) = d.ctx.api.clone() else { return };
        let tx = d.tx.clone();
        tokio::spawn(async move {
            let (name, v) = match episodes {
                None => ("search", api.search(&arg).await.map(|l| json!(l))),
                Some((index, pick)) => {
                    let anime = arg.parse().unwrap_or(0);
                    let r = api.episodes(anime).await.map(|l| {
                        let off = pick.filter(|p| p.anime == anime).map_or(0, |p| p.offset);
                        let last = i32::try_from(l.len().saturating_sub(1)).unwrap_or(i32::MAX);
                        let at = index.saturating_add(off).clamp(0, last);
                        json!({ "anime": anime, "list": l, "at": at })
                    });
                    ("episodes", r)
                }
            };
            let v = v.unwrap_or_else(|e| json!({ "error": e.to_string() }));
            let _ = tx.send(DmMsg::Reply(name, v));
        });
    }
}

fn ipc_path() -> IpcPath {
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let name = format!(
        "embymini-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    );
    IpcPath(if cfg!(windows) {
        PathBuf::from(format!(r"\\.\pipe\{name}"))
    } else {
        std::env::temp_dir().join(format!("{name}.sock"))
    })
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

/// mpv creates its IPC endpoint only once it is up: poll for it for up to 10 s, giving up early if mpv exits.
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
            .map(|k| {
                Entry::new(Item {
                    id: format!("e{k}"),
                    ..Default::default()
                })
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

    /// Danmaku: settings go first; without an API it says why and fetches nothing; panel settings and manual picks
    /// are saved; a loaded episode goes whole to the script (tagged with its item); results for another episode are
    /// dropped; nothing more is fetched after a bad address; changed blocked words are resent.
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
            seq: 0,
        });
        st.cur = Some(0);
        st.dm_show(0).await;
        st.dm_show(0).await; // init goes once
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
        let seq = st.dm.as_ref().unwrap().seq;
        st.on_dm_result(DmMsg::Loaded(1, seq, Ok(None))).await; // not the current episode: dropped
        st.on_dm_result(DmMsg::Loaded(0, seq, Ok(Some(loaded))))
            .await;
        st.on_dm_result(DmMsg::Loaded(0, seq, Err(DmFail::Setup("地址不对".into()))))
            .await;
        assert!(!st.dm_ready());
        st.dm_settings(None, vec!["剧透".into()]).await; // blocked words changed on the settings page: resent
        assert!(
            st.dm.as_ref().unwrap().dead.is_none(),
            "new settings clear the bad-address verdict"
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
        assert_eq!(cmds[1].1["kind"], "setup"); // no API set
        assert_eq!(
            (&cmds[1].1["start"], &cmds[1].1["item"]),
            (&json!(true), &json!(id0))
        );
        assert_eq!(
            cmds[4].1["item"], id0,
            "the script drops late results by item"
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

    /// A slow auto match finishing after a manual pick (or a reload) must not replace the newer result.
    #[tokio::test]
    async fn danmaku_drops_superseded_loads() {
        let (st_port, _) = emby::tests::fake(|_| ("404 Not Found", "{}".into())).await;
        let (mut st, mpv, _ev) = offline_state(1);
        let (tx, _rx) = mpsc::unbounded_channel();
        st.dm = Some(Dm {
            ctx: danmaku::Ctx {
                api: danmaku::Api::new(
                    emby::http_client().unwrap(),
                    &format!("http://127.0.0.1:{st_port}"),
                ),
                dir: std::env::temp_dir().join(format!("embymini-dm-seq-{}", std::process::id())),
                cfg: danmaku::Config::default(),
            },
            tx,
            shown: None,
            dead: None,
            told: false,
            init: true,
            seq: 0,
        });
        st.cur = Some(0);
        st.dm_load(0).await;
        st.on_danmaku(&json!(["embymini-danmaku-reload"])).await;
        let loaded = |title: &str| danmaku::Loaded {
            list: "[]".into(),
            title: title.into(),
            manual: false,
            stale: false,
        };
        st.on_dm_result(DmMsg::Loaded(0, 1, Ok(Some(loaded("old")))))
            .await;
        st.on_dm_result(DmMsg::Loaded(0, 2, Ok(Some(loaded("new")))))
            .await;
        // A new address from the settings page reloads at once, so a late failure from the old one is dropped
        let seq = |st: &State| st.dm.as_ref().unwrap().seq;
        let at = |path: &str| {
            danmaku::Api::new(
                emby::http_client().unwrap(),
                &format!("http://127.0.0.1:{st_port}{path}"),
            )
        };
        st.dm_settings(at(""), Vec::new()).await;
        assert_eq!(seq(&st), 2, "same address: no reload");
        st.dm_settings(at("/v2"), Vec::new()).await;
        assert_eq!(seq(&st), 3, "new address: reload");
        st.on_dm_result(DmMsg::Loaded(0, 2, Err(DmFail::Setup("bad".into()))))
            .await;
        assert!(
            st.dm_ready(),
            "the old address's failure does not mark the new one bad"
        );
        let loads: Vec<Value> = commands(st, mpv)
            .await
            .into_iter()
            .filter(|c| c[2] == "load")
            .map(|c| serde_json::from_str(c[3].as_str().unwrap()).unwrap())
            .collect();
        assert_eq!(loads.len(), 1, "{loads:?}");
        assert_eq!(loads[0]["title"], "new");
    }

    /// Panel lookups for a manual match, the preselected episode, saving a pick, and when problems pop up.
    #[tokio::test]
    async fn danmaku_lookups() {
        let (port, _) = emby::tests::fake(|path| {
            let ok = |v: Value| ("200 OK", v.to_string());
            match path {
                "/api/v2/search/anime" => {
                    ok(json!({"animes":[{"animeId":7,"animeTitle":"某剧","episodeCount":3}]}))
                }
                "/api/v2/bangumi/7" => ok(json!({"bangumi":{"episodes":[
                    {"episodeId":1},{"episodeId":2},{"episodeId":3}]}})),
                _ => ("404 Not Found", "{}".into()),
            }
        })
        .await;
        let (mut st, mpv, mut ev) = offline_state(1);
        st.queue[0].item = Item {
            id: "e0".into(),
            kind: "Episode".into(),
            series_id: Some("77".into()),
            series_name: Some("某剧".into()),
            index_number: Some(3),
            ..Default::default()
        };
        let (tx, mut rx) = mpsc::unbounded_channel();
        st.dm = Some(Dm {
            ctx: danmaku::Ctx {
                api: danmaku::Api::new(
                    emby::http_client().unwrap(),
                    &format!("http://127.0.0.1:{port}"),
                ),
                dir: std::env::temp_dir().join(format!("embymini-dm-look-{}", std::process::id())),
                cfg: danmaku::Config::default(),
            },
            tx,
            shown: None,
            dead: None,
            told: false,
            init: true,
            seq: 0,
        });
        st.cur = Some(0);
        assert!(st.dm_ready());
        let key = danmaku::key(&st.emby.tag, &st.queue[0].item);
        let seq = |st: &State| st.dm.as_ref().unwrap().seq;
        // Switching on loads the current episode once
        for (on, loads) in [(false, 0), (true, 1), (true, 1), (false, 1), (true, 1)] {
            let style =
                json!({ "on": on, "opacity": if loads == 1 { 0.5 } else { 0.8 } }).to_string();
            st.on_danmaku(&json!(["embymini-danmaku-style", style]))
                .await;
            assert_eq!(seq(&st), loads, "on = {on}");
        }
        while ev.try_recv().is_ok() {} // the DanmakuStyle saves
        // Unchanged words are not resent
        let api = st.dm.as_ref().unwrap().ctx.api.clone();
        st.dm_settings(api, Vec::new()).await;
        let mut ask = async |st: &mut State, msg: Value| {
            st.on_danmaku(&msg).await;
            loop {
                let m = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await;
                match m.expect("a reply within 5 s").unwrap() {
                    DmMsg::Reply(name, v) => return (name, v),
                    DmMsg::Loaded(..) => {} // the load from switching on
                }
            }
        };
        let (name, v) = ask(&mut st, json!(["embymini-danmaku-search", "某剧"])).await;
        assert_eq!((name, v[0]["id"].clone()), ("search", json!(7)));
        let mut at = async |st: &mut State, pick: Option<(i64, i32)>| {
            let picks = &mut st.dm.as_mut().unwrap().ctx.cfg.picks;
            picks.clear();
            if let Some((anime, offset)) = pick {
                picks.insert(key.clone(), danmaku::Pick { anime, offset });
            }
            let (name, v) = ask(st, json!(["embymini-danmaku-episodes", "7"])).await;
            assert_eq!((name, v["anime"].clone()), ("episodes", json!(7)));
            v["at"].as_i64().unwrap()
        };
        assert_eq!(at(&mut st, None).await, 2, "episode 3 is index 2");
        assert_eq!(
            at(&mut st, Some((7, -1))).await,
            1,
            "a saved offset applies"
        );
        assert_eq!(
            at(&mut st, Some((8, -1))).await,
            2,
            "another show's offset does not"
        );
        assert_eq!(
            at(&mut st, Some((7, 10))).await,
            2,
            "clamped to the last episode"
        );
        let (_, v) = ask(&mut st, json!(["embymini-danmaku-episodes", "9"])).await;
        assert!(v["error"].as_str().unwrap().contains("404"), "{v}");

        st.on_danmaku(&json!(["embymini-danmaku-pick", "7", "0"]))
            .await;
        assert!(
            matches!(ev.try_recv(), Ok(Event::DanmakuPick { pick, .. }) if pick == danmaku::Pick { anime: 7, offset: -2 })
        );
        let seq = seq(&st);
        assert_eq!(seq, 2, "the pick reloads");

        // One pop-up per playback: a success in between does not re-arm it
        for r in [
            Ok(None),
            Ok(Some(danmaku::Loaded {
                list: "[]".into(),
                title: String::new(),
                manual: true,
                stale: false,
            })),
            Ok(None),
        ] {
            st.on_dm_result(DmMsg::Loaded(0, seq, r)).await;
        }
        // Misconfigured: says why, and a load still in flight can no longer replace that
        st.dm.as_mut().unwrap().dead = Some("地址不对".into());
        assert!(!st.dm_ready());
        st.dm_load(0).await;
        st.on_dm_result(DmMsg::Loaded(0, seq, Ok(None))).await;

        let cmds = commands(st, mpv).await;
        assert!(!cmds.iter().any(|c| c[2] == "init"), "init is not resent");
        let status: Vec<Value> = cmds
            .into_iter()
            .filter(|c| c[2] == "status")
            .map(|c| serde_json::from_str(c[3].as_str().unwrap()).unwrap())
            .collect();
        let brief: Vec<_> = status
            .iter()
            .map(|v| (v["kind"].as_str().unwrap(), v["toast"].as_bool()))
            .collect();
        assert_eq!(
            brief,
            [
                ("loading", None),
                ("loading", None),
                ("none", Some(true)),
                ("none", Some(false)),
                ("setup", None)
            ]
        );
        assert_eq!(status[4]["text"], "地址不对");
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
        let (session, theirs) = fake_session(emby);
        let (ctl, ctl_rx) = mpsc::unbounded_channel();
        let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
        let run = tokio::spawn(session.run(ctl_rx, ev_tx));

        // Fake mpv: answers loadfile, reports start, and progress at 95% triggers a preload (to the stuck upstream)
        let (rd, mut wr) = tokio::io::split(theirs);
        let mut lines = BufReader::new(rd).lines();
        answer_loadfile(&mut lines, &mut wr).await;
        let mut say = async |v: Value| wr.write_all(format!("{v}\n").as_bytes()).await.unwrap();
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

    /// The queue: an episode brings its season, resuming the episode played; an episode missing from the list, or a
    /// season list that fails to load, plays alone; anything else plays alone; the item itself failing is an error.
    #[tokio::test]
    async fn queue_from_the_season() {
        let (port, _) = emby::tests::fake(|path| {
            let ep = |id: &str, series: &str| {
                format!(
                    r#"{{"Id":"{id}","Type":"Episode","SeriesId":"{series}","SeasonId":"ss","UserData":{{"PlaybackPositionTicks":25000000}}}}"#
                )
            };
            match path {
                p if p.ends_with("/playbackinfo") => (
                    "200 OK",
                    include_str!("../testdata/playbackinfo.json").into(),
                ),
                "/emby/users/uid/items/e2" => ("200 OK", ep("e2", "s")),
                "/emby/users/uid/items/e9" => ("200 OK", ep("e9", "s")),
                "/emby/users/uid/items/x" => ("200 OK", ep("x", "bad")),
                "/emby/users/uid/items/m" => ("200 OK", r#"{"Id":"m","Type":"Movie"}"#.into()),
                "/emby/shows/s/episodes" => (
                    "200 OK",
                    format!(r#"{{"Items":[{},{},{}]}}"#, ep("e1", "s"), ep("e2", "s"), ep("e3", "s")),
                ),
                _ => ("500 Internal Server Error", String::new()),
            }
        })
        .await;
        let emby = emby::tests::local(port);
        let ids = |q: &[Entry]| {
            q.iter()
                .map(|e| e.item.id.as_str())
                .collect::<Vec<_>>()
                .join(",")
        };

        let (q, at) = build_queue(&emby, "e2", None, false).await.unwrap();
        assert_eq!((ids(&q).as_str(), at), ("e1,e2,e3", 1));
        assert!(
            (q[1].start - 2.5).abs() < 1e-9,
            "resume point {}",
            q[1].start
        );
        assert_eq!(
            q.iter().map(|e| e.ready.is_some()).collect::<Vec<_>>(),
            [false, true, false]
        );
        let (q, _) = build_queue(&emby, "e2", None, true).await.unwrap();
        assert!(
            q[1].start.abs() < 1e-9,
            "from the start, got {}",
            q[1].start
        );

        for id in ["e9", "x", "m"] {
            let (q, at) = build_queue(&emby, id, None, false).await.unwrap();
            assert_eq!((ids(&q).as_str(), at), (id, 0));
            assert!(q[0].ready.is_some());
        }
        assert!(build_queue(&emby, "gone", None, false).await.is_err());
    }

    /// mpv's arguments keep their order: bundled config, personal config (only if the file exists), volume, cache
    /// dirs, thumbfast's mpv path, then what EmbyMini controls, then the caller's extras.
    #[test]
    fn mpv_args_order() {
        let dir = std::env::temp_dir().join(format!("embymini-args-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let conf = dir.join("mpv.conf");
        std::fs::write(&conf, "").unwrap();
        let opts = Options {
            mpv: "m".into(),
            config_dir: Some("cfg".into()),
            preload_percent: 80.0,
            volume: Some(55.0),
            cache_dir: Some("c".into()),
            user_conf: Some(conf.clone()),
            stream: None,
            danmaku: None,
            extra_args: vec!["--x".into()],
        };
        let ua = format!("--user-agent={}", emby::UA);
        let inc = format!("--include={}", conf.display());
        assert_eq!(
            mpv_args(&opts, Path::new("ipc")),
            [
                "--config-dir=cfg",
                inc.as_str(),
                "--volume=55",
                "--gpu-shader-cache-dir=c",
                "--icc-cache-dir=c",
                "--watch-later-dir=c",
                "--script-opts-append=thumbfast-mpv_path=m",
                "--input-ipc-server=ipc",
                ua.as_str(),
                "--force-window=immediate",
                "--idle=once",
                "--x",
            ]
        );
        let bare = Options {
            config_dir: None,
            volume: None,
            cache_dir: None,
            user_conf: Some(dir.join("missing.conf")),
            extra_args: Vec::new(),
            ..opts
        };
        assert_eq!(
            mpv_args(&bare, Path::new("ipc")),
            [
                "--script-opts-append=thumbfast-mpv_path=m",
                "--input-ipc-server=ipc",
                ua.as_str(),
                "--force-window=immediate",
                "--idle=once",
            ]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn bundled_dir_needs_the_mpv_folder() {
        let dir = std::env::temp_dir().join(format!("embymini-bundled-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(bundled_dir(&dir), None);
        std::fs::create_dir(dir.join("mpv")).unwrap();
        assert_eq!(bundled_dir(&dir), Some(dir.join("mpv")));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// PlaybackInfo → one episode's parameters: only external subtitles are added, named after the video's cache
    /// name; a server error code refuses playback (an empty one does not); cache names keep only safe characters.
    #[test]
    fn ready_from_real_playbackinfo() {
        let emby = crate::emby::tests::client();
        let item = Item {
            id: "1153511".into(),
            name: "片名".into(),
            ..Default::default()
        };
        let r = ready(&emby, &item, fixture("playbackinfo.json")).unwrap();
        assert_eq!(
            r.name,
            cache_name(&[&emby.tag, "1153511", "mediasource_1153511"])
        );
        assert!(
            r.name.ends_with("_1153511_mediasource-1153511"),
            "{}",
            r.name
        );
        let subs: Vec<_> = r
            .subs
            .iter()
            .map(|s| (s.file.as_str(), s.lang.as_str()))
            .collect();
        assert_eq!(subs, [(format!("{}_5.ass", r.name).as_str(), "zh-CN")]);
        assert_eq!(r.source_id, "mediasource_1153511");

        let mut info: emby::PlaybackInfo = fixture("playbackinfo.json");
        info.error_code = Some(String::new());
        assert!(ready(&emby, &item, info).is_ok());
        let mut info: emby::PlaybackInfo = fixture("playbackinfo.json");
        info.error_code = Some("NoCompatibleStream".into());
        assert!(matches!(
            ready(&emby, &item, info),
            Err(Error::Other(m)) if m == "服务器拒绝播放「片名」（NoCompatibleStream）"
        ));

        assert_eq!(cache_name(&["a.b/c", "Ab-9", "中"]), "a-b-c_Ab-9_-");
    }

    /// mpv exiting on its own: a clean exit says nothing; an exit code or a signal (no code) is shown to the user.
    #[cfg(unix)]
    #[test]
    fn crash_messages() {
        use std::os::unix::process::ExitStatusExt;
        let item = Item {
            name: "片名".into(),
            ..Default::default()
        };
        let msg = |raw| crash_message(&item, &Ok(ExitStatus::from_raw(raw)));
        assert_eq!(msg(0), None);
        assert_eq!(
            msg(1 << 8).as_deref(),
            Some("无法播放「片名」：播放器意外退出（退出码 1），启动参数见日志")
        );
        assert!(msg(9).unwrap().contains("退出码 无"), "killed by a signal");
        let gone = crash_message(&item, &Err(std::io::Error::other("wait failed")));
        assert!(gone.unwrap().contains("退出码 无"));
    }

    /// The Unix IPC socket file goes away with its path, also when a start is abandoned.
    #[cfg(unix)]
    #[test]
    fn ipc_socket_removed_on_drop() {
        let ipc = ipc_path();
        let file = ipc.0.clone();
        std::fs::write(&file, "").unwrap();
        drop(ipc);
        assert!(!file.exists());
    }

    /// Reports go out in order; a Progress with anything queued behind it is stale and skipped.
    #[tokio::test]
    async fn reporter_drops_stale_progress() {
        let (port, heads) = emby::tests::fake(|_| ("200 OK", "{}".into())).await;
        let (tx, task) = spawn_reporter(emby::tests::local(port));
        for path in ["/Sessions/Playing", "/a/Progress", "/b/Progress"] {
            tx.send((path.into(), json!({}))).unwrap();
        }
        drop(tx);
        task.await.unwrap();
        let sent: Vec<String> = heads
            .lock()
            .unwrap()
            .iter()
            .map(|h| h.split(' ').nth(1).unwrap_or_default().to_owned())
            .collect();
        assert_eq!(sent, ["/emby/Sessions/Playing", "/emby/b/Progress"]);
    }

    /// mpv dying before anything played tells the UI, unless we asked it to quit.
    #[cfg(unix)]
    #[tokio::test]
    async fn crash_reported_only_when_unasked() {
        for asked in [false, true] {
            let (mut session, theirs) = fake_session(crate::emby::tests::client());
            session.child = tokio::process::Command::new("false").spawn().unwrap();
            let (ctl, ctl_rx) = mpsc::unbounded_channel();
            let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
            let run = tokio::spawn(session.run(ctl_rx, ev_tx));
            let (rd, wr) = tokio::io::split(theirs);
            let mut lines = BufReader::new(rd).lines();
            if asked {
                ctl.send(Ctl::Quit).unwrap();
                while let Some(l) = lines.next_line().await.unwrap() {
                    if serde_json::from_str::<Value>(&l).unwrap()["command"] == json!(["quit"]) {
                        break;
                    }
                }
            }
            drop((lines, wr)); // mpv is gone
            run.await.unwrap();
            let mut got = Vec::new();
            while let Some(e) = ev_rx.recv().await {
                got.push(e);
            }
            let failed = got.iter().any(|e| matches!(e, Event::Failed { .. }));
            assert_eq!(failed, !asked, "asked to quit: {asked}; events {got:?}");
            assert!(matches!(got.last(), Some(Event::Ended { .. })));
        }
    }

    /// A two-episode session on the first episode, talking to a fake mpv (the returned end) and a dummy child.
    fn fake_session(emby: Emby) -> (Session, tokio::io::DuplexStream) {
        let (st, _, _) = offline_state(2);
        let mut queue = st.queue;
        queue[0].ready = Some(ready_for("e0"));
        let (ours, theirs) = tokio::io::duplex(1 << 16);
        let session = Session {
            emby,
            queue,
            at: 0,
            // A child that exits at once; Windows has no `true`
            child: if cfg!(windows) {
                tokio::process::Command::new("cmd")
                    .args(["/C", "exit"])
                    .spawn()
            } else {
                tokio::process::Command::new("true").spawn()
            }
            .unwrap(),
            io: Box::new(ours),
            ipc: IpcPath(std::env::temp_dir().join("embymini-fake-session.sock")),
            preload: 80.0,
            stream: None,
            danmaku: None,
        };
        (session, theirs)
    }

    /// Fake mpv: reads commands until the first loadfile and answers it with entry id 1.
    async fn answer_loadfile(
        lines: &mut tokio::io::Lines<BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>>,
        wr: &mut tokio::io::WriteHalf<tokio::io::DuplexStream>,
    ) {
        loop {
            let v: Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            if v["command"][0] == "loadfile" {
                let r = json!({ "request_id": v["request_id"], "error": "success", "data": { "playlist_entry_id": 1 } });
                wr.write_all(format!("{r}\n").as_bytes()).await.unwrap();
                return;
            }
        }
    }

    /// mpv may send invalid UTF-8 (its IPC docs say so: odd file tags, non-UTF-8 file names); one such line must not
    /// end the session, or keys and quit would stop working while mpv plays on.
    #[tokio::test]
    async fn invalid_utf8_from_mpv_keeps_the_session() {
        let (session, theirs) = fake_session(crate::emby::tests::client());
        let (ctl, ctl_rx) = mpsc::unbounded_channel();
        let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
        let run = tokio::spawn(session.run(ctl_rx, ev_tx));
        let (rd, mut wr) = tokio::io::split(theirs);
        let mut lines = BufReader::new(rd).lines();
        answer_loadfile(&mut lines, &mut wr).await;
        wr.write_all(b"{\"event\":\"property-change\",\"name\":\"current-tracks/audio\",\"data\":{\"title\":\"\xc4\xe3\"}}\n")
            .await
            .unwrap();
        let start = json!({ "event": "start-file", "playlist_entry_id": 1 });
        wr.write_all(format!("{start}\n").as_bytes()).await.unwrap();
        // start-file's episode list reaching mpv proves the bad line before it was handled; only then ask to quit
        let got_quit = tokio::time::timeout(Duration::from_secs(2), async {
            while let Ok(Some(l)) = lines.next_line().await {
                let cmd = serde_json::from_str::<Value>(&l).unwrap()["command"].take();
                if cmd[2] == "episodes" {
                    ctl.send(Ctl::Quit).unwrap();
                } else if cmd == json!(["quit"]) {
                    return true;
                }
            }
            false
        })
        .await;
        assert_eq!(got_quit, Ok(true), "a non-UTF-8 line ended the session");
        drop((lines, wr));
        run.await.unwrap();
        let mut got = Vec::new();
        while let Some(e) = ev_rx.recv().await {
            got.push(e);
        }
        assert!(
            matches!(got.last(), Some(Event::Ended { .. })),
            "session ended without Ended: {got:?}"
        );
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

    /// Events until the Failed one (sent from a background task).
    async fn until_failed(ev: &mut mpsc::UnboundedReceiver<Event>) -> Vec<Event> {
        let mut got = Vec::new();
        while let Ok(Some(e)) = tokio::time::timeout(Duration::from_secs(5), ev.recv()).await {
            let done = matches!(e, Event::Failed { .. });
            got.push(e);
            if done {
                break;
            }
        }
        got
    }

    /// An entry that errors out after it started still reports Stopped; a failed entry leaves the playlist and
    /// going back to it fetches PlaybackInfo afresh instead of reusing the old link and play session.
    #[tokio::test(flavor = "multi_thread")]
    async fn failed_entry_stops_and_refetches() {
        let dir = std::env::temp_dir().join(format!("embymini-fail-{}", std::process::id()));
        let (mut st, _mpv, mut ev) = offline_state(2);
        st.stream = Stream::start(&dir, false); // knows no status for these: no network involved
        for (k, e) in st.queue.iter_mut().enumerate() {
            e.ready = Some(ready_for(&format!("e{k}")));
            e.entry_id = Some(7 + k as i64);
            e.loaded = true;
        }
        st.on_message(json!({ "event": "start-file", "playlist_entry_id": 7 }))
            .await;
        st.on_message(json!({ "event": "file-loaded" })).await;
        st.on_message(json!({ "event": "end-file", "playlist_entry_id": 7, "reason": "error", "file_error": "boom" }))
            .await;
        let got = until_failed(&mut ev).await;
        assert!(
            got.iter().any(|e| matches!(e, Event::Stopped { .. })),
            "{got:?}"
        );
        assert!(
            matches!(got.last(), Some(Event::Failed { message, .. }) if message.contains("mpv 无法打开（boom）")),
            "{got:?}"
        );
        assert_eq!(st.cur, None);
        assert!(!st.queue[0].loaded, "not replayed from mpv's playlist");
        st.jump(0).await;
        assert_eq!(st.want, Some(0), "a failed entry must be prepared again");

        // Never loaded at all: nothing to report, but its session is spent too
        st.on_message(json!({ "event": "start-file", "playlist_entry_id": 8 }))
            .await;
        st.on_message(json!({ "event": "end-file", "playlist_entry_id": 8, "reason": "error", "file_error": "boom" }))
            .await;
        let got = until_failed(&mut ev).await;
        assert!(
            !got.iter().any(|e| matches!(e, Event::Stopped { .. })),
            "{got:?}"
        );
        assert!(
            st.queue[1].ready.is_some(),
            "kept: the session end stops its download"
        );
        assert_eq!(st.cur, None);
        st.jump(1).await;
        assert_eq!(st.want, Some(1));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Without the cache proxy a failure probes the link; a stuck upstream must not hold up mpv's event handling.
    #[tokio::test]
    async fn failure_probe_does_not_block() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((sock, _)) = listener.accept().await {
                held.push(sock);
            }
        });
        let (mut st, _mpv, _ev) = offline_state(1);
        let mut r = ready_for("e0");
        r.url = format!("http://127.0.0.1:{port}/e0");
        st.queue[0].ready = Some(r);
        st.queue[0].entry_id = Some(7);
        st.on_message(json!({ "event": "start-file", "playlist_entry_id": 7 }))
            .await;
        let end = st
            .on_message(json!({ "event": "end-file", "playlist_entry_id": 7, "reason": "error" }));
        assert!(
            tokio::time::timeout(Duration::from_secs(5), end)
                .await
                .is_ok(),
            "fail() waited for the probe"
        );
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
        assert_eq!(fail_reason(Some(304), "x"), "拉流返回 HTTP 304");
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

    /// What one run of mpv with the bundled config showed.
    #[derive(Debug, PartialEq)]
    struct MpvRun {
        config: bool,
        modernz: bool,
        thumbfast: bool,
        builtin_osc: bool,
        /// Error lines (`[e]`) from mpv's log; should be none on Windows (bad config lines and unknown options land here).
        errors: Vec<String>,
    }

    fn bundled_mpv_run(config_dir: &Path) -> MpvRun {
        let exe = if cfg!(windows) {
            config_dir.join("mpv.exe") // CI downloads mpv.exe into src-tauri/mpv/ before cargo test
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

    /// The personal config overrides the bundled one, but cannot change what EmbyMini controls (such as the UA). Uses the real [`mpv_args`].
    #[test]
    #[cfg_attr(not(windows), ignore = "需要 PATH 里的 mpv；Windows CI 上默认运行")]
    fn user_conf_overrides_bundled() {
        let dir = std::env::temp_dir().join(format!("embymini-user-conf-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let conf = dir.join("mpv.conf");
        // The bundled config says 1GiB; the personal one says 123MiB and also tries to change the UA
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
        // The expanded line (the log also records the raw command line first, still with `${…}`)
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

    /// The danmaku script (layout, merging, escaping, panel, message handling) inside a real mpv against a fake `mp`:
    /// `testdata/danmaku_test.lua` loads `mpv/scripts/danmaku.lua`, drives it and quits with 1 on any failed check.
    #[test]
    #[cfg_attr(not(windows), ignore = "需要 PATH 里的 mpv；Windows CI 上默认运行")]
    fn danmaku_script() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mpv = if cfg!(windows) {
            dir.join("mpv").join("mpv.exe")
        } else {
            "mpv".into()
        };
        let out = std::process::Command::new(mpv)
            .args([
                "--no-config",
                "--idle=yes",
                "--vo=null",
                "--ao=null",
                "--force-window=no",
                &format!(
                    "--script={}",
                    dir.join("testdata").join("danmaku_test.lua").display()
                ),
                &format!(
                    "--script-opts=dmtest-src={}",
                    dir.join("mpv")
                        .join("scripts")
                        .join("danmaku.lua")
                        .display()
                ),
            ])
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout) + String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success() && text.contains("danmaku_test: ok"),
            "{text}"
        );
    }

    /// Copies the bundled config directory to `to` (as if installed elsewhere).
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

    /// The installed path chain: resource dir (with `\\?\` on Windows) → bundled_dir → mpv reads the bundled config, loads ModernZ,
    /// skips the built-in OSC and reports no config errors (except d3d11-adapter on machines without an NVIDIA GPU). Also run from a directory with Chinese characters and spaces (what %LOCALAPPDATA% looks like for a Chinese user name).
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
                // Control group: how mpv behaves without stripping the prefix. Printed only, not a pass condition
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
                // d3d11-adapter=NVIDIA always errors on machines without an NVIDIA GPU (CI): the machine, not a config mistake, so let it pass;
                // Linux mpv has no such option, so only Windows requires zero other errors
                let errors: Vec<_> = got
                    .errors
                    .iter()
                    .filter(|l| !l.contains("d3d11-adapter") && !l.contains("No adapter matching"))
                    .collect();
                assert!(errors.is_empty(), "mpv 报错：{errors:#?}");
            }
        };
        // Get the resource dir via canonicalize, as Tauri does (with the \\?\ prefix on Windows)
        check(&std::fs::canonicalize(env!("CARGO_MANIFEST_DIR")).unwrap());

        let odd = std::env::temp_dir().join(format!("EmbyMini 测试 {}", std::process::id()));
        copy_dir(
            &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("mpv"),
            &odd.join("mpv"),
        );
        check(&std::fs::canonicalize(&odd).unwrap());
        std::fs::remove_dir_all(&odd).unwrap();
    }

    // ---- Real-server end to end: headless mpv goes start → preload at the set progress → next episode → quit, then the server's watch state is checked and restored ----

    fn env(k: &str) -> String {
        std::env::var(k).unwrap_or_else(|_| panic!("缺环境变量 {k}"))
    }

    async fn playable(c: &Emby, item: &Item) -> bool {
        match prepare(c, item).await {
            Ok(r) => c.probe(&r.url).await == Some(206), // probe sends the token header
            Err(_) => false,
        }
    }

    /// Picks an episode whose stream and next episode's stream both load (some items have a working PlaybackInfo but a 403 stream). EMBY_TEST_ITEM picks one.
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

    /// Puts the watch state back as before the test (standard APIs only: PlayedItems + Sessions/Playing/Stopped).
    /// Returns why it could not (no panic here, so later items still get restored).
    async fn restore(c: &Emby, before: &Item) -> Option<String> {
        let path = format!("/Users/{}/PlayedItems/{}", c.user_id, before.id);
        if before.user_data.played {
            c.call(Method::POST, &path, NO_QUERY, None).await.unwrap();
        } else {
            c.call(Method::DELETE, &path, NO_QUERY, None).await.unwrap(); // also clears the position
            let p = before.user_data.playback_position_ticks;
            if p > 0 {
                // A bare Stopped does not save the position (tested 2026-10-13); it takes Playing → Stopped with a PlaySessionId
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

    /// Logs in to the real server with the account from the environment.
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

    /// mpv from PATH + the bundled config from the repo.
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

    /// Plays through the local cache proxy like the app does (cache in a temp dir, removed afterwards). Ahead-only: whole-file mode would
    /// pull whole episodes through the relay within the test's minute or two (GBs each time); stream.rs unit tests cover that mode.
    fn proxied(opts: Options, tag: &str) -> (Options, PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("embymini-e2e-cache-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let stream = Some(Stream::start(&dir, false).expect("缓存代理起不来"));
        (Options { stream, ..opts }, dir)
    }

    /// mpv processes started by this process that are still alive (found by this process's own IPC name on their command line).
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

    /// Start abandoned midway (the app drops the start future on exit): mpv's window is already open and must be killed with it,
    /// never left empty. The upstream accepts connections and never answers, so the start hangs on PlaybackInfo.
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
        // After 1.5 s the start is still waiting on the silent upstream while mpv is up; then give up
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

    /// Sends mpv a command over a separate IPC connection of the test's own and returns `data`.
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

    /// A broken upstream item (PlaybackInfo fine, stream 403): expect Failed with the 403 reason and no Stopped report.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "需要真服账号、PATH 里的 mpv，以及 EMBY_BROKEN_ITEM（一个直链 403 的条目）"]
    async fn e2e_broken_item() {
        let c = login_from_env().await;
        let id = env("EMBY_BROKEN_ITEM");
        // An item fixed upstream would change its watch state once played, so probe first and stop if it plays
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
        let side = session.ipc.0.clone(); // the test's own IPC connection for reading mpv properties
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
        // The user turns subtitles off in episode 1: episode 2 should keep that
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
        // Check right after it finished (reports go out from their own task, so wait a little): the later "previous" replays ep1 from the start, and some servers then clear its played state
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

        // Previous: stop episode 2 and replay episode 1 from the start
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

    /// Danmaku end to end: real server + real danmaku API (`DANMU_API`); after start the episode is matched and loaded
    /// into the script; with SHOT_DIR it screenshots the danmaku and the settings panel.
    /// `EMBY_TEST_ITEM` picks the episode (a Chinese drama or anime matches best); without it any episode is used,
    /// which may not match and then only checks nothing fails. Headless setup as in [`e2e_ui_shots`].
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
        let ipc = session.ipc.0.clone();
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

    /// Player screenshots + leak check: a real start (with a window), common actions simulated and captured to SHOT_DIR,
    /// while asserting that no address mpv can show (path, playlist, external subtitles) contains the token.
    /// Without a display: `Xvfb :98 -screen 0 2560x1600x24 &`, `DISPLAY=:98 MPV_EXTRA=--gpu-sw`.
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
        let ipc = session.ipc.0.clone();
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
        // Episode 1's external subtitle URL (after the switch the track list is episode 2's)
        let ep1_tracks = get_property(&ipc, "track-list").await;
        pause(3000).await;
        key("RIGHT").await; // the first seek after loading must show the progress bar too
        pause(500).await;
        shot("02-first-seek").await;
        key("UP").await;
        pause(300).await;
        shot("03-volume").await;
        key("F8").await;
        pause(1000).await;
        shot("04-episode-menu").await;
        key("DOWN").await;
        key("ENTER").await; // pick the next episode in the episode menu
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
        // Neither the token nor an api_key parameter may appear (assert messages never print the token)
        let header = c.token_header();
        let token = header.trim_start_matches("X-Emby-Token: ");
        assert!(!token.is_empty());
        let leaked = shown
            .iter()
            .map(Value::to_string)
            .filter(|v| v.contains(token) || v.contains("api_key"))
            .count();
        assert_eq!(leaked, 0, "mpv 能显示出来的地址里有 {leaked} 处带 token");
        // Video and external subtitles both go through the cache proxy (path first; embedded tracks have a null external-filename)
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

    /// Cache proxy acceptance (whole-file mode, real server + headless mpv): background whole-file download, seek time to uncached / cached
    /// positions, no more downloading after the player closes, and reopening the same episode starts from the cache. Pulls a few hundred MB through the relay.
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
            let ipc = session.ipc.0.clone();
            let (ctl, ctl_rx) = mpsc::unbounded_channel();
            let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
            let run = tokio::spawn(session.run(ctl_rx, ev_tx));
            assert!(matches!(ev_rx.recv().await, Some(Event::Started { .. })));
            (t0.elapsed(), ipc, ctl, ev_rx, run)
        };
        // Seek to `at` seconds and wait for time-pos to move past it
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
