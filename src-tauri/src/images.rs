//! 海报、背景图的本地缓存（原生客户端的做法，同 Nuke / Kingfisher / Coil 一类图片库）：
//! 界面用 `emby-img` 协议要图 → 先查磁盘缓存 → 没有再向上游要服务端缩好的尺寸，存下来再给界面。
//!
//! - 键 = 服务器 + 条目 + 图片类型 + 序号 + tag + 宽度。tag 变了就是新图，所以缓存的图永远不用重新验证。
//! - 磁盘上限 [`CAP`]，超了按最近使用时间（命中时刷新文件修改时间）删最旧的，删到 90%。
//! - 同一张图同时被要多次只向上游取一次；同时向上游取最多 [`PARALLEL`] 张，不把中继压垮。
//! - 回给界面时声明一年不变（immutable），同一次运行里 WebView 自己的内存缓存照样生效。
//! - 只从已配置的服务器取图（路径里是服务器 id，不是地址），不是通用转发。
//! - 诊断数据：按上游每 30 秒汇总一条 `img`（张数、命中、失败、未命中时的耗时分位、字节）。

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

/// 一次取图的结果：图片字节，或者给界面的 HTTP 状态码。
type Got = Result<Arc<Vec<u8>>, u16>;

#[derive(Default)]
struct Stats {
    n: u64,
    hit: u64,
    fail: u64,
    bytes: u64,
    miss_ms: Vec<u32>,
}

pub struct Images {
    dir: PathBuf,
    cap: u64,
    http: reqwest::Client,
    slots: Semaphore,
    inflight: Mutex<HashMap<String, Arc<OnceCell<Got>>>>,
    total: AtomicU64,
    stats: Mutex<HashMap<String, Stats>>,
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
        let t0 = Instant::now();
        let path = self.dir.join(req.file_name());
        if let Ok(b) = tokio::fs::read(&path).await {
            // 命中：刷新修改时间，淘汰时按它算「最近使用」
            let _ = std::fs::File::options()
                .write(true)
                .open(&path)
                .and_then(|f| f.set_modified(SystemTime::now()));
            self.note(srv, true, t0, Ok(b.len()));
            return Ok(Arc::new(b));
        }
        let cell = lock(&self.inflight)
            .entry(req.file_name())
            .or_default()
            .clone();
        let got = cell
            .get_or_init(|| async {
                let r = self.fetch(&req.upstream(api_base)).await;
                if let Ok(b) = &r {
                    self.store(&path, b).await;
                }
                r
            })
            .await
            .clone();
        lock(&self.inflight).remove(&req.file_name());
        self.note(
            srv,
            false,
            t0,
            got.as_ref().map(|b| b.len()).map_err(|s| *s),
        );
        got
    }

    async fn fetch(&self, url: &str) -> Got {
        let _slot = self.slots.acquire().await.map_err(|_| 503u16)?;
        let resp = self.http.get(url).send().await.map_err(|e| {
            log::warn!("取图失败：{}", e.without_url());
            502u16
        })?;
        let status = resp.status();
        if !status.is_success() {
            return Err(status.as_u16());
        }
        let b = resp.bytes().await.map_err(|_| 502u16)?;
        Ok(Arc::new(b.to_vec()))
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

    fn note(&self, srv: &str, hit: bool, t0: Instant, r: Result<usize, u16>) {
        if !telemetry::enabled() {
            return;
        }
        let mut stats = lock(&self.stats);
        let s = stats.entry(srv.to_owned()).or_default();
        s.n += 1;
        match r {
            Ok(n) => {
                s.bytes += n as u64;
                if hit {
                    s.hit += 1;
                } else {
                    s.miss_ms.push(t0.elapsed().as_millis() as u32);
                }
            }
            Err(_) => s.fail += 1,
        }
    }

    fn flush_stats(&self) {
        let all = std::mem::take(&mut *lock(&self.stats));
        for (srv, mut s) in all {
            s.miss_ms.sort_unstable();
            let q = |p: f64| {
                s.miss_ms
                    .get(
                        ((s.miss_ms.len() as f64 * p) as usize)
                            .min(s.miss_ms.len().saturating_sub(1)),
                    )
                    .copied()
            };
            telemetry::rec(
                "img",
                json!({
                    "srv": srv, "n": s.n, "hit": s.hit, "fail": s.fail, "kb": s.bytes / 1024,
                    "p50": q(0.5), "p90": q(0.9), "p99": q(0.99), "max": s.miss_ms.last(),
                }),
            );
        }
    }
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
