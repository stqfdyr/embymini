//! Local caching proxy: mpv and the seek-bar preview process read video (`http://127.0.0.1:<port>/v/<key>/…`) and
//! subtitles (`/s/<key>/…`) from here; it saves whole files block by block while playing, serving what is on disk
//! and fetching the rest upstream.
//!
//! - Loopback only; `key` is a random 128-bit id per registered file, so this is no open relay.
//! - The token stays here: mpv and the preview process never see it; redirects are followed here, with the token
//!   only for the server's origin.
//! - Disk: `<cache>/<server>_<item>_<version>/`, [`BLOCK`]-sized blocks; done blocks are `<n>`, ones being written
//!   `<n>.<seq>.part`, and `meta.json` holds the total size (a mismatch with upstream discards the dir). The blocks
//!   present are the files in the dir; evicting is deleting a file.
//! - Download tasks (one writer per block at a time):
//!   - Main task: one per file, open-ended and sequential; at a block that is done or being written it disconnects
//!     and restarts at the next missing block after the main player's last read, wrapping at the end until the whole
//!     file is there ("whole" mode). Ahead-only (whole off, or at the cap) writes [`AHEAD`] blocks past the read position.
//!     A main-player read elsewhere within 2 s of a new main task (mkv reads the header, then the index at the end)
//!     gets a two-block short task instead of a tug of war.
//!   - Preview tasks: for the seek-bar preview (`X-EmbyMini-Role: thumb`) and next-episode prefetch, closed ranges
//!     of 1–2 blocks, at most [`PREVIEWS`] per file.
//!   - Hedging: a request with no body after 3 s gets a twin at the same position; the first with data wins; at most
//!     two at once, later replaced at 5 and 8 s.
//! - Keeping: the player calls [`Stream::retain`] on each episode change (current + queued next); the rest stop and
//!   are deleted. Exit deletes nothing, so reopening a film plays from disk.
//! - Capacity: min(20 GiB, free space + current video cache − 10 GiB); at the cap only the window ahead is cached and
//!   writing a block evicts the file's least recently used ones.

use bytes::Bytes;
use hyper::body::{Frame, Incoming};
use hyper::header::HeaderValue;
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

use crate::{lock, sub_total};

/// Block size. Block files rather than one big sparse file: NTFS zero-fills synchronously when a non-sparse file is written far ahead.
pub const BLOCK: u64 = 4 << 20;
/// Ahead-only caching writes this many blocks past the main player's read position (64 MiB, about one mpv readahead).
const AHEAD: u64 = 16;
/// A main-player read this close past the main task's write point waits for it; farther away it gets a new main task (a seek).
const MAIN_NEAR: u64 = 2;
/// Preview / prefetch tasks per file; a new one cancels the oldest (previews only care about the latest position).
const PREVIEWS: usize = 2;
/// Hedging (seconds): with no body yet, start another request; later steps replace the older of the two.
const HEDGE: [u64; 3] = [3, 5, 8];
/// Give up when no request yields data (504); the reader will ask again.
const OPEN_LIMIT: Duration = Duration::from_secs(30);
/// A first read's wait for the size: a little past [`OPEN_LIMIT`], so the first request's own verdict lands first.
const SIZE_WAIT: Duration = Duration::from_secs(OPEN_LIMIT.as_secs() + 5);
const _: () = assert!(
    SIZE_WAIT.as_secs() > OPEN_LIMIT.as_secs(),
    "SIZE_WAIT must outlast OPEN_LIMIT"
);
/// Largest chunk per body frame sent to mpv.
const READ_CHUNK: u64 = 256 << 10;
/// Video cache cap when the disk has room.
const MAX_CAP: u64 = 20 << 30;
/// Free space always left on the disk.
const RESERVE: u64 = 10 << 30;
/// Cap floor: even a nearly full disk must hold the window ahead, or nothing could be written at all.
const MIN_CAP: u64 = 2 * AHEAD * BLOCK;
/// Subtitle cache cap.
const SUBS_CAP: u64 = 200 << 20;
const ROLE: &str = "x-embymini-role";

fn blocks(size: u64) -> usize {
    size.div_ceil(BLOCK) as usize
}

fn block_len(size: u64, b: usize) -> u64 {
    BLOCK.min(size - b as u64 * BLOCK)
}

/// Who reads: the main player, or a preview-type reader (the seek-bar preview, or the next-episode prefetch
/// started by [`Stream::prefetch`] without HTTP); the two kinds of preview are scheduled alike.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Role {
    Main,
    Preview,
}

/// Credentials for upstream requests: the token only goes to hosts with the API root's origin.
#[derive(Clone)]
pub struct Auth {
    pub token: String,
    pub origin: String,
}

struct Src {
    url: String,
    auth: Auth,
    /// Where the last redirect led; later Range requests go straight there and fall back to `url` on failure.
    resolved: Option<String>,
}

#[derive(Debug)]
enum Fail {
    /// Definitive upstream HTTP status (see [`crate::status`]): passed to mpv as is, so a 403 / 404 can be explained.
    Status(u16),
    /// Upstream temporarily unavailable (5xx, 429…, common with flaky relays): retried like a network error,
    /// honouring `Retry-After`; mpv only gets the code if it never recovers.
    Busy(u16, Option<Duration>),
    Net(String),
    /// Could not connect (nothing was sent): handled like `Net`; only subtitles use it to decide on a retry.
    Connect(String),
    Timeout,
}

impl Fail {
    fn code(&self) -> u16 {
        match self {
            Self::Status(s) | Self::Busy(s, _) => *s,
            Self::Net(_) | Self::Connect(_) => 502,
            Self::Timeout => 504,
        }
    }

    /// Worth retrying (network error, timeout, 5xx).
    fn transient(&self) -> bool {
        !matches!(self, Self::Status(_))
    }
}

/// Why [`Stream::pump`] stopped.
enum Stop {
    /// Reached the requested end, or a block that is done / being written.
    Hit,
    /// Ahead-only: [`AHEAD`] blocks past the read position; block b is left for later (connection released).
    Ahead(usize),
}

struct Part {
    path: PathBuf,
    written: u64,
}

struct Task {
    id: u64,
    task: tokio::task::JoinHandle<()>,
}

struct Preview {
    t: Task,
    blocks: std::ops::RangeInclusive<usize>,
    role: Role,
}

struct Main {
    t: Task,
    /// Block being written (or about to be).
    at: usize,
    since: Instant,
}

#[derive(Default)]
struct St {
    size: Option<u64>,
    done: Vec<bool>,
    /// Last read / write of each block (global tick); at the cap the least recently used go first.
    touched: Vec<u64>,
    writing: HashMap<usize, Part>,
    main: Option<Main>,
    /// Preview and prefetch tasks (the main player's short tasks live here too).
    previews: Vec<Preview>,
    last_read: u64,
    /// Last failure and when it happened; no retry before `retry_at`.
    fail: Option<(Fail, Instant)>,
    retry_at: Option<Instant>,
    seq: u64,
}

impl St {
    fn has(&self, b: usize) -> bool {
        self.done.get(b).copied().unwrap_or(false) || self.writing.contains_key(&b)
    }

    /// Block of the main player's last read.
    fn read_block(&self) -> usize {
        (self.last_read / BLOCK) as usize
    }

    /// Some preview-type task is fetching block b.
    fn previewing(&self, b: usize) -> bool {
        self.previews.iter().any(|p| p.blocks.contains(&b))
    }

    fn busy(&self) -> bool {
        self.main.is_some() || !self.previews.is_empty()
    }

    /// Upstream HTTP error seen after `since` (readers answer with it instead of waiting forever).
    fn status_since(&self, since: Instant) -> Option<u16> {
        match &self.fail {
            Some((Fail::Status(s), at)) if *at >= since => Some(*s),
            _ => None,
        }
    }
}

struct File {
    /// Dropped by retain (directory being deleted): connected readers stop scheduling downloads and disconnect.
    gone: AtomicBool,
    dir: PathBuf,
    key: String,
    src: Mutex<Src>,
    st: Mutex<St>,
    /// Bumped on new data or state changes; readers wake on it.
    changed: watch::Sender<u64>,
    /// Upstream bytes for this file alone (the player's stall log; the top-bar speed uses the total).
    up: AtomicU64,
}

impl File {
    /// Opens (or creates) a file's cache dir: blocks whose size matches meta.json count as done; partial
    /// and unknown files are deleted.
    fn open(dir: PathBuf, src: Src) -> Self {
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
            // Canonical names only: `01` or `+1` would parse as block 1 yet never be read or deleted.
            let whole = fname.parse::<usize>().ok().filter(|&b| {
                b.to_string() == fname
                    && size.is_some_and(|s| {
                        b < n && e.metadata().is_ok_and(|m| m.len() == block_len(s, b))
                    })
            });
            match whole {
                Some(b) => done[b] = true,
                None => {
                    let _ = std::fs::remove_file(&p);
                }
            }
        }
        Self {
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
            up: AtomicU64::new(0),
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

    /// Stops all of this file's downloads (the cache stays).
    fn stop(&self) {
        let mut st = self.st();
        let tasks: Vec<_> = st
            .main
            .take()
            .map(|m| m.t)
            .into_iter()
            .chain(st.previews.drain(..).map(|p| p.t))
            .collect();
        drop(st);
        abort_all(tasks);
    }
}

/// A block being written: dropped unfinished (task cancelled, error) it deletes its `.part` and unregisters.
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

/// Removes a download task from its file's state when it ends (including cancellation).
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
        st.previews.retain(|p| p.t.id != self.id);
        drop(st);
        self.f.bump();
    }
}

enum Key {
    Video(String),
    Sub(Arc<SubSrc>),
}

struct SubSrc {
    /// File name in the cache.
    file: String,
    src: Mutex<Src>,
}

pub struct Inner {
    port: u16,
    video: PathBuf,
    subs: PathBuf,
    http: reqwest::Client,
    /// Cache whole videos while playing (setting, off by default); off caches only a window ahead of the reader.
    whole: AtomicBool,
    cap: AtomicU64,
    /// Bytes of done blocks in the video cache.
    total: AtomicU64,
    /// Bytes downloaded upstream (the title bar's speed is its rate of change).
    up_bytes: AtomicU64,
    tick: AtomicU64,
    ids: AtomicU64,
    files: Mutex<HashMap<String, Arc<File>>>,
    keys: Mutex<HashMap<String, Key>>,
    /// Dirs due for deletion: renamed into a trash dir, then deleted slowly; one registered again before the rename stays.
    doomed: Mutex<HashSet<String>>,
    /// A delayed sweep retry is queued: one is enough, however many retains happen meanwhile.
    sweep_retry: AtomicBool,
}

/// Handle to the proxy (cheap to clone).
#[derive(Clone)]
pub struct Stream(Arc<Inner>);

impl Deref for Stream {
    type Target = Inner;
    fn deref(&self) -> &Inner {
        &self.0
    }
}

/// Body sent to mpv: empty, whole (small), or chunks from a read task over a bounded channel (backpressure: when mpv
/// stops reading, so does the task).
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

