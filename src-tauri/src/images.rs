//! Local cache for posters and backdrops, the way native image libraries (Nuke / Kingfisher / Coil) do it:
//! the UI asks via the `emby-img` scheme → disk cache → otherwise the server-resized size from upstream,
//! stored before it is handed back.
//!
//! - Upstream images much wider than asked (some servers ignore maxWidth) are downsampled before storing
//!   ([`downsample`]).
//! - Key = server + item + image type + index + tag + width. A new tag is a new image, so cached files never
//!   need revalidation.
//! - Disk cap [`CAP`]: above it the least recently used (file mtime, refreshed on hit) go first, down to 90%.
//! - Concurrent requests for one image fetch it once; at most [`PARALLEL`] upstream fetches at a time.
//! - Responses are marked immutable for a year, so the WebView's memory cache works within a run.
//! - Upstream 404s are remembered by "API root + file name" for [`MISS_TTL`] and answered 404 directly (some
//!   servers list a Logo tag whose image does not exist; the home carousel would refetch it every turn).
//!   A changed address fetches again; clearing the image cache forgets them.
//! - Images come only from configured servers (the path holds a server id, not an address); not a generic proxy.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::{OnceCell, Semaphore};

use crate::lock;

pub const SCHEME: &str = "emby-img";
/// Disk cap. A poster (240–480 px) is 20–60 KB, a backdrop (1600 px) 200–400 KB: room for tens of thousands.
pub const CAP: u64 = 1024 * 1024 * 1024;
/// Upstream fetches at a time: enough to fill a poster grid without swamping a relay.
const PARALLEL: usize = 8;
/// How long an upstream 404 is answered without asking again.
const MISS_TTL: Duration = Duration::from_secs(30 * 60);

/// One image the UI asks for: path `<server id>/<item id>/<type>/<index>/<tag>/<width>`.
#[derive(Debug, PartialEq, Clone)]
pub struct Req {
    pub server: String,
    pub item: String,
    pub kind: String,
    pub index: u32,
    pub tag: String,
    pub width: u32,
}

impl Req {
    /// Parses the request path (the WebView may encode `/` as `%2F`). Segments allow only ASCII letters, digits
    /// and `-`, so no traversal and no way to build another address.
    pub fn parse(path: &str) -> Option<Self> {
        let path = path
            .trim_start_matches('/')
            .replace("%2F", "/")
            .replace("%2f", "/");
        let seg: Vec<&str> = path.split('/').collect();
        let ok = |s: &str| {
            !s.is_empty()
                && s.len() <= 64
                && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        };
        let [server, item, kind, index, tag, width] = seg[..] else {
            return None;
        };
        if ![server, item, kind, tag].into_iter().all(ok) {
            return None;
        }
        Some(Self {
            server: server.into(),
            item: item.into(),
            kind: kind.into(),
            index: index.parse().ok()?,
            tag: tag.into(),
            width: width.parse().ok().filter(|w| (16..=7680).contains(w))?,
        })
    }

    fn file_name(&self) -> String {
        format!(
            "{}_{}_{}_{}_{}_{}",
            self.server, self.item, self.kind, self.index, self.tag, self.width
        )
    }

    fn upstream(&self, api_base: &str) -> String {
        format!(
            "{api_base}/Items/{}/Images/{}/{}?tag={}&maxWidth={}&quality=90",
            self.item, self.kind, self.index, self.tag, self.width
        )
    }
}

/// MIME type from the file header (Emby posters are JPEG, logos PNG with alpha); unknown falls back to JPEG.
fn content_type(b: &[u8]) -> &'static str {
    image::guess_format(b).map_or("image/jpeg", |f| f.to_mime_type())
}

/// Only images this many times wider than asked get resized (server-resized ones are stored as is, not re-encoded).
const RESIZE_OVER: f64 = 1.2;

