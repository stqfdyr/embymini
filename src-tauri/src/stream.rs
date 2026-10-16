//! 本地缓存代理：mpv 和进度条预览子进程都从这里读视频（`http://127.0.0.1:<端口>/v/<key>/…`）、字幕（`/s/<key>/…`），
//! 这里边播边把整个文件按块落盘，已下好的部分从磁盘回，没有的向上游要。
//!
//! - 只绑回环地址；`key` 是每个文件登记时随机生成的 128 位 id，只对应那一个文件，不是通用转发。
//! - token 只在这里：mpv 和预览子进程都拿不到；重定向自己跟，只向服务器同源发 token。
//! - 磁盘：`<缓存>/<上游>_<条目>_<版本>/`，按 [`BLOCK`] 分块，完成的块叫 `<n>`，正在写的 `<n>.<序号>.part`，
//!   `meta.json` 记总大小（和上游对不上就整个作废）。有哪些块 = 目录里有哪些文件，淘汰就是删文件。
//! - 下载任务（同一块同一时刻只有一个任务在写）：
//!   - 主任务：每个文件一个，开区间顺序往后写；碰到已有的块就断开，从主播放器最近读的位置之后的下一个缺块重开，
//!     到文件尾回绕补前面的缺块，直到整片下完（「整片」模式）。只缓存前方（关掉整片或到了容量上限）时只写到读的位置前方 [`AHEAD`] 块。
//!     主任务刚开不到 2 秒又有主播放器要别处（mkv 打开时先读头、再读文件尾的索引）：给一个两块的短任务，不来回抢。
//!   - 预览任务：进度条预览（请求头 `X-EmbyMini-Role: thumb`）和预取下一集用，闭区间只取 1–2 块，每个文件最多 [`PREVIEWS`] 个。
//!   - 卡住对冲：请求 3 秒没收到正文，在同一位置再开一个，先出数据的留下；最多同时 2 个，之后按 5、8 秒再换。
//! - 保留：播放器每次换集调 [`Stream::retain`]（当前集 + 已在 mpv 列表里的下一集），其余的停掉并删除；退出不删，下次打开同一部片直接走本地。
//! - 容量：min(20 GiB, 可用空间 + 现有视频缓存 − 10 GiB)，到了就只缓存前方，读需要的块时删当前文件里最久没读的块。
//! - 诊断数据：每个文件释放时记一条 `stream`（上游字节、本地命中、对冲、首字节耗时、覆盖率）。

use crate::telemetry;
use bytes::Bytes;
use hyper::body::{Frame, Incoming};
use hyper::{Request, Response, StatusCode};
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::future::Future;
use std::io::Write;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::{mpsc, watch};

/// 块大小。分块文件而不是一个大稀疏文件：NTFS 的非稀疏文件在远处偏移写入要同步补零。
pub const BLOCK: u64 = 4 << 20;
/// 只缓存前方时，写到主播放器读的位置前方这么多块（64 MiB，等于 mpv 自己的一段预读）。
const AHEAD: u64 = 16;
/// 主播放器要的块在主任务写入点前方这么多块以内就等它，否则在那里另开主任务（拖动进度）。
const MAIN_NEAR: u64 = 2;
const PREVIEWS: usize = 2;
/// 对冲：第一个请求多久没出数据就再开一个，之后每隔多久换掉较早的那个。
const HEDGE: [u64; 3] = [3, 5, 8];
/// 一直拿不到数据就放弃（回 504），读的一方会再来。
const OPEN_LIMIT: Duration = Duration::from_secs(30);
const READ_CHUNK: u64 = 256 << 10;
const MAX_CAP: u64 = 20 << 30;
/// 磁盘至少留这么多可用空间。
const RESERVE: u64 = 10 << 30;
/// 容量下限：盘快满时也要放得下前方这一段，否则什么都写不进去。
const MIN_CAP: u64 = 2 * AHEAD * BLOCK;
const SUBS_CAP: u64 = 200 << 20;
const ROLE: &str = "x-embymini-role";

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn blocks(size: u64) -> usize {
    size.div_ceil(BLOCK) as usize
}

fn block_len(size: u64, b: usize) -> u64 {
    BLOCK.min(size - b as u64 * BLOCK)
}

/// 谁在读：主播放器、进度条预览，或预取下一集（不经 HTTP，由 [`Stream::prefetch`] 发起）。
#[derive(Clone, Copy, PartialEq, Debug)]
enum Role {
    Main,
    Thumb,
    Prefetch,
}

/// 向上游要数据时带的鉴权：token 只发给 API 根地址同源的主机。
#[derive(Clone)]
pub struct Auth {
    pub token: String,
    pub origin: String,
}

struct Src {
    url: String,
    auth: Auth,
    /// 重定向后的地址，后续 Range 请求直接用；出错时退回原地址。
    resolved: Option<String>,
}

#[derive(Debug)]
enum Fail {
    /// 上游回的 HTTP 状态码（4xx 等），是定论：原样回给 mpv（403、404 时界面能给出原因）。
    Status(u16),
    /// 上游暂时出错（5xx、429，中继和上游不稳时常见）：和网络错误一样重试，一直不好才把这个码回给 mpv。
    Busy(u16),
    Net(String),
    Timeout,
}

impl Fail {
    fn code(&self) -> u16 {
        match self {
            Self::Status(s) | Self::Busy(s) => *s,
            Self::Net(_) => 502,
            Self::Timeout => 504,
        }
    }

    /// 重试有没有意义（网络错误、超时、5xx）。
    fn transient(&self) -> bool {
        !matches!(self, Self::Status(_))
    }
}

/// [`Stream::pump`] 为什么停下。
enum Stop {
    /// 写到了要的末尾，或碰到已有 / 有人在写的块。
    Hit,
    /// 只缓存前方：写到读的位置前方 [`AHEAD`] 块了，第 b 块先不写（连接已放掉）。
    Ahead(usize),
}

struct Part {
    path: PathBuf,
    written: u64,
}

struct Task {
    id: u64,
    task: tauri::async_runtime::JoinHandle<()>,
}

struct Main {
    t: Task,
    /// 正在写（或马上要写）的块。
    at: usize,
    since: Instant,
}

#[derive(Default)]
struct St {
    size: Option<u64>,
    done: Vec<bool>,
    /// 每块最近一次读写的时刻（全局计数），到容量上限时删最久没用的。
    touched: Vec<u64>,
    writing: HashMap<usize, Part>,
    main: Option<Main>,
    /// 预览任务、它要的块区间（闭区间）和角色（主播放器的短任务也在这里）。
    previews: Vec<(Task, usize, usize, Role)>,
    last_read: u64,
    /// 最近一次失败和它的时刻；在这之后的某个时刻前不再重试。
    fail: Option<(Fail, Instant)>,
    retry_at: Option<Instant>,
    seq: u64,
}

impl St {
    fn has(&self, b: usize) -> bool {
        self.done.get(b).copied().unwrap_or(false) || self.writing.contains_key(&b)
    }

    fn busy(&self) -> bool {
        self.main.is_some() || !self.previews.is_empty()
    }

    /// 在 `since` 之后发生的上游 HTTP 错误（读的一方据此回错误码，而不是一直等）。
    fn status_since(&self, since: Instant) -> Option<u16> {
        match &self.fail {
            Some((Fail::Status(s), at)) if *at >= since => Some(*s),
            _ => None,
        }
    }
}

/// 一个文件的统计，释放时记一条 `stream`。
#[derive(Default)]
struct Stats {
    /// 上游字节：主任务 / 预览 / 预取下一集。
    up: [u64; 3],
    /// 回给 mpv 的字节，和其中读的时候已经在磁盘上的部分。
    served: u64,
    hit: u64,
    /// 请求数：主播放器 / 预览；预览请求全程没等上游的次数。
    reqs: [u64; 2],
    thumb_hit: u64,
    opens: u64,
    hedges: u64,
    fails: u64,
    ttfb_ms: Vec<u64>,
    wait_ms: u64,
}

struct File {
    name: String,
    /// 已被 retain 掉（目录在删）：还连着的读不再安排下载、直接断开。
    gone: AtomicBool,
    dir: PathBuf,
    key: String,
    src: Mutex<Src>,
    st: Mutex<St>,
    /// 写进新数据、状态变了就加一，读的一方据此醒来。
    changed: watch::Sender<u64>,
    stats: Mutex<Stats>,
}

impl File {
    /// 打开（或新建）一个文件的缓存目录：按 meta.json 的总大小认出完整的块，残缺的、来历不明的删掉。
    fn open(dir: PathBuf, name: &str, src: Src) -> Self {
        let _ = std::fs::create_dir_all(&dir);
        let size = std::fs::read(dir.join("meta.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .and_then(|v| v["size"].as_u64());
        let n = size.map_or(0, blocks);
        let mut done = vec![false; n];
        for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let p = e.path();
            let fname = e.file_name().to_string_lossy().into_owned();
            if fname == "meta.json" {
                continue;
            }
            let ok = match (fname.parse::<usize>(), size) {
                (Ok(b), Some(s)) if b < n => e.metadata().is_ok_and(|m| m.len() == block_len(s, b)),
                _ => false,
            };
            match fname.parse::<usize>() {
                Ok(b) if ok => done[b] = true,
                _ => {
                    let _ = std::fs::remove_file(&p);
                }
            }
        }
        Self {
            name: name.to_owned(),
            gone: AtomicBool::new(false),
            dir,
            key: uuid::Uuid::new_v4().simple().to_string(),
            src: Mutex::new(src),
            st: Mutex::new(St {
                size,
                touched: vec![0; n],
                done,
                ..Default::default()
            }),
            changed: watch::Sender::new(0),
            stats: Mutex::new(Stats::default()),
        }
    }

    fn st(&self) -> MutexGuard<'_, St> {
        lock(&self.st)
    }

