//! 海报、背景图的本地缓存（原生客户端的做法，同 Nuke / Kingfisher / Coil 一类图片库）：
//! 界面用 `emby-img` 协议要图 → 先查磁盘缓存 → 没有再向上游要服务端缩好的尺寸，存下来再给界面。
//!
//! - 上游给的图比要的宽很多时（有的服务器不理会 maxWidth，一律给原图）先缩到要的宽度再存（[`downsample`]，主流图片库的下采样）。
//! - 键 = 服务器 + 条目 + 图片类型 + 序号 + tag + 宽度。tag 变了就是新图，所以缓存的图永远不用重新验证。
//! - 磁盘上限 [`CAP`]，超了按最近使用时间（命中时刷新文件修改时间）删最旧的，删到 90%。
//! - 同一张图同时被要多次只向上游取一次；同时向上游取最多 [`PARALLEL`] 张，不把中继压垮。
//! - 回给界面时声明一年不变（immutable），同一次运行里 WebView 自己的内存缓存照样生效。
//! - 只从已配置的服务器取图（路径里是服务器 id，不是地址），不是通用转发。
//! - 诊断数据：按上游、图片类型每 30 秒汇总一条 `img`（张数、命中、失败与状态码、排队 / 下载耗时分桶、缩图前后字节）。

use crate::telemetry;
use serde_json::json;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::{OnceCell, Semaphore};

pub const SCHEME: &str = "emby-img";
/// 磁盘缓存上限。海报（240–480 px）一张 20–60 KB，背景图（1600 px）一张 200–400 KB，1 GB 够存几万张。
pub const CAP: u64 = 1024 * 1024 * 1024;
const PARALLEL: usize = 8;
const STATS_EVERY: Duration = Duration::from_secs(30);

/// 界面要的一张图：路径 `<服务器 id>/<条目 id>/<类型>/<序号>/<tag>/<宽度>`。
#[derive(Debug, PartialEq)]
pub struct Req {
    pub server: String,
    pub item: String,
    pub kind: String,
    pub index: u32,
    pub tag: String,
    pub width: u32,
}

impl Req {
    /// 解析请求路径（WebView 可能把 `/` 编码成 `%2F`）。每段只允许字母、数字和 `-`，杜绝路径穿越和拼出别的地址。
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

/// 按文件头判断图片格式（Emby 海报是 JPEG，Logo 是带透明的 PNG）。
pub fn content_type(b: &[u8]) -> &'static str {
    match b {
        [0x89, b'P', b'N', b'G', ..] => "image/png",
        [
            b'R',
            b'I',
            b'F',
            b'F',
            _,
            _,
            _,
            _,
            b'W',
            b'E',
            b'B',
            b'P',
            ..,
        ] => "image/webp",
        [b'G', b'I', b'F', ..] => "image/gif",
        _ => "image/jpeg",
    }
}

/// 比要的宽这么多倍以上才缩（服务器已按 maxWidth 缩好的图原样存，不重新编码）。
const RESIZE_OVER: f64 = 1.2;

/// 上游给的图明显比要的宽时缩到要的宽度（等比），带透明的存 PNG、其余存 JPEG（质量 90）：
/// 小卡片不用解码整张原图（省内存、滚动不卡），磁盘缓存也存得下更多。
/// 认不出格式、解码失败、缩完反而更大时原样返回。
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

/// 一次取图的结果：图片字节，或者给界面的 HTTP 状态码。
type Got = Result<Arc<Vec<u8>>, u16>;

/// 耗时分桶的上界（毫秒）：第 k 桶是 `[BOUNDS[k-1], BOUNDS[k])`，最后一桶是 30 秒以上。
/// 上报分桶计数而不是分位数：分桶可以跨时间段相加，分位数不行。报表按同一组边界解读，不能改。
pub const BOUNDS: [u32; 9] = [50, 100, 200, 500, 1000, 2000, 5000, 10000, 30000];

fn bucket(ms: u64) -> usize {
    BOUNDS
        .iter()
        .position(|&b| ms < u64::from(b))
        .unwrap_or(BOUNDS.len())
}

/// 一次未命中、由自己向上游取的图：等并发名额、下载 + 缩图各多久，上游给了多少字节。
struct Fetched {
    wait_ms: u64,
    fetch_ms: u64,
    up_bytes: usize,
}

