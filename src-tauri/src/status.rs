//! 对外 HTTP 回包的统一处理：Emby API、图片、视频 / 字幕缓存代理、弹幕接口共用同一套状态码分类、重试和文案。
//!
//! | 类别 | 状态码 | 重试 | 说明 |
//! |---|---|---|---|
//! | [`Class::Auth`] | 401 | 否 | 凭据失效：Emby 清 token、引导重新登录；弹幕提示检查接口地址 |
//! | [`Class::Denied`] | 403 | 否 | 拒绝访问 |
//! | [`Class::Missing`] | 404、410 | 否 | 不存在 |
//! | [`Class::Busy`] | 408、425、429、502、503、504 | 是 | 上游、反代、中继临时不可用或限流 |
//! | [`Class::Failed`] | 其余 5xx | 否 | 服务器自身出错，马上重试多半还是一样 |
//! | [`Class::Rejected`] | 其余 4xx | 否 | 请求本身不被接受 |
//!
//! 重试：最多 [`RETRIES`] 次，等 0.5 秒、1.5 秒；回包带 `Retry-After`（秒）就按它，最多等 5 秒。
//! 连不上（请求没发出去）也重试；超时不重试（已经等过一整个超时）。

use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    Ok,
    Auth,
    Denied,
    Missing,
    Busy,
    Failed,
    Rejected,
}

pub fn classify(code: u16) -> Class {
    match code {
        200..=399 => Class::Ok,
        401 => Class::Auth,
        403 => Class::Denied,
        404 | 410 => Class::Missing,
        408 | 425 | 429 | 502..=504 => Class::Busy,
        500..=599 => Class::Failed,
        _ => Class::Rejected,
    }
}

/// 第一次之外最多再试几次。
pub const RETRIES: usize = 2;
const BACKOFF_MS: [u64; RETRIES] = [500, 1500];
const MAX_WAIT: Duration = Duration::from_secs(5);

/// 第 `attempt` 次重试（从 0 起）前等多久。
pub fn backoff(attempt: usize, retry_after: Option<Duration>) -> Duration {
    retry_after.map_or_else(
        || Duration::from_millis(BACKOFF_MS[attempt.min(RETRIES - 1)]),
        |d| d.min(MAX_WAIT),
    )
}

/// `Retry-After` 的秒数写法（HTTP 日期写法很少见，不认）。
pub fn retry_after(h: &reqwest::header::HeaderMap) -> Option<Duration> {
    h.get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()
        .map(Duration::from_secs)
}

/// 这次结果值不值得重试；值得的话返回服务器要求等的时间（没要求为 None）。
fn retryable(r: &reqwest::Result<reqwest::Response>) -> Option<Option<Duration>> {
    match r {
        Ok(resp) if classify(resp.status().as_u16()) == Class::Busy => {
            Some(retry_after(resp.headers()))
        }
        Err(e) if e.is_connect() => Some(None),
        _ => None,
    }
}

/// 发请求，暂时性失败按上面的规则重试。`build` 每次新建请求（RequestBuilder 发出去就没了）。
/// 返回最后一次的结果和重试了几次。
pub async fn send(
    build: impl Fn() -> reqwest::RequestBuilder,
) -> (reqwest::Result<reqwest::Response>, usize) {
    let mut n = 0;
    loop {
        let r = build().send().await;
        match retryable(&r) {
            Some(wait) if n < RETRIES => {
                tokio::time::sleep(backoff(n, wait)).await;
                n += 1;
            }
            _ => return (r, n),
        }
    }
}

/// 给人看的说明。`detail` 是回包正文：只有简短的纯文本才附上（HTML 错误页、JSON 不展示）。
pub fn describe(code: u16, detail: &str) -> String {
    let what = match classify(code) {
        Class::Ok => "成功",
        Class::Auth => "未登录或登录已失效",
        Class::Denied => "服务器拒绝访问",
        Class::Missing => "服务器上找不到",
        Class::Busy if code == 429 => "请求太频繁，服务器限流了，稍后再试",
        Class::Busy => "服务器暂时不可用，稍后再试",
        Class::Failed => "服务器内部出错",
        Class::Rejected => "请求被服务器拒绝",
    };
    let d = detail.trim();
    let plain = !d.is_empty() && d.chars().count() <= 120 && !d.starts_with(['<', '{', '[']);
    if plain {
        format!("{what}（{code}）：{d}")
    } else {
        format!("{what}（{code}）")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn classes_and_text() {
        assert_eq!(classify(204), Class::Ok);
        assert_eq!(classify(401), Class::Auth);
        assert_eq!(classify(410), Class::Missing);
        for c in [408, 425, 429, 502, 503, 504] {
            assert_eq!(classify(c), Class::Busy, "{c}");
        }
        assert_eq!(classify(500), Class::Failed);
        assert_eq!(classify(418), Class::Rejected);
        assert_eq!(
            describe(502, "<html>bad gateway</html>"),
            "服务器暂时不可用，稍后再试（502）"
        );
        assert_eq!(
            describe(400, "Value cannot be null."),
            "请求被服务器拒绝（400）：Value cannot be null."
        );
        assert_eq!(describe(404, "{\"a\":1}"), "服务器上找不到（404）");
        assert_eq!(backoff(0, None), Duration::from_millis(500));
        assert_eq!(backoff(5, None), Duration::from_millis(1500));
        assert_eq!(backoff(0, Some(Duration::from_secs(60))), MAX_WAIT);
    }

    /// 前两次 503（第一次带 Retry-After: 0）、第三次 200：重试两次后成功；一直 404 不重试。
    #[tokio::test]
    async fn retries_only_transient() {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            let mut n = 0;
            while let Ok((mut s, _)) = l.accept().await {
                let mut buf = [0u8; 1024];
                let len = s.read(&mut buf).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..len]).to_string();
                n += 1;
                let resp: &[u8] = if head.starts_with("GET /missing") {
                    b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                } else if n == 1 {
                    b"HTTP/1.1 503 Busy\r\nretry-after: 0\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                } else if n == 2 {
                    b"HTTP/1.1 502 Bad\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                } else {
                    b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok"
                };
                let _ = s.write_all(resp).await;
            }
        });
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let (r, n) = send(|| http.get(format!("http://{addr}/x"))).await;
        assert_eq!((r.unwrap().status().as_u16(), n), (200, 2));
        let (r, n) = send(|| http.get(format!("http://{addr}/missing"))).await;
        assert_eq!((r.unwrap().status().as_u16(), n), (404, 0));
    }
}
