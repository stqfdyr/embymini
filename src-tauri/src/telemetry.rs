//! 诊断上报：设置里打开才记，关着时每个记录点只是一次原子读。
//!
//! 记录点（[`rec`]）→ 有界通道（满了丢弃并计数，绝不阻塞调用方）→ 写入任务：按类限流，每 5 秒追加到
//! `pending-*.ndjson`，攒满 64 KB 或满 5 分钟压成一个 gzip 批次 `batch-*.gz`；目录超过 20 MB 删最旧的批次。
//! 上传任务：启动 2 分钟后开始，按时间顺序一批一批送，成功才删；失败按 1 → 2 → 4 … 分钟退避（封顶 30 分钟），
//! 没送完的下次启动接着送。批次 id 唯一，服务端按 id 去重，回包丢了重发也不会重复入库。
//!
//! 地址和 token 只在打包时从环境变量注入（`EMBYMINI_TEL_URL`、`EMBYMINI_TEL_TOKEN`），没注入的构建不显示开关。
//! 不记账号、密码、token、服务器地址、搜索词、片名（只记条目 id）。

use flate2::{Compression, write::GzEncoder};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::{fs, io};
use tokio::sync::mpsc;

const URL: Option<&str> = option_env!("EMBYMINI_TEL_URL");
const TOKEN: Option<&str> = option_env!("EMBYMINI_TEL_TOKEN");
/// 打包时注入了上报地址和 token：设置里才显示开关。CI 里 Secrets 没设时环境变量是空串，也算没注入。
pub const AVAILABLE: bool =
    matches!(URL, Some(u) if !u.is_empty()) && matches!(TOKEN, Some(t) if !t.is_empty());

/// 攒满这么多（未压缩）就封一批；Worker 那边单批上限 256 KB（压缩后），留足余量。
const BATCH_BYTES: u64 = 64 * 1024;
const BATCH_AGE: Duration = Duration::from_secs(300);
const FLUSH_EVERY: Duration = Duration::from_secs(5);
/// 暂存目录上限，超了删最旧的批次。
const DIR_LIMIT: u64 = 20 * 1024 * 1024;
/// 单次启动最多记这么多条，防止某个 bug 刷屏。
const TOTAL_LIMIT: u32 = 20_000;
/// 启动后多久才第一次上传，不和启动、首页加载抢带宽。
const FIRST_UPLOAD: Duration = Duration::from_secs(120);
/// 起播后这段时间不上传。
const HOLD_FOR_PLAY: Duration = Duration::from_secs(15);

static ON: AtomicBool = AtomicBool::new(false);
static TX: OnceLock<mpsc::Sender<Value>> = OnceLock::new();
static DROPPED: AtomicU64 = AtomicU64::new(0);
static HOLD_UNTIL: Mutex<Option<Instant>> = Mutex::new(None);