    fn bump(&self) {
        self.changed.send_modify(|v| *v = v.wrapping_add(1));
    }

    fn block_path(&self, b: usize) -> PathBuf {
        self.dir.join(b.to_string())
    }

    /// 停掉这个文件的全部下载任务（缓存留着）。
    fn stop(&self) {
        let mut st = self.st();
        let tasks: Vec<_> = st
            .main
            .take()
            .map(|m| m.t)
            .into_iter()
            .chain(st.previews.drain(..).map(|(t, ..)| t))
            .collect();
        drop(st);
        for t in tasks {
            t.task.abort();
        }
    }
}

/// 正在写的一块：没写完就被丢掉（任务取消、出错）时删掉 `.part`、撤销登记。
struct Writing {
    f: Arc<File>,
    b: usize,
    path: PathBuf,
    done: bool,
}

impl Drop for Writing {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        let mut st = self.f.st();
        if st.writing.get(&self.b).is_some_and(|p| p.path == self.path) {
            st.writing.remove(&self.b);
        }
        drop(st);
        let _ = std::fs::remove_file(&self.path);
        self.f.bump();
    }
}

/// 下载任务结束（含被取消）时把自己从文件状态里撤掉。
struct TaskGuard {
    f: Arc<File>,
    id: u64,
}

impl Drop for TaskGuard {
    fn drop(&mut self) {
        let mut st = self.f.st();
        if st.main.as_ref().is_some_and(|m| m.t.id == self.id) {
            st.main = None;
        }
        st.previews.retain(|(t, ..)| t.id != self.id);
        drop(st);
        self.f.bump();
    }
}

enum Key {
    Video(String),
    Sub(Arc<SubSrc>),
}

struct SubSrc {
    /// 缓存里的文件名。
    file: String,
    src: Mutex<Src>,
}

pub struct Inner {
    port: u16,
    video: PathBuf,
    subs: PathBuf,
    http: reqwest::Client,
    /// 边播边缓存整个视频（设置项，默认开）；关掉时只缓存读的位置前方一段。
    whole: AtomicBool,
    cap: AtomicU64,
    /// 视频缓存的总字节（完成的块）。
    total: AtomicU64,
    /// 上游下载的总字节（顶栏网速、每分钟诊断数据按它算差）。
    up_bytes: AtomicU64,
    tick: AtomicU64,
    ids: AtomicU64,
    files: Mutex<HashMap<String, Arc<File>>>,
    keys: Mutex<HashMap<String, Key>>,
    /// 等着删的目录：先改名进回收目录再慢慢删；改名前又被登记（又要播它）就不删了。
    doomed: Mutex<HashSet<String>>,
}

/// 本地缓存代理的句柄（可随意克隆）。
#[derive(Clone)]
pub struct Stream(Arc<Inner>);

impl Deref for Stream {
    type Target = Inner;
    fn deref(&self) -> &Inner {
        &self.0
    }
}

/// 回给 mpv 的正文：整段（小的）、空、或从读任务经有界通道一块块来（背压：mpv 不读，读任务就停）。
pub enum Body {
    Empty,
    Full(Option<Bytes>),
    Chan(mpsc::Receiver<Bytes>),
}

impl hyper::body::Body for Body {
    type Data = Bytes;
    type Error = Infallible;
    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        match self.get_mut() {
            Self::Empty => Poll::Ready(None),
            Self::Full(b) => Poll::Ready(b.take().map(|b| Ok(Frame::data(b)))),
            Self::Chan(rx) => rx.poll_recv(cx).map(|o| o.map(|b| Ok(Frame::data(b)))),
        }
    }
}

fn status(code: u16) -> Response<Body> {
    let mut r = Response::new(Body::Empty);
    *r.status_mut() = StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_GATEWAY);
    r
}

/// 请求里的 Range：`bytes=a-`、`bytes=a-b`、`bytes=-n`；别的写法（多段）当没有，回整个文件。
#[derive(Debug, PartialEq, Clone, Copy)]
enum Range {
    From(u64, Option<u64>),
    Suffix(u64),
}

fn parse_range(h: &str) -> Option<Range> {
    let r = h.trim().strip_prefix("bytes=")?;
    if r.contains(',') {
        return None;
    }
    let (a, b) = r.split_once('-')?;
    let (a, b) = (a.trim(), b.trim());
    if a.is_empty() {
        return b.parse().ok().map(Range::Suffix);
    }
    let a = a.parse().ok()?;
    let b = if b.is_empty() {
        None
    } else {
        Some(b.parse().ok()?)
    };
    Some(Range::From(a, b))
}

/// 按总大小落成闭区间；起点超出文件返回 None（416）。
fn resolve(r: Option<Range>, size: u64) -> Option<(u64, u64)> {
    match r {
        None => (size > 0).then(|| (0, size - 1)),
        Some(Range::From(a, b)) => (a < size).then(|| (a, b.unwrap_or(u64::MAX).min(size - 1))),
        Some(Range::Suffix(n)) => (n > 0 && size > 0).then(|| (size - n.min(size), size - 1)),
    }
}

/// `Content-Range: bytes a-b/total` → (a, total)。
fn content_range(h: &str) -> Option<(u64, u64)> {
    let r = h.trim().strip_prefix("bytes ")?;
    let (span, total) = r.split_once('/')?;
    let (a, _) = span.split_once('-')?;
    Some((a.trim().parse().ok()?, total.trim().parse().ok()?))
}

fn same_origin(a: &str, b: &str) -> bool {
    match (reqwest::Url::parse(a), reqwest::Url::parse(b)) {
        (Ok(a), Ok(b)) => a.origin() == b.origin(),
        _ => false,
    }
}

/// 上游给出数据的一个请求：回包、已经到手的第一段正文、本次请求的起点和文件总大小。
struct Opened {
    resp: reqwest::Response,
    first: Bytes,
    total: u64,
}

type Attempt<'a> = Pin<Box<dyn Future<Output = Result<Opened, Fail>> + Send + 'a>>;

async fn maybe<F: Future + Unpin>(f: &mut Option<F>) -> F::Output {
    match f {
        Some(f) => f.await,
        None => std::future::pending().await,
    }
}