/// 30 秒内某个上游、某类图（Primary / Backdrop / Logo …）的汇总。
#[derive(Default)]
struct Stats {
    n: u64,
    hit: u64,
    /// 同一张图正被别的请求取，等它的结果（不算一次上游请求）。
    shared: u64,
    fail: u64,
    fail_st: std::collections::BTreeMap<u16, u64>,
    /// 存进缓存 / 给界面的字节（缩图后）。
    bytes: u64,
    /// 上游给的原始字节：远大于 `bytes` 说明上游不理会 maxWidth、给了原图。
    up_bytes: u64,
    wait: [u64; BOUNDS.len() + 1],
    fetch: [u64; BOUNDS.len() + 1],
}

pub struct Images {
    dir: PathBuf,
    cap: u64,
    http: reqwest::Client,
    slots: Semaphore,
    inflight: Mutex<HashMap<String, Arc<OnceCell<Got>>>>,
    total: AtomicU64,
    stats: Mutex<HashMap<(String, String), Stats>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
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
            total: AtomicU64::new(0),
            stats: Mutex::new(HashMap::new()),
        })
    }

    /// 后台任务：统计现有缓存大小（启动时）、按上游定时汇总诊断数据。
    pub fn start(self: &Arc<Self>) {
        let me = self.clone();
        tauri::async_runtime::spawn_blocking(move || {
            let total = list(&me.dir).iter().map(|(_, n, _)| n).sum();
            me.total.store(total, Ordering::Relaxed);
            me.trim();
        });
        let me = self.clone();
        tauri::async_runtime::spawn(async move {
            let mut t = tokio::time::interval(STATS_EVERY);
            loop {
                t.tick().await;
                me.flush_stats();
            }
        });
    }

    /// 取一张图：磁盘缓存 → 上游。`api_base` 是这台服务器的 API 根（找不到服务器时调用方直接回 404）。
    pub async fn get(&self, req: &Req, api_base: &str, srv: &str) -> Got {
        let path = self.dir.join(req.file_name());
        if let Ok(b) = tokio::fs::read(&path).await {
            // 命中：刷新修改时间，淘汰时按它算「最近使用」
            let _ = std::fs::File::options()
                .write(true)
                .open(&path)
                .and_then(|f| f.set_modified(SystemTime::now()));
            self.note(srv, &req.kind, |s| {
                s.hit += 1;
                s.bytes += b.len() as u64;
            });
            return Ok(Arc::new(b));
        }
        let cell = lock(&self.inflight)
            .entry(req.file_name())
            .or_default()
            .clone();
        let mut fetched = None; // 只有真正向上游取的那个请求才有
        let got = cell
            .get_or_init(|| async {
                let width = req.width;
                let t0 = Instant::now();
                let mut wait_ms = 0;
                let r = match self.fetch(&req.upstream(api_base), &mut wait_ms).await {
                    Ok(b) => {
                        let up_bytes = b.len();
                        // 解码、缩放是 CPU 活，不占异步线程
                        let r = tokio::task::spawn_blocking(move || downsample(b, width))
                            .await
                            .map(Arc::new)
                            .map_err(|_| 500u16);
                        fetched = Some(Fetched {
                            wait_ms,
                            fetch_ms: (t0.elapsed().as_millis() as u64).saturating_sub(wait_ms),
                            up_bytes,
                        });
                        r
                    }
                    Err(s) => Err(s),
                };
                if let Ok(b) = &r {
                    self.store(&path, b).await;
                }
                r
            })
            .await
            .clone();
        lock(&self.inflight).remove(&req.file_name());
        self.note(srv, &req.kind, |s| match (&got, &fetched) {
            (Err(st), _) => {
                s.fail += 1;
                *s.fail_st.entry(*st).or_default() += 1;
            }
            (Ok(b), Some(f)) => {
                s.bytes += b.len() as u64;
                s.up_bytes += f.up_bytes as u64;
                s.wait[bucket(f.wait_ms)] += 1;
                s.fetch[bucket(f.fetch_ms)] += 1;
            }
            (Ok(b), None) => {
                s.shared += 1;
                s.bytes += b.len() as u64;
            }
        });
        got
    }

    /// 向上游取原始字节；`wait_ms` 填上等并发名额花了多久。
    async fn fetch(&self, url: &str, wait_ms: &mut u64) -> Result<Vec<u8>, u16> {
        let t0 = Instant::now();
        let _slot = self.slots.acquire().await.map_err(|_| 503u16)?;
        *wait_ms = t0.elapsed().as_millis() as u64;
        let resp = self.http.get(url).send().await.map_err(|e| {
            log::warn!("取图失败：{}", e.without_url());
            502u16
        })?;
        let status = resp.status();
        if !status.is_success() {
            return Err(status.as_u16());
        }
        resp.bytes().await.map(|b| b.to_vec()).map_err(|_| 502u16)
    }

    /// 先写临时文件再改名：写到一半断电不会留下半张图被当成命中。
    async fn store(&self, path: &Path, b: &[u8]) {
        let tmp = path.with_extension("tmp");
        if tokio::fs::write(&tmp, b).await.is_ok() && tokio::fs::rename(&tmp, path).await.is_ok() {
            let total = self.total.fetch_add(b.len() as u64, Ordering::Relaxed) + b.len() as u64;
            if total > self.cap {
                self.trim();
            }
        }
    }

    /// 超过上限时按最近使用时间删最旧的，删到上限的 90%（留出余量，不至于每存一张就删一次）。
    fn trim(&self) {
        let mut files = list(&self.dir);
        let mut total: u64 = files.iter().map(|(_, n, _)| n).sum();
        if total > self.cap {
            files.sort_by_key(|(_, _, t)| *t);
            for (p, n, _) in files {
                if total <= self.cap / 10 * 9 {
                    break;
                }
                if std::fs::remove_file(&p).is_ok() {
                    total -= n;
                }
            }
        }
        self.total.store(total, Ordering::Relaxed);
    }

    /// 记一张图（诊断数据关着时什么都不做）。
    fn note(&self, srv: &str, kind: &str, f: impl FnOnce(&mut Stats)) {
        if !telemetry::enabled() {
            return;
        }
        let mut stats = lock(&self.stats);
        let s = stats.entry((srv.to_owned(), kind.to_owned())).or_default();
        s.n += 1;
        f(s);
    }

    fn flush_stats(&self) {
        let all = std::mem::take(&mut *lock(&self.stats));
        for ((srv, kind), s) in all {
            telemetry::rec("img", stats_json(&srv, &kind, &s));
        }
    }
}