pub fn set_enabled(on: bool) {
    ON.store(on && AVAILABLE, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    ON.load(Ordering::Relaxed)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// 记一条。`v` 是 JSON 对象，补上时间 `t` 和类别 `k`。关着时什么都不做。
pub fn rec(k: &str, v: Value) {
    if !enabled() {
        return;
    }
    let Some(tx) = TX.get() else { return };
    let mut m = match v {
        Value::Object(m) => m,
        v => Map::from_iter([("v".to_owned(), v)]),
    };
    m.insert("t".into(), now_ms().into());
    m.insert("k".into(), k.into());
    if tx.try_send(Value::Object(m)).is_err() {
        DROPPED.fetch_add(1, Ordering::Relaxed);
    }
}

/// 写入任务收到它就清空暂存（见 [`purge`]）；不是真的记录类别。
const PURGE: &str = "__purge";
/// 写入任务收到它就立刻落盘（见 [`flush_now`]）。
const FLUSH: &str = "__flush";
static FLUSHED: tokio::sync::Notify = tokio::sync::Notify::const_new();

/// 程序退出前调用：写入任务平时 5 秒才落一次盘，最后几秒（这一集的结束、mpv 退出码、Stopped 请求）
/// 不落盘就丢了。让它立刻写，最多等 `wait`。没开过诊断数据时什么都不做。
pub fn flush_now(wait: Duration) {
    let Some(tx) = TX.get() else { return };
    tauri::async_runtime::block_on(async {
        let done = FLUSHED.notified();
        tokio::pin!(done);
        done.as_mut().enable(); // 先登记再发，写入任务通知得再快也收得到
        if tx.try_send(json!({ "k": FLUSH })).is_ok() {
            let _ = tokio::time::timeout(wait, done).await;
        }
    });
}

/// 用户关掉诊断数据：没送出去的也不送了，删掉暂存（交给写入任务做，不和它正在写的文件冲突）。
pub fn purge() {
    if let Some(tx) = TX.get() {
        let _ = tx.try_send(json!({ "k": PURGE }));
    }
}

/// 起播时调用：接下来一小段时间不上传，把带宽让给起播。
pub fn hold_uploads() {
    *HOLD_UNTIL.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now() + HOLD_FOR_PLAY);
}

fn held() -> bool {
    HOLD_UNTIL
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_some_and(|t| Instant::now() < t)
}

/// 每批开头的一行：是谁、哪个版本、哪次启动。
pub struct Meta {
    pub device: String,
    pub name: String,
    pub version: &'static str,
}

/// 启动写入和上传两个后台任务（没注入地址的构建什么都不做）。
pub fn start(dir: PathBuf, meta: Meta) {
    let (Some(url), Some(token)) = (URL, TOKEN) else {
        return;
    };
    if !AVAILABLE {
        return;
    }
    let (tx, rx) = mpsc::channel(4096);
    if TX.set(tx).is_err() {
        return;
    }
    let meta = json!({
        "k": "meta", "device": meta.device, "name": meta.name, "version": meta.version,
        "session": uuid::Uuid::new_v4().to_string(), "os": std::env::consts::OS,
    });
    tauri::async_runtime::spawn(write_loop(dir.clone(), meta, rx));
    tauri::async_runtime::spawn(upload_loop(dir, url, token));
}

// ---------- 限流 ----------

/// 每类每分钟的上限；`log` / `ui_js_error` / `mpv_log` 同文案每分钟只记一条，其余计数。超限的每分钟汇总成一条 `capped`。
struct Caps {
    minute: u64,
    counts: HashMap<String, u32>,
    over: HashMap<String, u32>,
    seen: HashSet<String>,
    total: u32,
}

impl Caps {
    fn new() -> Self {
        Self {
            minute: 0,
            counts: HashMap::new(),
            over: HashMap::new(),
            seen: HashSet::new(),
            total: 0,
        }
    }

    fn limit(kind: &str) -> u32 {
        match kind {
            "api" => 600,
            "op" => 120,
            _ => 300,
        }
    }

    /// 返回要写的行：放行时是这条本身，跨分钟时先补上上一分钟的 `capped` 汇总。
    fn admit(&mut self, v: Value) -> Vec<Value> {
        let mut out = Vec::new();
        let minute = v["t"].as_u64().unwrap_or(0) / 60_000;
        if minute != self.minute {
            out.extend(self.over.drain().map(
                |(kind, n)| json!({ "k": "capped", "t": minute * 60_000, "kind": kind, "n": n }),
            ));
            self.counts.clear();
            self.seen.clear();
            self.minute = minute;
        }
        let kind = v["k"].as_str().unwrap_or("").to_owned();
        let dedup = matches!(kind.as_str(), "log" | "ui_js_error" | "mpv_log")
            && !self
                .seen
                .insert(format!("{kind}\0{}", v["msg"].as_str().unwrap_or("")));
        let n = self.counts.entry(kind.clone()).or_default();
        *n += 1;
        if dedup || *n > Self::limit(&kind) || self.total >= TOTAL_LIMIT {
            *self.over.entry(kind).or_default() += 1;
        } else {
            self.total += 1;
            out.push(v);
        }
        out
    }
}

// ---------- 落盘与封批 ----------

struct Spool {
    dir: PathBuf,
    meta: Value,
    buf: Vec<u8>,
    /// 当前未封批的文件；第一次写入时建，开头是 meta 行。
    raw: Option<(PathBuf, u64, Instant)>,
}

impl Spool {
    fn push(&mut self, v: &Value) {
        let _ = serde_json::to_writer(&mut self.buf, v);
        self.buf.push(b'\n');
    }

    /// 定时（和退出前）落盘：补上队列满丢弃的计数，追加到文件。
    fn write_out(&mut self) {
        let dropped = DROPPED.swap(0, Ordering::Relaxed);
        if dropped > 0 {
            self.push(&json!({ "k": "dropped", "t": now_ms(), "n": dropped }));
        }
        if let Err(e) = self.flush() {
            log::warn!("诊断数据写盘失败：{e}");
        }
    }

    /// 丢掉内存里的行和目录里所有暂存、批次。
    fn purge(&mut self) {
        self.buf.clear();
        self.raw = None;
        for e in fs::read_dir(&self.dir).into_iter().flatten().flatten() {
            let _ = fs::remove_file(e.path());
        }
    }

    /// 把内存里的行追加到文件；够大或够久就封批。
    fn flush(&mut self) -> io::Result<()> {
        if !self.buf.is_empty() {
            if self.raw.is_none() {
                let p = self
                    .dir
                    .join(format!("pending-{}-{}.ndjson", now_ms(), short_id()));
                let mut head = serde_json::to_vec(&self.meta)?;
                head.push(b'\n');
                fs::write(&p, &head)?;
                self.raw = Some((p, head.len() as u64, Instant::now()));
            }
            if let Some((p, len, _)) = &mut self.raw {
                fs::OpenOptions::new()
                    .append(true)
                    .open(&*p)?
                    .write_all(&self.buf)?;
                *len += self.buf.len() as u64;
            }
            self.buf.clear();
        }
        if let Some((p, len, at)) = &self.raw
            && (*len >= BATCH_BYTES || at.elapsed() >= BATCH_AGE)
        {
            let p = p.clone();
            self.raw = None;
            seal(&self.dir, &p)?;
            trim(&self.dir, DIR_LIMIT);
        }
        Ok(())
    }
}

fn short_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..12].to_owned()
}