impl Stream {
    /// 起本地服务。绑不上端口（极少见）时返回 None，播放退回直连。
    pub fn start(root: &Path, whole: bool) -> Option<Self> {
        let l = std::net::TcpListener::bind(("127.0.0.1", 0))
            .and_then(|l| l.set_nonblocking(true).map(|_| l))
            .map_err(|e| log::warn!("本地缓存代理起不来，播放改为直连：{e}"))
            .ok()?;
        let port = l.local_addr().ok()?.port();
        let http = reqwest::Client::builder()
            .no_proxy()
            .user_agent(crate::emby::UA)
            // HTTP/2 默认 64 KB 的流窗口在高延迟线路上限速；连接池让预览的短请求复用连接
            .http1_only()
            .connect_timeout(Duration::from_secs(10))
            // 一直没数据就断开重连（对冲只管第一个字节，这里管中途卡住）
            .read_timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .ok()?;
        let me = Self(Arc::new(Inner {
            port,
            video: root.join("video"),
            subs: root.join("subs"),
            http,
            whole: AtomicBool::new(whole),
            cap: AtomicU64::new(MAX_CAP),
            total: AtomicU64::new(0),
            up_bytes: AtomicU64::new(0),
            tick: AtomicU64::new(1),
            ids: AtomicU64::new(1),
            files: Mutex::new(HashMap::new()),
            keys: Mutex::new(HashMap::new()),
            doomed: Mutex::new(HashSet::new()),
        }));
        let _ = std::fs::create_dir_all(&me.video);
        let _ = std::fs::create_dir_all(&me.subs);
        let m = me.clone();
        tauri::async_runtime::spawn(async move {
            let l = match tokio::net::TcpListener::from_std(l) {
                Ok(l) => l,
                Err(e) => return log::warn!("本地缓存代理起不来：{e}"),
            };
            loop {
                let Ok((sock, _)) = l.accept().await else {
                    continue;
                };
                let m = m.clone();
                tauri::async_runtime::spawn(async move {
                    let svc = hyper::service::service_fn(move |req| {
                        let m = m.clone();
                        async move { Ok::<_, Infallible>(m.handle(req).await) }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(hyper_util::rt::TokioIo::new(sock), svc)
                        .await;
                });
            }
        });
        let m = me.clone();
        tauri::async_runtime::spawn_blocking(move || {
            // 上次没删完的回收目录
            for e in std::fs::read_dir(&m.video).into_iter().flatten().flatten() {
                if e.file_name().to_string_lossy().starts_with(".trash-") {
                    let _ = std::fs::remove_dir_all(e.path());
                }
            }
            m.recount();
        });
        log::info!("本地缓存代理：127.0.0.1:{port}");
        Some(me)
    }

    pub fn set_whole(&self, on: bool) {
        self.whole.store(on, Ordering::Relaxed);
        for f in lock(&self.files).values() {
            f.bump(); // 等着的主任务重新判断要不要往后写
        }
    }

    /// 上游下载的总字节（只增）。
    pub fn up_bytes(&self) -> u64 {
        self.up_bytes.load(Ordering::Relaxed)
    }

    /// 登记一个视频文件，返回给 mpv 的本地地址。`name` 是缓存目录名（同一个文件每次都一样，下次直接用上次的缓存）。
    pub fn video(&self, name: &str, url: &str, auth: &Auth) -> String {
        lock(&self.doomed).remove(name);
        let mut files = lock(&self.files);
        let f = match files.get(name) {
            Some(f) => {
                let mut s = lock(&f.src);
                if s.url != url {
                    (s.url, s.resolved) = (url.to_owned(), None);
                }
                s.auth = auth.clone();
                drop(s);
                f.clone()
            }
            None => {
                let src = Src {
                    url: url.to_owned(),
                    auth: auth.clone(),
                    resolved: None,
                };
                let f = Arc::new(File::open(self.video.join(name), name, src));
                lock(&self.keys).insert(f.key.clone(), Key::Video(name.to_owned()));
                files.insert(name.to_owned(), f.clone());
                f
            }
        };
        format!("http://127.0.0.1:{}/v/{}/stream", self.port, f.key)
    }

    /// 登记一个外挂字幕，返回本地地址（结尾带原文件名，mpv 按扩展名认格式）。第一次读时整个取下存进字幕缓存。
    pub fn sub(&self, file: &str, url: &str, auth: &Auth) -> String {
        let key = uuid::Uuid::new_v4().simple().to_string();
        let src = Src {
            url: url.to_owned(),
            auth: auth.clone(),
            resolved: None,
        };
        let s = Arc::new(SubSrc {
            file: file.to_owned(),
            src: Mutex::new(src),
        });
        lock(&self.keys).insert(key.clone(), Key::Sub(s));
        format!("http://127.0.0.1:{}/s/{key}/{file}", self.port)
    }

    /// 最近一次失败是不是上游回的 HTTP 错误（播放失败时给出 403 / 404 的原因）。
    pub fn fail_status(&self, name: &str) -> Option<u16> {
        let f = lock(&self.files).get(name)?.clone();
        match &f.st().fail {
            Some((Fail::Status(s) | Fail::Busy(s), _)) => Some(*s),
            _ => None,
        }
    }

    /// 只留下这些文件的缓存（当前集、已就绪的下一集），其余的停掉下载、删除目录。
    pub fn retain(&self, keep: &[String]) {
        let mut gone = Vec::new();
        {
            let mut doomed = lock(&self.doomed);
            let mut files = lock(&self.files);
            let mut keys = lock(&self.keys);
            files.retain(|name, f| {
                let k = keep.contains(name);
                if !k {
                    gone.push(f.clone());
                }
                k
            });
            keys.retain(|_, k| match k {
                Key::Video(n) => keep.contains(n),
                // 字幕的缓存名以视频的缓存名开头；mpv 在 sub-add 时就整个读完了，之后不会再来要
                Key::Sub(sub) => keep.iter().any(|n| sub.file.starts_with(n.as_str())),
            });
            for e in std::fs::read_dir(&self.video)
                .into_iter()
                .flatten()
                .flatten()
            {
                let n = e.file_name().to_string_lossy().into_owned();
                if !n.starts_with(".trash-") && !keep.contains(&n) && e.path().is_dir() {
                    doomed.insert(n);
                }
            }
        }
        for f in &gone {
            f.gone.store(true, Ordering::Relaxed);
            f.stop();
            f.bump();
            self.report(f);
        }
        let me = self.clone();
        tauri::async_runtime::spawn_blocking(move || me.sweep());
    }

    /// 删掉等着删的目录：在锁里改名进回收目录（改名前又被登记的就不删），锁外再慢慢删。
    /// Windows 上目录里还有文件开着时改名会失败，留到下次。
    fn sweep(&self) {
        let trash: Vec<PathBuf> = {
            let mut doomed = lock(&self.doomed);
            let mut out = Vec::new();
            doomed.retain(|n| {
                let to = self
                    .video
                    .join(format!(".trash-{}", uuid::Uuid::new_v4().simple()));
                match std::fs::rename(self.video.join(n), &to) {
                    Ok(()) => {
                        out.push(to);
                        false
                    }
                    Err(e) => e.kind() != std::io::ErrorKind::NotFound,
                }
            });
            out
        };
        // 改了名就不算进总量（recount 跳过回收目录）：先重算，新片不用等旧目录删完才有空间
        self.recount();
        for t in trash {
            let _ = std::fs::remove_dir_all(t);
        }
        if !lock(&self.doomed).is_empty() {
            let me = self.clone();
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(Duration::from_secs(10)).await;
                tauri::async_runtime::spawn_blocking(move || me.sweep());
            });
        }
    }

    /// 重新统计视频缓存总量（不算回收目录），顺带按磁盘可用空间定容量上限。
    fn recount(&self) {
        let total = self.video_size();
        self.total.store(total, Ordering::Relaxed);
        if let Some(free) = free_space(&self.video) {
            let cap = (free + total)
                .saturating_sub(RESERVE)
                .clamp(MIN_CAP, MAX_CAP);
            self.cap.store(cap, Ordering::Relaxed);
        }
    }