/// Shrinks an image clearly wider than asked to that width (keeping aspect); alpha stays PNG, the rest JPEG q90.
/// Small cards then never decode a full original (less memory, smooth scrolling) and the cache holds more.
/// Unknown format, decode failure, or a bigger result returns the input unchanged.
pub fn downsample(bytes: Vec<u8>, width: u32) -> Vec<u8> {
    use image::{ImageFormat, imageops::FilterType};
    let Ok(fmt) = image::guess_format(&bytes) else {
        return bytes;
    };
    if !matches!(
        fmt,
        ImageFormat::Jpeg | ImageFormat::Png | ImageFormat::WebP
    ) {
        return bytes;
    }
    let Ok(img) = image::load_from_memory_with_format(&bytes, fmt) else {
        return bytes;
    };
    if f64::from(img.width()) <= f64::from(width) * RESIZE_OVER {
        return bytes;
    }
    let small = img.resize(width, u32::MAX, FilterType::CatmullRom);
    let mut out = std::io::Cursor::new(Vec::new());
    let ok = if small.color().has_alpha() {
        small.write_to(&mut out, ImageFormat::Png).is_ok()
    } else {
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 90)
            .encode_image(&small.to_rgb8())
            .is_ok()
    };
    let out = out.into_inner();
    if ok && out.len() < bytes.len() {
        out
    } else {
        bytes
    }
}

/// Result of one image request: the bytes, or the HTTP status for the UI.
type Got = Result<Arc<Vec<u8>>, u16>;

pub struct Images {
    dir: PathBuf,
    cap: u64,
    http: reqwest::Client,
    slots: Semaphore,
    inflight: Mutex<HashMap<String, Arc<OnceCell<Got>>>>,
    /// Upstream 404s: "API root + space + file name" → when seen. The API root means a changed server address
    /// does not inherit the old 404s. First field counts clears: a 404 from a request sent before a clear is
    /// not recorded (otherwise it would stick for another 30 minutes).
    missing: Mutex<(u64, HashMap<String, Instant>)>,
    total: AtomicU64,
    /// One over-cap trim at a time: parallel ones would list the same files and delete past the 90% mark.
    trimming: AtomicBool,
}

impl Images {
    pub fn new(dir: PathBuf, cap: u64, http: reqwest::Client) -> Arc<Self> {
        let _ = std::fs::create_dir_all(&dir);
        Arc::new(Self {
            dir,
            cap,
            http,
            slots: Semaphore::new(PARALLEL),
            inflight: Mutex::new(HashMap::new()),
            missing: Mutex::new((0, HashMap::new())),
            total: AtomicU64::new(0),
            trimming: AtomicBool::new(false),
        })
    }

    /// Background job at startup: sum up the existing cache and trim it.
    pub fn start(self: &Arc<Self>) {
        let me = self.clone();
        tauri::async_runtime::spawn_blocking(move || me.recount());
    }

    /// Gets one image: disk cache → upstream. `api_base` is that server's API root (unknown servers get a 404
    /// from the caller).
    pub async fn get(&self, req: &Req, api_base: &str) -> Got {
        let key = req.file_name();
        let miss_key = format!("{api_base} {key}");
        let epoch = {
            let m = lock(&self.missing);
            if m.1.get(&miss_key).is_some_and(|t| t.elapsed() < MISS_TTL) {
                return Err(404);
            }
            m.0
        };
        let path = self.dir.join(&key);
        if let Some(b) = cached(&path).await {
            return Ok(Arc::new(b));
        }
        let cell = lock(&self.inflight).entry(key.clone()).or_default().clone();
        let got = cell
            .get_or_init(|| async {
                let width = req.width;
                let b = self.fetch(&req.upstream(api_base)).await?;
                // Decoding and resizing are CPU work: keep them off the async workers.
                let r = tokio::task::spawn_blocking(move || downsample(b, width))
                    .await
                    .map(Arc::new)
                    .map_err(|_| 500u16);
                if let Ok(b) = &r {
                    self.store(&path, b).await;
                }
                r
            })
            .await
            .clone();
        lock(&self.inflight).remove(&key);
        if got == Err(404) {
            let mut m = lock(&self.missing);
            if m.0 == epoch {
                m.1.retain(|_, t| t.elapsed() < MISS_TTL);
                m.1.insert(miss_key, Instant::now());
            }
        }
        got
    }