/// 把一个 pending 文件压成批次。批次文件名 `batch-<毫秒>-<随机>.gz`，按名字排序就是时间顺序，`<毫秒>-<随机>` 即批次 id。
fn seal(dir: &Path, raw: &Path) -> io::Result<()> {
    let data = fs::read(raw)?;
    let mut gz = GzEncoder::new(Vec::new(), Compression::default());
    gz.write_all(&data)?;
    let tmp = dir.join(format!("batch-{}-{}.tmp", now_ms(), short_id()));
    fs::write(&tmp, gz.finish()?)?;
    fs::rename(&tmp, tmp.with_extension("gz"))?;
    fs::remove_file(raw)
}

fn batches(dir: &Path) -> Vec<(PathBuf, u64)> {
    let mut v: Vec<(PathBuf, u64)> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| {
            let n = e.file_name();
            let n = n.to_string_lossy();
            n.starts_with("batch-") && n.ends_with(".gz")
        })
        .map(|e| (e.path(), e.metadata().map_or(0, |m| m.len())))
        .collect();
    v.sort();
    v
}

/// 目录超过上限时删最旧的批次，删了多少记一条。
fn trim(dir: &Path, limit: u64) {
    let all = batches(dir);
    let mut total: u64 = all.iter().map(|(_, n)| n).sum();
    let (mut files, mut bytes) = (0, 0);
    for (p, n) in all {
        if total <= limit {
            break;
        }
        if fs::remove_file(&p).is_ok() {
            total -= n;
            files += 1;
            bytes += n;
        }
    }
    if files > 0 {
        rec("spool_dropped", json!({ "files": files, "bytes": bytes }));
    }
}

async fn write_loop(dir: PathBuf, meta: Value, mut rx: mpsc::Receiver<Value>) {
    if let Err(e) = fs::create_dir_all(&dir) {
        log::warn!("诊断数据目录建不了：{e}");
        return;
    }
    // 上次没封批的先封上，接着送
    for e in fs::read_dir(&dir).into_iter().flatten().flatten() {
        if e.file_name().to_string_lossy().starts_with("pending-") {
            let _ = seal(&dir, &e.path());
        }
    }
    let mut spool = Spool {
        dir,
        meta,
        buf: Vec::new(),
        raw: None,
    };
    let mut caps = Caps::new();
    let mut tick = tokio::time::interval(FLUSH_EVERY);
    loop {
        tokio::select! {
            v = rx.recv() => match v {
                Some(v) if v["k"] == PURGE => spool.purge(),
                Some(v) if v["k"] == FLUSH => {
                    spool.write_out();
                    FLUSHED.notify_waiters();
                }
                Some(v) => for line in caps.admit(v) { spool.push(&line) },
                None => break,
            },
            _ = tick.tick() => spool.write_out(),
        }
    }
}

// ---------- 上传 ----------

/// 第 n 次连续失败后等多久：1、2、4 … 分钟，封顶 30 分钟，±20% 抖动。
fn backoff(n: u32) -> Duration {
    let base = 60u64
        .saturating_mul(1 << n.saturating_sub(1).min(5))
        .min(1800);
    let jitter = (uuid::Uuid::new_v4().as_u128() % 401) as u64; // 0..=400 → 0.8..1.2
    Duration::from_millis(base * (800 + jitter))
}