    /// 视频缓存的字节（不算正在删的回收目录）。
    fn video_size(&self) -> u64 {
        std::fs::read_dir(&self.video)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| !e.file_name().to_string_lossy().starts_with(".trash-"))
            .map(|e| dir_size(&e.path()))
            .sum()
    }

    /// 视频缓存、字幕缓存各占多少字节。
    pub fn usage(&self) -> (u64, u64) {
        (self.video_size(), dir_size(&self.subs))
    }

    /// 清理视频缓存；`playing`（在播或正在起播）时保留已登记的文件——换集时 retain 过，登记着的就是这次播放的当前集和下一集。
    pub fn clear_video(&self, playing: bool) {
        let keep: Vec<String> = if playing {
            lock(&self.files).keys().cloned().collect()
        } else {
            Vec::new()
        };
        self.retain(&keep);
    }

    pub fn clear_subs(&self) {
        let _ = std::fs::remove_dir_all(&self.subs);
        let _ = std::fs::create_dir_all(&self.subs);
    }

    /// 预取下一集：第一块和最后两块（mkv 的索引在文件尾），切过去时打开文件不走网络。
    pub fn prefetch(&self, name: &str) {
        let Some(f) = lock(&self.files).get(name).cloned() else {
            return;
        };
        let me = self.clone();
        tauri::async_runtime::spawn(async move {
            let mut rx = f.changed.subscribe();
            let t0 = Instant::now();
            loop {
                let abort = {
                    let mut st = f.st();
                    if let Some(size) = st.size {
                        let n = blocks(size);
                        let lo = n.saturating_sub(2);
                        let a = me.ensure(&f, &mut st, lo, n - 1, Role::Prefetch);
                        let b = me.ensure(&f, &mut st, 0, 0, Role::Prefetch);
                        drop(st);
                        abort_all(a.into_iter().chain(b));
                        return;
                    }
                    if st.fail.is_some() || t0.elapsed() > OPEN_LIMIT {
                        return;
                    }
                    me.ensure(&f, &mut st, 0, 0, Role::Prefetch)
                };
                abort_all(abort);
                let _ = tokio::time::timeout(Duration::from_secs(1), rx.changed()).await;
            }
        });
    }

    /// 一次播放结束（mpv 退出）：停掉这些文件的下载，缓存留着（下次打开同一部片直接用）；每个文件记一条诊断数据。
    /// 不停的话整片模式会在关掉播放器之后把整部片下完。只停给出的这几个：换片时新的一次播放可能已经开始了。
    pub fn idle(&self, names: &[String]) {
        let files: Vec<_> = {
            let all = lock(&self.files);
            names.iter().filter_map(|n| all.get(n).cloned()).collect()
        };
        for f in files {
            f.stop();
            self.report(&f);
        }
    }

    /// 程序退出前：每个文件记一条诊断数据。
    pub fn report_all(&self) {
        for f in lock(&self.files).values() {
            self.report(f);
        }
    }

    fn report(&self, f: &File) {
        if !telemetry::enabled() {
            return;
        }
        let s = std::mem::take(&mut *lock(&f.stats));
        if s.reqs == [0, 0] && s.up == [0; 3] {
            return;
        }
        let mut ttfb = s.ttfb_ms.clone();
        ttfb.sort_unstable();
        let (cover, size) = {
            let st = f.st();
            let n = st.done.len().max(1);
            let c = st.done.iter().filter(|d| **d).count();
            ((c * 100 / n) as u64, st.size.map(|s| s >> 20))
        };
        let mut seg = f.name.splitn(3, '_');
        let kb = |b: u64| b >> 10;
        telemetry::rec(
            "stream",
            json!({
                "srv": seg.next(), "item": seg.next(),
                "up_kb": s.up.map(kb), "served_kb": kb(s.served), "hit_kb": kb(s.hit),
                "reqs": s.reqs, "thumb_hit": s.thumb_hit, "opens": s.opens, "hedges": s.hedges, "fails": s.fails,
                "ttfb_p50": ttfb.get(ttfb.len() / 2), "ttfb_max": ttfb.last(), "wait_ms": s.wait_ms,
                "cover": cover, "mb": size, "whole": self.whole.load(Ordering::Relaxed),
            }),
        );
    }

    async fn handle(&self, req: Request<Incoming>) -> Response<Body> {
        let head = req.method() == hyper::Method::HEAD;
        if !head && req.method() != hyper::Method::GET {
            return status(405);
        }
        let role = match req.headers().get(ROLE).and_then(|v| v.to_str().ok()) {
            Some("thumb") => Role::Thumb,
            _ => Role::Main,
        };
        let range = req
            .headers()
            .get(hyper::header::RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(parse_range);
        let mut seg = req.uri().path().trim_start_matches('/').split('/');
        let (kind, key) = (seg.next(), seg.next().unwrap_or_default());
        // 先放开 keys 再拿 files（登记时是先 files 后 keys）
        let found = match lock(&self.keys).get(key) {
            Some(Key::Video(n)) if kind == Some("v") => Some(Ok(n.clone())),
            Some(Key::Sub(s)) if kind == Some("s") => Some(Err(s.clone())),
            _ => None,
        };
        let target = match found {
            Some(Ok(n)) => lock(&self.files).get(&n).cloned().map(Ok),
            Some(Err(s)) => Some(Err(s)),
            None => None,
        };
        match target {
            Some(Ok(f)) => self.serve_video(f, range, head, role).await,
            Some(Err(s)) => self.serve_sub(&s, head).await,
            None => status(404),
        }
    }

    async fn serve_video(
        &self,
        f: Arc<File>,
        range: Option<Range>,
        head: bool,
        role: Role,
    ) -> Response<Body> {
        lock(&f.stats).reqs[(role != Role::Main) as usize] += 1;
        let t0 = Instant::now();
        let first = match range {
            Some(Range::From(a, _)) => (a / BLOCK) as usize,
            _ => 0,
        };
        let size = match self.wait_size(&f, first, role, t0).await {
            Ok(s) => s,
            Err(code) => return status(code),
        };
        let Some((start, end)) = resolve(range, size) else {
            let mut r = status(416);
            r.headers_mut().insert(
                hyper::header::CONTENT_RANGE,
                format!("bytes */{size}").parse().unwrap(),
            );
            return r;
        };
        // 要的位置没缓存、上游刚回过错误：直接回那个错误码（mpv 报错，界面给出原因），不让它干等
        {
            let st = f.st();
            let b = (start / BLOCK) as usize;
            if !st.has(b)
                && let Some((Fail::Status(s), at)) = &st.fail
                && st.retry_at.is_some_and(|r| Instant::now() < r)
                && *at <= t0
            {
                return status(*s);
            }
        }
        let mut r = Response::new(Body::Empty);
        let h = r.headers_mut();
        h.insert(hyper::header::ACCEPT_RANGES, "bytes".parse().unwrap());
        h.insert(
            hyper::header::CONTENT_TYPE,
            "application/octet-stream".parse().unwrap(),
        );
        h.insert(
            hyper::header::CONTENT_LENGTH,
            (end - start + 1).to_string().parse().unwrap(),
        );
        if range.is_some() {
            *r.status_mut() = StatusCode::PARTIAL_CONTENT;
            r.headers_mut().insert(
                hyper::header::CONTENT_RANGE,
                format!("bytes {start}-{end}/{size}").parse().unwrap(),
            );
        }
        if !head {
            let (tx, rx) = mpsc::channel(4);
            let me = self.clone();
            tauri::async_runtime::spawn(async move { me.read_loop(f, start, end, role, tx).await });
            *r.body_mut() = Body::Chan(rx);
        }
        r
    }

    /// 等文件总大小（第一次读这个文件时要先从上游拿到）。上游回了 HTTP 错误就回那个码；网络不通一直重试到 [`OPEN_LIMIT`]。
    async fn wait_size(
        &self,
        f: &Arc<File>,
        b: usize,
        role: Role,
        t0: Instant,
    ) -> Result<u64, u16> {
        let mut rx = f.changed.subscribe();
        loop {
            let abort = {
                let mut st = f.st();
                if f.gone.load(Ordering::Relaxed) {
                    return Err(410); // 换片时被删了
                }
                if let Some(s) = st.size {
                    return Ok(s);
                }
                if let Some(s) = st.status_since(t0) {
                    return Err(s);
                }
                if t0.elapsed() > OPEN_LIMIT + Duration::from_secs(5) {
                    return Err(st.fail.as_ref().map_or(504, |(e, _)| e.code()));
                }
                self.ensure(f, &mut st, b, b, role)
            };
            abort_all(abort);
            let _ = tokio::time::timeout(Duration::from_secs(1), rx.changed()).await;
        }
    }

    /// 把 [start, end] 一段段交给 mpv：块在磁盘上就读文件，正在下载就读已写到的部分，都没有就安排下载再等。
    async fn read_loop(
        self,
        f: Arc<File>,
        mut pos: u64,
        end: u64,
        role: Role,
        tx: mpsc::Sender<Bytes>,
    ) {
        enum Next {
            Read(PathBuf, u64, bool),
            Wait(Vec<Task>),
            Gone,
        }
        let t0 = Instant::now();
        let mut rx = f.changed.subscribe();
        let mut open: Option<(PathBuf, tokio::fs::File, u64)> = None;
        let mut waited = Duration::ZERO;
        let mut all_hit = true;
        while pos <= end {
            let b = (pos / BLOCK) as usize;
            let off = pos % BLOCK;
            let next = {
                let mut st = f.st();
                if role == Role::Main {
                    st.last_read = pos;
                }
                if f.gone.load(Ordering::Relaxed) {
                    Next::Gone // 换片时被删了：断开，mpv 那边这时已经在播别的
                } else if st.done.get(b).copied().unwrap_or(false) {
                    st.touched[b] = self.tick.fetch_add(1, Ordering::Relaxed);
                    let len = block_len(st.size.unwrap_or(0), b);
                    Next::Read(f.block_path(b), len, true)
                } else if let Some(p) = st.writing.get(&b).filter(|p| p.written > off) {
                    Next::Read(p.path.clone(), p.written, false)
                } else if st.status_since(t0).is_some() && !st.busy() {
                    Next::Gone // 上游在这之后回了错误：断开，mpv 重连时会拿到错误码
                } else {
                    let hi = if off + (1 << 20) > BLOCK { b + 1 } else { b };
                    Next::Wait(self.ensure(&f, &mut st, b, hi, role))
                }
            };
            let (path, avail, hit) = match next {
                Next::Gone => break,
                Next::Wait(abort) => {
                    abort_all(abort);
                    all_hit = false;
                    let w = Instant::now();
                    let _ = tokio::time::timeout(Duration::from_secs(1), rx.changed()).await;
                    waited += w.elapsed();
                    if tx.is_closed() {
                        break;
                    }
                    continue;
                }
                Next::Read(p, avail, hit) => (p, avail, hit),
            };
            let n = (avail - off).min(READ_CHUNK).min(end - pos + 1);
            if open.as_ref().is_none_or(|(p, ..)| *p != path) {
                match tokio::fs::File::open(&path).await {
                    Ok(h) => open = Some((path.clone(), h, u64::MAX)),
                    Err(_) => {
                        // `.part` 刚写完改了名，或者块被删了：重新看状态
                        if hit {
                            f.repair(&self, b);
                        }
                        open = None;
                        tokio::task::yield_now().await;
                        continue;
                    }
                }
            }
            let Some((_, h, at)) = open.as_mut() else {
                continue;
            };
            if *at != off && h.seek(std::io::SeekFrom::Start(off)).await.is_err() {
                open = None;
                continue;
            }
            let mut buf = vec![0; n as usize];
            if h.read_exact(&mut buf).await.is_err() {
                if hit {
                    f.repair(&self, b);
                }
                open = None;
                continue;
            }
            *at = off + n;
            all_hit &= hit;
            {
                let mut s = lock(&f.stats);
                s.served += n;
                if hit {
                    s.hit += n;
                }
            }
            if tx.send(Bytes::from(buf)).await.is_err() {
                break; // mpv 断开了（拖动进度会关掉旧连接）
            }
            pos += n;
        }
        let mut s = lock(&f.stats);
        s.wait_ms += waited.as_millis() as u64;
        if role == Role::Thumb && all_hit {
            s.thumb_hit += 1;
        }
    }

    /// 读的一方要第 b 块（预览要 [b, hi]）：已有或有人在写就等；否则按角色安排下载。返回要取消的旧任务（锁外取消）。
    fn ensure(&self, f: &Arc<File>, st: &mut St, b: usize, hi: usize, role: Role) -> Vec<Task> {
        if st.has(b)
            || f.gone.load(Ordering::Relaxed)
            || st.retry_at.is_some_and(|t| Instant::now() < t)
            || st.size.is_some_and(|s| b >= blocks(s))
        {
            return Vec::new();
        }
        let near = st
            .main
            .as_ref()
            .is_some_and(|m| m.at <= b && b <= m.at + MAIN_NEAR as usize);
        if near || st.previews.iter().any(|(_, lo, h, _)| *lo <= b && b <= *h) {
            return Vec::new();
        }
        let id = self.ids.fetch_add(1, Ordering::Relaxed);
        // 主任务刚开不久又有主播放器的读要别处（同时有两个连接在读）：给它两块的短任务，不来回抢主任务
        let young = st
            .main
            .as_ref()
            .is_some_and(|m| m.since.elapsed() < Duration::from_secs(2));
        let (role, hi) = match role {
            Role::Main if young => (Role::Main, b + 1),
            Role::Main => {
                let task = tauri::async_runtime::spawn(self.clone().main_task(f.clone(), b, id));
                let m = Main {
                    t: Task { id, task },
                    at: b,
                    since: Instant::now(),
                };
                return st.main.replace(m).map(|m| m.t).into_iter().collect();
            }
            r => (r, hi),
        };
        let mut old = Vec::new();
        // 预览只关心最新的位置：名额满了取消最早的预览 / 预取（主播放器的短任务不算名额、不取消）
        let side = |r: &Role| *r != Role::Main;
        if st.previews.iter().filter(|p| side(&p.3)).count() >= PREVIEWS
            && let Some(k) = st.previews.iter().position(|p| side(&p.3))
        {
            old.push(st.previews.remove(k).0);
        }
        let task =
            tauri::async_runtime::spawn(self.clone().preview_task(f.clone(), b, hi, id, role));
        st.previews.push((Task { id, task }, b, hi, role));
        old
    }

    async fn main_task(self, f: Arc<File>, mut b: usize, id: u64) {
        let _g = TaskGuard { f: f.clone(), id };
        let mine = || {
            f.st().main.as_ref().is_some_and(|m| m.t.id == id) && !f.gone.load(Ordering::Relaxed)
        };
        let mut rx = f.changed.subscribe();
        let mut back = 1;
        loop {
            let r = async {
                let o = self.open(&f, b as u64 * BLOCK, None, Role::Main).await?;
                let n = f.learn(&self, o.total);
                self.pump(&f, o, b, n, Some(id), Role::Main).await
            }
            .await;
            match r {
                Ok(Stop::Ahead(at)) => {
                    // 读的位置往前走到只差半段再接着要：一次请求写 8 块左右，不至于每走一块就开一个请求
                    loop {
                        if !mine() {
                            return;
                        }
                        let rb = (f.st().last_read / BLOCK) as usize;
                        if !self.limited() || at <= rb + AHEAD as usize / 2 {
                            break;
                        }
                        let _ = tokio::time::timeout(Duration::from_secs(1), rx.changed()).await;
                    }
                }
                Ok(Stop::Hit) => back = 1,
                Err(e) if e.transient() => {
                    // 网络错误、5xx：自己退避重试（1、2、4…30 秒），整片模式不能断在这里；读的一方要别处会换掉这个任务
                    f.failed(e);
                    tokio::time::sleep(Duration::from_secs(back)).await;
                    back = (back * 2).min(30);
                    if !mine() {
                        return;
                    }
                }
                Err(e) => return f.failed(e),
            }
            // 碰到已有的块断开了、写到了文件尾、或重试：从读的位置往后找下一个缺块
            match self.next_missing(&f) {
                Some(nb) => {
                    b = nb;
                    if let Some(m) = f.st().main.as_mut().filter(|m| m.t.id == id) {
                        m.at = nb;
                    }
                }
                None => return,
            }
        }
    }

    async fn preview_task(self, f: Arc<File>, lo: usize, hi: usize, id: u64, role: Role) {
        let _g = TaskGuard { f: f.clone(), id };
        let end = (hi as u64 + 1) * BLOCK;
        let end = f.st().size.map_or(end, |s| end.min(s)) - 1;
        let r = async {
            let o = self.open(&f, lo as u64 * BLOCK, Some(end), role).await?;
            let n = f.learn(&self, o.total);
            self.pump(&f, o, lo, (hi + 1).min(n), None, role).await
        }
        .await;
        let r = r.map(|_| ());
        if let Err(e) = r {
            f.failed(e);
        }
    }

    /// 主任务的下一个起点：读的位置往后第一个缺块（没人在写、没有预览要它），到尾回绕；只缓存前方时只看前方 [`AHEAD`] 块。
    fn next_missing(&self, f: &File) -> Option<usize> {
        let st = f.st();
        let n = st.done.len();
        let rb = ((st.last_read / BLOCK) as usize).min(n.saturating_sub(1));
        let free =
            |b: &usize| !st.has(*b) && !st.previews.iter().any(|(_, lo, hi, _)| lo <= b && b <= hi);
        if self.limited() {
            return (rb..(rb + AHEAD as usize + 1).min(n)).find(free);
        }
        (rb..n).chain(0..rb).find(free)
    }

    /// 只缓存前方：设置关了整片缓存，或到了容量上限。
    fn limited(&self) -> bool {
        !self.whole.load(Ordering::Relaxed)
            || self.total.load(Ordering::Relaxed) + AHEAD * BLOCK > self.cap.load(Ordering::Relaxed)
    }

    /// 向上游要 [start, end]（None = 到文件尾），带对冲：3 秒没出数据再开一个，先出数据的留下。
    async fn open(
        &self,
        f: &File,
        start: u64,
        end: Option<u64>,
        role: Role,
    ) -> Result<Opened, Fail> {
        let t0 = Instant::now();
        let mut a: Attempt = Box::pin(self.attempt(&f.src, start, end));
        let mut b: Option<Attempt> = None;
        let mut step = 0;
        let mut next = tokio::time::Instant::now() + Duration::from_secs(HEDGE[0]);
        let r = loop {
            tokio::select! {
                r = &mut a => match r {
                    Err(e) if e.transient() && b.is_some() => a = b.take().unwrap(),
                    r => break r,
                },
                r = maybe(&mut b) => match r {
                    Err(e) if e.transient() => b = None,
                    r => break r,
                },
                _ = tokio::time::sleep_until(next) => {
                    if t0.elapsed() >= OPEN_LIMIT {
                        break Err(Fail::Timeout);
                    }
                    lock(&f.stats).hedges += 1;
                    if let Some(x) = b.take() {
                        a = x; // 丢掉较早的那个
                    }
                    b = Some(Box::pin(self.attempt(&f.src, start, end)));
                    step += 1;
                    next += Duration::from_secs(HEDGE[step.min(HEDGE.len() - 1)]);
                }
            }
        };
        let mut s = lock(&f.stats);
        s.opens += 1;
        if let Ok(o) = &r {
            s.ttfb_ms.push(t0.elapsed().as_millis() as u64);
            s.up[role as usize] += o.first.len() as u64;
            self.up_bytes
                .fetch_add(o.first.len() as u64, Ordering::Relaxed);
        }
        r
    }

    /// 一次请求：发出去、跟重定向、确认是要的那段，等到第一段正文。
    async fn attempt(
        &self,
        src: &Mutex<Src>,
        start: u64,
        end: Option<u64>,
    ) -> Result<Opened, Fail> {
        let range = match end {
            Some(e) => format!("bytes={start}-{e}"),
            None => format!("bytes={start}-"),
        };
        let mut resp = self.send(src, Some(&range)).await?;
        let st = resp.status().as_u16();
        let cr = resp
            .headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(content_range);
        let total = match (st, cr) {
            (206, Some((a, total))) if a == start => total,
            (200, _) if start == 0 => resp.content_length().ok_or(Fail::Status(502))?,
            (200 | 206, _) => {
                // ponytail: 不支持 Range 的上游不缓存（Emby 都支持）；要支持就从头读、跳过前面的字节
                log::warn!("上游没按 Range 回数据（{st}）");
                return Err(Fail::Status(502));
            }
            (429 | 500..=599, _) => return Err(Fail::Busy(st)),
            _ => return Err(Fail::Status(st)),
        };
        let first = resp
            .chunk()
            .await
            .map_err(|e| Fail::Net(e.without_url().to_string()))?
            .unwrap_or_default();
        Ok(Opened { resp, first, total })
    }

    /// 发请求、自己跟重定向（最多 5 次）：token 只发给 API 根地址同源的主机。
    /// 先用上次重定向到的地址，失败（网络错误或非 2xx）再从原地址来一次。
    async fn send(&self, src: &Mutex<Src>, range: Option<&str>) -> Result<reqwest::Response, Fail> {
        let (orig, resolved, auth) = {
            let s = lock(src);
            (s.url.clone(), s.resolved.clone(), s.auth.clone())
        };
        let tries = resolved.into_iter().chain([orig.clone()]);
        let mut last = Fail::Net("没有请求".into());
        for (k, start) in tries.enumerate() {
            let fallback = k == 0 && start != orig;
            let mut url = start;
            let r = 'hops: {
                for _ in 0..=5 {
                    let mut rb = self.http.get(&url);
                    if let Some(r) = range {
                        rb = rb.header(reqwest::header::RANGE, r);
                    }
                    if same_origin(&url, &auth.origin) {
                        rb = rb.header("X-Emby-Token", &auth.token);
                    }
                    let resp = match rb.send().await {
                        Ok(r) => r,
                        Err(e) => break 'hops Err(Fail::Net(e.without_url().to_string())),
                    };
                    let loc = resp
                        .headers()
                        .get(reqwest::header::LOCATION)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|l| reqwest::Url::parse(&url).ok()?.join(l).ok());
                    match loc {
                        Some(l) if resp.status().is_redirection() => url = l.into(),
                        _ => break 'hops Ok(resp),
                    }
                }
                Err(Fail::Net("重定向太多".into()))
            };
            match r {
                Ok(resp) if resp.status().is_success() => {
                    lock(src).resolved = (url != orig).then_some(url);
                    return Ok(resp);
                }
                Ok(resp) if !fallback => return Ok(resp),
                Ok(resp) => last = Fail::Busy(resp.status().as_u16()),
                Err(e) => last = e,
            }
            lock(src).resolved = None;
        }
        Err(last)
    }

    /// 把回包从第 `from` 块起逐块写到 `to`（不含）为止：每块先登记再写，写完改名；块已有或有人在写就停下。
    async fn pump(
        &self,
        f: &Arc<File>,
        o: Opened,
        from: usize,
        to: usize,
        main: Option<u64>,
        role: Role,
    ) -> Result<Stop, Fail> {
        let Opened {
            mut resp,
            first: mut pending,
            ..
        } = o;
        let size = f.st().size.unwrap_or(0);
        let io = |e: std::io::Error| Fail::Net(format!("写缓存失败：{e}"));
        for b in from..to {
            // 只缓存前方：写到读的位置前方 AHEAD 块就放掉连接。挂着不读的话，读超时（10 秒）和中继的发送超时都会把它断掉
            if main.is_some()
                && self.limited()
                && b > (f.st().last_read / BLOCK) as usize + AHEAD as usize
            {
                return Ok(Stop::Ahead(b));
            }
            let Some(mut w) = self.claim(f, b, main, size) else {
                return Ok(Stop::Hit);
            };
            let len = block_len(size, b);
            let mut file = std::fs::File::create(&w.path).map_err(io)?;
            let mut got = 0;
            while got < len {
                if pending.is_empty() {
                    pending = match resp.chunk().await {
                        Ok(Some(c)) => c,
                        Ok(None) => return Err(Fail::Net("上游提前断开".into())),
                        Err(e) => return Err(Fail::Net(e.without_url().to_string())),
                    };
                    lock(&f.stats).up[role as usize] += pending.len() as u64;
                    self.up_bytes
                        .fetch_add(pending.len() as u64, Ordering::Relaxed);
                }
                let part = pending.split_to(pending.len().min((len - got) as usize));
                // ponytail: 同步写，几十 KB 一次落到系统缓存是微秒级；真卡了再改 spawn_blocking
                file.write_all(&part).map_err(io)?;
                got += part.len() as u64;
                if let Some(p) = f.st().writing.get_mut(&b) {
                    p.written = got;
                }
                f.bump();
            }
            drop(file);
            std::fs::rename(&w.path, f.block_path(b)).map_err(io)?;
            w.done = true;
            {
                let mut st = f.st();
                st.writing.remove(&b);
                st.done[b] = true;
                st.touched[b] = self.tick.fetch_add(1, Ordering::Relaxed);
            }
            self.total.fetch_add(len, Ordering::Relaxed);
            f.bump();
        }
        Ok(Stop::Hit)
    }

    /// 登记要写第 b 块。块已有、有人在写、主任务已被换掉时返回 None。超容量时先删当前文件里最久没用的块；删不出来也照写
    /// （读的一方正等着它；拒绝的话任务会立即重开请求、空转）——超出的只是读的位置附近几块，主任务到了上限只写前方一段。
    fn claim(&self, f: &Arc<File>, b: usize, main: Option<u64>, size: u64) -> Option<Writing> {
        let mut st = f.st();
        if let Some(id) = main {
            match st.main.as_mut() {
                Some(m) if m.t.id == id => m.at = b,
                _ => return None,
            }
        }
        if b >= st.done.len() || st.has(b) {
            return None;
        }
        let len = block_len(size, b);
        let cap = self.cap.load(Ordering::Relaxed);
        while self.total.load(Ordering::Relaxed) + len > cap {
            // 读的位置附近的不删
            let rb = (st.last_read / BLOCK) as usize;
            let keep = rb.saturating_sub(2)..=rb + AHEAD as usize;
            let Some(victim) = (0..st.done.len())
                .filter(|&k| st.done[k] && !keep.contains(&k))
                .min_by_key(|&k| st.touched[k])
            else {
                break;
            };
            st.done[victim] = false;
            let _ = std::fs::remove_file(f.block_path(victim));
            sub_total(&self.total, block_len(size, victim));
        }
        st.seq += 1;
        let path = f.dir.join(format!("{b}.{}.part", st.seq));
        st.writing.insert(
            b,
            Part {
                path: path.clone(),
                written: 0,
            },
        );
        Some(Writing {
            f: f.clone(),
            b,
            path,
            done: false,
        })
    }
}