/// 一条 `img` 记录：`wait` / `fetch` 是按 [`BOUNDS`] 分桶的张数（只算自己向上游取的），`fail_st` 是失败的状态码计数。
fn stats_json(srv: &str, kind: &str, s: &Stats) -> serde_json::Value {
    json!({
        "srv": srv, "kind": kind, "n": s.n, "hit": s.hit, "shared": s.shared, "fail": s.fail,
        "fail_st": s.fail_st.iter().map(|(k, v)| (k.to_string(), *v)).collect::<HashMap<_, _>>(),
        "kb": s.bytes / 1024, "up_kb": s.up_bytes / 1024, "wait": s.wait, "fetch": s.fetch,
    })
}

/// 缓存目录里的图片：(路径, 大小, 修改时间)。临时文件不算。
fn list(dir: &Path) -> Vec<(PathBuf, u64, SystemTime)> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().extension().is_none())
        .filter_map(|e| {
            let m = e.metadata().ok()?;
            Some((e.path(), m.len(), m.modified().ok()?))
        })
        .collect()
}

/// 自定义协议的响应。
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

    /// 耗时分桶的边界；一条 img 记录的字段（报表按这些字段解读）。
    #[test]
    fn stats_buckets_and_record() {
        assert_eq!(bucket(0), 0);
        assert_eq!(bucket(49), 0);
        assert_eq!(bucket(50), 1);
        assert_eq!(bucket(29_999), 8);
        assert_eq!(bucket(30_000), 9);
        let mut s = Stats {
            n: 3,
            hit: 1,
            fail: 1,
            bytes: 2048,
            up_bytes: 10240,
            ..Default::default()
        };
        s.fail_st.insert(403, 1);
        s.wait[bucket(10)] += 1;
        s.fetch[bucket(2500)] += 1;
        let v = stats_json("s1", "Primary", &s);
        assert_eq!(v["kind"], "Primary");
        assert_eq!(v["fail_st"]["403"], 1);
        assert_eq!(v["kb"], 2);
        assert_eq!(v["up_kb"], 10);
        assert_eq!(v["wait"][0], 1);
        assert_eq!(v["fetch"][6], 1);
        assert_eq!(v["fetch"].as_array().unwrap().len(), BOUNDS.len() + 1);
    }
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
            "s/i/Primary/0/t",           // 少一段
            "s/../Primary/0/t/240",      // 穿越
            "s/i/Primary/0/t%3Fx=1/240", // 带查询
            "s/i/Primary/x/t/240",       // 序号不是数字
            "s/i/Primary/0/t/99999",     // 宽度离谱
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

    /// 服务器给原图时：缩到要的宽度、保持比例；带透明的仍是 PNG；本来就够小的、认不出的原样返回。
    #[test]
    fn downsample_big_images_only() {
        use image::{DynamicImage, GenericImageView, ImageFormat, RgbImage, RgbaImage};
        // 有纹理的原图（纯色图压缩得太好，缩完不一定更小）
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
        assert_eq!(content_type(&small), "image/png", "带透明的要保留 PNG");
        assert_eq!(
            image::load_from_memory(&small).unwrap().dimensions(),
            (480, 160)
        );

        // 已经是要的尺寸（服务器缩好的）：原样，不重新编码
        let fit = encode(
            DynamicImage::ImageRgb8(RgbImage::new(260, 390)),
            ImageFormat::Jpeg,
        );
        assert_eq!(downsample(fit.clone(), 240), fit);
        assert_eq!(downsample(b"not an image".to_vec(), 240), b"not an image");
    }

    #[test]
    fn sniff_types() {
        assert_eq!(content_type(b"\x89PNG\r\n"), "image/png");
        assert_eq!(content_type(b"RIFF1234WEBPVP8"), "image/webp");
        assert_eq!(content_type(b"\xff\xd8\xff"), "image/jpeg");
    }

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("embymini-img-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    /// 同一张图并发要 5 次只向上游取 1 次；第二轮从磁盘出；上游 404 原样回给界面。
    #[tokio::test]
    async fn fetch_once_then_serve_from_disk() {
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
                    h.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(100)).await; // 让并发请求叠在一起
                    let resp: &[u8] = if head.contains("/Items/404/") {
                        b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                    } else {
                        b"HTTP/1.1 200 OK\r\ncontent-length: 4\r\nconnection: close\r\n\r\n\x89PNG"
                    };
                    s.write_all(resp).await.unwrap();
                });
            }
        });
        let dir = tmpdir("fetch");
        let img = Images::new(
            dir.clone(),
            CAP,
            reqwest::Client::builder().no_proxy().build().unwrap(),
        );
        let req = Req::parse("s/1/Primary/0/t/240").unwrap();
        let all = futures_join(&img, &req, &base).await;
        assert!(
            all.iter()
                .all(|r| r.as_ref().unwrap().as_slice() == b"\x89PNG")
        );
        assert_eq!(hits.load(Ordering::SeqCst), 1, "并发的同一张图只取一次");
        assert_eq!(
            img.get(&req, &base, "x").await.unwrap().as_slice(),
            b"\x89PNG"
        );
        assert_eq!(hits.load(Ordering::SeqCst), 1, "第二次从磁盘出");
        let missing = Req::parse("s/404/Primary/0/t/240").unwrap();
        assert_eq!(img.get(&missing, &base, "x").await, Err(404));
        assert!(!dir.join(missing.file_name()).exists(), "失败的不落盘");
        std::fs::remove_dir_all(dir).unwrap();
    }

    async fn futures_join(img: &Arc<Images>, req: &Req, base: &str) -> Vec<Got> {
        let mut v = Vec::new();
        let tasks: Vec<_> = (0..5)
            .map(|_| {
                let (img, base) = (img.clone(), base.to_owned());
                let req = Req::parse(&format!(
                    "{}/{}/{}/{}/{}/{}",
                    req.server, req.item, req.kind, req.index, req.tag, req.width
                ))
                .unwrap();
                tokio::spawn(async move { img.get(&req, &base, "x").await })
            })
            .collect();
        for t in tasks {
            v.push(t.await.unwrap());
        }
        v
    }

    /// 超过上限删最近最少用的，删到 90%。
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
        img.trim();
        // 300 > 250：删最旧的 old 后 200 ≤ 225（90%）就停
        assert!(!dir.join("old").exists());
        assert!(
            dir.join("mid").exists() && dir.join("new").exists(),
            "最近用过的留着"
        );
        assert_eq!(img.total.load(Ordering::Relaxed), 200);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
