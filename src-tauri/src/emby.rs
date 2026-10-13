//! Emby HTTP：鉴权头、UA、通用请求，以及播放要用的几个强类型接口。
//! 浏览类数据只有前端用，不在这里定义 DTO（前端经 `emby` 命令拿原始 JSON）。

use crate::config::Server;
use reqwest::{Client, Method, StatusCode};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize, Serializer, ser::SerializeStruct};
use serde_json::Value;
use std::time::{Duration, Instant};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// 客户端自己的 UA，不伪装成别的客户端；个别服务器对 UA 的限制交给反向代理处理。
pub const UA: &str = concat!("EmbyMini/", env!("CARGO_PKG_VERSION"));
/// 超过这个时长的请求记一条日志，用来对照「起播慢 / 页面慢」是不是卡在上游。
const SLOW: Duration = Duration::from_secs(3);
/// 不带查询参数时传给 [`Emby::call`]。
pub const NO_QUERY: &[(&str, &str)] = &[];

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("未登录或登录已失效")]
    Unauthorized,
    #[error("{}", status_msg(*.0, .1))]
    Status(u16, String),
    /// 文案已按连不上 / 超时 / 证书分好类，见 [`net`]。
    #[error("{0}")]
    Network(String),
    #[error("{0}")]
    Other(String),
}

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

/// 5xx 多是上游或反代临时出错，回包是一页 HTML，不展示。
fn status_msg(code: u16, body: &str) -> String {
    match code {
        500..=599 => format!("服务器暂时出错（{code}），稍后再试"),
        _ => format!("服务器返回 {code}：{body}"),
    }
}