impl File {
    /// 上游告诉了总大小：第一次就记下（写 meta.json）；和缓存里记的不一样（文件换了）就把旧块全删掉。返回块数。
    fn learn(&self, s: &Inner, total: u64) -> usize {
        let mut st = self.st();
        st.fail = None;
        st.retry_at = None;
        if st.size == Some(total) {
            return st.done.len();
        }
        if let Some(old) = st.size {
            log::info!("缓存的文件大小变了（{old} → {total}），重新缓存");
            for b in 0..st.done.len() {
                if st.done[b] {
                    let _ = std::fs::remove_file(self.block_path(b));
                    sub_total(&s.total, block_len(old, b));
                }
            }
        }
        let n = blocks(total);
        st.size = Some(total);
        st.done = vec![false; n];
        st.touched = vec![0; n];
        // 先写临时文件再改名：写到一半断电，meta 坏了会把整个目录的块都当成来历不明删掉
        let tmp = self.dir.join("meta.json.tmp");
        if std::fs::write(&tmp, json!({ "size": total }).to_string()).is_ok() {
            let _ = std::fs::rename(&tmp, self.dir.join("meta.json"));
        }
        drop(st);
        self.bump();
        n
    }

    fn failed(&self, e: Fail) {
        let why = match &e {
            Fail::Status(s) | Fail::Busy(s) => format!("HTTP {s}"),
            Fail::Net(m) => m.clone(),
            Fail::Timeout => format!("{} 秒没有数据", OPEN_LIMIT.as_secs()),
        };
        lock(&self.stats).fails += 1;
        let back = match e {
            Fail::Status(_) => 3,
            _ => 1,
        };
        let mut st = self.st();
        // 只记一连串失败里的第一次（成功取到数据时清零）：上游长时间不通时每秒重试，不刷屏
        if st.fail.is_none() {
            log::warn!("缓存代理向上游取数据失败：{why}");
        }
        let now = Instant::now();
        st.retry_at = Some(now + Duration::from_secs(back));
        st.fail = Some((e, now));
        drop(st);
        self.bump();
    }

