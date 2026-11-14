//! Emby HTTP: auth headers, UA, the generic request, and the few typed endpoints playback needs.
//! Browsing data is only used by the UI, so it has no DTOs here (the UI gets raw JSON via the `emby` command).

use crate::config::Server;
use crate::status;
use reqwest::{Client, Method, StatusCode};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize, Serializer, ser::SerializeStruct};
use serde_json::Value;
use std::time::{Duration, Instant};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Our own UA, never impersonating another client; per-server UA rules belong in the reverse proxy.
pub const UA: &str = concat!("EmbyMini/", env!("CARGO_PKG_VERSION"));
/// Requests slower than this are logged, to tell whether a slow start or page is waiting on upstream.
const SLOW: Duration = Duration::from_secs(3);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Whole-request timeout; timeouts are not retried (see [`status`]).
const API_TIMEOUT: Duration = Duration::from_secs(30);
/// 400 Mbps: high enough that the server never asks to transcode, so the original file is played.
const MAX_BITRATE: &str = "400000000";
/// Pass to [`Emby::call`] when there is no query.
pub const NO_QUERY: &[(&str, &str)] = &[];

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("未登录或登录已失效")]
    Unauthorized,
    /// Status code + start of the body; wording in [`status::describe`].
    #[error("{}", status::describe(*.0, .1))]
    Status(u16, String),
    /// Already worded by kind (unreachable / timeout / TLS), see [`net`].
    #[error("{0}")]
    Network(String),
    #[error("{0}")]
    Other(String),
}

/// Serialized as `{kind, message}`; the UI keys off `kind` (e.g. `unauthorized` → sign in again).
impl Serialize for Error {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let kind = match self {
            Self::Unauthorized => "unauthorized",
            Self::Status(..) => "status",
            Self::Network(_) => "network",
            Self::Other(_) => "other",
        };
        let mut m = s.serialize_struct("Error", 2)?;
        m.serialize_field("kind", kind)?;
        m.serialize_field("message", &self.to_string())?;
        m.end()
    }
}

/// Network error → readable error: timeout, TLS and unreachable worded apart, with the source chain in brackets
/// (reqwest's top level only says "error sending request"). The top level is skipped: it holds the full URL and query.
pub(crate) fn net(e: &reqwest::Error) -> Error {
    let mut chain = Vec::new();
    let mut src = std::error::Error::source(e);
    while let Some(s) = src {
        chain.push(s.to_string());
        src = s.source();
    }
    let detail = chain.join("：");
    let lower = detail.to_ascii_lowercase();
    let what = if e.is_timeout() {
        "服务器响应超时"
    } else if ["certificate", "tls", "ssl"]
        .iter()
        .any(|k| lower.contains(k))
    {
        "证书或 TLS 握手失败"
    } else if e.is_connect() {
        "连不上服务器"
    } else {
        "网络错误"
    };
    Error::Network(if detail.is_empty() {
        what.into()
    } else {
        format!("{what}（{detail}）")
    })
}

/// Runs `send` (a [`status::send`] call), decodes the reply and logs failures, retries and slow calls.
/// Logs only method and path: never the query, token or password.
async fn request(
    method: &str,
    path: &str,
    send: impl Future<Output = (reqwest::Result<reqwest::Response>, usize)>,
) -> Result<Value, Error> {
    let t0 = Instant::now();
    let (sent, retries) = send.await;
    let r = match sent {
        Err(e) => Err(net(&e)),
        Ok(resp) => json_or_error(resp).await,
    };
    let took = t0.elapsed();
    match &r {
        Err(e) => log::warn!("{method} {path} 失败（{took:.1?}，重试 {retries} 次）：{e}"),
        Ok(_) if retries > 0 => log::info!("{method} {path} 重试 {retries} 次后成功（{took:.1?}）"),
        Ok(_) if took >= SLOW => log::info!("{method} {path} 慢请求：{took:.1?}"),
        Ok(_) => {}
    }
    r
}

/// Always direct, never the system proxy: reqwest reads it once when the client is built, so after the proxy
/// app is switched off or moves port, requests keep failing until restart.
pub fn http_client() -> reqwest::Result<Client> {
    Client::builder()
        .no_proxy()
        .user_agent(UA)
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(API_TIMEOUT)
        .build()
}