#[derive(Debug, PartialEq)]
enum Sent {
    Ok,
    /// 服务端明确拒收且重发也没用（批次太大、格式不对）：删掉。
    Rejected(u16),
    Retry(String),
}

async fn send(http: &reqwest::Client, url: &str, token: &str, batch: &Path) -> Sent {
    let Some(id) = batch
        .file_stem()
        .and_then(|s| s.to_str())
        .and_then(|s| s.strip_prefix("batch-"))
    else {
        return Sent::Rejected(0);
    };
    let Ok(body) = fs::read(batch) else {
        return Sent::Rejected(0);
    };
    let r = http
        .post(url)
        .bearer_auth(token)
        .header("Content-Type", "application/gzip")
        .header("X-Batch-Id", id)
        .body(body)
        .send()
        .await;
    match r.map(|r| r.status().as_u16()) {
        Ok(s) if (200..300).contains(&s) => Sent::Ok,
        Ok(s @ (400 | 413)) => Sent::Rejected(s),
        Ok(s) => Sent::Retry(format!("HTTP {s}")),
        // 分类 + 错误链（顶层只有一句 error sending request），同 Emby 请求的错误文案
        Err(e) => {
            let (class, err) = crate::emby::net_classified(&e);
            Sent::Retry(format!("{class}: {err}"))
        }
    }
}

