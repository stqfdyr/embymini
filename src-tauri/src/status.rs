//! One way to handle HTTP replies from outside: Emby API, images, the video / subtitle cache proxy and the danmaku
//! API share the same status classes, retries and wording.
//!
//! | Class | Codes | Retry | Meaning |
//! |---|---|---|---|
//! | [`Class::Auth`] | 401 | no | credentials gone: Emby clears the token and asks to sign in; danmaku asks to check the API URL |
//! | [`Class::Denied`] | 403 | no | access denied |
//! | [`Class::Missing`] | 404, 410 | no | not there |
//! | [`Class::Busy`] | 408, 425, 429, 502, 503, 504, 520–524 | yes (not 408, 504) | upstream, reverse proxy or relay (incl. Cloudflare origin errors) briefly down or rate limiting |
//! | [`Class::Failed`] | other 5xx | no | the server itself failed; an immediate retry likely fails the same way |
//! | [`Class::Rejected`] | other 4xx | no | the request itself is refused |
//!
//! Retries: at most [`RETRIES`], waiting 0.5 s then 1.5 s, or the reply's `Retry-After` (seconds), capped at 5 s.
//! Unreachable (nothing sent) is retried too; timeouts, 408 and 504 are not (a whole timeout has already passed).
//! Non-idempotent requests (POST) retry only when unreachable or on 429: otherwise the server may already have
//! acted on them. Read-only POSTs use [`send_readonly`].
//! The video cache proxy has its own retries (`stream.rs`: any 5xx is transient, chunks retry indefinitely).

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
        408 | 425 | 429 | 502..=504 | 520..=524 => Class::Busy,
        500..=599 => Class::Failed,
        _ => Class::Rejected,
    }
}

/// Retries after the first attempt.
pub const RETRIES: usize = 2;
const BACKOFF_MS: [u64; RETRIES] = [500, 1500];
const MAX_WAIT: Duration = Duration::from_secs(5);
/// Longest reply body (chars) [`describe`] shows; Emby's 4xx bodies are usually one short sentence.
pub const DETAIL_MAX: usize = 120;

/// Wait before retry number `attempt` (from 0).
pub fn backoff(attempt: usize, retry_after: Option<Duration>) -> Duration {
    retry_after.map_or_else(
        || Duration::from_millis(BACKOFF_MS[attempt.min(RETRIES - 1)]),
        |d| d.min(MAX_WAIT),
    )
}

/// `Retry-After` in seconds (the rare HTTP-date form is ignored).
pub fn retry_after(h: &reqwest::header::HeaderMap) -> Option<Duration> {
    h.get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()
        .map(Duration::from_secs)
}

/// Whether this status code is worth a retry (rules in the module docs).
pub fn retry_code(code: u16, idempotent: bool) -> bool {
    classify(code) == Class::Busy && !matches!(code, 408 | 504) && (idempotent || code == 429)
}

/// Whether this result is worth a retry; if so, the wait the server asked for (`None` = no request).
pub fn retry_wait(
    r: &reqwest::Result<reqwest::Response>,
    idempotent: bool,
) -> Option<Option<Duration>> {
    match r {
        Ok(resp) => {
            retry_code(resp.status().as_u16(), idempotent).then(|| retry_after(resp.headers()))
        }
        Err(e) => e.is_connect().then_some(None),
    }
}

/// Sends, retrying transient failures per the rules above (idempotency from the method). `build` makes a fresh
/// request each time (a sent RequestBuilder is gone). Returns the last result and the number of retries.
pub async fn send(
    build: impl Fn() -> reqwest::RequestBuilder,
) -> (reqwest::Result<reqwest::Response>, usize) {
    send_as(build, false).await
}

/// Like [`send`] for a read-only request that is safe to resend (a POST query, login): retried as idempotent.
pub async fn send_readonly(
    build: impl Fn() -> reqwest::RequestBuilder,
) -> (reqwest::Result<reqwest::Response>, usize) {
    send_as(build, true).await
}

async fn send_as(
    build: impl Fn() -> reqwest::RequestBuilder,
    readonly: bool,
) -> (reqwest::Result<reqwest::Response>, usize) {
    let mut n = 0;
    loop {
        let (client, req) = build().build_split();
        let req = match req {
            Ok(req) => req,
            Err(e) => return (Err(e), n),
        };
        let idempotent = readonly || req.method().is_idempotent();
        let r = client.execute(req).await;
        match retry_wait(&r, idempotent) {
            Some(wait) if n < RETRIES => {
                tokio::time::sleep(backoff(n, wait)).await;
                n += 1;
            }
            _ => return (r, n),
        }
    }
}

/// Readable text for users. `detail` is the reply body: shown only when it is short plain text (not HTML or JSON).
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
    let plain = !d.is_empty() && d.chars().count() <= DETAIL_MAX && !d.starts_with(['<', '{', '[']);
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
        assert_eq!(classify(403), Class::Denied);
        assert_eq!(classify(410), Class::Missing);
        for c in [408, 425, 429, 502, 503, 504, 522] {
            assert_eq!(classify(c), Class::Busy, "{c}");
        }
        assert!(retry_code(503, true) && retry_code(429, false));
        assert!(!retry_code(503, false), "POST is not resent on 503");
        assert!(
            !retry_code(504, true) && !retry_code(408, true),
            "timeouts are not retried"
        );
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
        assert_eq!(
            describe(429, ""),
            "请求太频繁，服务器限流了，稍后再试（429）"
        );
        let mut h = reqwest::header::HeaderMap::new();
        assert_eq!(retry_after(&h), None);
        h.insert(reqwest::header::RETRY_AFTER, " 2 ".parse().unwrap());
        assert_eq!(retry_after(&h), Some(Duration::from_secs(2)));
        h.insert(
            reqwest::header::RETRY_AFTER,
            "Wed, 21 Oct 2015 07:28:00 GMT".parse().unwrap(),
        );
        assert_eq!(retry_after(&h), None, "HTTP-date form is ignored");
        assert_eq!(backoff(0, None), Duration::from_millis(500));
        assert_eq!(backoff(5, None), Duration::from_millis(1500));
        assert_eq!(backoff(0, Some(Duration::from_secs(60))), MAX_WAIT);
    }

    /// 503 (with Retry-After: 0), 502, then 200: succeeds after two retries; 404 is not retried; POST is not resent on 502.
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
                } else if head.starts_with("POST") {
                    b"HTTP/1.1 502 Bad\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
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
        let (r, n) = send(|| http.post(format!("http://{addr}/p"))).await;
        assert_eq!((r.unwrap().status().as_u16(), n), (502, 0));
        let (r, n) = send_readonly(|| http.post(format!("http://{addr}/p"))).await;
        assert_eq!(
            (r.unwrap().status().as_u16(), n),
            (502, 2),
            "read-only POST is retried"
        );
    }
}