/// 网络层错误按连不上 / 超时 / 证书分类，括号里附整条错误链（reqwest 顶层只有一句 error sending request）。
/// 只取 source 链，不取顶层：顶层带完整 URL（含查询参数，如搜索词）。
fn net(e: reqwest::Error) -> Error {
    let mut chain = Vec::new();
    let mut src = std::error::Error::source(&e);
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

/// 失败和慢请求写日志（只记方法和路径，不记查询参数、token、密码）。
fn log_result<T>(method: &str, path: &str, t0: Instant, r: &Result<T, Error>) {
    let took = t0.elapsed();
    match r {
        Err(e) => log::warn!("{method} {path} 失败（{took:.1?}）：{e}"),
        Ok(_) if took >= SLOW => log::info!("{method} {path} 慢请求：{took:.1?}"),
        Ok(_) => {}
    }
}

/// Emby 请求一律直连，不读系统代理：reqwest 只在建 Client 时读一次系统代理，
/// 之后代理软件开关、换端口都不会跟着变，读到的旧地址会让请求一直失败到重启。
pub fn http_client() -> reqwest::Result<Client> {
    Client::builder()
        .no_proxy()
        .user_agent(UA)
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
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

async fn json_or_error(resp: reqwest::Response) -> Result<Value, Error> {
    let status = resp.status();
    if status == StatusCode::UNAUTHORIZED {
        return Err(Error::Unauthorized);
    }
    let bytes = resp.bytes().await.map_err(net)?;
    if !status.is_success() {
        let text = String::from_utf8_lossy(&bytes).chars().take(200).collect();
        return Err(Error::Status(status.as_u16(), text));
    }
    if bytes.is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_slice(&bytes).map_err(|e| Error::Other(format!("回包不是 JSON：{e}")))
}

/// 把通用 JSON 回包转成播放要用的强类型；`what` 写进错误信息，方便定位是哪个接口变了形状。
fn decode<T: DeserializeOwned>(what: &str, v: Value) -> Result<T, Error> {
    serde_json::from_value(v).map_err(|e| Error::Other(format!("{what} 回包格式不符：{e}")))
}

/// 服务器名称（`/System/Info/Public`，无需登录），添加服务器时顺便校验地址可达。
pub async fn server_name(http: &Client, base: &str) -> Result<String, Error> {
    const PATH: &str = "/System/Info/Public";
    let t0 = Instant::now();
    let r = match http.get(format!("{base}{PATH}")).send().await {
        Ok(resp) => json_or_error(resp).await,
        Err(e) => Err(net(e)),
    };
    log_result("GET", PATH, t0, &r);
    Ok(r?["ServerName"].as_str().unwrap_or("Emby").to_owned())
}

/// 用户名密码换 token，返回 `(user_id, token)`。
pub async fn login(
    http: &Client,
    base: &str,
    device_id: &str,
    user: &str,
    password: &str,
) -> Result<(String, String), Error> {
    const PATH: &str = "/Users/AuthenticateByName";
    let t0 = Instant::now();
    let r = match http
        .post(format!("{base}{PATH}"))
        .header("X-Emby-Authorization", auth_header(device_id, "", ""))
        .json(&serde_json::json!({ "Username": user, "Pw": password }))
        .send()
        .await
    {
        Ok(resp) => json_or_error(resp).await,
        Err(e) => Err(net(e)),
    };
    let r = match r {
        Err(Error::Unauthorized) => Err(Error::Other("用户名或密码错误".into())),
        r => r,
    };
    log_result("POST", PATH, t0, &r);
    let v = r?;
    match (v["User"]["Id"].as_str(), v["AccessToken"].as_str()) {
        (Some(uid), Some(token)) if !uid.is_empty() && !token.is_empty() => {
            Ok((uid.to_owned(), token.to_owned()))
        }
        _ => Err(Error::Other("登录回包缺少 User.Id / AccessToken".into())),
    }
}

/// 已登录的某个服务器。
#[derive(Clone)]
pub struct Emby {
    http: Client,
    base: String,
    auth: String,
    token: String,
    pub user_id: String,
}

impl Emby {
    pub fn new(http: Client, server: &Server, device_id: &str) -> Self {
        Self {
            http,
            base: server.api_base(),
            auth: auth_header(device_id, &server.user_id, &server.token),
            token: server.token.clone(),
            user_id: server.user_id.clone(),
        }
    }

    /// 通用请求。`path` 是 `/emby` 之后的部分，必须以 `/` 开头；查询参数单独传（键值对列表）。
    /// 失败和慢请求写日志（[`log_result`]）。
    pub async fn call<Q: Serialize + ?Sized>(
        &self,
        method: Method,
        path: &str,
        query: &Q,
        body: Option<&Value>,
    ) -> Result<Value, Error> {
        check_path(path)?;
        let t0 = Instant::now();
        let mut rb = self
            .http
            .request(method.clone(), format!("{}{path}", self.base))
            .header("X-Emby-Authorization", &self.auth)
            .header("X-Emby-Token", &self.token)
            .query(query);
        if let Some(b) = body {
            rb = rb.json(b);
        }
        let r = match rb.send().await {
            Ok(resp) => json_or_error(resp).await,
            Err(e) => Err(net(e)),
        };
        log_result(method.as_str(), path, t0, &r);
        r
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

    /// 某季的全部单集（服务端已按集号排好）。
    pub async fn episodes(&self, series_id: &str, season_id: &str) -> Result<Vec<Item>, Error> {
        let r: Items = self
            .get(
                &format!("/Shows/{series_id}/Episodes"),
                &[("UserId", &self.user_id), ("SeasonId", season_id)],
            )
            .await?;
        Ok(r.items)
    }

    pub async fn playback_info(&self, id: &str) -> Result<PlaybackInfo, Error> {
        let q = [
            ("UserId", self.user_id.as_str()),
            ("StartTimeTicks", "0"),
            ("IsPlayback", "true"),
            ("AutoOpenLiveStream", "false"),
            ("MaxStreamingBitrate", "400000000"),
        ];
        let v = self
            .call(
                Method::POST,
                &format!("/Items/{id}/PlaybackInfo"),
                &q,
                Some(&Value::Object(Default::default())),
            )
            .await?;
        decode("PlaybackInfo", v)
    }

    /// 直链：优先用 PlaybackInfo 给的 `DirectStreamUrl`（反向代理可能改写它），没有再按标准格式拼。
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

    /// 外挂字幕地址。`DeliveryUrl` 在实测中常为 null，按标准格式拼。
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

    /// 只在播放失败时调用：取直链第一个字节，拿到最终 HTTP 状态码（会跟随 302），用于给用户说清原因。
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

    /// 给 mpv 的请求头：token 走请求头而不是 URL（见 [`strip_key`]）。
    pub fn token_header(&self) -> String {
        format!("X-Emby-Token: {}", self.token)
    }
}

/// 去掉服务端直链里自带的 api_key。token 改由 mpv 的请求头带（[`Emby::token_header`]），
/// 不进 URL，就不会出现在 mpv 的播放列表、统计页（`i`）和日志里，截图、录屏也不会泄露。
/// 代价：直链 302 到别的主机时，mpv 也会把这个请求头带过去。
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

fn subtitle_ext(codec: Option<&str>) -> String {
    match codec.map(str::to_ascii_lowercase).as_deref() {
        None | Some("subrip") => "srt".into(),
        Some("pgssub") => "sup".into(),
        Some(c) => c.to_owned(),
    }
}

/// 前端传来的路径只能是本服务器 API 下的相对路径。
fn check_path(path: &str) -> Result<(), Error> {
    if !path.starts_with('/')
        || path.contains("://")
        || path.contains("..")
        || path.contains(['?', '#'])
    {
        return Err(Error::Other(format!("非法路径：{path}")));
    }
    Ok(())
}

// ---- 播放用到的字段（以 testdata/ 里的真实回包为准，缺字段一律取默认值） ----

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
            "服务端直链自带的 api_key 要去掉：{u}"
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
        assert_eq!(c.token_header(), "X-Emby-Token: tok");
        assert_eq!(subtitle_ext(Some("subrip")), "srt");
        assert_eq!(subtitle_ext(Some("PGSSUB")), "sup");
    }

    #[test]
    fn path_guard() {
        assert!(check_path("/Users/x/Views").is_ok());
        for bad in ["Users", "/a/../b", "https://evil/x", "/a?b=1", "/a#x"] {
            assert!(check_path(bad).is_err(), "{bad}");
        }
    }

    /// 连不上、超时要分开说；5xx 不展示 HTML 回包。
    #[tokio::test]
    async fn network_errors_are_classified() {
        let http = http_client().unwrap();
        // 端口 1 没人监听：连接被拒
        let e = server_name(&http, "http://127.0.0.1:1/emby")
            .await
            .unwrap_err();
        assert!(e.to_string().starts_with("连不上服务器（"), "{e}");
        // 收下连接但不回话：超时
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
            "服务器暂时出错（502），稍后再试"
        );
        assert_eq!(
            Error::Status(404, "x".into()).to_string(),
            "服务器返回 404：x"
        );
    }

    #[test]
    fn error_serializes_with_kind() {
        let v = serde_json::to_value(Error::Unauthorized).unwrap();
        assert_eq!(v["kind"], "unauthorized");
    }
}