    /// Fetches the raw bytes. Failures return a status (network errors count as 502) passed on to the UI.
    async fn fetch(&self, url: &str) -> Result<Vec<u8>, u16> {
        // Transient failures retry per crate::status; the slot is released while waiting, so a few
        // rate-limited images do not block the whole page.
        let mut n = 0;
        loop {
            let slot = self.slots.acquire().await.map_err(|_| 503u16)?;
            let sent = self.http.get(url).send().await;
            match crate::status::retry_wait(&sent, true) {
                Some(wait) if n < crate::status::RETRIES => {
                    // Hand the connection back too, not only the slot, while waiting.
                    drop((slot, sent));
                    tokio::time::sleep(crate::status::backoff(n, wait)).await;
                    n += 1;
                    continue;
                }
                _ => {}
            }
            let resp = sent.map_err(|e| {
                log::warn!("取图失败：{}", e.without_url());
                502u16
            })?;
            let status = resp.status();
            if !status.is_success() {
                return Err(status.as_u16());
            }
            return resp.bytes().await.map(|b| b.to_vec()).map_err(|_| 502u16);
        }
    }

    /// Writes a temp file and renames it, so a crash mid-write never leaves half an image that counts as a hit.
    async fn store(&self, path: &Path, b: &[u8]) {
        let tmp = path.with_extension("tmp");
        if tokio::fs::write(&tmp, b).await.is_ok() && tokio::fs::rename(&tmp, path).await.is_ok() {
            let total = self.total.fetch_add(b.len() as u64, Ordering::Relaxed) + b.len() as u64;
            if total > self.cap && !self.trimming.swap(true, Ordering::AcqRel) {
                // Lists and deletes across the whole directory: off the async workers. Subtracts what it
                // deleted instead of storing its count, which would drop bytes stored meanwhile.
                let (dir, cap) = (self.dir.clone(), self.cap);
                if let Ok((_, removed)) =
                    tokio::task::spawn_blocking(move || trim_dir(&dir, cap)).await
                {
                    // Saturating: a clear meanwhile may already have recounted below `removed`.
                    crate::stream::sub_total(&self.total, removed);
                }
                self.trimming.store(false, Ordering::Release);
            }
        }
    }

    /// Trims and sets the total from what is on disk. Blocking.
    fn recount(&self) {
        let (left, _) = trim_dir(&self.dir, self.cap);
        self.total.store(left, Ordering::Relaxed);
    }

    /// Cache manager: bytes used.
    pub fn usage(&self) -> u64 {
        self.total.load(Ordering::Relaxed)
    }

    /// Cache manager: delete everything and forget the 404s. Blocking.
    pub fn clear(&self) {
        let mut m = lock(&self.missing);
        m.0 += 1;
        m.1.clear();
        drop(m);
        for (p, ..) in list(&self.dir) {
            let _ = std::fs::remove_file(p);
        }
        self.recount();
    }
}

/// Over `cap`, deletes the least recently used (mtime, refreshed on hit) down to 90% of the cap (headroom so
/// not every store trims). Returns (bytes left, bytes deleted). Blocking; the subtitle cache uses it too.
pub fn trim_dir(dir: &Path, cap: u64) -> (u64, u64) {
    let mut files = list(dir);
    let mut total: u64 = files.iter().map(|(_, n, _)| n).sum();
    let mut removed = 0;
    if total > cap {
        files.sort_by_key(|(_, _, t)| *t);
        for (p, n, _) in files {
            if total <= cap / 10 * 9 {
                break;
            }
            if std::fs::remove_file(&p).is_ok() {
                total -= n;
                removed += n;
            }
        }
    }
    (total, removed)
}

/// Files in a cache dir: (path, size, mtime). Half-written temp files (`.tmp`) are skipped.
fn list(dir: &Path) -> Vec<(PathBuf, u64, SystemTime)> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().extension().is_none_or(|x| x != "tmp"))
        .filter_map(|e| {
            let m = e.metadata().ok()?;
            Some((e.path(), m.len(), m.modified().ok()?))
        })
        .collect()
}

/// Reads a cached image and refreshes its mtime, which trimming reads as "last used".
/// One blocking-pool trip: a grid hits dozens at once, and slow disks or antivirus must not stall async workers.
async fn cached(path: &Path) -> Option<Vec<u8>> {
    let p = path.to_owned();
    tokio::task::spawn_blocking(move || {
        let b = std::fs::read(&p).ok()?;
        let _ = std::fs::File::options()
            .write(true)
            .open(&p)
            .and_then(|f| f.set_modified(SystemTime::now()));
        Some(b)
    })
    .await
    .ok()
    .flatten()
}