/// A request's Range: `bytes=a-`, `bytes=a-b`, `bytes=-n`; anything else (several ranges, end before start) is
/// ignored and the whole file is sent, as RFC 9110 allows.
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
        Some(b.parse::<u64>().ok().filter(|&b| b >= a)?)
    };
    Some(Range::From(a, b))
}

/// The closed span for a file of `size`; None (416) when the start is past the end.
fn resolve(r: Option<Range>, size: u64) -> Option<(u64, u64)> {
    match r {
        None => (size > 0).then(|| (0, size - 1)),
        Some(Range::From(a, b)) => (a < size).then(|| (a, b.unwrap_or(u64::MAX).min(size - 1))),
        Some(Range::Suffix(n)) => (n > 0 && size > 0).then(|| (size - n.min(size), size - 1)),
    }
}

/// `Content-Range: bytes a-b/total` → (a, total).
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

/// Blocks a read at `pos` asks for, [b, hi]: in the block's last MiB the next one too, since a preview decodes past
/// its position (only previews use `hi`).
fn wanted(pos: u64) -> (usize, usize) {
    let b = (pos / BLOCK) as usize;
    (
        b,
        if pos % BLOCK + (1 << 20) > BLOCK {
            b + 1
        } else {
            b
        },
    )
}

/// A request that produced data: the response, its first body chunk and the file's total size.
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

/// A reader's next step ([`Stream::locate`]).
enum Next {
    /// Read from this file, which holds the block up to the given length; `true` for a done block.
    Read(PathBuf, u64, bool),
    /// Wait for the block; the tasks were superseded and are aborted outside the lock.
    Wait(Vec<Task>),
    /// Stop: retained away or refused upstream.
    Gone,
}