async fn upload_loop(dir: PathBuf, url: &'static str, token: &'static str) {
    let Ok(http) = reqwest::Client::builder()
        .no_proxy()
        .user_agent(crate::emby::UA)
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(60))
        .build()
    else {
        return;
    };
    tokio::time::sleep(FIRST_UPLOAD).await;
    let mut fails = 0;
    loop {
        let next = if !enabled() || held() {
            Some(Duration::from_secs(30))
        } else if let Some((batch, _)) = batches(&dir).into_iter().next() {
            let t0 = Instant::now();
            let r = send(&http, url, token, &batch).await;
            let ms = t0.elapsed().as_millis() as u64;
            let ok = r == Sent::Ok;
            rec(
                "upload",
                json!({ "ok": ok, "ms": ms, "err": match &r { Sent::Retry(e) => e.clone(), Sent::Rejected(s) => format!("rejected {s}"), Sent::Ok => String::new() } }),
            );
            match r {
                Sent::Ok | Sent::Rejected(_) => {
                    if let Sent::Rejected(s) = r {
                        log::warn!("诊断批次被拒收（{s}），丢弃");
                    }
                    let _ = fs::remove_file(&batch);
                    fails = 0;
                    None // 接着送下一批
                }
                Sent::Retry(_) => {
                    fails += 1;
                    Some(backoff(fails))
                }
            }
        } else {
            Some(Duration::from_secs(60))
        };
        if let Some(d) = next {
            tokio::time::sleep(d).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn at(k: &str, t: u64, extra: Value) -> Value {
        let mut v = json!({ "k": k, "t": t });
        if let (Some(m), Some(x)) = (v.as_object_mut(), extra.as_object()) {
            m.extend(x.clone());
        }
        v
    }

    #[test]
    fn caps_limit_per_kind_and_dedup_errors() {
        let mut c = Caps::new();
        let mut kept = 0;
        for _ in 0..700 {
            kept += c.admit(at("api", 60_000, json!({}))).len();
        }
        assert_eq!(kept, 600);
        // 同文案的错误每分钟一条
        assert_eq!(c.admit(at("log", 60_000, json!({ "msg": "x" }))).len(), 1);
        assert_eq!(c.admit(at("log", 60_000, json!({ "msg": "x" }))).len(), 0);
        assert_eq!(c.admit(at("log", 60_000, json!({ "msg": "y" }))).len(), 1);
        // 界面 JS 错误、mpv 警告同样去重，不同类别的同文案互不影响
        assert_eq!(
            c.admit(at("ui_js_error", 60_000, json!({ "msg": "x" })))
                .len(),
            1
        );
        assert_eq!(
            c.admit(at("ui_js_error", 60_000, json!({ "msg": "x" })))
                .len(),
            0
        );
        assert_eq!(
            c.admit(at("mpv_log", 60_000, json!({ "msg": "x" }))).len(),
            1
        );
        // 下一分钟：先补上一分钟的汇总，再放行
        let out = c.admit(at("api", 120_000, json!({})));
        let capped: HashMap<String, u64> = out
            .iter()
            .filter(|v| v["k"] == "capped")
            .map(|v| {
                (
                    v["kind"].as_str().unwrap().to_owned(),
                    v["n"].as_u64().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            capped,
            HashMap::from([
                ("api".into(), 100),
                ("log".into(), 1),
                ("ui_js_error".into(), 1)
            ])
        );
        assert_eq!(out.last().unwrap()["k"], "api");
    }

    #[test]
    fn spool_seals_batches_and_trims_oldest() {
        let dir = std::env::temp_dir().join(format!("embymini-tel-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let mut s = Spool {
            dir: dir.clone(),
            meta: json!({ "k": "meta" }),
            buf: Vec::new(),
            raw: None,
        };
        // 不够 64 KB：只落盘不封批
        s.push(&json!({ "k": "op", "name": "play" }));
        s.flush().unwrap();
        assert!(batches(&dir).is_empty());
        // 攒满：封成一批，内容是 meta 行 + 全部记录
        let big = "x".repeat(1000);
        for _ in 0..70 {
            s.push(&json!({ "k": "op", "pad": big }));
        }
        s.flush().unwrap();
        let b = batches(&dir);
        assert_eq!(b.len(), 1);
        let mut text = String::new();
        flate2::read::GzDecoder::new(fs::File::open(&b[0].0).unwrap())
            .read_to_string(&mut text)
            .unwrap();
        let lines: Vec<Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines[0]["k"], "meta");
        assert_eq!(lines[1]["name"], "play");
        assert_eq!(lines.len(), 72);
        // 再封一批，上限只够留一批：删最旧的
        for _ in 0..70 {
            s.push(&json!({ "k": "op", "pad": big }));
        }
        s.flush().unwrap();
        let newest = batches(&dir).pop().unwrap().0;
        trim(&dir, fs::metadata(&newest).unwrap().len());
        assert_eq!(
            batches(&dir),
            vec![(newest.clone(), fs::metadata(&newest).unwrap().len())]
        );
        // 关掉诊断：批次、未封批的文件、内存里的行都清掉，之后还能照常写
        s.push(&json!({ "k": "op" }));
        s.flush().unwrap();
        s.push(&json!({ "k": "op" }));
        s.purge();
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
        s.push(&json!({ "k": "op" }));
        s.flush().unwrap();
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        fs::remove_dir_all(dir).unwrap();
    }

    /// 连不上时的重试原因带分类和错误链，不是一句笼统的 error sending request。
    #[tokio::test]
    async fn send_failure_is_classified() {
        let dir = std::env::temp_dir().join(format!("embymini-tel-send-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let batch = dir.join("batch-1-abc.gz");
        fs::write(&batch, b"x").unwrap();
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let r = send(&http, "http://127.0.0.1:1/ingest", "t", &batch).await;
        fs::remove_dir_all(dir).unwrap();
        let Sent::Retry(e) = r else { panic!("{r:?}") };
        assert!(e.starts_with("connect: 连不上服务器（"), "{e}");
    }

    /// 上传带 token 和批次 id；2xx 算成功，400 / 413 丢弃，其余重试。
    #[tokio::test]
    async fn send_classifies_responses() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/ingest", l.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let mut heads = Vec::new();
            for status in [
                "204 No Content",
                "413 Payload Too Large",
                "503 Service Unavailable",
            ] {
                let (mut s, _) = l.accept().await.unwrap();
                let mut buf = vec![0; 8192];
                let n = s.read(&mut buf).await.unwrap();
                heads.push(String::from_utf8_lossy(&buf[..n]).to_lowercase());
                s.write_all(
                    format!("HTTP/1.1 {status}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                        .as_bytes(),
                )
                .await
                .unwrap();
            }
            heads
        });
        let dir = std::env::temp_dir().join(format!("embymini-tel-send-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let batch = dir.join("batch-123-abc.gz");
        fs::write(&batch, b"gz").unwrap();
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        assert_eq!(send(&http, &url, "tok", &batch).await, Sent::Ok);
        assert_eq!(send(&http, &url, "tok", &batch).await, Sent::Rejected(413));
        assert!(matches!(
            send(&http, &url, "tok", &batch).await,
            Sent::Retry(_)
        ));
        let heads = server.await.unwrap();
        assert!(heads[0].contains("authorization: bearer tok"));
        assert!(heads[0].contains("x-batch-id: 123-abc"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn backoff_grows_and_caps() {
        let s = |n| backoff(n).as_secs();
        assert!((48..=72).contains(&s(1)));
        assert!((96..=144).contains(&s(2)));
        assert!((1440..=2160).contains(&s(10)));
    }
}