/// Response for the custom scheme.
pub fn response(got: Got) -> tauri::http::Response<Vec<u8>> {
    let b = tauri::http::Response::builder();
    match got {
        Ok(bytes) => b
            .status(200)
            .header("Content-Type", content_type(&bytes))
            .header("Cache-Control", "public, max-age=31536000, immutable")
            .body(bytes.to_vec()),
        Err(status) => b.status(status).body(Vec::new()),
    }
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn parse_accepts_only_safe_paths() {
        let r = Req::parse("/a1b2-c3%2F1354593%2FPrimary%2F0%2Fabc123%2F240").unwrap();
        assert_eq!(
            r,
            Req {
                server: "a1b2-c3".into(),
                item: "1354593".into(),
                kind: "Primary".into(),
                index: 0,
                tag: "abc123".into(),
                width: 240
            }
        );
        assert_eq!(
            r.upstream("https://h/emby"),
            "https://h/emby/Items/1354593/Images/Primary/0?tag=abc123&maxWidth=240&quality=90"
        );
        for bad in [
            "s/i/Primary/0/t",           // a segment short
            "s/../Primary/0/t/240",      // traversal
            "s/i/Primary/0/t%3Fx=1/240", // smuggled query
            "s/i/Primary/x/t/240",       // index not a number
            "s/i/Primary/0/t/99999",     // absurd width
            "s/i/Pri.mary/0/t/240",
        ] {
            assert!(Req::parse(bad).is_none(), "{bad}");
        }
    }

    fn encode(img: image::DynamicImage, fmt: image::ImageFormat) -> Vec<u8> {
        let mut c = std::io::Cursor::new(Vec::new());
        img.write_to(&mut c, fmt).unwrap();
        c.into_inner()
    }

    /// Originals shrink to the asked width keeping aspect; alpha stays PNG; fitting or unknown input is unchanged.
    #[test]
    fn downsample_big_images_only() {
        use image::{DynamicImage, GenericImageView, ImageFormat, RgbImage, RgbaImage};
        // Textured source (a flat image compresses so well the resized one may not be smaller).
        let poster = DynamicImage::ImageRgb8(RgbImage::from_fn(1000, 1500, |x, y| {
            image::Rgb([(x * 7) as u8, (y * 13) as u8, ((x ^ y) * 3) as u8])
        }));
        let big = encode(poster, ImageFormat::Jpeg);
        let small = downsample(big.clone(), 240);
        let img = image::load_from_memory(&small).unwrap();
        assert_eq!(img.dimensions(), (240, 360));
        assert_eq!(content_type(&small), "image/jpeg");
        assert!(
            small.len() < big.len() / 4,
            "{} → {}",
            big.len(),
            small.len()
        );

        let logo = DynamicImage::ImageRgba8(RgbaImage::from_fn(1200, 400, |x, y| {
            image::Rgba([(x % 255) as u8, (y % 255) as u8, 0, (x % 2 * 255) as u8])
        }));
        let small = downsample(encode(logo, ImageFormat::Png), 480);
        assert_eq!(content_type(&small), "image/png", "alpha keeps PNG");
        assert_eq!(
            image::load_from_memory(&small).unwrap().dimensions(),
            (480, 160)
        );

        // Already the asked size (server-resized): unchanged, not re-encoded.
        let fit = encode(
            DynamicImage::ImageRgb8(RgbImage::new(260, 390)),
            ImageFormat::Jpeg,
        );
        assert_eq!(downsample(fit.clone(), 240), fit);
        assert_eq!(downsample(b"not an image".to_vec(), 240), b"not an image");
    }

    #[test]
    fn sniff_types() {
        assert_eq!(content_type(b"\x89PNG\r\n\x1a\n"), "image/png");
        assert_eq!(content_type(b"RIFF1234WEBPVP8 "), "image/webp");
        assert_eq!(content_type(b"GIF89a"), "image/gif");
        assert_eq!(content_type(b"\xff\xd8\xff"), "image/jpeg");
        assert_eq!(content_type(b"????"), "image/jpeg");
    }

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("embymini-img-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    /// Five concurrent asks fetch once; the next comes from disk and refreshes its mtime; an upstream 404 is
    /// passed on and remembered (per address, forgotten on clear).
    #[tokio::test]
    async fn fetch_once_then_serve_from_disk() {
        let (base, hits) = upstream(|head, _| {
            if head.contains("/Items/404/") {
                b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
            } else {
                OK
            }
        })
        .await;
        let dir = tmpdir("fetch");
        let img = images(&dir, CAP);
        let req = Req::parse("s/1/Primary/0/t/240").unwrap();
        let all = futures_join(&img, &req, &base).await;
        assert!(
            all.iter()
                .all(|r| r.as_ref().unwrap().as_slice() == b"JPEG")
        );
        assert_eq!(hits.load(Ordering::SeqCst), 1, "concurrent asks fetch once");
        let file = dir.join(req.file_name());
        let old = SystemTime::now() - Duration::from_secs(3600);
        std::fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(old)
            .unwrap();
        assert_eq!(img.get(&req, &base).await.unwrap().as_slice(), b"JPEG");
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "second ask served from disk"
        );
        let touched = std::fs::metadata(&file).unwrap().modified().unwrap();
        assert!(
            touched > old + Duration::from_secs(60),
            "hit refreshes mtime"
        );
        let missing = Req::parse("s/404/Primary/0/t/240").unwrap();
        assert_eq!(img.get(&missing, &base).await, Err(404));
        assert!(
            !dir.join(missing.file_name()).exists(),
            "failures are not stored"
        );
        let before = hits.load(Ordering::SeqCst);
        assert_eq!(img.get(&missing, &base).await, Err(404));
        assert_eq!(
            hits.load(Ordering::SeqCst),
            before,
            "404 remembered, upstream not asked"
        );
        img.clear();
        assert_eq!(img.get(&missing, &base).await, Err(404));
        assert_eq!(
            hits.load(Ordering::SeqCst),
            before + 1,
            "asked again after a clear"
        );
        let other = base.replace("/emby", "/emby2");
        assert_eq!(img.get(&missing, &other).await, Err(404));
        assert_eq!(
            hits.load(Ordering::SeqCst),
            before + 2,
            "another address does not inherit the 404"
        );
        // A request in flight during a clear: its late 404 is not recorded.
        let gone = Req::parse("s/404/Logo/0/t/240").unwrap();
        let t = {
            let (img, base) = (img.clone(), base.clone());
            tokio::spawn(async move { img.get(&gone, &base).await })
        };
        tokio::time::sleep(Duration::from_millis(30)).await;
        img.clear();
        assert_eq!(t.await.unwrap(), Err(404));
        let gone = Req::parse("s/404/Logo/0/t/240").unwrap();
        assert_eq!(img.get(&gone, &base).await, Err(404));
        assert_eq!(
            hits.load(Ordering::SeqCst),
            before + 4,
            "a request sent before the clear does not record its 404"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    const OK: &[u8] = b"HTTP/1.1 200 OK\r\ncontent-length: 4\r\nconnection: close\r\n\r\nJPEG";
    const BUSY: &[u8] = b"HTTP/1.1 503 Service Unavailable\r\nretry-after: 0\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";

    /// Local upstream answering request number `n` (from 1) with `reply(head, n)`; returns its API root and hit count.
    async fn upstream(reply: fn(&str, u64) -> &'static [u8]) -> (String, Arc<AtomicU64>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/emby", l.local_addr().unwrap());
        let hits = Arc::new(AtomicU64::new(0));
        let h = hits.clone();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                let h = h.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0; 4096];
                    let n = s.read(&mut buf).await.unwrap();
                    let head = String::from_utf8_lossy(&buf[..n]).to_string();
                    let k = h.fetch_add(1, Ordering::SeqCst) + 1;
                    tokio::time::sleep(Duration::from_millis(100)).await; // let concurrent asks overlap
                    s.write_all(reply(&head, k)).await.unwrap();
                });
            }
        });
        (base, hits)
    }

    fn images(dir: &Path, cap: u64) -> Arc<Images> {
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        Images::new(dir.to_owned(), cap, http)
    }

    /// Transient upstream failures are retried; a persistent one gives up after `RETRIES` and passes the status on.
    #[tokio::test]
    async fn transient_failures_retry() {
        let dir = tmpdir("retry");
        let req = Req::parse("s/1/Primary/0/t/240").unwrap();
        let (base, hits) = upstream(|_, n| if n == 1 { BUSY } else { OK }).await;
        assert_eq!(
            images(&dir, CAP).get(&req, &base).await.unwrap().as_slice(),
            b"JPEG"
        );
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        let req = Req::parse("s/2/Primary/0/t/240").unwrap();
        let (base, hits) = upstream(|_, _| BUSY).await;
        assert_eq!(images(&dir, CAP).get(&req, &base).await, Err(503));
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1 + crate::status::RETRIES as u64
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A store that crosses the cap trims the oldest right away and keeps `usage` exact.
    #[tokio::test]
    async fn store_over_cap_trims() {
        let dir = tmpdir("cap");
        let (base, _) = upstream(|_, _| OK).await;
        let img = images(&dir, 10);
        let now = SystemTime::now();
        for i in 1..=3u64 {
            let req = Req::parse(&format!("s/{i}/Primary/0/t/240")).unwrap();
            img.get(&req, &base).await.unwrap();
            assert_eq!(img.usage(), [4, 8, 8][i as usize - 1], "after image {i}");
            if let Ok(f) = std::fs::File::options()
                .write(true)
                .open(dir.join(req.file_name()))
            {
                // Explicit ages: coarse file-system clocks could otherwise tie them.
                f.set_modified(now - Duration::from_secs(100 - i)).unwrap();
            }
        }
        let first = Req::parse("s/1/Primary/0/t/240").unwrap();
        assert!(!dir.join(first.file_name()).exists(), "oldest trimmed");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Startup sums up what is already on disk.
    #[test]
    fn start_counts_existing_cache() {
        let dir = tmpdir("start");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a"), [0u8; 3]).unwrap();
        std::fs::write(dir.join("b"), [0u8; 5]).unwrap();
        let img = Images::new(dir.clone(), CAP, reqwest::Client::new());
        img.start();
        let deadline = Instant::now() + Duration::from_secs(5);
        while img.usage() != 8 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(img.usage(), 8);
        std::fs::remove_dir_all(dir).unwrap();
    }

    async fn futures_join(img: &Arc<Images>, req: &Req, base: &str) -> Vec<Got> {
        let mut v = Vec::new();
        let tasks: Vec<_> = (0..5)
            .map(|_| {
                let (img, base, req) = (img.clone(), base.to_owned(), req.clone());
                tokio::spawn(async move { img.get(&req, &base).await })
            })
            .collect();
        for t in tasks {
            v.push(t.await.unwrap());
        }
        v
    }

    /// Over the cap the least recently used go, down to 90%.
    #[test]
    fn trim_removes_least_recently_used() {
        let dir = tmpdir("trim");
        std::fs::create_dir_all(&dir).unwrap();
        let img = Images::new(dir.clone(), 250, reqwest::Client::new());
        let base = SystemTime::now();
        for (i, name) in ["old", "mid", "new"].into_iter().enumerate() {
            let p = dir.join(name);
            std::fs::write(&p, vec![0u8; 100]).unwrap();
            std::fs::File::options()
                .write(true)
                .open(&p)
                .unwrap()
                .set_modified(base + Duration::from_secs(i as u64))
                .unwrap();
        }
        img.recount();
        // 300 > 250: deleting the oldest leaves 200 ≤ 225 (90%), so it stops.
        assert!(!dir.join("old").exists());
        assert!(
            dir.join("mid").exists() && dir.join("new").exists(),
            "recently used kept"
        );
        assert_eq!(img.total.load(Ordering::Relaxed), 200);
        // Exactly at the cap nothing goes; half-written temp files are not counted.
        std::fs::write(dir.join("x.tmp"), [0u8; 50]).unwrap();
        assert_eq!(trim_dir(&dir, 200), (200, 0));
        assert!(dir.join("mid").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