    /// 记着有、读却读不到的块（被外部删了）：改回没有。
    fn repair(&self, s: &Inner, b: usize) {
        let mut st = self.st();
        if st.done.get(b).copied().unwrap_or(false) && !self.block_path(b).is_file() {
            st.done[b] = false;
            sub_total(&s.total, block_len(st.size.unwrap_or(0), b));
        }
    }
}

impl Stream {
    async fn serve_sub(&self, s: &SubSrc, head: bool) -> Response<Body> {
        let path = self.subs.join(&s.file);
        let bytes = match tokio::fs::read(&path).await {
            Ok(b) => {
                let _ = std::fs::File::options()
                    .write(true)
                    .open(&path)
                    .and_then(|f| f.set_modified(std::time::SystemTime::now()));
                Bytes::from(b)
            }
            Err(_) => {
                let resp = match self.send(&s.src, None).await {
                    Ok(r) if r.status().is_success() => r,
                    Ok(r) => return status(r.status().as_u16()),
                    Err(e) => return status(e.code()),
                };
                let Ok(b) = resp.bytes().await else {
                    return status(502);
                };
                let tmp = path.with_extension("tmp");
                if std::fs::write(&tmp, &b).is_ok() && std::fs::rename(&tmp, &path).is_ok() {
                    let dir = self.subs.clone();
                    tauri::async_runtime::spawn_blocking(move || {
                        crate::images::trim_dir(&dir, SUBS_CAP)
                    });
                }
                b
            }
        };
        let mut r = Response::new(if head {
            Body::Empty
        } else {
            Body::Full(Some(bytes.clone()))
        });
        r.headers_mut().insert(
            hyper::header::CONTENT_LENGTH,
            bytes.len().to_string().parse().unwrap(),
        );
        r
    }
}

/// 记账往下减时不越过 0：统计有误差时宁可少算，绕回成天文数字会让淘汰把缓存删光。
fn sub_total(total: &AtomicU64, n: u64) {
    // fetch_update 在新版里改名 try_update，旧版没有新名字：手写比较交换
    let mut v = total.load(Ordering::Relaxed);
    while let Err(now) =
        total.compare_exchange_weak(v, v.saturating_sub(n), Ordering::Relaxed, Ordering::Relaxed)
    {
        v = now;
    }
}

fn abort_all(tasks: impl IntoIterator<Item = Task>) {
    for t in tasks {
        t.task.abort();
    }
}

/// 目录（含子目录）里文件的总字节。
pub fn dir_size(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => dir_size(&e.path()),
            _ => e.metadata().map_or(0, |m| m.len()),
        })
        .sum()
}