impl Stream {
    /// Starts the server. None if no port can be bound (rare): playback then goes direct.
    pub fn start(root: &Path, whole: bool) -> Option<Self> {
        let l = std::net::TcpListener::bind(("127.0.0.1", 0))
            .and_then(|l| l.set_nonblocking(true).map(|_| l))
            .map_err(|e| log::warn!("本地缓存代理起不来，播放改为直连：{e}"))
            .ok()?;
        let port = l.local_addr().ok()?.port();
        let http = reqwest::Client::builder()
            .no_proxy()
            .user_agent(crate::emby::UA)
            // HTTP/2's default 64 KB stream window throttles high-latency links; the pool lets preview requests reuse connections
            .http1_only()
            .connect_timeout(Duration::from_secs(10))
            // Reconnect when data stops mid-body (hedging only covers the first byte)
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
            sweep_retry: AtomicBool::new(false),
        }));
        let _ = std::fs::create_dir_all(&me.video);
        let _ = std::fs::create_dir_all(&me.subs);
        crate::rt::spawn(me.clone().accept(l));
        let m = me.clone();
        crate::rt::spawn_blocking(move || {
            // Trash dirs a previous run did not finish deleting
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

    /// Serves connections for the life of the process.
    async fn accept(self, l: std::net::TcpListener) {
        let l = match tokio::net::TcpListener::from_std(l) {
            Ok(l) => l,
            Err(e) => return log::warn!("本地缓存代理起不来：{e}"),
        };
        loop {
            let sock = match l.accept().await {
                Ok((sock, _)) => sock,
                Err(_) => {
                    // Out of handles and the like: retrying at once would spin a core
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            let m = self.clone();
            crate::rt::spawn(async move {
                let svc = hyper::service::service_fn(move |req| {
                    let m = m.clone();
                    async move { Ok::<_, Infallible>(m.handle(req).await) }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(hyper_util::rt::TokioIo::new(sock), svc)
                    .await;
            });
        }
    }

    pub fn set_whole(&self, on: bool) {
        self.whole.store(on, Ordering::Relaxed);
        for f in lock(&self.files).values() {
            f.bump(); // waiting main tasks re-check whether to write further
        }
    }

    /// Bytes downloaded upstream so far (only grows).
    pub fn up_bytes(&self) -> u64 {
        self.up_bytes.load(Ordering::Relaxed)
    }

    /// Bytes downloaded upstream for one registered file (0 when not registered).
    pub fn file_up_bytes(&self, name: &str) -> u64 {
        lock(&self.files)
            .get(name)
            .map_or(0, |f| f.up.load(Ordering::Relaxed))
    }

    /// Registers a video and returns its local URL for mpv. `name` is the cache dir, the same for a file every time,
    /// so a later run reuses its cache.
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
                let f = Arc::new(File::open(self.video.join(name), src));
                lock(&self.keys).insert(f.key.clone(), Key::Video(name.to_owned()));
                files.insert(name.to_owned(), f.clone());
                f
            }
        };
        format!("http://127.0.0.1:{}/v/{}/stream", self.port, f.key)
    }

    /// Registers an external subtitle and returns its local URL, ending in the file name (mpv tells the format by
    /// the extension). The first read fetches it whole into the subtitle cache.
    pub fn sub(&self, file: &str, url: &str, auth: &Auth) -> String {
        let key = uuid::Uuid::new_v4().simple().to_string();
        // The extension is the server's codec string: one plain name, or `/`, `\` and `..` would write outside the cache
        let mut file: String = file
            .chars()
            .map(|c| match c {
                'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '_' | '-' => c,
                _ => '-',
            })
            .collect();
        if file.trim_matches('.').is_empty() {
            file.clone_from(&key);
        }
        let src = Src {
            url: url.to_owned(),
            auth: auth.clone(),
            resolved: None,
        };
        let s = Arc::new(SubSrc {
            file: file.clone(),
            src: Mutex::new(src),
        });
        lock(&self.keys).insert(key.clone(), Key::Sub(s));
        format!("http://127.0.0.1:{}/s/{key}/{file}", self.port)
    }

    /// The upstream HTTP status of the last failure, if it was one (explains a 403 / 404 when playback fails).
    pub fn fail_status(&self, name: &str) -> Option<u16> {
        let f = lock(&self.files).get(name)?.clone();
        match &f.st().fail {
            Some((Fail::Status(s) | Fail::Busy(s, _), _)) => Some(*s),
            _ => None,
        }
    }

    /// Keeps only these files' caches (current episode, queued next one); stops and deletes the rest.
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
                // Subtitle cache names start with the video's; mpv reads them whole at sub-add and never again.
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
        }
        let me = self.clone();
        crate::rt::spawn_blocking(move || {
            // Every episode change refreshes the cap from free disk space and corrects drift in `total`.
            if !me.sweep(false) {
                me.recount();
            }
        });
    }

    /// Deletes doomed dirs: renamed into the trash under the lock (a name registered again first is spared),
    /// then removed outside it. On Windows the rename fails while a file inside is open; retried every 10 s
    /// by a single chain (`retry` marks a run of it). Returns whether anything was renamed (and recounted).
    fn sweep(&self, retry: bool) -> bool {
        let mut stuck = Vec::new();
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
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
                    Err(e) => {
                        stuck.push(format!("{n}（{e}）"));
                        true
                    }
                }
            });
            out
        };
        let renamed = !trash.is_empty();
        // Trash dirs are not counted (recount skips them): recount first so a new film need not wait for
        // the old dir to be deleted.
        if renamed {
            self.recount();
        }
        for t in trash {
            let _ = std::fs::remove_dir_all(t);
        }
        // The retry chain owns the flag; any other sweep starts a chain only if none is pending.
        if !stuck.is_empty() && (retry || !self.sweep_retry.swap(true, Ordering::AcqRel)) {
            if !retry {
                log::warn!("缓存目录暂时删不掉，每 10 秒重试：{}", stuck.join("、"));
            }
            let me = self.clone();
            crate::rt::spawn(async move {
                tokio::time::sleep(Duration::from_secs(10)).await;
                crate::rt::spawn_blocking(move || me.sweep(true));
            });
        } else if retry {
            self.sweep_retry.store(false, Ordering::Release);
        }
        renamed
    }

    /// Recounts the video cache (trash excluded) and sets the cap from the disk's free space.
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

    /// Bytes in the video cache, excluding trash dirs being deleted.
    fn video_size(&self) -> u64 {
        std::fs::read_dir(&self.video)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| !e.file_name().to_string_lossy().starts_with(".trash-"))
            .map(|e| dir_size(&e.path()))
            .sum()
    }

    /// Bytes used by the video and subtitle caches.
    pub fn usage(&self) -> (u64, u64) {
        (self.video_size(), dir_size(&self.subs))
    }

    /// Clears the video cache. While `playing` (or starting) registered files stay: retain ran at the episode
    /// change, so they are exactly this playback's current and next episode.
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

    /// Prefetches the next episode's first block and last two (mkv keeps its index at the end), so switching opens offline.
    pub fn prefetch(&self, name: &str) {
        let Some(f) = lock(&self.files).get(name).cloned() else {
            return;
        };
        let me = self.clone();
        crate::rt::spawn(async move {
            let mut rx = f.changed.subscribe();
            let t0 = Instant::now();
            loop {
                let abort = {
                    let mut st = f.st();
                    if let Some(size) = st.size {
                        let n = blocks(size);
                        let lo = n.saturating_sub(2);
                        let a = me.ensure(&f, &mut st, lo, n.saturating_sub(1), Role::Preview);
                        let b = me.ensure(&f, &mut st, 0, 0, Role::Preview);
                        drop(st);
                        abort_all(a.into_iter().chain(b));
                        return;
                    }
                    if st.fail.is_some() || t0.elapsed() > OPEN_LIMIT {
                        return;
                    }
                    me.ensure(&f, &mut st, 0, 0, Role::Preview)
                };
                abort_all(abort);
                let _ = tokio::time::timeout(Duration::from_secs(1), rx.changed()).await;
            }
        });
    }

    /// A playback ended (mpv quit): stops these files' downloads and keeps the cache. Otherwise whole-file mode
    /// would finish the download after the player closed. Only these: a new playback may already have started.
    pub fn idle(&self, names: &[String]) {
        let files: Vec<_> = {
            let all = lock(&self.files);
            names.iter().filter_map(|n| all.get(n).cloned()).collect()
        };
        for f in files {
            f.stop();
        }
    }

    async fn handle(&self, req: Request<Incoming>) -> Response<Body> {
        let head = req.method() == hyper::Method::HEAD;
        if !head && req.method() != hyper::Method::GET {
            return status(405);
        }
        let role = match req.headers().get(ROLE).and_then(|v| v.to_str().ok()) {
            Some("thumb") => Role::Preview,
            _ => Role::Main,
        };
        let range = req
            .headers()
            .get(hyper::header::RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(parse_range);
        let mut seg = req.uri().path().trim_start_matches('/').split('/');
        let (kind, key) = (seg.next(), seg.next().unwrap_or_default());
        // Release `keys` before taking `files` (registration takes `files`, then `keys`)
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
        let t0 = Instant::now();
        let first = match range {
            Some(Range::From(a, _)) => a,
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
                format!("bytes */{size}")
                    .parse()
                    .expect("digits and ASCII make a valid header value"),
            );
            return r;
        };
        // Not cached here and upstream just refused: answer with that code (mpv fails, the UI says why) instead of a stall
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
        h.insert(
            hyper::header::ACCEPT_RANGES,
            HeaderValue::from_static("bytes"),
        );
        h.insert(
            hyper::header::CONTENT_TYPE,
            HeaderValue::from_static("application/octet-stream"),
        );
        h.insert(
            hyper::header::CONTENT_LENGTH,
            HeaderValue::from(end - start + 1),
        );
        if range.is_some() {
            *r.status_mut() = StatusCode::PARTIAL_CONTENT;
            r.headers_mut().insert(
                hyper::header::CONTENT_RANGE,
                format!("bytes {start}-{end}/{size}")
                    .parse()
                    .expect("digits and ASCII make a valid header value"),
            );
        }
        if !head {
            let (tx, rx) = mpsc::channel(4);
            let me = self.clone();
            crate::rt::spawn(async move { me.read_loop(f, start, end, role, tx).await });
            *r.body_mut() = Body::Chan(rx);
        }
        r
    }

    /// Waits for the file's total size (the first read needs it from upstream). An upstream HTTP error is answered
    /// with its code; network trouble is retried for [`SIZE_WAIT`].
    async fn wait_size(
        &self,
        f: &Arc<File>,
        pos: u64,
        role: Role,
        t0: Instant,
    ) -> Result<u64, u16> {
        let (b, hi) = wanted(pos);
        let mut rx = f.changed.subscribe();
        loop {
            let abort = {
                let mut st = f.st();
                if f.gone.load(Ordering::Relaxed) {
                    return Err(410); // retained away on an episode change
                }
                if let Some(s) = st.size {
                    return Ok(s);
                }
                if let Some(s) = st.status_since(t0) {
                    return Err(s);
                }
                if t0.elapsed() > SIZE_WAIT {
                    return Err(st.fail.as_ref().map_or(504, |(e, _)| e.code()));
                }
                self.ensure(f, &mut st, b, hi, role)
            };
            abort_all(abort);
            let _ = tokio::time::timeout(Duration::from_secs(1), rx.changed()).await;
        }
    }

    /// Streams [pos, end] to mpv in chunks: from the block file when done, from the `.part` as far as written,
    /// otherwise schedules a download and waits.
    async fn read_loop(
        self,
        f: Arc<File>,
        mut pos: u64,
        end: u64,
        role: Role,
        tx: mpsc::Sender<Bytes>,
    ) {
        let t0 = Instant::now();
        let mut rx = f.changed.subscribe();
        let mut open: Option<(PathBuf, tokio::fs::File, u64)> = None;
        // Checked every round, not only while waiting: a block that keeps failing to read must not outlive mpv
        while pos <= end && !tx.is_closed() {
            let b = (pos / BLOCK) as usize;
            let off = pos % BLOCK;
            let (path, avail, hit) = match self.locate(&f, pos, role, t0) {
                Next::Gone => break,
                Next::Wait(abort) => {
                    abort_all(abort);
                    let _ = tokio::time::timeout(Duration::from_secs(1), rx.changed()).await;
                    continue;
                }
                Next::Read(p, avail, hit) => (p, avail, hit),
            };
            let n = (avail - off).min(READ_CHUNK).min(end - pos + 1);
            if open.as_ref().is_none_or(|(p, ..)| *p != path) {
                match tokio::fs::File::open(&path).await {
                    Ok(h) => open = Some((path.clone(), h, u64::MAX)),
                    Err(_) => {
                        // The `.part` was just renamed, or the block deleted: look again
                        open = None;
                        if hit {
                            f.repair(&self, b);
                            // Still done means the file is there but unreadable (locked, I/O error): don't spin
                            tokio::time::sleep(Duration::from_millis(100)).await;
                        } else {
                            tokio::task::yield_now().await;
                        }
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
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
            *at = off + n;
            if tx.send(Bytes::from(buf)).await.is_err() {
                break; // mpv hung up (a seek closes the old connection)
            }
            pos += n;
        }
    }

    /// What a reader at `pos` does next; a main-player read also records its position.
    fn locate(&self, f: &Arc<File>, pos: u64, role: Role, t0: Instant) -> Next {
        let b = (pos / BLOCK) as usize;
        let off = pos % BLOCK;
        let mut st = f.st();
        if role == Role::Main {
            st.last_read = pos;
        }
        if f.gone.load(Ordering::Relaxed) {
            Next::Gone // retained away: mpv is playing something else by now
        } else if st.done.get(b).copied().unwrap_or(false) {
            st.touched[b] = self.tick.fetch_add(1, Ordering::Relaxed);
            let len = block_len(st.size.unwrap_or(0), b);
            Next::Read(f.block_path(b), len, true)
        } else if let Some(p) = st.writing.get(&b).filter(|p| p.written > off) {
            Next::Read(p.path.clone(), p.written, false)
        } else if st.status_since(t0).is_some() && !st.busy() {
            Next::Gone // upstream refused since: hang up, mpv's reconnect gets the code
        } else {
            Next::Wait(self.ensure(f, &mut st, b, wanted(pos).1, role))
        }
    }

    /// A reader needs block b (a preview [b, hi]): wait if it is done or coming; else schedule a download by role.
    /// Returns superseded tasks for the caller to abort outside the lock.
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
        if near || st.previewing(b) {
            return Vec::new();
        }
        let id = self.ids.fetch_add(1, Ordering::Relaxed);
        // A young main task and a read elsewhere (two connections, e.g. mkv header then index): a two-block short task, no tug of war
        let young = st
            .main
            .as_ref()
            .is_some_and(|m| m.since.elapsed() < Duration::from_secs(2));
        let (role, hi) = match role {
            Role::Main if young => (Role::Main, b + 1),
            Role::Main => {
                let task = crate::rt::spawn(self.clone().main_task(f.clone(), b, id));
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
        // Previews only care about the latest position: when full, cancel the oldest (the main player's short tasks don't count)
        let side = |p: &Preview| p.role != Role::Main;
        if st.previews.iter().filter(|p| side(p)).count() >= PREVIEWS
            && let Some(k) = st.previews.iter().position(side)
        {
            old.push(st.previews.remove(k).t);
        }
        let task = crate::rt::spawn(self.clone().preview_task(f.clone(), b, hi, id));
        st.previews.push(Preview {
            t: Task { id, task },
            blocks: b..=hi,
            role,
        });
        old
    }

    async fn main_task(self, f: Arc<File>, mut b: usize, id: u64) {
        let _g = TaskGuard { f: f.clone(), id };
        let mine = || {
            f.st().main.as_ref().is_some_and(|m| m.t.id == id) && !f.gone.load(Ordering::Relaxed)
        };
        let mut rx = f.changed.subscribe();
        loop {
            let r = async {
                let o = self.open(&f, b as u64 * BLOCK, None).await?;
                let n = f.learn(&self, o.total);
                self.pump(&f, o, b, n, Some(id)).await
            }
            .await;
            match r {
                Ok(Stop::Ahead(at)) => {
                    // Resume once the reader is within half the window: about 8 blocks per request, not one request per block
                    loop {
                        if !mine() {
                            return;
                        }
                        let rb = f.st().read_block();
                        if !self.limited() || at <= rb + AHEAD as usize / 2 {
                            break;
                        }
                        let _ = tokio::time::timeout(Duration::from_secs(1), rx.changed()).await;
                    }
                }
                Ok(Stop::Hit) => {}
                Err(e) if e.transient() => {
                    // Network error / 5xx: retry after the readers' hold-off (no growing backoff: a reader near this task
                    // waits for it, so it must not sleep longer than the reader was told); whole-file mode never stalls
                    tokio::time::sleep(f.failed(e)).await;
                    if !mine() {
                        return;
                    }
                }
                Err(e) => {
                    f.failed(e);
                    return;
                }
            }
            // Hit a done block, reached the end, or retrying: continue at the next missing block after the read position
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

    async fn preview_task(self, f: Arc<File>, lo: usize, hi: usize, id: u64) {
        let _g = TaskGuard { f: f.clone(), id };
        let end = (hi as u64 + 1) * BLOCK;
        let end = f.st().size.map_or(end, |s| end.min(s)) - 1;
        let r = async {
            let o = self.open(&f, lo as u64 * BLOCK, Some(end)).await?;
            let n = f.learn(&self, o.total);
            self.pump(&f, o, lo, (hi + 1).min(n), None).await
        }
        .await;
        if let Err(e) = r {
            f.failed(e);
        }
    }

    /// The main task's next start: the first missing block (nobody writing or previewing it) from the read position,
    /// wrapping at the end; ahead-only looks just [`AHEAD`] blocks forward.
    fn next_missing(&self, f: &File) -> Option<usize> {
        let st = f.st();
        let n = st.done.len();
        let rb = st.read_block().min(n.saturating_sub(1));
        let free = |b: &usize| !st.has(*b) && !st.previewing(*b);
        if self.limited() {
            return (rb..(rb + AHEAD as usize + 1).min(n)).find(free);
        }
        (rb..n).chain(0..rb).find(free)
    }

    /// Ahead-only caching: whole-file caching is off in settings, or the cap is reached.
    fn limited(&self) -> bool {
        !self.whole.load(Ordering::Relaxed)
            || self.total.load(Ordering::Relaxed) + AHEAD * BLOCK > self.cap.load(Ordering::Relaxed)
    }

    /// Requests [start, end] (None = to the end) with hedging: no data in 3 s starts another; the first with data wins.
    async fn open(&self, f: &File, start: u64, end: Option<u64>) -> Result<Opened, Fail> {
        let deadline = tokio::time::Instant::now() + OPEN_LIMIT;
        let mut a: Attempt = Box::pin(self.attempt(&f.src, start, end));
        let mut b: Option<Attempt> = None;
        let mut step = 0;
        let mut next = tokio::time::Instant::now() + Duration::from_secs(HEDGE[0]);
        let r = loop {
            tokio::select! {
                r = &mut a => match r {
                    Err(e) if e.transient() && b.is_some() => {
                        a = b.take().expect("the guard checked b is Some");
                    }
                    r => break r,
                },
                r = maybe(&mut b) => match r {
                    Err(e) if e.transient() => b = None,
                    r => break r,
                },
                _ = tokio::time::sleep_until(deadline) => break Err(Fail::Timeout),
                _ = tokio::time::sleep_until(next) => {
                    if let Some(x) = b.take() {
                        a = x; // drop the older one
                    }
                    b = Some(Box::pin(self.attempt(&f.src, start, end)));
                    step += 1;
                    next += Duration::from_secs(HEDGE[step.min(HEDGE.len() - 1)]);
                }
            }
        };
        if let Ok(o) = &r {
            self.up_bytes
                .fetch_add(o.first.len() as u64, Ordering::Relaxed);
            f.up.fetch_add(o.first.len() as u64, Ordering::Relaxed);
        }
        r
    }

    /// One request: send, follow redirects, check it is the asked range, wait for the first body chunk.
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
                // shortcut: upstreams without Range support are not cached (Emby has it); to support one, read from 0 and skip
                log::warn!("上游没按 Range 回数据（{st}）");
                return Err(Fail::Status(502));
            }
            // A broken playback costs more than a retry: every 5xx (500, Cloudflare 52x) is transient, unlike crate::status
            _ if st >= 500 || crate::status::classify(st) == crate::status::Class::Busy => {
                return Err(Fail::Busy(st, crate::status::retry_after(resp.headers())));
            }
            _ => return Err(Fail::Status(st)),
        };
        let first = resp
            .chunk()
            .await
            .map_err(|e| Fail::Net(e.without_url().to_string()))?
            .unwrap_or_default();
        Ok(Opened { resp, first, total })
    }

    /// Sends a request following redirects itself (at most 5): the token only goes to the API root's origin.
    /// Tries the last redirect target first; on failure (network error or non-2xx) once more from the original URL.
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
                        Err(e) => {
                            let connect = e.is_connect();
                            let m = e.without_url().to_string();
                            break 'hops Err(if connect {
                                Fail::Connect(m)
                            } else {
                                Fail::Net(m)
                            });
                        }
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
                Ok(resp) => last = Fail::Busy(resp.status().as_u16(), None),
                Err(e) => last = e,
            }
            lock(src).resolved = None;
        }
        Err(last)
    }

    /// Writes the response block by block from `from` up to `to` (exclusive): claim, write, rename;
    /// stops at a block that is done or being written.
    async fn pump(
        &self,
        f: &Arc<File>,
        o: Opened,
        from: usize,
        to: usize,
        main: Option<u64>,
    ) -> Result<Stop, Fail> {
        let Opened {
            mut resp,
            first: mut pending,
            ..
        } = o;
        let size = f.st().size.unwrap_or(0);
        let io = |e: std::io::Error| Fail::Net(format!("写缓存失败：{e}"));
        for b in from..to {
            // Ahead-only: release the connection AHEAD blocks past the reader; an idle one dies to the 10 s read timeout or the relay's
            if main.is_some() && self.limited() && b > f.st().read_block() + AHEAD as usize {
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
                    self.up_bytes
                        .fetch_add(pending.len() as u64, Ordering::Relaxed);
                    f.up.fetch_add(pending.len() as u64, Ordering::Relaxed);
                }
                let part = pending.split_to(pending.len().min((len - got) as usize));
                // shortcut: sync write of tens of KB into the page cache (microseconds); move to spawn_blocking if it ever shows up
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

    /// Claims block b for writing; None if it is done, being written, or this main task was replaced. Over the cap it
    /// first evicts this file's least recently used blocks; if none can go it writes anyway (a reader waits for it, and
    /// refusing would make the task reopen at once and spin): the excess is a few blocks near the reader.
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
            // never evict around the read position
            let rb = st.read_block();
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
    /// Upstream told the total size: recorded the first time (meta.json); if it differs from the cached one
    /// (file replaced) all old blocks go. Returns the block count.
    fn learn(&self, s: &Inner, total: u64) -> usize {
        let mut st = self.st();
        st.fail = None;
        st.retry_at = None;
        if st.size == Some(total) {
            return st.done.len();
        }
        if let Some(old) = st.size {
            log::info!("缓存的文件大小变了（{old} → {total}），重新缓存");
            for (b, &done) in st.done.iter().enumerate() {
                if done {
                    let _ = std::fs::remove_file(self.block_path(b));
                    sub_total(&s.total, block_len(old, b));
                }
            }
        }
        let n = blocks(total);
        st.size = Some(total);
        st.done = vec![false; n];
        st.touched = vec![0; n];
        // Temp file + rename: a torn meta.json would make every block in the dir look unknown and get deleted.
        let tmp = self.dir.join("meta.json.tmp");
        if std::fs::write(&tmp, json!({ "size": total }).to_string()).is_ok() {
            let _ = std::fs::rename(&tmp, self.dir.join("meta.json"));
        }
        drop(st);
        self.bump();
        n
    }

    /// Records a failure; returns how long readers hold off (the main task waits exactly as long).
    fn failed(&self, e: Fail) -> Duration {
        let why = match &e {
            Fail::Status(s) | Fail::Busy(s, _) => format!("HTTP {s}"),
            Fail::Net(m) | Fail::Connect(m) => m.clone(),
            Fail::Timeout => format!("{} 秒没有数据", OPEN_LIMIT.as_secs()),
        };
        let back = match e {
            Fail::Status(_) => Duration::from_secs(3),
            Fail::Busy(_, wait) => crate::status::backoff(1, wait).max(Duration::from_secs(1)),
            _ => Duration::from_secs(1),
        };
        let mut st = self.st();
        // Log only the first of a run of failures (reset on success): a long outage retries every second.
        if st.fail.is_none() {
            log::warn!("缓存代理向上游取数据失败：{why}");
        }
        let now = Instant::now();
        st.retry_at = Some(now + back);
        st.fail = Some((e, now));
        drop(st);
        self.bump();
        back
    }

    /// A block marked done but missing or cut short on disk (changed from outside) goes back to not done;
    /// the next write renames over it.
    fn repair(&self, s: &Inner, b: usize) {
        let mut st = self.st();
        // Checked under `done`: the size may have shrunk since the reader saw the block
        if !st.done.get(b).copied().unwrap_or(false) {
            return;
        }
        let len = block_len(st.size.unwrap_or(0), b);
        if std::fs::metadata(self.block_path(b)).map(|m| m.len()).ok() != Some(len) {
            st.done[b] = false;
            sub_total(&s.total, len);
        }
    }
}

impl Stream {
    async fn serve_sub(&self, s: &SubSrc, head: bool) -> Response<Body> {
        let path = self.subs.join(&s.file);
        let bytes = match crate::images::cached(&path).await {
            Some(b) => Bytes::from(b),
            None => {
                // mpv asks for a subtitle once, at load: without it the episode has none, so retry per crate::status
                let mut n = 0;
                let resp = loop {
                    let (code, wait) = match self.send(&s.src, None).await {
                        Ok(r) if r.status().is_success() => break r,
                        Ok(r) => (r.status().as_u16(), crate::status::retry_after(r.headers())),
                        Err(e @ Fail::Connect(_)) => (e.code(), None),
                        Err(e) => return status(e.code()), // timeout, bad certificate…: a retry fails alike
                    };
                    if n >= crate::status::RETRIES || !crate::status::retry_code(code, true) {
                        return status(code);
                    }
                    tokio::time::sleep(crate::status::backoff(n, wait)).await;
                    n += 1;
                };
                let Ok(b) = resp.bytes().await else {
                    return status(502);
                };
                // Unique: the same subtitle can be fetched twice at once (registered under two keys)
                let tmp = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4().simple()));
                if tokio::fs::write(&tmp, &b).await.is_ok()
                    && tokio::fs::rename(&tmp, &path).await.is_ok()
                {
                    let dir = self.subs.clone();
                    crate::rt::spawn_blocking(move || crate::images::trim_dir(&dir, SUBS_CAP));
                } else {
                    let _ = tokio::fs::remove_file(&tmp).await;
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
            HeaderValue::from(bytes.len()),
        );
        r
    }
}

fn abort_all(tasks: impl IntoIterator<Item = Task>) {
    for t in tasks {
        t.task.abort();
    }
}

/// Total bytes of the files in `dir`, recursively.
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

/// Free space on the disk holding `dir`.
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
        // An end before the start is invalid: ignored (whole file), never a negative length
        assert_eq!(parse_range("bytes=5-3"), None);
        assert_eq!(parse_range("bytes=5-5"), Some(Range::From(5, Some(5))));
        assert_eq!(resolve(Some(Range::From(10, None)), 100), Some((10, 99)));
        assert_eq!(
            resolve(Some(Range::From(10, Some(500))), 100),
            Some((10, 99))
        );
        assert_eq!(resolve(Some(Range::Suffix(5)), 100), Some((95, 99)));
        assert_eq!(resolve(Some(Range::From(100, None)), 100), None);
        assert_eq!(resolve(None, 100), Some((0, 99)));
        // Empty file, or an empty suffix: nothing to send (416)
        assert_eq!(resolve(None, 0), None);
        assert_eq!(resolve(Some(Range::Suffix(0)), 100), None);
        assert_eq!(resolve(Some(Range::Suffix(5)), 0), None);
        assert_eq!(resolve(Some(Range::Suffix(500)), 100), Some((0, 99)));
        // In the last MiB of a block, the next one too
        let mib = 1 << 20;
        assert_eq!(wanted(BLOCK - mib), (0, 0));
        assert_eq!(wanted(BLOCK - mib + 1), (0, 1));
        assert_eq!(wanted(BLOCK + 5), (1, 1));
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

    /// A logged request: path, Range, whether the token was sent, arrival time.
    type Logged = (String, String, bool, Instant);

    /// Test upstream: a Range server whose behaviour depends on the path; logs each request (path, Range, token sent).
    struct Upstream {
        base: String,
        other: String,
        log: Arc<Mutex<Vec<Logged>>>,
        data: Arc<Vec<u8>>,
        /// Connections currently sending a body.
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
        log: Arc<Mutex<Vec<Logged>>>,
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
                lock(&log).push((path.clone(), range.clone(), token, Instant::now()));
                let cut = |log: &Mutex<Vec<Logged>>| {
                    lock(log).push((format!("cut {path}"), String::new(), false, Instant::now()));
                };
                if path.starts_with("/403") {
                    let _ = s.write_all(b"HTTP/1.1 403 Forbidden\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").await;
                    return;
                }
                if let (true, Some(to)) = (
                    path.starts_with("/redir") || path.starts_with("/hop"),
                    &redirect,
                ) {
                    let target = if path.starts_with("/hop") {
                        "once"
                    } else {
                        "f"
                    };
                    let r = format!(
                        "HTTP/1.1 302 Found\r\nlocation: {to}/{target}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                    );
                    let _ = s.write_all(r.as_bytes()).await;
                    return;
                }
                // First two requests: 502 (flaky relay)
                if path.starts_with("/flaky") && flaky.fetch_add(1, Ordering::SeqCst) < 2 {
                    let _ = s.write_all(b"HTTP/1.1 502 Bad Gateway\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").await;
                    cut(&log);
                    return;
                }
                // Second request: 429 with Retry-After (rate-limiting relay)
                if path.starts_with("/busy") && flaky.fetch_add(1, Ordering::SeqCst) == 1 {
                    let _ = s.write_all(b"HTTP/1.1 429 Too Many Requests\r\nretry-after: 2\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").await;
                    cut(&log);
                    return;
                }
                // Requests 2–5: 503 (an outage with no data at all)
                if path.starts_with("/down")
                    && (1..=4).contains(&flaky.fetch_add(1, Ordering::SeqCst))
                {
                    let _ = s.write_all(b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").await;
                    cut(&log);
                    return;
                }
                // Second request: 403 (a redirect target that expired)
                if path.starts_with("/once") && flaky.fetch_add(1, Ordering::SeqCst) == 1 {
                    let _ = s.write_all(b"HTTP/1.1 403 Forbidden\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").await;
                    return;
                }
                // Every request after the first: 403 (a signed URL that expired)
                if path.starts_with("/expire") && flaky.fetch_add(1, Ordering::SeqCst) >= 1 {
                    let _ = s.write_all(b"HTTP/1.1 403 Forbidden\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").await;
                    return;
                }
                // Always 502 (an upstream that never recovers)
                if path.starts_with("/always502") {
                    let _ = s.write_all(b"HTTP/1.1 502 Bad Gateway\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").await;
                    return;
                }
                if path.starts_with("/sub") {
                    let _ = s.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 5\r\nconnection: close\r\n\r\nhello").await;
                    return;
                }
                let n = data.len() as u64;
                // Upstreams ignoring Range: the whole file as 200, or as 206 starting at 0
                let ignore = path.starts_with("/norange") || path.starts_with("/badrange");
                let (a, b) = match parse_range(&range).and_then(|r| resolve(Some(r), n)) {
                    Some(x) if !ignore => x,
                    _ => (0, n - 1),
                };
                let line = if path.starts_with("/norange") {
                    "200 OK"
                } else {
                    "206 Partial Content"
                };
                let h = format!(
                    "HTTP/1.1 {line}\r\ncontent-range: bytes {a}-{b}/{n}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    b - a + 1
                );
                if s.write_all(h.as_bytes()).await.is_err() {
                    return;
                }
                // First request: headers but no body (cold read stalls upstream)
                if path.starts_with("/stall") && stalls.fetch_add(1, Ordering::SeqCst) == 0 {
                    tokio::time::sleep(Duration::from_secs(20)).await;
                    return;
                }
                active.fetch_add(1, Ordering::SeqCst);
                let _active = Active(active);
                let mut sent = 0u64;
                for chunk in data[a as usize..=b as usize].chunks(64 << 10) {
                    // Slow upstream: about 1.3 MB/s
                    if path.contains("slow") {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                    // First request: cut after 6 MiB (mid-stream drop)
                    if path.starts_with("/cut")
                        && sent > 6 << 20
                        && cuts.fetch_add(1, Ordering::SeqCst) == 0
                    {
                        return;
                    }
                    // Every request: cut after a bit over 2 blocks (a relay that keeps dropping)
                    if path.starts_with("/drops") && sent > 9 << 20 {
                        cut(&log);
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

    /// Retry delays: from each `cut <path>` entry (the upstream dropped or refused) to the next request for `path`.
    /// Timing the gaps rather than the whole read keeps data transfer out of it: under load moving tens of MB
    /// in a debug build takes seconds (measured: 9.9 s against an 8 s bound for about 4 s of waits).
    fn retry_delays(u: &Upstream, path: &str) -> Vec<Duration> {
        let cut = format!("cut {path}");
        let mut end = None;
        let mut out = Vec::new();
        for (p, _, _, at) in lock(&u.log).iter() {
            if *p == cut {
                end = Some(*at);
            } else if p.starts_with(path)
                && let Some(t) = end.take()
            {
                out.push(*at - t);
            }
        }
        out
    }

    fn reqs(u: &Upstream, path: &str) -> Vec<Logged> {
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

    /// Random and concurrent ranges match the source byte for byte; once complete, a new proxy on the same dir makes no upstream requests.
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
            assert!(body == u.data[a as usize..=b as usize], "{a}-{b} differs");
        }
        let (st, body) = get(&url, None, false).await;
        assert_eq!((st, body.len() as u64), (200, n));
        assert!(body == *u.data);
        // Whole-file mode: the background fills every block
        let f = lock(&s.files).get("srv_1_a").unwrap().clone();
        assert!(
            until(|| f.st().done.iter().all(|d| *d), 20).await,
            "whole file not completed"
        );
        assert!(until(|| !f.st().busy(), 5).await);
        let before = reqs(&u, "/f").len();
        // A new proxy (next launch): everything is local
        let s2 = Stream::start(&dir, true).unwrap();
        let url2 = s2.video("srv_1_a", &format!("{}/f", u.base), &auth(&u));
        let (_, body) = get(&url2, Some("bytes=5000000-"), false).await;
        assert!(body == u.data[5_000_000..]);
        assert_eq!(
            reqs(&u, "/f").len(),
            before,
            "cached data must not be fetched again"
        );
        drop(f);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A body-less upstream gets hedged after 3 s and the first with data wins; a mid-stream cut resumes with identical content.
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
        // Upper bounds leave seconds of slack for loaded machines and CI; they only need to beat the 20 s stall
        assert!(
            took >= Duration::from_secs(3) && took < Duration::from_secs(10),
            "{took:?}"
        );
        assert!(reqs(&u, "/stall").len() >= 2);

        let url = s.video("srv_3_a", &format!("{}/cut", u.base), &auth(&u));
        let (st, body) = get(&url, None, false).await;
        assert_eq!(st, 200);
        assert!(body == *u.data, "content differs after resuming");
        assert!(reqs(&u, "/cut").len() >= 2);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Previews fetch 1–2 blocks and never refetch; a far seek leaves just one main task.
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
        assert_eq!(
            reqs(&u, "/f").len(),
            n,
            "a fetched position must not be requested again"
        );

        // Main player: read at the start, wait 2 s (no longer a young main task), then seek to block 30
        let _ = get(&url, Some("bytes=0-99"), false).await;
        tokio::time::sleep(Duration::from_secs(2)).await;
        let _ = get(
            &url,
            Some(&format!("bytes={}-{}", 30 * BLOCK, 30 * BLOCK + 99)),
            false,
        )
        .await;
        tokio::time::sleep(Duration::from_secs(2)).await;
        // Ahead-only: the first main task stops AHEAD blocks in and, once replaced, writes no more; the new one writes 30..=30 + AHEAD
        let done: Vec<usize> = {
            let st = f.st();
            (0..st.done.len()).filter(|&b| st.done[b]).collect()
        };
        let want: Vec<usize> = (0..=AHEAD as usize)
            .chain([20])
            .chain(30..=30 + AHEAD as usize)
            .collect();
        assert_eq!(done, want);
        // Seeking back behind the main task starts a new one there instead of waiting for it
        let at = 25 * BLOCK;
        let r = tokio::time::timeout(
            Duration::from_secs(5),
            get(&url, Some(&format!("bytes={at}-{}", at + 99)), false),
        )
        .await;
        assert_eq!(r.map(|r| r.0).ok(), Some(206), "seek back stalled");
        drop(f);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A 403 reaches mpv as is; cross-host redirects carry no token, same-origin ones do; subtitles are fetched once.
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
        assert!(first[0].2, "same origin must carry the token");
        let moved: Vec<_> = lock(&u.log)
            .iter()
            .filter(|r| r.0 == "/f")
            .cloned()
            .collect();
        assert!(
            !moved.is_empty() && moved.iter().all(|r| !r.2),
            "no token for another host: {moved:?}"
        );
        let _ = &u.other;

        let sub = s.sub("srv_7_a_3.srt", &format!("{}/sub", u.base), &auth(&u));
        assert!(sub.ends_with("/srv_7_a_3.srt"));
        assert_eq!(get(&sub, None, false).await.1, b"hello");
        assert_eq!(get(&sub, None, false).await.1, b"hello");
        assert_eq!(
            reqs(&u, "/sub").len(),
            1,
            "second subtitle read must be local"
        );

        // The name carries the server's codec string: it must not lead out of the subtitle cache
        for evil in ["../escaped.srt", "..", "a/b\\c?.srt"] {
            let sub = s.sub(evil, &format!("{}/sub", u.base), &auth(&u));
            // By key: a client would collapse the `..` in the URL itself
            let key = sub.split('/').nth(4).unwrap();
            let sub = format!("http://127.0.0.1:{}/s/{key}/x", s.port);
            assert_eq!(
                get(&sub, None, false).await,
                (200, b"hello".to_vec()),
                "{evil}"
            );
        }
        assert!(!dir.join("escaped.srt").exists(), "wrote outside the cache");
        let names: Vec<_> = std::fs::read_dir(dir.join("subs"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert!(names.contains(&"..-escaped.srt".to_owned()), "{names:?}");
        assert!(names.contains(&"a-b-c-.srt".to_owned()), "{names:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Request handling around the reads: methods, key kinds, a read across a block boundary, HEAD.
    #[tokio::test(flavor = "multi_thread")]
    async fn requests_and_block_edges() {
        let u = upstream((3 * BLOCK) as usize).await;
        let dir = tmp("edges");
        let s = Stream::start(&dir, true).unwrap();
        let url = s.video("srv_30_a", &format!("{}/f", u.base), &auth(&u));
        let sub = s.sub("srv_30_a_1.srt", &format!("{}/sub", u.base), &auth(&u));
        let c = reqwest::Client::builder().no_proxy().build().unwrap();
        let post = c.post(&url).send().await.unwrap();
        assert_eq!(post.status().as_u16(), 405);
        let head = c.head(&url).send().await.unwrap();
        assert_eq!(head.status().as_u16(), 200);
        assert_eq!(
            head.headers()["content-length"],
            (3 * BLOCK).to_string().as_str()
        );
        // A key only answers under its own kind
        let vkey = url.split('/').nth(4).unwrap();
        let skey = sub.split('/').nth(4).unwrap();
        let wrong = |k: &str, key: &str| format!("http://127.0.0.1:{}/{k}/{key}/x", s.port);
        assert_eq!(get(&wrong("s", vkey), None, false).await.0, 404);
        assert_eq!(
            get(&wrong("v", skey), Some("bytes=0-9"), false).await.0,
            404
        );
        // Across a block boundary, starting near the end of block 0: the chunk stops at the block's end
        let a = BLOCK - 1000;
        let (st, body) = get(&url, Some(&format!("bytes={a}-{}", a + 1999)), false).await;
        assert_eq!((st, body.len()), (206, 2000));
        assert!(body == u.data[a as usize..a as usize + 2000]);
        let f = lock(&s.files).get("srv_30_a").unwrap().clone();
        assert!(until(|| f.st().done.iter().all(|&d| d), 10).await);
        let (_, body) = get(&url, Some(&format!("bytes={a}-{}", a + 1999)), false).await;
        assert!(body == u.data[a as usize..a as usize + 2000], "from disk");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A preview in the last MiB of a block fetches the next block too (decoding runs past the position).
    #[tokio::test(flavor = "multi_thread")]
    async fn preview_near_block_end_takes_next_block() {
        let u = upstream((10 * BLOCK) as usize).await;
        let dir = tmp("preview-end");
        let s = Stream::start(&dir, false).unwrap();
        let url = s.video("srv_31_a", &format!("{}/f", u.base), &auth(&u));
        let at = 5 * BLOCK - 1000;
        let (st, body) = get(&url, Some(&format!("bytes={at}-{}", at + 99)), true).await;
        assert!(st == 206 && body == u.data[at as usize..at as usize + 100]);
        let f = lock(&s.files).get("srv_31_a").unwrap().clone();
        assert!(until(|| !f.st().busy(), 5).await);
        let r = reqs(&u, "/f");
        assert_eq!(r.len(), 1, "{r:?}");
        assert_eq!(r[0].1, format!("bytes={}-{}", 4 * BLOCK, 6 * BLOCK - 1));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Registering a cache name again with a new upstream URL drops the old one and its redirect target.
    #[tokio::test(flavor = "multi_thread")]
    async fn reregistering_moves_to_new_url() {
        let u = upstream((3 * BLOCK) as usize).await;
        let dir = tmp("rereg");
        let s = Stream::start(&dir, false).unwrap();
        let url = s.video("srv_32_a", &format!("{}/redir", u.base), &auth(&u));
        let (st, _) = get(&url, Some("bytes=0-9"), true).await;
        assert_eq!(st, 206);
        let url = s.video("srv_32_a", &format!("{}/g", u.base), &auth(&u));
        let at = 2 * BLOCK;
        let (st, _) = get(&url, Some(&format!("bytes={at}-{}", at + 9)), true).await;
        assert_eq!(st, 206);
        assert_eq!(reqs(&u, "/g").len(), 1, "still reading the old address");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Subtitles: transient failures are retried, a definitive refusal is passed on after one request.
    #[tokio::test(flavor = "multi_thread")]
    async fn subtitle_retries_transient_only() {
        let u = upstream(1000).await;
        let dir = tmp("sub-retry");
        let s = Stream::start(&dir, false).unwrap();
        let sub = s.sub("srv_33_a_1.srt", &format!("{}/flaky", u.base), &auth(&u));
        let (st, body) = get(&sub, None, false).await;
        assert_eq!((st, body.as_slice()), (200, &u.data[..]));
        assert_eq!(reqs(&u, "/flaky").len(), 3, "two 502s, then data");
        let sub = s.sub("srv_33_a_2.srt", &format!("{}/403", u.base), &auth(&u));
        assert_eq!(get(&sub, None, false).await.0, 403);
        assert_eq!(reqs(&u, "/403").len(), 1, "a 403 must not be retried");
        // Never recovers: the first try and crate::status::RETRIES more, then the code goes to mpv
        let sub = s.sub(
            "srv_33_a_3.srt",
            &format!("{}/always502", u.base),
            &auth(&u),
        );
        let got = tokio::time::timeout(Duration::from_secs(15), get(&sub, None, false)).await;
        assert_eq!(got.expect("retries without end").0, 502);
        assert_eq!(reqs(&u, "/always502").len(), 1 + crate::status::RETRIES);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A block still being written is served as far as it has arrived, not after the whole block.
    #[tokio::test(flavor = "multi_thread")]
    async fn reads_follow_a_block_being_written() {
        let u = upstream((2 * BLOCK) as usize).await;
        let dir = tmp("partial");
        let s = Stream::start(&dir, false).unwrap();
        let url = s.video("srv_37_a", &format!("{}/slow", u.base), &auth(&u));
        let t0 = Instant::now();
        let (st, body) = get(&url, Some("bytes=0-99"), false).await;
        assert!(st == 206 && body == u.data[..100]);
        // The slow upstream takes about 3 s per block; the first 100 bytes are in the `.part` well before that
        assert!(
            t0.elapsed() < Duration::from_millis(2000),
            "{:?}",
            t0.elapsed()
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A done block cut short on disk (outside tampering) is fetched again instead of re-read forever.
    #[tokio::test(flavor = "multi_thread")]
    async fn truncated_block_is_refetched() {
        let u = upstream((3 * BLOCK) as usize).await;
        let dir = tmp("truncated");
        let s = Stream::start(&dir, false).unwrap();
        let url = s.video("srv_20_a", &format!("{}/f", u.base), &auth(&u));
        assert_eq!(get(&url, Some("bytes=0-9"), false).await.0, 206);
        let f = lock(&s.files).get("srv_20_a").unwrap().clone();
        assert!(until(|| f.st().done.iter().all(|&d| d), 5).await);
        // Block 1, not 0: the repaired block must be the one read, not block 0 by accident
        std::fs::File::options()
            .write(true)
            .open(f.block_path(1))
            .unwrap()
            .set_len(5)
            .unwrap();
        let got = tokio::time::timeout(
            Duration::from_secs(10),
            get(&url, Some(&format!("bytes={BLOCK}-{}", BLOCK + 99)), false),
        )
        .await
        .expect("reader spins on the short block");
        let at = BLOCK as usize;
        assert_eq!((got.0, got.1.as_slice()), (206, &u.data[at..at + 100]));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// retain deletes the other dirs and stops their downloads; at the cap nothing is fetched beyond the window.
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
            "a retained-away video takes its subtitles along"
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

        // Cap of MIN_CAP: even whole-file mode only writes the window ahead
        let s = Stream::start(&dir, true).unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await; // startup computes the cap from the disk; let it finish first
        s.cap.store(MIN_CAP, Ordering::Relaxed);
        let a = s.video("srv_10_a", &format!("{}/f", u.base), &auth(&u));
        let _ = get(&a, Some("bytes=0-9"), false).await;
        let f = lock(&s.files).get("srv_10_a").unwrap().clone();
        tokio::time::sleep(Duration::from_secs(2)).await;
        let n = f.st().done.iter().filter(|d| **d).count() as u64;
        assert!(n <= AHEAD + 2, "still writing past the cap: {n} blocks");
        drop(f);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Ahead-only: after AHEAD blocks the upstream connection is released (an idle one would time out); it resumes once the reader moves half the window.
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
            "upstream connection kept after the window was full"
        );
        assert!(
            f.st().main.is_some(),
            "the main task should still wait for the reader"
        );
        // Read at block 5: 12 blocks short, no resume; at block 10: 7 short (under half of 16), resume up to 26
        let _ = get(
            &url,
            Some(&format!("bytes={}-{}", 5 * BLOCK, 5 * BLOCK + 9)),
            false,
        )
        .await;
        // Reading cached blocks wakes nobody: the parked task re-checks once a second
        tokio::time::sleep(Duration::from_millis(1500)).await;
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
        assert_eq!(
            reqs(&u, "/f").len(),
            2,
            "resuming must open exactly one new request"
        );
        // Turning whole-file caching on resumes the parked main task through to the end
        s.set_whole(true);
        assert!(until(|| done(&f).len() == 60, 10).await, "{:?}", done(&f));
        drop(f);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 5xx is transient: the main task backs off and retries, the reader gets data, mpv never sees the 502.
    #[tokio::test(flavor = "multi_thread")]
    async fn transient_5xx_is_retried() {
        let u = upstream(BLOCK as usize + 100).await;
        let dir = tmp("flaky");
        let s = Stream::start(&dir, false).unwrap();
        let url = s.video("srv_12_a", &format!("{}/flaky", u.base), &auth(&u));
        let (st, body) = get(&url, Some("bytes=0-99"), false).await;
        assert_eq!(st, 206);
        assert!(body == u.data[..100]);
        assert_eq!(reqs(&u, "/flaky").len(), 3);
        let d = retry_delays(&u, "/flaky");
        assert!(
            d.len() == 2 && d.iter().all(|d| *d < Duration::from_secs(3)),
            "{d:?}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Repeated cuts retry within a second each time: the wait must not keep growing (a reader waits on the main task).
    #[tokio::test(flavor = "multi_thread")]
    async fn repeated_cuts_keep_backoff_short() {
        let u = upstream((10 * BLOCK) as usize).await;
        let dir = tmp("drops");
        let s = Stream::start(&dir, false).unwrap();
        let url = s.video("srv_16_a", &format!("{}/drops", u.base), &auth(&u));
        let (st, body) = get(&url, None, false).await;
        assert_eq!(st, 200);
        assert!(body == *u.data, "content differs after cuts");
        assert_eq!(reqs(&u, "/drops").len(), 5);
        // 4 cuts, each retried after about 1 s; a doubling backoff waits 1, 2, 4, 8 s
        let d = retry_delays(&u, "/drops");
        assert!(
            d.len() == 4 && d.iter().all(|d| *d < Duration::from_millis(2500)),
            "{d:?}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// An upstream ignoring Range is refused (502) instead of caching misplaced data; from offset 0 a 200 is fine.
    #[tokio::test(flavor = "multi_thread")]
    async fn upstream_ignoring_range_is_refused() {
        let u = upstream((3 * BLOCK) as usize).await;
        let dir = tmp("norange");
        let s = Stream::start(&dir, false).unwrap();
        let at = 2 * BLOCK;
        for p in ["norange", "badrange"] {
            let url = s.video(
                &format!("srv_24_{p}"),
                &format!("{}/{p}", u.base),
                &auth(&u),
            );
            let (st, _) = get(&url, Some(&format!("bytes={at}-{}", at + 9)), true).await;
            assert_eq!(st, 502, "{p}");
        }
        let url = s.video("srv_24_ok", &format!("{}/norange", u.base), &auth(&u));
        let (st, body) = get(&url, Some("bytes=0-99"), false).await;
        assert_eq!(st, 206);
        assert!(body == u.data[..100]);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A redirect target is reused for later requests; once it fails, the original URL is asked again.
    #[tokio::test(flavor = "multi_thread")]
    async fn redirect_target_is_reused_and_refreshed() {
        let u = upstream((3 * BLOCK) as usize).await;
        let dir = tmp("hop");
        let s = Stream::start(&dir, false).unwrap();
        let url = s.video("srv_25_a", &format!("{}/hop", u.base), &auth(&u));
        let (st, _) = get(&url, Some("bytes=0-9"), true).await;
        assert_eq!(st, 206);
        // Straight to the remembered target, which now answers 403: back to /hop, which redirects anew
        let at = 2 * BLOCK;
        let (st, body) = get(&url, Some(&format!("bytes={at}-{}", at + 9)), true).await;
        assert_eq!(st, 206);
        assert!(body == u.data[at as usize..at as usize + 10]);
        assert_eq!(reqs(&u, "/hop").len(), 2);
        assert_eq!(reqs(&u, "/once").len(), 3);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Over the cap, a claim evicts this file's blocks away from the reader.
    #[tokio::test(flavor = "multi_thread")]
    async fn capacity_evicts_blocks_away_from_reader() {
        let u = upstream((40 * BLOCK) as usize).await;
        let dir = tmp("evict");
        let s = Stream::start(&dir, false).unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await; // startup computes the cap from the disk; let it finish first
        s.cap.store(8 * BLOCK, Ordering::Relaxed);
        let url = s.video("srv_26_a", &format!("{}/f", u.base), &auth(&u));
        let f = lock(&s.files).get("srv_26_a").unwrap().clone();
        let done = || -> Vec<usize> {
            let st = f.st();
            (0..st.done.len()).filter(|&b| st.done[b]).collect()
        };
        let _ = get(&url, Some("bytes=0-9"), false).await;
        // Everything near the reader is kept, so the window ahead is written over the cap
        assert!(
            until(|| done().len() == AHEAD as usize + 1, 5).await,
            "{:?}",
            done()
        );
        let at = 30 * BLOCK;
        let _ = get(&url, Some(&format!("bytes={at}-{}", at + 9)), false).await;
        assert!(
            until(|| done() == (30..40).collect::<Vec<_>>(), 10).await,
            "{:?}",
            done()
        );
        assert_eq!(s.total.load(Ordering::Relaxed), 10 * BLOCK);
        drop(f);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A 4xx after the size is known ends the main task and reaches mpv instead of being retried forever.
    #[tokio::test(flavor = "multi_thread")]
    async fn status_mid_playback_reaches_reader() {
        let u = upstream((3 * BLOCK) as usize).await;
        let dir = tmp("expire");
        let s = Stream::start(&dir, false).unwrap();
        let url = s.video("srv_22_a", &format!("{}/expire", u.base), &auth(&u));
        let at = 2 * BLOCK;
        let (st, _) = get(&url, Some(&format!("bytes={at}-{}", at + 9)), true).await;
        assert_eq!(st, 206);
        // The read in flight ends early; the reconnect gets the 403
        let first =
            tokio::time::timeout(Duration::from_secs(5), get(&url, Some("bytes=0-99"), false))
                .await;
        assert!(first.is_ok(), "reader hung on a definitive error");
        let (st, _) = get(&url, Some("bytes=0-99"), false).await;
        assert_eq!(st, 403);
        assert_eq!(reqs(&u, "/expire").len(), 2, "a 403 must not be retried");
        // Judged by the block asked for: uncached block 1 fails at once, cached block 2 still plays
        let (st, _) = get(&url, Some(&format!("bytes={BLOCK}-{}", BLOCK + 9)), false).await;
        assert_eq!(st, 403);
        let (st, body) = get(&url, Some(&format!("bytes={at}-{}", at + 9)), false).await;
        let a = at as usize;
        assert_eq!((st, body.as_slice()), (206, &u.data[a..a + 10]));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Dispatch rules: a young main task gets a short task beside it instead of being replaced; previews
    /// keep at most PREVIEWS slots, cancelling the oldest, and the main player's short tasks take no slot.
    #[tokio::test(flavor = "multi_thread")]
    async fn ensure_dispatch_rules() {
        let u = upstream((60 * BLOCK) as usize).await;
        let dir = tmp("ensure");
        let s = Stream::start(&dir, true).unwrap();
        let _ = s.video("srv_21_a", &format!("{}/slow", u.base), &auth(&u));
        let f = lock(&s.files).get("srv_21_a").unwrap().clone();
        f.learn(&s, 60 * BLOCK);
        let ensure = |b: usize, role: Role| -> Vec<u64> {
            let mut st = f.st();
            let old = s.ensure(&f, &mut st, b, b, role);
            drop(st);
            let ids = old.iter().map(|t| t.id).collect();
            abort_all(old);
            ids
        };
        let slots = || -> Vec<(usize, usize, Role)> {
            let st = f.st();
            let mut v: Vec<_> = st
                .previews
                .iter()
                .map(|p| (*p.blocks.start(), *p.blocks.end(), p.role))
                .collect();
            v.sort_by_key(|p| p.0);
            v
        };
        assert!(ensure(0, Role::Main).is_empty());
        // Within MAIN_NEAR blocks past the main task's write point: wait for it, nothing new
        assert!(ensure(MAIN_NEAR as usize, Role::Main).is_empty());
        assert!(slots().is_empty(), "{:?}", slots());
        // Young main task: the far read gets blocks 40–41 on the side, the main task stays at 0
        assert!(ensure(40, Role::Main).is_empty());
        assert_eq!(f.st().main.as_ref().map(|m| m.at), Some(0));
        assert!(ensure(10, Role::Preview).is_empty());
        assert!(ensure(20, Role::Preview).is_empty());
        assert_eq!(
            slots(),
            [
                (10, 10, Role::Preview),
                (20, 20, Role::Preview),
                (40, 41, Role::Main)
            ]
        );
        // A third preview cancels the oldest preview, never the main player's short task
        assert_eq!(ensure(30, Role::Preview).len(), 1);
        assert_eq!(
            slots(),
            [
                (20, 20, Role::Preview),
                (30, 30, Role::Preview),
                (40, 41, Role::Main)
            ]
        );
        // Nothing new for blocks already being fetched
        assert!(ensure(41, Role::Preview).is_empty());
        assert_eq!(slots().len(), 3);
        f.stop();
        drop(f);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A failed preview holds off before the next request instead of hammering a flaky upstream.
    #[tokio::test(flavor = "multi_thread")]
    async fn preview_failures_back_off() {
        let u = upstream(BLOCK as usize).await;
        let dir = tmp("flaky-thumb");
        let s = Stream::start(&dir, false).unwrap();
        let url = s.video("srv_20_a", &format!("{}/flaky", u.base), &auth(&u));
        let (st, body) = get(&url, Some("bytes=0-99"), true).await;
        assert_eq!(st, 206);
        assert!(body == u.data[..100]);
        assert_eq!(reqs(&u, "/flaky").len(), 3);
        // Two 502s, each followed by a 1.5 s hold-off (crate::status::backoff)
        let d = retry_delays(&u, "/flaky");
        let ok = |d: &Duration| *d >= Duration::from_millis(1400) && *d < Duration::from_secs(3);
        assert!(d.len() == 2 && d.iter().all(ok), "{d:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Whole-file mode keeps going through cuts with no reader left (the main task retries on its own).
    #[tokio::test(flavor = "multi_thread")]
    async fn whole_mode_survives_cuts_without_reader() {
        let u = upstream((10 * BLOCK) as usize).await;
        let dir = tmp("drops-whole");
        let s = Stream::start(&dir, true).unwrap();
        let url = s.video("srv_18_a", &format!("{}/drops", u.base), &auth(&u));
        let _ = get(&url, Some("bytes=0-99"), false).await;
        let f = lock(&s.files).get("srv_18_a").unwrap().clone();
        assert!(
            until(|| f.st().done.iter().all(|&d| d), 15).await,
            "{:?}",
            f.st().done
        );
        // Every byte received counts, including the partial blocks lost to the cuts
        assert!(s.up_bytes() >= 10 * BLOCK, "{}", s.up_bytes());
        assert_eq!(
            s.file_up_bytes("srv_18_a"),
            s.up_bytes(),
            "the only file has all the bytes"
        );
        assert_eq!(s.file_up_bytes("unknown"), 0);
        drop(f);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A prefetch the upstream refuses asks once and stops.
    #[tokio::test(flavor = "multi_thread")]
    async fn prefetch_gives_up_on_refusal() {
        let u = upstream(BLOCK as usize).await;
        let dir = tmp("prefetch-403");
        let s = Stream::start(&dir, false).unwrap();
        let _ = s.video("srv_34_a", &format!("{}/403", u.base), &auth(&u));
        s.prefetch("srv_34_a");
        // A refused prefetch stops; retrying would ask again once the 3 s hold-off ends
        tokio::time::sleep(Duration::from_millis(4500)).await;
        assert_eq!(reqs(&u, "/403").len(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The main task stops at a block that is already done and restarts at the next missing one (no second download).
    #[tokio::test(flavor = "multi_thread")]
    async fn main_task_skips_done_blocks() {
        let u = upstream((8 * BLOCK) as usize).await;
        let dir = tmp("skip-done");
        let s = Stream::start(&dir, true).unwrap();
        let url = s.video("srv_35_a", &format!("{}/f", u.base), &auth(&u));
        let at = 5 * BLOCK;
        let (st, _) = get(&url, Some(&format!("bytes={at}-{}", at + 9)), true).await;
        assert_eq!(st, 206);
        let f = lock(&s.files).get("srv_35_a").unwrap().clone();
        assert!(until(|| !f.st().busy(), 5).await);
        let _ = get(&url, Some("bytes=0-99"), false).await;
        assert!(until(|| f.st().done.iter().all(|&d| d), 10).await);
        let r: Vec<_> = reqs(&u, "/f").into_iter().map(|r| r.1).collect();
        assert!(r.contains(&format!("bytes={}-", 6 * BLOCK)), "{r:?}");
        drop(f);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A refusal seen by another task does not cut a reader whose data is still coming from a running task.
    #[tokio::test(flavor = "multi_thread")]
    async fn refusal_elsewhere_keeps_running_reads() {
        let u = upstream((4 * BLOCK) as usize).await;
        let dir = tmp("busy-read");
        let s = Stream::start(&dir, false).unwrap();
        // First request streams slowly (about 1.3 MB/s); every later one gets 403
        let url = s.video("srv_36_a", &format!("{}/expire-slow", u.base), &auth(&u));
        let reading = tokio::spawn({
            let url = url.clone();
            async move { get(&url, Some(&format!("bytes=0-{}", 2 * BLOCK - 1)), false).await }
        });
        tokio::time::sleep(Duration::from_secs(1)).await;
        // A preview past MAIN_NEAR gets a task of its own, which the upstream refuses (that reader waits on too)
        let at = 3 * BLOCK;
        let preview = url.clone();
        tokio::spawn(
            async move { get(&preview, Some(&format!("bytes={at}-{}", at + 9)), true).await },
        );
        assert!(until(|| reqs(&u, "/expire").len() >= 2, 5).await);
        let (st, body) = tokio::time::timeout(Duration::from_secs(20), reading)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(st, 206);
        assert!(
            body == u.data[..2 * BLOCK as usize],
            "cut short at {} bytes",
            body.len()
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Prefetch takes the first block and the last two, and nothing else.
    #[tokio::test(flavor = "multi_thread")]
    async fn prefetch_takes_head_and_tail() {
        let u = upstream((10 * BLOCK) as usize).await;
        let dir = tmp("prefetch");
        let s = Stream::start(&dir, true).unwrap();
        let _ = s.video("srv_19_a", &format!("{}/f", u.base), &auth(&u));
        s.prefetch("srv_19_a");
        let f = lock(&s.files).get("srv_19_a").unwrap().clone();
        let done = || -> Vec<usize> {
            let st = f.st();
            (0..st.done.len()).filter(|&b| st.done[b]).collect()
        };
        assert!(until(|| done() == [0, 8, 9], 5).await, "{:?}", done());
        assert!(until(|| !f.st().busy(), 5).await);
        assert_eq!(done(), [0, 8, 9]);
        assert_eq!(reqs(&u, "/f").len(), 2, "{:?}", reqs(&u, "/f"));
        // Prefetching again finds everything cached
        s.prefetch("srv_19_a");
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(reqs(&u, "/f").len(), 2, "{:?}", reqs(&u, "/f"));
        drop(f);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// An outage without any progress does not stretch the main task's retries either: once upstream is back,
    /// a waiting reader gets data within the hold-off it was told (1.5 s for a 503).
    #[tokio::test(flavor = "multi_thread")]
    async fn outage_retries_keep_pace_with_readers() {
        let u = upstream((3 * BLOCK) as usize).await;
        let dir = tmp("down");
        let s = Stream::start(&dir, false).unwrap();
        let url = s.video("srv_27_a", &format!("{}/down", u.base), &auth(&u));
        let at = 2 * BLOCK;
        let (st, _) = get(&url, Some(&format!("bytes={at}-{}", at + 9)), true).await;
        assert_eq!(st, 206);
        let (st, body) = get(&url, Some("bytes=0-99"), false).await;
        assert_eq!(st, 206);
        assert!(body == u.data[..100]);
        assert_eq!(reqs(&u, "/down").len(), 6);
        // 4 × 1.5 s; a doubling backoff waits 1.5, 2, 4, 8 s
        let d = retry_delays(&u, "/down");
        assert!(
            d.len() == 4 && d.iter().all(|d| *d < Duration::from_secs(3)),
            "{d:?}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The main task honours Retry-After instead of retrying after its own 1 s.
    #[tokio::test(flavor = "multi_thread")]
    async fn main_task_waits_retry_after() {
        let u = upstream((3 * BLOCK) as usize).await;
        let dir = tmp("busy");
        let s = Stream::start(&dir, false).unwrap();
        let url = s.video("srv_17_a", &format!("{}/busy", u.base), &auth(&u));
        // A preview learns the size first, so the main task meets the 429 and keeps going
        let at = 2 * BLOCK;
        let (st, _) = get(&url, Some(&format!("bytes={at}-{}", at + 9)), true).await;
        assert_eq!(st, 206);
        let (st, body) = get(&url, Some("bytes=0-99"), false).await;
        assert_eq!(st, 206);
        assert!(body == u.data[..100]);
        assert_eq!(reqs(&u, "/busy").len(), 3);
        // Retry-After is 2 s: not the own 1 s retry, nor the 5 s cap
        let d = retry_delays(&u, "/busy");
        let ok = |d: &Duration| *d >= Duration::from_millis(1900) && *d < Duration::from_secs(4);
        assert!(d.len() == 1 && d.iter().all(ok), "{d:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Retaining away a file being read drops the reader and schedules nothing more; idle at playback end stops whole-file downloads.
    #[tokio::test(flavor = "multi_thread")]
    async fn retain_and_idle_stop_downloads() {
        let u = upstream((20 * BLOCK) as usize).await;
        let dir = tmp("stop");
        let s = Stream::start(&dir, true).unwrap();
        let url = s.video("srv_13_a", &format!("{}/slow", u.base), &auth(&u));
        let reading = tokio::spawn(async move { get(&url, None, false).await });
        // Wait until the reader really receives data (size known, block 0 partly written): this tests removal mid-read, not while waiting for the size
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
            .expect("reader not dropped after retain")
            .unwrap();
        assert!(body.len() < u.data.len());
        assert!(until(|| u.active.load(Ordering::SeqCst) == 0, 3).await);
        let n = reqs(&u, "/slow").len();
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(
            reqs(&u, "/slow").len(),
            n,
            "a removed file still fetches from upstream"
        );

        let url = s.video("srv_14_a", &format!("{}/slow", u.base), &auth(&u));
        let _ = get(&url, Some("bytes=0-99"), false).await;
        assert!(
            u.active.load(Ordering::SeqCst) > 0,
            "whole-file mode should keep downloading"
        );
        let first = dir.join("video/srv_14_a/0");
        assert!(until(|| first.exists(), 10).await);
        s.idle(&["srv_14_a".into()]);
        assert!(
            until(|| u.active.load(Ordering::SeqCst) == 0, 3).await,
            "still downloading after playback ended"
        );
        assert!(first.exists(), "the cache must stay");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A cap too small even for the blocks around the reader still writes (slightly over) instead of refusing and spinning on new requests.
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

    fn src() -> Src {
        Src {
            url: "http://127.0.0.1:9/f".into(),
            auth: Auth {
                token: String::new(),
                origin: String::new(),
            },
            resolved: None,
        }
    }

    /// Every retain recounts, even with no dir to delete (sweep only recounts after renaming one).
    #[tokio::test(flavor = "multi_thread")]
    async fn retain_recounts_even_with_nothing_to_delete() {
        let dir = tmp("recount");
        let s = Stream::start(&dir, false).unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await; // startup recount first
        s.total.store(12345, Ordering::Relaxed);
        s.retain(&[]);
        assert!(
            until(|| s.total.load(Ordering::Relaxed) == 0, 5).await,
            "every retain corrects drift in the total"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Startup recount (trash excluded, subdirs included), per-cache usage, clearing, and a synchronous sweep.
    #[tokio::test(flavor = "multi_thread")]
    async fn storage_accounting() {
        let dir = tmp("account");
        let v = dir.join("video");
        std::fs::create_dir_all(v.join("a/nested")).unwrap();
        std::fs::create_dir_all(v.join(".trash-old")).unwrap();
        std::fs::create_dir_all(dir.join("subs")).unwrap();
        std::fs::write(v.join("a/0"), [0u8; 10]).unwrap();
        std::fs::write(v.join("a/nested/x"), [0u8; 5]).unwrap();
        std::fs::write(v.join(".trash-old/y"), [0u8; 3]).unwrap();
        std::fs::write(dir.join("subs/s.srt"), [0u8; 7]).unwrap();
        assert!(
            free_space(&v).is_some(),
            "the cap needs the disk's free space"
        );
        let s = Stream::start(&dir, true).unwrap();
        assert!(
            until(|| s.total.load(Ordering::Relaxed) == 15, 5).await,
            "startup recount"
        );
        assert!(
            until(|| !v.join(".trash-old").exists(), 5).await,
            "old trash removed"
        );
        let cap = s.cap.load(Ordering::Relaxed);
        assert!((MIN_CAP..=MAX_CAP).contains(&cap));
        std::fs::create_dir_all(v.join(".trash-new")).unwrap();
        std::fs::write(v.join(".trash-new/z"), [0u8; 100]).unwrap();
        assert_eq!(s.usage(), (15, 7), "trash dirs not counted");
        s.clear_subs();
        assert_eq!(s.usage(), (15, 0));
        assert!(dir.join("subs").is_dir(), "subs dir recreated");

        // A doomed name that is already gone is dropped; a real dir is moved out and the total recounted at once.
        lock(&s.doomed).extend(["a".to_owned(), "ghost".to_owned()]);
        assert!(s.sweep(false));
        assert!(lock(&s.doomed).is_empty());
        assert!(
            !s.sweep_retry.load(Ordering::Relaxed),
            "nothing left: no retry queued"
        );
        assert!(!v.join("a").exists());
        assert_eq!(s.total.load(Ordering::Relaxed), 0);
        assert!(!s.sweep(false), "nothing to rename");
        // A dir that cannot be renamed stays doomed with one retry chain; a finished retry clears the flag.
        // (Renaming below a regular file is ENOTDIR on unix; Windows reports it as NotFound.)
        if cfg!(unix) {
            std::fs::write(v.join("plain"), b"").unwrap();
            lock(&s.doomed).insert("plain/sub".into());
            s.sweep(false);
            assert!(lock(&s.doomed).contains("plain/sub"));
            assert!(s.sweep_retry.load(Ordering::Relaxed));
            lock(&s.doomed).clear();
            s.sweep(true);
            assert!(!s.sweep_retry.load(Ordering::Relaxed));
        }

        std::fs::create_dir_all(v.join("b")).unwrap();
        s.clear_video(false);
        assert!(
            until(|| !v.join("b").exists(), 5).await,
            "clear_video deletes"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Opening keeps only blocks whose size matches meta.json; repair un-marks a block deleted from outside.
    #[tokio::test(flavor = "multi_thread")]
    async fn open_keeps_valid_blocks_and_repairs() {
        let dir = tmp("open");
        let d = dir.join("video/r");
        std::fs::create_dir_all(&d).unwrap();
        let size = BLOCK + 10;
        std::fs::write(d.join("meta.json"), format!(r#"{{"size":{size}}}"#)).unwrap();
        for (name, len) in [
            ("0", 3),
            ("1", 10),
            ("01", 10),
            ("2", 10),
            ("1.4.part", 1),
            ("meta.json.tmp", 1),
            ("junk", 1),
        ] {
            std::fs::write(d.join(name), vec![0u8; len]).unwrap();
        }
        let f = File::open(d.clone(), src());
        assert_eq!(
            f.st().done,
            [false, true],
            "short block 0 and out-of-range 2 dropped"
        );
        let mut left: Vec<_> = std::fs::read_dir(&d)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(left, ["1", "meta.json"]);

        let s = Stream::start(&dir, true).unwrap();
        let on_disk = dir_size(&dir.join("video"));
        assert!(
            until(|| s.total.load(Ordering::Relaxed) == on_disk, 5).await,
            "startup recount done before the total is set by hand"
        );
        s.total.store(100, Ordering::Relaxed);
        f.repair(&s, 0);
        assert_eq!(
            s.total.load(Ordering::Relaxed),
            100,
            "a block never done is left alone"
        );
        f.repair(&s, 1);
        assert!(f.st().done[1], "present block stays");
        std::fs::remove_file(d.join("1")).unwrap();
        f.repair(&s, 1);
        assert!(!f.st().done[1]);
        assert_eq!(s.total.load(Ordering::Relaxed), 90);
        sub_total(&s.total, 1000);
        assert_eq!(s.total.load(Ordering::Relaxed), 0, "saturates at 0");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Failure backoff per kind, `status_since` only reports newer errors, and `bump` wakes readers.
    #[test]
    fn failure_backoff_and_wakeups() {
        let dir = tmp("fail");
        let f = File::open(dir.join("x"), src());
        let rx = f.changed.subscribe();
        let before = Instant::now();
        let wait = |e: Fail| {
            f.failed(e);
            f.st().retry_at.unwrap() - Instant::now()
        };
        // Lower bounds are loose so a stalled CI runner passes; a wrong arm is still off by about 2x.
        let w = wait(Fail::Status(404));
        assert!(
            w > Duration::from_millis(2500) && w <= Duration::from_secs(3),
            "{w:?}"
        );
        assert_eq!(f.st().status_since(before), Some(404));
        assert_eq!(
            f.st().status_since(Instant::now()),
            None,
            "older errors are not reported"
        );
        let w = wait(Fail::Busy(503, Some(Duration::from_secs(2))));
        assert!(
            w > Duration::from_millis(1500) && w <= Duration::from_secs(2),
            "{w:?}"
        );
        assert_eq!(
            f.st().status_since(before),
            None,
            "busy is not a final status"
        );
        let w = wait(Fail::Net("x".into()));
        assert!(
            w > Duration::from_millis(500) && w <= Duration::from_secs(1),
            "{w:?}"
        );
        assert!(rx.has_changed().unwrap(), "failures wake readers");
        // Codes mpv gets, and which failures are retried
        let all = [
            Fail::Status(404),
            Fail::Busy(503, None),
            Fail::Net(String::new()),
            Fail::Connect(String::new()),
            Fail::Timeout,
        ];
        assert_eq!(
            all.iter().map(Fail::code).collect::<Vec<_>>(),
            [404, 503, 502, 502, 504]
        );
        assert_eq!(
            all.iter().map(Fail::transient).collect::<Vec<_>>(),
            [false, true, true, true, true]
        );
        assert!(!f.st().busy());
        let _ = std::fs::remove_dir_all(dir);
    }
}