fn device_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "PC".into())
}

fn auth_header(device_id: &str, user_id: &str, token: &str) -> String {
    let mut h = format!(
        r#"Emby UserId="{user_id}", Client="EmbyMini", Device="{}", DeviceId="{device_id}", Version="{VERSION}""#,
        device_name()
    );
    if !token.is_empty() {
        h.push_str(&format!(r#", Token="{token}""#));
    }
    h
}

/// Reply → JSON. 401 is decided before reading the body; an empty body (204) is `Null`.
async fn json_or_error(resp: reqwest::Response) -> Result<Value, Error> {
    let status = resp.status();
    if status == StatusCode::UNAUTHORIZED {
        return Err(Error::Unauthorized);
    }
    let bytes = resp.bytes().await.map_err(|e| net(&e))?;
    if !status.is_success() {
        // Trimmed first (as `describe` does); one char over the limit tells it the body is too long
        let text = String::from_utf8_lossy(&bytes)
            .trim()
            .chars()
            .take(status::DETAIL_MAX + 1)
            .collect();
        Err(Error::Status(status.as_u16(), text))
    } else if bytes.is_empty() {
        Ok(Value::Null)
    } else {
        serde_json::from_slice(&bytes).map_err(|e| Error::Other(format!("回包不是 JSON：{e}")))
    }
}

/// Generic JSON → typed value; `what` goes into the error to show which endpoint changed shape.
fn decode<T: DeserializeOwned>(what: &str, v: Value) -> Result<T, Error> {
    serde_json::from_value(v).map_err(|e| Error::Other(format!("{what} 回包格式不符：{e}")))
}

/// Server name (`/System/Info/Public`, no sign-in needed); adding a server uses it to check the address.
pub async fn server_name(http: &Client, base: &str) -> Result<String, Error> {
    const PATH: &str = "/System/Info/Public";
    let v = request(
        "GET",
        PATH,
        status::send(|| http.get(format!("{base}{PATH}"))),
    )
    .await?;
    Ok(v["ServerName"].as_str().unwrap_or("Emby").to_owned())
}

/// User name + password → `(user_id, token)`.
pub async fn login(
    http: &Client,
    base: &str,
    device_id: &str,
    user: &str,
    password: &str,
) -> Result<(String, String), Error> {
    const PATH: &str = "/Users/AuthenticateByName";
    // A resend creates at most one extra session: harmless
    let sent = status::send_readonly(|| {
        http.post(format!("{base}{PATH}"))
            .header("X-Emby-Authorization", auth_header(device_id, "", ""))
            .json(&serde_json::json!({ "Username": user, "Pw": password }))
    });
    let v = match request("POST", PATH, sent).await {
        Err(Error::Unauthorized) => return Err(Error::Other("用户名或密码错误".into())),
        r => r?,
    };
    match (v["User"]["Id"].as_str(), v["AccessToken"].as_str()) {
        (Some(uid), Some(token)) if !uid.is_empty() && !token.is_empty() => {
            Ok((uid.to_owned(), token.to_owned()))
        }
        _ => Err(Error::Other("登录回包缺少 User.Id / AccessToken".into())),
    }
}

/// A signed-in server.
#[derive(Clone)]
pub struct Emby {
    http: Client,
    base: String,
    auth: String,
    token: String,
    pub user_id: String,
    /// Upstream tag ([`crate::config::server_tag`]): cache file names and danmaku picks are keyed by it.
    pub tag: String,
}

impl Emby {
    pub fn new(http: Client, server: &Server, device_id: &str) -> Self {
        Self {
            http,
            base: server.api_base(),
            auth: auth_header(device_id, &server.user_id, &server.token),
            token: server.token.clone(),
            user_id: server.user_id.clone(),
            tag: crate::config::server_tag(&server.api_base()),
        }
    }

    /// Generic request. `path` is the part after `/emby` and starts with `/`; the query is passed separately.
    /// Transient failures are retried per [`status`].
    pub async fn call<Q: Serialize + ?Sized>(
        &self,
        method: Method,
        path: &str,
        query: &Q,
        body: Option<&Value>,
    ) -> Result<Value, Error> {
        let url = api_url(&self.base, path)?;
        let sent = status::send(|| {
            let rb = self
                .http
                .request(method.clone(), url.clone())
                .header("X-Emby-Authorization", &self.auth)
                .header("X-Emby-Token", &self.token)
                .query(query);
            match body {
                Some(b) => rb.json(b),
                None => rb,
            }
        });
        request(method.as_str(), path, sent).await
    }

    async fn get<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<T, Error> {
        decode(path, self.call(Method::GET, path, query, None).await?)
    }

    pub async fn item(&self, id: &str) -> Result<Item, Error> {
        self.get(&format!("/Users/{}/Items/{id}", self.user_id), NO_QUERY)
            .await
    }

    /// All episodes of a season, already in episode order.
    pub async fn episodes(&self, series_id: &str, season_id: &str) -> Result<Vec<Item>, Error> {
        let r: Items = self
            .get(
                &format!("/Shows/{series_id}/Episodes"),
                &[("UserId", &self.user_id), ("SeasonId", season_id)],
            )
            .await?;
        Ok(r.items)
    }

    /// `source`: the version picked on the detail page (1080p, 4K, …); `None` = the server's first.
    pub async fn playback_info(
        &self,
        id: &str,
        source: Option<&str>,
    ) -> Result<PlaybackInfo, Error> {
        let mut q = vec![
            ("UserId", self.user_id.as_str()),
            ("StartTimeTicks", "0"),
            ("IsPlayback", "true"),
            ("AutoOpenLiveStream", "false"),
            ("MaxStreamingBitrate", MAX_BITRATE),
        ];
        if let Some(s) = source {
            q.push(("MediaSourceId", s));
        }
        let v = self
            .call(
                Method::POST,
                &format!("/Items/{id}/PlaybackInfo"),
                &q,
                Some(&Value::Object(Default::default())),
            )
            .await?;
        let mut info: PlaybackInfo = decode("PlaybackInfo", v)?;
        if let Some(s) = source.filter(|s| !info.prefer(s)) {
            log::warn!("PlaybackInfo 里没有选中的版本 {s}，改播默认版本");
        }
        Ok(info)
    }

    /// Direct URL: PlaybackInfo's `DirectStreamUrl` (a reverse proxy may rewrite it), else the standard form.
    pub fn stream_url(&self, item_id: &str, src: &MediaSource) -> String {
        let url = match src.direct_stream_url.as_deref().filter(|u| !u.is_empty()) {
            Some(u) => self.abs(u),
            None => format!(
                "{}/Videos/{item_id}/stream.{}?Static=true&MediaSourceId={}",
                self.base,
                src.container.as_deref().unwrap_or("mkv"),
                src.id
            ),
        };
        strip_key(url)
    }

    /// External subtitle URL. `DeliveryUrl` is usually null in practice, so the standard form is built.
    pub fn subtitle_url(&self, item_id: &str, src: &MediaSource, st: &MediaStream) -> String {
        let url = match st.delivery_url.as_deref().filter(|u| !u.is_empty()) {
            Some(u) => self.abs(u),
            None => format!(
                "{}/Videos/{item_id}/{}/Subtitles/{}/Stream.{}",
                self.base,
                src.id,
                st.index,
                subtitle_ext(st.codec.as_deref())
            ),
        };
        strip_key(url)
    }

    fn abs(&self, u: &str) -> String {
        if u.starts_with("http://") || u.starts_with("https://") {
            u.to_owned()
        } else if u.starts_with('/') {
            format!("{}{u}", self.base)
        } else {
            format!("{}/{u}", self.base)
        }
    }

    /// Only when playback failed without the cache proxy (the proxy records its own status): fetch the first byte
    /// of the direct URL, following redirects, to tell the user why.
    pub async fn probe(&self, url: &str) -> Option<u16> {
        let r = self
            .http
            .get(url)
            .header("X-Emby-Token", &self.token)
            .header("Range", "bytes=0-0")
            .send()
            .await
            .ok()?;
        Some(r.status().as_u16())
    }

    /// For the cache proxy: the token goes only to hosts with the API root's origin.
    pub fn stream_auth(&self) -> crate::stream::Auth {
        crate::stream::Auth {
            token: self.token.clone(),
            origin: self.base.clone(),
        }
    }

    /// mpv's header when there is no cache proxy: the token travels in a header, not the URL (see [`strip_key`]).
    pub fn token_header(&self) -> String {
        format!("X-Emby-Token: {}", self.token)
    }
}

/// Drops the server's own `api_key` from direct URLs: the token is added as a header by the cache proxy (or mpv
/// without it, [`Emby::token_header`]) and so never shows in mpv's playlist, stats page (`i`), logs or screenshots.
/// Without the proxy, mpv also sends that header when the URL redirects to another host (the proxy does not).
fn strip_key(url: String) -> String {
    let Ok(mut u) = reqwest::Url::parse(&url) else {
        return url;
    };
    let kept: Vec<(String, String)> = u
        .query_pairs()
        .filter(|(k, _)| !k.eq_ignore_ascii_case("api_key"))
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    if kept.is_empty() {
        u.set_query(None);
    } else {
        u.query_pairs_mut().clear().extend_pairs(kept);
    }
    u.into()
}

pub fn subtitle_ext(codec: Option<&str>) -> String {
    match codec.map(str::to_ascii_lowercase).as_deref() {
        None | Some("subrip") => "srt".into(),
        Some("pgssub") => "sup".into(),
        Some(c) => c.to_owned(),
    }
}

/// `base` + a path from the UI, which must stay under the API root with no query or fragment.
/// Checked after parsing: the parser turns `%2e%2e`, `\` and dots split by tab / newline into `..` and `/`.
fn api_url(base: &str, path: &str) -> Result<reqwest::Url, Error> {
    let bad = || Error::Other(format!("非法路径：{path}"));
    let root = reqwest::Url::parse(base).map_err(|_| bad())?;
    let url = reqwest::Url::parse(&format!("{base}{path}")).map_err(|_| bad())?;
    let under = format!("{}/", root.path().trim_end_matches('/'));
    if url.origin() != root.origin()
        || !url.path().starts_with(&under)
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(bad());
    }
    Ok(url)
}

// ---- Fields playback uses (per the real replies in testdata/; missing fields default) ----

#[derive(Deserialize, Default, Debug, Clone)]
#[serde(rename_all = "PascalCase", default)]
pub struct Items {
    pub items: Vec<Item>,
}

#[derive(Deserialize, Default, Debug, Clone)]
#[serde(rename_all = "PascalCase", default)]
pub struct Item {
    pub id: String,
    pub name: String,
    #[serde(rename = "Type")]
    pub kind: String,
    pub series_id: Option<String>,
    pub season_id: Option<String>,
    pub series_name: Option<String>,
    pub index_number: Option<i32>,
    pub parent_index_number: Option<i32>,
    pub run_time_ticks: Option<i64>,
    /// Movie year; danmaku matches on "title (year)".
    pub production_year: Option<i32>,
    pub user_data: UserData,
}

#[derive(Deserialize, Default, Debug, Clone)]
#[serde(rename_all = "PascalCase", default)]
pub struct UserData {
    pub playback_position_ticks: i64,
    pub played: bool,
}

#[derive(Deserialize, Default, Debug, Clone)]
#[serde(rename_all = "PascalCase", default)]
pub struct PlaybackInfo {
    pub play_session_id: Option<String>,
    pub media_sources: Vec<MediaSource>,
    pub error_code: Option<String>,
}

impl PlaybackInfo {
    /// Moves the chosen version first (playback uses the first). Servers should return only that one for
    /// `MediaSourceId`, but some ignore it and return all. Returns false if it is not there.
    fn prefer(&mut self, source: &str) -> bool {
        let i = self.media_sources.iter().position(|m| m.id == source);
        if let Some(i) = i {
            self.media_sources.swap(0, i);
        }
        i.is_some()
    }
}

#[derive(Deserialize, Default, Debug, Clone)]
#[serde(rename_all = "PascalCase", default)]
pub struct MediaSource {
    pub id: String,
    pub container: Option<String>,
    pub direct_stream_url: Option<String>,
    pub default_audio_stream_index: Option<i32>,
    pub default_subtitle_stream_index: Option<i32>,
    pub media_streams: Vec<MediaStream>,
}

#[derive(Deserialize, Default, Debug, Clone)]
#[serde(rename_all = "PascalCase", default)]
pub struct MediaStream {
    pub index: i32,
    #[serde(rename = "Type")]
    pub kind: String,
    pub codec: Option<String>,
    pub language: Option<String>,
    pub display_title: Option<String>,
    pub title: Option<String>,
    pub is_external: bool,
    pub delivery_url: Option<String>,
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::path::PathBuf;

    pub fn fixture<T: DeserializeOwned>(name: &str) -> T {
        let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("testdata")
            .join(name);
        serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap()
    }

    pub fn client() -> Emby {
        let s = Server {
            id: "s".into(),
            name: String::new(),
            url: "https://h.example".into(),
            port: 443,
            user: "u".into(),
            user_id: "uid".into(),
            token: "tok".into(),
        };
        Emby::new(Client::new(), &s, "dev")
    }

    #[test]
    fn typed_fixtures_parse() {
        let pi: PlaybackInfo = fixture("playbackinfo.json");
        assert!(pi.play_session_id.is_some());
        let src = &pi.media_sources[0];
        assert!(
            src.direct_stream_url
                .as_deref()
                .unwrap()
                .starts_with("/videos/")
        );
        assert!(
            src.media_streams
                .iter()
                .any(|s| s.kind == "Subtitle" && s.is_external)
        );
        let eps: Items = fixture("episodes.json");
        assert!(
            eps.items
                .iter()
                .all(|e| e.kind == "Episode" && e.series_id.is_some() && e.season_id.is_some())
        );
        let series: Item = fixture("series.json");
        assert_eq!(series.kind, "Series");
    }

    #[test]
    fn urls() {
        let c = client();
        let pi: PlaybackInfo = fixture("playbackinfo.json");
        let src = &pi.media_sources[0];
        let u = c.stream_url("1", src);
        assert!(u.starts_with("https://h.example/emby/videos/"), "{u}");
        assert!(
            !u.contains("api_key"),
            "the server's own api_key must be stripped: {u}"
        );

        let bare = MediaSource {
            id: "ms1".into(),
            container: Some("mp4".into()),
            ..Default::default()
        };
        assert_eq!(
            c.stream_url("9", &bare),
            "https://h.example/emby/Videos/9/stream.mp4?Static=true&MediaSourceId=ms1"
        );

        let ext = src.media_streams.iter().find(|s| s.is_external).unwrap();
        let su = c.subtitle_url("1", src, ext);
        assert!(
            su.ends_with(&format!("/Subtitles/{}/Stream.ass", ext.index)),
            "{su}"
        );
        assert_eq!(
            strip_key("https://h/emby/v?A=1&api_key=x&B=2".into()),
            "https://h/emby/v?A=1&B=2"
        );
        assert_eq!(
            strip_key("https://h/emby/v?api_key=x".into()),
            "https://h/emby/v"
        );
        let given = MediaStream {
            delivery_url: Some("/Videos/1/s.srt?api_key=k".into()),
            ..ext.clone()
        };
        assert_eq!(
            c.subtitle_url("1", src, &given),
            "https://h.example/emby/Videos/1/s.srt"
        );
        let cdn = MediaSource {
            direct_stream_url: Some("https://cdn.example/v.mkv?api_key=k&t=1".into()),
            ..bare.clone()
        };
        assert_eq!(c.stream_url("9", &cdn), "https://cdn.example/v.mkv?t=1");
        assert_eq!(c.token_header(), "X-Emby-Token: tok");
        assert_eq!(subtitle_ext(Some("subrip")), "srt");
        assert_eq!(subtitle_ext(Some("PGSSUB")), "sup");
    }

    #[test]
    fn chosen_version_goes_first() {
        let src = |id: &str| MediaSource {
            id: id.into(),
            ..Default::default()
        };
        let mut pi = PlaybackInfo {
            play_session_id: None,
            media_sources: vec![src("1080"), src("2160")],
            error_code: None,
        };
        assert!(pi.prefer("2160"));
        assert_eq!(pi.media_sources[0].id, "2160");
        assert!(!pi.prefer("gone")); // unknown id: unchanged
        assert_eq!(pi.media_sources[0].id, "2160");
    }

    #[test]
    fn path_guard() {
        let base = "https://h.example:443/emby";
        for ok in ["/Users/x/Views", "/Persons/Tom%20Hanks", "/a/../b"] {
            assert!(api_url(base, ok).is_ok(), "{ok}");
        }
        for bad in [
            "Users",
            "/../x",
            "https://evil/x",
            "@evil/x",
            "/a?b=1",
            "/a#x",
            "/%2e%2e/x",
            "/..\\x",
            "/.\t./x",
            "/.\n./x",
        ] {
            assert!(api_url(base, bad).is_err(), "{bad:?}");
        }
    }

    /// Unreachable and timeout are worded apart; HTML bodies of 5xx are not shown.
    #[tokio::test]
    async fn network_errors_are_classified() {
        let http = http_client().unwrap();
        // Nothing listens on port 1: connection refused
        let e = server_name(&http, "http://127.0.0.1:1/emby")
            .await
            .unwrap_err();
        assert!(e.to_string().starts_with("连不上服务器（"), "{e}");
        // Accepts but never answers: timeout
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let slow = Client::builder()
            .no_proxy()
            .timeout(Duration::from_millis(300))
            .build()
            .unwrap();
        let base = format!("http://{}/emby", l.local_addr().unwrap());
        let e = server_name(&slow, &base).await.unwrap_err();
        assert!(e.to_string().starts_with("服务器响应超时"), "{e}");
        assert_eq!(
            Error::Status(502, "<html>".into()).to_string(),
            "服务器暂时不可用，稍后再试（502）"
        );
        assert_eq!(
            Error::Status(404, "x".into()).to_string(),
            "服务器上找不到（404）：x"
        );
    }

    type Heads = std::sync::Arc<std::sync::Mutex<Vec<String>>>;

    /// Local HTTP server: `route(path)` → (status line, body), the path lowercased. Returns its port and every request head, lowercased.
    async fn fake(route: fn(&str) -> (&'static str, String)) -> (u16, Heads) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        let heads = Heads::default();
        let seen = heads.clone();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                let mut buf = [0u8; 4096];
                let n = s.read(&mut buf).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).to_lowercase();
                let target = head.split(' ').nth(1).unwrap_or_default();
                let (status, body) = route(target.split('?').next().unwrap_or_default());
                seen.lock().unwrap().push(head);
                let resp = format!(
                    "HTTP/1.1 {status}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(resp.as_bytes()).await;
            }
        });
        (port, heads)
    }

    fn local(port: u16) -> Emby {
        let server = Server {
            id: "s".into(),
            name: String::new(),
            url: "http://127.0.0.1".into(),
            port,
            user: "u".into(),
            user_id: "uid".into(),
            token: "tok".into(),
        };
        Emby::new(http_client().unwrap(), &server, "dev")
    }

    fn testdata(name: &str) -> String {
        std::fs::read_to_string(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("testdata")
                .join(name),
        )
        .unwrap()
    }

    /// `call` turns each kind of reply into the right result and sends our UA and auth headers.
    #[tokio::test]
    async fn call_maps_replies() {
        let (port, heads) = fake(|path| match path {
            "/emby/unauth" => ("401 Unauthorized", String::new()),
            "/emby/empty" => ("204 No Content", String::new()),
            "/emby/html" => ("200 OK", "<html>".into()),
            "/emby/denied" => ("403 Forbidden", "Access denied".into()),
            "/emby/long" => ("400 Bad Request", format!("\n{}", "x".repeat(300))),
            _ => ("200 OK", r#"{"a":1}"#.into()),
        })
        .await;
        let c = local(port);
        let call = |p: &'static str| c.call(Method::GET, p, NO_QUERY, None);
        assert_eq!(call("/ok").await.unwrap()["a"], 1);
        assert_eq!(call("/empty").await.unwrap(), Value::Null);
        assert!(matches!(call("/unauth").await, Err(Error::Unauthorized)));
        assert!(
            matches!(call("/html").await, Err(Error::Other(ref m)) if m.starts_with("回包不是 JSON"))
        );
        assert!(matches!(
            call("/denied").await,
            Err(Error::Status(403, ref d)) if d == "Access denied"
        ));
        // A long body is never shown cut off, even after leading whitespace
        let e = call("/long").await.unwrap_err().to_string();
        assert_eq!(e, "请求被服务器拒绝（400）");

        let head = heads.lock().unwrap()[0].clone();
        // Independent of `device_name` so a broken lookup fails here
        let host = std::env::var("COMPUTERNAME")
            .or_else(|_| std::env::var("HOSTNAME"))
            .unwrap_or_else(|_| "PC".into());
        let device = format!(r#"device="{host}""#).to_lowercase();
        for want in [
            format!("user-agent: {}", UA.to_lowercase()),
            r#"x-emby-authorization: emby userid="uid", client="embymini""#.into(),
            device,
            r#"deviceid="dev""#.into(),
            r#"token="tok""#.into(),
            "x-emby-token: tok".into(),
        ] {
            assert!(head.contains(&want), "missing {want:?} in\n{head}");
        }
    }

    /// Login returns the user id and token, sends no token itself, and words a 401 as a bad password.
    #[tokio::test]
    async fn login_reads_user_and_token() {
        let (port, heads) = fake(|path| match path {
            "/ok/users/authenticatebyname" => (
                "200 OK",
                r#"{"User":{"Id":"u1"},"AccessToken":"t1"}"#.into(),
            ),
            "/denied/users/authenticatebyname" => ("401 Unauthorized", String::new()),
            _ => ("200 OK", r#"{"User":{"Id":"u1"},"AccessToken":""}"#.into()),
        })
        .await;
        let http = http_client().unwrap();
        let at = |p: &str| format!("http://127.0.0.1:{port}/{p}");
        let r = login(&http, &at("ok"), "dev", "u", "pw").await.unwrap();
        assert_eq!(r, ("u1".to_owned(), "t1".to_owned()));
        let head = heads.lock().unwrap()[0].clone();
        assert!(
            head.contains(r#"deviceid="dev""#) && !head.contains("token"),
            "{head}"
        );
        let e = login(&http, &at("denied"), "dev", "u", "pw")
            .await
            .unwrap_err();
        assert_eq!(e.to_string(), "用户名或密码错误");
        let e = login(&http, &at("empty"), "dev", "u", "pw")
            .await
            .unwrap_err();
        assert!(e.to_string().contains("AccessToken"), "{e}");
    }

    /// Typed endpoints parse real replies; PlaybackInfo asks for the original file and the chosen version.
    #[tokio::test]
    async fn typed_endpoints() {
        let (port, heads) = fake(|path| match path {
            "/emby/users/uid/items/1" => ("200 OK", testdata("series.json")),
            "/emby/shows/s/episodes" => ("200 OK", testdata("episodes.json")),
            "/emby/items/1/playbackinfo" => ("200 OK", testdata("playbackinfo.json")),
            _ => ("403 Forbidden", String::new()),
        })
        .await;
        let c = local(port);
        assert_eq!(c.item("1").await.unwrap().kind, "Series");
        let eps = c.episodes("s", "x").await.unwrap();
        assert!(eps.len() > 1 && eps.iter().all(|e| e.kind == "Episode" && !e.id.is_empty()));
        let pi = c.playback_info("1", Some("ms")).await.unwrap();
        assert!(pi.play_session_id.is_some() && !pi.media_sources.is_empty());
        let head = heads.lock().unwrap().last().unwrap().clone();
        assert!(
            head.contains("maxstreamingbitrate=400000000") && head.contains("mediasourceid=ms"),
            "{head}"
        );
        // probe: final status of the direct URL, asked with the token and a one-byte range
        assert_eq!(
            c.probe(&format!("http://127.0.0.1:{port}/v")).await,
            Some(403)
        );
        let head = heads.lock().unwrap().last().unwrap().clone();
        assert!(head.contains("range: bytes=0-0") && head.contains("x-emby-token: tok"));
    }

    #[test]
    fn error_serializes_with_kind() {
        let v = serde_json::to_value(Error::Unauthorized).unwrap();
        assert_eq!(v["kind"], "unauthorized");
    }
}