/// `dir` 所在磁盘的可用空间。
fn free_space(dir: &Path) -> Option<u64> {
    let dir = dunce::canonicalize(dir).ok()?;
    let disks = sysinfo::Disks::new_with_refreshed_list();
    disks
        .list()
        .iter()
        .filter(|d| dir.starts_with(d.mount_point()))
        .max_by_key(|d| d.mount_point().as_os_str().len())
        .map(|d| d.available_space())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[test]
    fn ranges() {
        assert_eq!(parse_range("bytes=10-"), Some(Range::From(10, None)));
        assert_eq!(parse_range("bytes=10-19"), Some(Range::From(10, Some(19))));
        assert_eq!(parse_range("bytes=-5"), Some(Range::Suffix(5)));
        assert_eq!(parse_range("bytes=1-2,5-6"), None);
        assert_eq!(parse_range("items=1-2"), None);
        assert_eq!(resolve(Some(Range::From(10, None)), 100), Some((10, 99)));
        assert_eq!(
            resolve(Some(Range::From(10, Some(500))), 100),
            Some((10, 99))
        );
        assert_eq!(resolve(Some(Range::Suffix(5)), 100), Some((95, 99)));
        assert_eq!(resolve(Some(Range::From(100, None)), 100), None);
        assert_eq!(resolve(None, 100), Some((0, 99)));
        assert_eq!(
            content_range("bytes 4194304-8388607/44040192"),
            Some((4194304, 44040192))
        );
        assert!(same_origin(
            "https://h.example/emby/x",
            "https://h.example/emby"
        ));
        assert!(!same_origin(
            "https://cdn.example/x",
            "https://h.example/emby"
        ));
        assert!(!same_origin(
            "http://h.example:8096/x",
            "https://h.example/emby"
        ));
    }

    /// 测试用上游：Range 服务器，按路径决定行为；记下每个请求（路径、Range、带没带 token）。
    struct Upstream {
        base: String,
        other: String,
        log: Arc<Mutex<Vec<(String, String, bool)>>>,
        data: Arc<Vec<u8>>,
        /// 正在发正文的连接数。
        active: Arc<AtomicU64>,
    }

    fn data(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i * 31 + i / 4099) as u8).collect()
    }

    struct Active(Arc<AtomicU64>);
    impl Drop for Active {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    async fn serve(
        l: tokio::net::TcpListener,
        log: Arc<Mutex<Vec<(String, String, bool)>>>,
        data: Arc<Vec<u8>>,
        redirect: Option<String>,
        active: Arc<AtomicU64>,
    ) {
        let stalls = Arc::new(AtomicU64::new(0));
        let cuts = Arc::new(AtomicU64::new(0));
        let flaky = Arc::new(AtomicU64::new(0));
        while let Ok((mut s, _)) = l.accept().await {
            let (log, data, redirect) = (log.clone(), data.clone(), redirect.clone());
            let (stalls, cuts, flaky, active) =
                (stalls.clone(), cuts.clone(), flaky.clone(), active.clone());
            tokio::spawn(async move {
                let mut buf = vec![0; 8192];
                let mut head = String::new();
                while !head.contains("\r\n\r\n") {
                    let Ok(n) = s.read(&mut buf).await else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    head.push_str(&String::from_utf8_lossy(&buf[..n]));
                }
                let path = head.split(' ').nth(1).unwrap_or_default().to_owned();
                let hdr = |k: &str| {
                    head.lines()
                        .find(|l| l.to_ascii_lowercase().starts_with(k))
                        .map(|l| l[k.len()..].trim().to_owned())
                };
                let range = hdr("range:").unwrap_or_default();
                let token = hdr("x-emby-token:").is_some();
                lock(&log).push((path.clone(), range.clone(), token));
                if path.starts_with("/403") {
                    let _ = s.write_all(b"HTTP/1.1 403 Forbidden\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").await;
                    return;
                }
                if let (true, Some(to)) = (path.starts_with("/redir"), &redirect) {
                    let r = format!(
                        "HTTP/1.1 302 Found\r\nlocation: {to}/f\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                    );
                    let _ = s.write_all(r.as_bytes()).await;
                    return;
                }
                // 前两次 502（中继不稳）
                if path.starts_with("/flaky") && flaky.fetch_add(1, Ordering::SeqCst) < 2 {
                    let _ = s.write_all(b"HTTP/1.1 502 Bad Gateway\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").await;
                    return;
                }
                if path.starts_with("/sub") {
                    let _ = s.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 5\r\nconnection: close\r\n\r\nhello").await;
                    return;
                }
                let n = data.len() as u64;
                let (a, b) = match parse_range(&range).and_then(|r| resolve(Some(r), n)) {
                    Some(x) => x,
                    None => (0, n - 1),
                };
                let h = format!(
                    "HTTP/1.1 206 Partial Content\r\ncontent-range: bytes {a}-{b}/{n}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    b - a + 1
                );
                if s.write_all(h.as_bytes()).await.is_err() {
                    return;
                }
                // 第一次请求：发完头不给正文（上游冷读取卡住）
                if path.starts_with("/stall") && stalls.fetch_add(1, Ordering::SeqCst) == 0 {
                    tokio::time::sleep(Duration::from_secs(20)).await;
                    return;
                }
                active.fetch_add(1, Ordering::SeqCst);
                let _active = Active(active);
                let mut sent = 0u64;
                for chunk in data[a as usize..=b as usize].chunks(64 << 10) {
                    // 慢速上游：约 1.3 MB/s
                    if path.starts_with("/slow") {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                    // 第一次请求：发了 6 MiB 就断开（中途断线）
                    if path.starts_with("/cut")
                        && sent > 6 << 20
                        && cuts.fetch_add(1, Ordering::SeqCst) == 0
                    {
                        return;
                    }
                    if s.write_all(chunk).await.is_err() {
                        return;
                    }
                    sent += chunk.len() as u64;
                }
            });
        }
    }

    async fn upstream(size: usize) -> Upstream {
        let log = Arc::new(Mutex::new(Vec::new()));
        let data = Arc::new(data(size));
        let active = Arc::new(AtomicU64::new(0));
        let l2 = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let other = format!("http://{}", l2.local_addr().unwrap());
        tokio::spawn(serve(l2, log.clone(), data.clone(), None, active.clone()));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(serve(
            l,
            log.clone(),
            data.clone(),
            Some(other.clone()),
            active.clone(),
        ));
        Upstream {
            base,
            other,
            log,
            data,
            active,
        }
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("embymini-stream-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn auth(u: &Upstream) -> Auth {
        Auth {
            token: "tok".into(),
            origin: format!("{}/emby", u.base),
        }
    }

    async fn get(url: &str, range: Option<&str>, thumb: bool) -> (u16, Vec<u8>) {
        let c = reqwest::Client::builder().no_proxy().build().unwrap();
        let mut rb = c.get(url);
        if let Some(r) = range {
            rb = rb.header("range", r);
        }
        if thumb {
            rb = rb.header(ROLE, "thumb");
        }
        let r = rb.send().await.unwrap();
        let st = r.status().as_u16();
        (st, r.bytes().await.map(|b| b.to_vec()).unwrap_or_default())
    }

    fn reqs(u: &Upstream, path: &str) -> Vec<(String, String, bool)> {
        lock(&u.log)
            .iter()
            .filter(|r| r.0.starts_with(path))
            .cloned()
            .collect()
    }

    async fn until(mut f: impl FnMut() -> bool, secs: u64) -> bool {
        let t0 = Instant::now();
        while !f() {
            if t0.elapsed() > Duration::from_secs(secs) {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        true
    }

    /// 随机区间、并发读都和源文件逐字节一致；整片下完后新开代理读同一目录零上游请求。
    #[tokio::test(flavor = "multi_thread")]
    async fn reads_match_and_cache_is_reused() {
        let u = upstream((10 * BLOCK + 12345) as usize).await;
        let dir = tmp("match");
        let s = Stream::start(&dir, true).unwrap();
        let url = s.video("srv_1_a", &format!("{}/f", u.base), &auth(&u));
        let n = u.data.len() as u64;
        let cases = [
            (0, 99),
            (BLOCK - 10, BLOCK + 10),
            (3 * BLOCK + 7, 7 * BLOCK),
            (n - 100, n - 1),
        ];
        let mut tasks = Vec::new();
        for (a, b) in cases {
            let url = url.clone();
            tasks.push(tokio::spawn(async move {
                (
                    a,
                    b,
                    get(&url, Some(&format!("bytes={a}-{b}")), false).await,
                )
            }));
        }
        for t in tasks {
            let (a, b, (st, body)) = t.await.unwrap();
            assert_eq!(st, 206);
            assert!(
                body == u.data[a as usize..=b as usize],
                "{a}-{b} 内容不一致"
            );
        }
        let (st, body) = get(&url, None, false).await;
        assert_eq!((st, body.len() as u64), (200, n));
        assert!(body == *u.data);
        // 整片模式：后台补完全部块
        let f = lock(&s.files).get("srv_1_a").unwrap().clone();
        assert!(
            until(|| f.st().done.iter().all(|d| *d), 20).await,
            "没有整片下完"
        );
        assert!(until(|| !f.st().busy(), 5).await);
        let before = reqs(&u, "/f").len();
        // 新开一个代理（模拟下次启动）：全部走本地
        let s2 = Stream::start(&dir, true).unwrap();
        let url2 = s2.video("srv_1_a", &format!("{}/f", u.base), &auth(&u));
        let (_, body) = get(&url2, Some("bytes=5000000-"), false).await;
        assert!(body == u.data[5_000_000..]);
        assert_eq!(reqs(&u, "/f").len(), before, "已缓存的部分不该再向上游要");
        drop(f);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 上游卡住 3 秒没正文就对冲，先出数据的留下；中途断线后续上，内容仍一致。
    #[tokio::test(flavor = "multi_thread")]
    async fn hedges_stalls_and_resumes_after_cut() {
        let u = upstream((3 * BLOCK) as usize).await;
        let dir = tmp("hedge");
        let s = Stream::start(&dir, false).unwrap();
        let url = s.video("srv_2_a", &format!("{}/stall", u.base), &auth(&u));
        let t0 = Instant::now();
        let (st, body) = get(&url, Some("bytes=0-1023"), false).await;
        assert_eq!(st, 206);
        assert!(body == u.data[..1024]);
        let took = t0.elapsed();
        assert!(
            took >= Duration::from_secs(3) && took < Duration::from_secs(6),
            "{took:?}"
        );
        assert!(reqs(&u, "/stall").len() >= 2);

        let url = s.video("srv_3_a", &format!("{}/cut", u.base), &auth(&u));
        let (st, body) = get(&url, None, false).await;
        assert_eq!(st, 200);
        assert!(body == *u.data, "断线续传后内容不一致");
        assert!(reqs(&u, "/cut").len() >= 2);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 预览只取 1–2 块，取过的位置再要不发请求；拖到远处只剩一个主任务。
    #[tokio::test(flavor = "multi_thread")]
    async fn preview_takes_few_blocks_and_seek_keeps_one_main() {
        let u = upstream((60 * BLOCK) as usize).await;
        let dir = tmp("preview");
        let s = Stream::start(&dir, false).unwrap();
        let url = s.video("srv_4_a", &format!("{}/f", u.base), &auth(&u));
        let at = 20 * BLOCK + 100;
        let (st, body) = get(&url, Some(&format!("bytes={at}-{}", at + 999)), true).await;
        assert_eq!(st, 206);
        assert!(body == u.data[at as usize..at as usize + 1000]);
        let f = lock(&s.files).get("srv_4_a").unwrap().clone();
        assert!(until(|| !f.st().busy(), 5).await);
        let r = reqs(&u, "/f");
        assert_eq!(r.len(), 1, "{r:?}");
        assert_eq!(r[0].1, format!("bytes={}-{}", 20 * BLOCK, 21 * BLOCK - 1));
        let n = r.len();
        let _ = get(
            &url,
            Some(&format!("bytes={}-{}", at + 5000, at + 9999)),
            true,
        )
        .await;
        assert_eq!(reqs(&u, "/f").len(), n, "取过的位置不该再请求");

        // 主播放器：开头读一点，过 2 秒（主任务不再算刚开）拖到 30 块处读一点
        let _ = get(&url, Some("bytes=0-99"), false).await;
        tokio::time::sleep(Duration::from_secs(2)).await;
        let _ = get(
            &url,
            Some(&format!("bytes={}-{}", 30 * BLOCK, 30 * BLOCK + 99)),
            false,
        )
        .await;
        tokio::time::sleep(Duration::from_secs(2)).await;
        // 只缓存前方：开头那个主任务停在前方 AHEAD 块，被换掉后不再往后写；新的从 30 块写到 30 + AHEAD
        let st = f.st();
        let done: Vec<usize> = (0..st.done.len()).filter(|&b| st.done[b]).collect();
        drop(st);
        let want: Vec<usize> = (0..=AHEAD as usize)
            .chain([20])
            .chain(30..=30 + AHEAD as usize)
            .collect();
        assert_eq!(done, want);
        drop(f);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 403 原样回给 mpv；跨主机重定向不带 token、同源带；字幕取一次后走本地。
    #[tokio::test(flavor = "multi_thread")]
    async fn status_redirect_and_subs() {
        let u = upstream(BLOCK as usize + 10).await;
        let dir = tmp("status");
        let s = Stream::start(&dir, true).unwrap();
        let url = s.video("srv_5_a", &format!("{}/403", u.base), &auth(&u));
        assert_eq!(get(&url, Some("bytes=0-"), false).await.0, 403);
        assert_eq!(s.fail_status("srv_5_a"), Some(403));

        let url = s.video("srv_6_a", &format!("{}/redir", u.base), &auth(&u));
        let (st, body) = get(&url, Some("bytes=0-9"), false).await;
        assert_eq!((st, body.as_slice()), (206, &u.data[..10]));
        let first = reqs(&u, "/redir");
        assert!(first[0].2, "同源要带 token");
        let moved: Vec<_> = lock(&u.log)
            .iter()
            .filter(|r| r.0 == "/f")
            .cloned()
            .collect();
        assert!(
            !moved.is_empty() && moved.iter().all(|r| !r.2),
            "跳到别的主机不能带 token：{moved:?}"
        );
        let _ = &u.other;

        let sub = s.sub("srv_7_a_3.srt", &format!("{}/sub", u.base), &auth(&u));
        assert!(sub.ends_with("/srv_7_a_3.srt"));
        assert_eq!(get(&sub, None, false).await.1, b"hello");
        assert_eq!(get(&sub, None, false).await.1, b"hello");
        assert_eq!(reqs(&u, "/sub").len(), 1, "字幕第二次走本地");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// retain 删其余目录、停掉它们的下载；到容量上限时不再往后预取。
    #[tokio::test(flavor = "multi_thread")]
    async fn retain_and_capacity() {
        let u = upstream((40 * BLOCK) as usize).await;
        let dir = tmp("retain");
        let s = Stream::start(&dir, false).unwrap();
        let a = s.video("srv_8_a", &format!("{}/f", u.base), &auth(&u));
        let _b = s.video("srv_9_a", &format!("{}/f", u.base), &auth(&u));
        let sub_a = s.sub("srv_8_a_3.srt", &format!("{}/sub", u.base), &auth(&u));
        let sub_b = s.sub("srv_9_a_3.srt", &format!("{}/sub", u.base), &auth(&u));
        let _ = get(&a, Some("bytes=0-9"), false).await;
        std::fs::create_dir_all(dir.join("video/old_1_x")).unwrap();
        s.retain(&["srv_8_a".into()]);
        assert_eq!(get(&sub_a, None, false).await.0, 200);
        assert_eq!(
            get(&sub_b, None, false).await.0,
            404,
            "retain 掉的视频，字幕登记一起清掉"
        );
        assert!(
            until(
                || !dir.join("video/srv_9_a").exists() && !dir.join("video/old_1_x").exists(),
                5
            )
            .await
        );
        assert!(dir.join("video/srv_8_a").exists());
        assert!(lock(&s.files).get("srv_9_a").is_none());
        let _ = std::fs::remove_dir_all(&dir);

        // 容量只够 MIN_CAP：整片模式也只写到前方一段
        let s = Stream::start(&dir, true).unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await; // 启动时按磁盘算容量，等它跑完再改
        s.cap.store(MIN_CAP, Ordering::Relaxed);
        let a = s.video("srv_10_a", &format!("{}/f", u.base), &auth(&u));
        let _ = get(&a, Some("bytes=0-9"), false).await;
        let f = lock(&s.files).get("srv_10_a").unwrap().clone();
        tokio::time::sleep(Duration::from_secs(2)).await;
        let n = f.st().done.iter().filter(|d| **d).count() as u64;
        assert!(n <= AHEAD + 2, "到了容量上限还在往后写：{n} 块");
        drop(f);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 只缓存前方：写满前方 AHEAD 块就放掉上游连接（挂着不读会被读超时断开），读的位置往前走半段再续上。
    #[tokio::test(flavor = "multi_thread")]
    async fn limited_mode_releases_connection_and_resumes() {
        let u = upstream((60 * BLOCK) as usize).await;
        let dir = tmp("limited");
        let s = Stream::start(&dir, false).unwrap();
        let url = s.video("srv_11_a", &format!("{}/f", u.base), &auth(&u));
        let _ = get(&url, Some("bytes=0-99"), false).await;
        let f = lock(&s.files).get("srv_11_a").unwrap().clone();
        let done = |f: &File| -> Vec<usize> {
            let st = f.st();
            (0..st.done.len()).filter(|&b| st.done[b]).collect()
        };
        assert!(
            until(|| done(&f).len() == AHEAD as usize + 1, 5).await,
            "{:?}",
            done(&f)
        );
        assert!(
            until(|| u.active.load(Ordering::SeqCst) == 0, 3).await,
            "写满前方后还挂着上游连接"
        );
        assert!(f.st().main.is_some(), "主任务应该还在等读的位置往前走");
        // 读到第 5 块：离写到的位置还差 12 块，不续；读到第 10 块：差 7 块（不到半段 8 块），续上写到 26
        let _ = get(
            &url,
            Some(&format!("bytes={}-{}", 5 * BLOCK, 5 * BLOCK + 9)),
            false,
        )
        .await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(done(&f).len(), AHEAD as usize + 1);
        let _ = get(
            &url,
            Some(&format!("bytes={}-{}", 10 * BLOCK, 10 * BLOCK + 9)),
            false,
        )
        .await;
        assert!(
            until(|| done(&f) == (0..=26).collect::<Vec<_>>(), 5).await,
            "{:?}",
            done(&f)
        );
        assert_eq!(reqs(&u, "/f").len(), 2, "续写应只开一个新请求");
        drop(f);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 5xx 当成暂时的：主任务退避重试，读的一方等到数据，不把 502 回给 mpv。
    #[tokio::test(flavor = "multi_thread")]
    async fn transient_5xx_is_retried() {
        let u = upstream(BLOCK as usize + 100).await;
        let dir = tmp("flaky");
        let s = Stream::start(&dir, false).unwrap();
        let url = s.video("srv_12_a", &format!("{}/flaky", u.base), &auth(&u));
        let t0 = Instant::now();
        let (st, body) = get(&url, Some("bytes=0-99"), false).await;
        assert_eq!(st, 206);
        assert!(body == u.data[..100]);
        assert!(t0.elapsed() < Duration::from_secs(8), "{:?}", t0.elapsed());
        assert_eq!(reqs(&u, "/flaky").len(), 3);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// retain 掉还在读的文件：读的连接断开，不再安排下载；播放结束（idle）停掉整片下载。
    #[tokio::test(flavor = "multi_thread")]
    async fn retain_and_idle_stop_downloads() {
        let u = upstream((20 * BLOCK) as usize).await;
        let dir = tmp("stop");
        let s = Stream::start(&dir, true).unwrap();
        let url = s.video("srv_13_a", &format!("{}/slow", u.base), &auth(&u));
        let reading = tokio::spawn(async move { get(&url, None, false).await });
        // 等读的一方真在收数据（总大小已知、第一块写了一部分）：测的是读数据中途被删，不是等总大小时
        let f = lock(&s.files).get("srv_13_a").unwrap().clone();
        assert!(
            until(
                || f.st().writing.get(&0).is_some_and(|p| p.written > 1 << 20),
                5
            )
            .await
        );
        drop(f);
        s.retain(&[]);
        let (_, body) = tokio::time::timeout(Duration::from_secs(5), reading)
            .await
            .expect("retain 后读的连接没断")
            .unwrap();
        assert!(body.len() < u.data.len());
        assert!(until(|| u.active.load(Ordering::SeqCst) == 0, 3).await);
        let n = reqs(&u, "/slow").len();
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(reqs(&u, "/slow").len(), n, "删掉的文件还在向上游要数据");

        let url = s.video("srv_14_a", &format!("{}/slow", u.base), &auth(&u));
        let _ = get(&url, Some("bytes=0-99"), false).await;
        assert!(
            u.active.load(Ordering::SeqCst) > 0,
            "整片模式应在后台接着下"
        );
        let first = dir.join("video/srv_14_a/0");
        assert!(until(|| first.exists(), 10).await);
        s.idle(&["srv_14_a".into()]);
        assert!(
            until(|| u.active.load(Ordering::SeqCst) == 0, 3).await,
            "播放结束后还在下"
        );
        assert!(first.exists(), "缓存要留着");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 容量小到连读的位置附近都放不下时照样写（只是超一点），不能拒绝写而让任务空转重开请求。
    #[tokio::test(flavor = "multi_thread")]
    async fn tiny_capacity_still_serves() {
        let u = upstream((8 * BLOCK) as usize).await;
        let dir = tmp("tiny");
        let s = Stream::start(&dir, false).unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        s.cap.store(BLOCK, Ordering::Relaxed);
        let url = s.video("srv_15_a", &format!("{}/f", u.base), &auth(&u));
        let (st, body) = get(&url, Some(&format!("bytes=0-{}", 3 * BLOCK)), false).await;
        assert_eq!(st, 206);
        assert!(body == u.data[..=3 * BLOCK as usize]);
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(reqs(&u, "/f").len() <= 2, "{:?}", reqs(&u, "/f"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
