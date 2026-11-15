//! Danmaku data from the community `/api/v2` danmaku API (`match`, `search/anime`, `bangumi/{id}`, `comment/{id}`).
//!
//! The API address is user-supplied and may carry a token in its path: it never goes into logs, nor do URLs in errors.
//! This module only matches, fetches and caches per episode (each goes to the script whole over IPC); layout and
//! drawing happen per display frame in `mpv/scripts/danmaku.lua`, since mpv subtitles only redraw on new video frames.

use crate::emby::{self, Item};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

/// Cached danmaku younger than this is used as is (shows still airing keep gaining comments).
const FRESH: Duration = Duration::from_secs(24 * 3600);
/// Cache files not rewritten for this long are deleted on the next successful fetch.
const KEEP: Duration = Duration::from_secs(7 * 24 * 3600);
/// A 40k-comment episode takes the API close to 20 s when cold.
const TIMEOUT: Duration = Duration::from_secs(90);

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct Config {
    /// API address (`/api/v2` style, may carry a token); empty = danmaku off.
    pub api: String,
    /// Blocked words: a comment containing any of them (case-insensitive) is hidden.
    pub words: Vec<String>,
    pub style: Style,
    /// Manual matches, keyed by [`key`].
    pub picks: HashMap<String, Pick>,
}

/// Display settings, edited in the player's danmaku panel (fields mirror the panel, see danmaku.lua).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct Style {
    pub on: bool,
    /// Opacity 0.1–1.
    pub opacity: f64,
    /// Share of the height scrolling and top danmaku may use: 0.25 / 0.5 / 0.75 / 1.
    pub area: f64,
    /// Font size factor 0.5–2.
    pub scale: f64,
    /// Speed step 1 (slowest) – 5 (fastest).
    pub speed: u8,
    pub bold: bool,
    /// Border: `heavy` / `outline` / `shadow` (45° drop shadow).
    pub border: String,
    /// Merge repeats posted close together.
    pub merge: bool,
    pub no_scroll: bool,
    pub no_top: bool,
    pub no_bottom: bool,
    /// Hide coloured danmaku (keep white only).
    pub no_color: bool,
}

impl Default for Style {
    fn default() -> Self {
        Self {
            on: true,
            opacity: 0.8,
            area: 0.5,
            scale: 1.0,
            speed: 3,
            bold: true,
            border: "heavy".into(),
            merge: true,
            no_scroll: false,
            no_top: false,
            no_bottom: false,
            no_color: false,
        }
    }
}

impl Style {
    /// Values come from the script; out-of-range ones are pulled back in.
    pub fn clamped(mut self) -> Self {
        let num = |v: f64, lo: f64, hi: f64, d: f64| {
            Some(v)
                .filter(|v| v.is_finite())
                .map_or(d, |v| v.clamp(lo, hi))
        };
        self.opacity = num(self.opacity, 0.1, 1.0, 0.8);
        self.area = num(self.area, 0.25, 1.0, 0.5);
        self.scale = num(self.scale, 0.5, 2.0, 1.0);
        self.speed = self.speed.clamp(1, 5);
        if !matches!(self.border.as_str(), "heavy" | "outline" | "shadow") {
            self.border = "heavy".into();
        }
        self
    }
}

/// A manual match: which API show this show (this season) is, and how far its episode numbering is shifted.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Pick {
    pub anime: i64,
    /// API episode position (from 0) = Emby episode number - 1 + offset.
    pub offset: i32,
}

/// Validates and normalizes an API address: http / https only, trailing `/` removed. Empty means off.
pub fn normalize(input: &str) -> Result<String, String> {
    let s = input.trim().trim_end_matches('/');
    if s.is_empty() {
        return Ok(String::new());
    }
    match reqwest::Url::parse(s) {
        Ok(u) if matches!(u.scheme(), "http" | "https") && u.host_str().is_some() => {
            Ok(s.to_owned())
        }
        _ => Err("弹幕接口地址应为 http:// 或 https:// 开头的完整地址".into()),
    }
}

/// Manual match key: episodes by upstream + series + season (one match per season), anything else by upstream + item.
pub fn key(tag: &str, item: &Item) -> String {
    match (&item.series_id, item.kind.as_str()) {
        (Some(series), "Episode") => {
            // No season number counts as season 1, as in match_name
            format!("{tag}:{series}:{}", item.parent_index_number.unwrap_or(1))
        }
        _ => format!("{tag}:{}", item.id),
    }
}

/// Name for automatic matching: "Series SxxEyy" (season 1 if missing) or "Title (year)",
/// the file-name forms the API parses.
pub fn match_name(item: &Item) -> String {
    match (
        &item.series_name,
        item.parent_index_number,
        item.index_number,
    ) {
        (Some(series), s, Some(e)) => format!("{series} S{:02}E{e:02}", s.unwrap_or(1)),
        _ => match item.production_year {
            Some(y) => format!("{} ({y})", item.name),
            None => item.name.clone(),
        },
    }
}

/// API episode position (from 0, before any offset) for an Emby item; items without an episode number are 0.
pub fn index(item: &Item) -> i32 {
    match item.kind.as_str() {
        "Episode" => item.index_number.unwrap_or(1).saturating_sub(1),
        _ => 0,
    }
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Anime {
    pub id: i64,
    pub title: String,
    pub count: i64,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Episode {
    pub id: i64,
    pub title: String,
}

/// The three outcomes of an API failure (status classes in [`crate::status`]).
#[derive(Debug, Clone, PartialEq)]
pub enum Fail {
    /// Wrong address or token (401 / 403, or not JSON): stop asking for this playback and point to the settings.
    Setup(String),
    /// Temporarily unavailable (rate limit, 5xx, unreachable; already retried): fall back to an old cache.
    Busy(String),
    /// The API said it failed (e.g. no danmaku for this episode).
    Other(String),
}

impl std::fmt::Display for Fail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (Self::Setup(m) | Self::Busy(m) | Self::Other(m)) = self;
        f.write_str(m)
    }
}

/// Reply → result. `json` is None when the body is not JSON.
fn judge(code: u16, json: Option<Value>) -> Result<Value, Fail> {
    use crate::status::{Class, classify, describe};
    let msg = json
        .as_ref()
        .and_then(|v| v["errorMessage"].as_str())
        .filter(|m| !m.is_empty())
        .map(str::to_owned);
    let class = classify(code);
    let fallback = || match class {
        Class::Ok => "请求失败".to_owned(),
        _ => describe(code, ""),
    };
    let text = |m: Option<String>| format!("弹幕接口：{}", m.unwrap_or_else(fallback));
    match (class, json) {
        (Class::Ok, Some(v)) if v.is_object() && v["success"] != false => Ok(v),
        (Class::Auth | Class::Denied, _) => Err(Fail::Setup(format!(
            "弹幕接口拒绝访问（{code}），令牌可能已失效，请在设置里检查弹幕接口地址"
        ))),
        // A correct address answers errors in JSON; HTML (a proxy's 404 page, a firewall challenge)
        // means the address is wrong or blocked
        (_, None) if code != 429 && code < 500 => Err(Fail::Setup(format!(
            "弹幕接口地址不对或被拦截（{code}，回的不是 JSON），请在设置里检查弹幕接口地址"
        ))),
        (Class::Busy | Class::Failed, _) | (_, None) => Err(Fail::Busy(text(msg))),
        // `success:false`, or another status the API answered in JSON
        _ => Err(Fail::Other(text(msg))),
    }
}

#[derive(Clone)]
pub struct Api {
    base: String,
    http: Client,
}

impl Api {
    /// None when the address is empty or invalid (the settings page validates it; this is a backstop).
    pub fn new(http: Client, url: &str) -> Option<Self> {
        normalize(url)
            .ok()
            .filter(|b| !b.is_empty())
            .map(|base| Self { base, http })
    }

    /// Sends with retries on transient failures ([`crate::status`]) and judges the reply ([`judge`]).
    /// Errors carry no URL.
    async fn send(&self, build: impl Fn() -> reqwest::RequestBuilder) -> Result<Value, Fail> {
        // Every endpoint is a query (match only uses POST for its parameters), so resending is harmless
        let (sent, _) = crate::status::send_readonly(|| build().timeout(TIMEOUT)).await;
        let net = |e: reqwest::Error| Fail::Busy(format!("弹幕接口：{}", emby::net(&e)));
        let r = sent.map_err(net)?;
        let code = r.status().as_u16();
        let body = r.bytes().await.map_err(net)?;
        judge(code, serde_json::from_slice(&body).ok())
    }

    async fn get(&self, path: &str, query: &[(&str, &str)]) -> Result<Value, Fail> {
        let url = format!("{}/api/v2/{path}", self.base);
        self.send(|| self.http.get(&url).query(query)).await
    }

    /// Automatic match by name; takes the first result.
    pub async fn matched(&self, name: &str) -> Result<Option<Episode>, Fail> {
        let url = format!("{}/api/v2/match", self.base);
        let body = json!({ "fileName": name, "matchMode": "fileNameOnly" });
        let v = self.send(|| self.http.post(&url).json(&body)).await?;
        Ok(parse_match(&v))
    }

    pub async fn search(&self, keyword: &str) -> Result<Vec<Anime>, Fail> {
        let v = self.get("search/anime", &[("keyword", keyword)]).await?;
        Ok(parse_search(&v))
    }

    /// All episodes of a show, in API order.
    pub async fn episodes(&self, anime: i64) -> Result<Vec<Episode>, Fail> {
        let v = self.get(&format!("bangumi/{anime}"), &[]).await?;
        Ok(parse_bangumi(&v))
    }

    /// An episode's danmaku (related sources included, converted to Simplified Chinese) as time-sorted [`Row`]s.
    pub async fn comments(&self, episode: i64) -> Result<Vec<Row>, Fail> {
        let q = [("withRelated", "true"), ("chConvert", "1")];
        let v = self.get(&format!("comment/{episode}"), &q).await?;
        Ok(parse_comments(&v))
    }
}

fn parse_match(v: &Value) -> Option<Episode> {
    let m = v["matches"].as_array()?.first()?;
    Some(Episode {
        id: m["episodeId"].as_i64()?,
        title: join_title(m["animeTitle"].as_str(), m["episodeTitle"].as_str()),
    })
}

fn parse_search(v: &Value) -> Vec<Anime> {
    v["animes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| {
            Some(Anime {
                id: a["animeId"].as_i64()?,
                title: a["animeTitle"].as_str()?.to_owned(),
                count: a["episodeCount"].as_i64().unwrap_or(0),
            })
        })
        .collect()
}

fn parse_bangumi(v: &Value) -> Vec<Episode> {
    let b = &v["bangumi"];
    let anime = b["animeTitle"].as_str();
    b["episodes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| {
            Some(Episode {
                id: e["episodeId"].as_i64()?,
                title: join_title(anime, e["episodeTitle"].as_str()),
            })
        })
        .collect()
}

fn join_title(anime: Option<&str>, ep: Option<&str>) -> String {
    [anime, ep]
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Estimated text width in em for the script's layout, by UTF-8 length:
/// 1 byte 0.55, 2 bytes 0.6, 3 bytes (CJK) 1, 4 bytes 1.1.
/// Done here because the script took a second or two for 40k comments.
fn em_width(s: &str) -> f64 {
    let w: f64 = s
        .chars()
        .map(|c| match c.len_utf8() {
            1 => 0.55,
            2 => 0.6,
            3 => 1.0,
            _ => 1.1,
        })
        .sum();
    (w * 100.0).round() / 100.0
}

/// One danmaku as the script takes it: `[seconds, mode, colour, text, width in em]`.
pub type Row = (f64, u8, u32, String, f64);

/// `p` = `seconds,mode,colour (decimal RGB),[source]`; mode 4 bottom, 5 top, anything else scrolls.
/// Returns rows sorted by time.
fn parse_comments(v: &Value) -> Vec<Row> {
    let mut out: Vec<Row> = v["comments"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| {
            // Newlines become spaces; U+FE0F after emoji is dropped (some fonts draw it as a box)
            let text = c["m"]
                .as_str()?
                .replace(['\r', '\n'], " ")
                .replace('\u{FE0F}', "");
            let text = text.trim();
            if text.is_empty() {
                return None;
            }
            let mut p = c["p"].as_str()?.split(',');
            let t: f64 = p
                .next()?
                .parse()
                .ok()
                .filter(|t: &f64| t.is_finite() && *t >= 0.0)?;
            let mode = p
                .next()
                .and_then(|m| m.parse::<u8>().ok())
                .filter(|m| matches!(m, 4 | 5))
                .unwrap_or(1);
            let color = p
                .next()
                .and_then(|c| c.parse::<u32>().ok())
                .unwrap_or(0xFF_FFFF)
                & 0xFF_FFFF;
            let t = (t * 100.0).round() / 100.0;
            Some((t, mode, color, text.to_owned(), em_width(text)))
        })
        .collect();
    out.sort_by(|a, b| a.0.total_cmp(&b.0));
    out
}

/// Danmaku environment for one playback (taken from the config at start).
#[derive(Clone)]
pub struct Ctx {
    /// None when no API is set: the panel says to fill it in the settings.
    pub api: Option<Api>,
    /// Cache dir (one `<API address hash>-<episodeId>.json` per episode).
    pub dir: PathBuf,
    pub cfg: Config,
}

/// What finding one episode's danmaku needs (built from the Emby item in the playback loop, handed to a task).
#[derive(Clone, Debug)]
pub struct Want {
    pub name: String,
    pub index: i32,
    pub pick: Option<Pick>,
}

/// A loaded episode: its danmaku (the cache file's text, a JSON array) and where it came from, for display.
#[derive(Serialize, Debug, PartialEq)]
pub struct Loaded {
    pub list: String,
    pub title: String,
    pub manual: bool,
    /// The API was unavailable; this is an old cache.
    pub stale: bool,
}

/// Finds the episode and its danmaku (a fresh cache is used directly). Ok(None) when nothing matched.
/// When the API fails softly ([`Fail::Busy`] / [`Fail::Other`]) an old cache of the episode is used instead.
pub async fn load(api: &Api, dir: &Path, w: &Want) -> Result<Option<Loaded>, Fail> {
    let ep = match &w.pick {
        Some(p) => {
            let i = w.index.saturating_add(p.offset);
            let eps = api.episodes(p.anime).await?;
            usize::try_from(i).ok().and_then(|i| eps.into_iter().nth(i))
        }
        None => api.matched(&w.name).await?,
    };
    let Some(ep) = ep else { return Ok(None) };
    // Episode ids are only unique within one API: the file name carries the address hash so APIs never share a cache
    let path = dir.join(format!(
        "{}-{}.json",
        crate::config::server_tag(&api.base),
        ep.id
    ));
    let mut stale = false;
    let mut list = None;
    if !fresh(&path, FRESH) {
        match api.comments(ep.id).await {
            Ok(v) => {
                prune(dir);
                // Plain numbers and strings: serializing cannot fail
                let text = serde_json::to_string(&v).unwrap_or_else(|_| "[]".into());
                if let Err(e) = write(&path, &text) {
                    log::warn!("{e}");
                }
                list = Some(text);
            }
            Err(e @ (Fail::Busy(_) | Fail::Other(_))) if path.is_file() => {
                log::warn!("{e}，先用旧的弹幕缓存");
                stale = true;
            }
            Err(e) => return Err(e),
        }
    }
    let list = match list {
        Some(l) => l,
        None => tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| Fail::Other(format!("读弹幕缓存失败：{e}")))?,
    };
    Ok(Some(Loaded {
        list,
        title: ep.title,
        manual: w.pick.is_some(),
        stale,
    }))
}

fn fresh(path: &Path, within: Duration) -> bool {
    path.metadata().and_then(|m| m.modified()).is_ok_and(|t| {
        SystemTime::now()
            .duration_since(t)
            .is_ok_and(|d| d < within)
    })
}

/// Writes a temp file, then renames it. One episode may be fetched twice at once (prefetch vs start,
/// repeated reloads), so temp names are unique.
fn write(path: &Path, text: &str) -> Result<(), String> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let fail = |e: std::io::Error| format!("写弹幕缓存失败：{e}");
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d).map_err(fail)?;
    }
    let tmp = path.with_extension(format!("{}.tmp", SEQ.fetch_add(1, Ordering::Relaxed)));
    let r = std::fs::write(&tmp, text).and_then(|()| std::fs::rename(&tmp, path));
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r.map_err(fail)
}

/// Deletes cache files not rewritten for a week.
fn prune(dir: &Path) {
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        if !fresh(&e.path(), KEEP) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(kind: &str, series: Option<&str>, s: Option<i32>, e: Option<i32>) -> Item {
        Item {
            id: "9".into(),
            name: "第三集".into(),
            kind: kind.into(),
            series_id: series.map(|_| "77".into()),
            series_name: series.map(Into::into),
            parent_index_number: s,
            index_number: e,
            production_year: Some(2024),
            ..Default::default()
        }
    }

    #[test]
    fn names_keys_and_index() {
        let ep = item("Episode", Some("庆余年"), Some(2), Some(3));
        assert_eq!(match_name(&ep), "庆余年 S02E03");
        assert_eq!(key("t", &ep), "t:77:2");
        assert_eq!(
            key("t", &item("Episode", Some("某番"), None, Some(7))),
            "t:77:1"
        );
        assert_eq!(index(&ep), 2);
        let no_season = item("Episode", Some("某番"), None, Some(7));
        assert_eq!(match_name(&no_season), "某番 S01E07");
        let mut movie = item("Movie", None, None, None);
        movie.name = "沙丘2".into();
        assert_eq!(match_name(&movie), "沙丘2 (2024)");
        assert_eq!(key("t", &movie), "t:9");
        assert_eq!(index(&movie), 0);
        let odd = item("Episode", Some("某番"), None, Some(i32::MIN));
        assert_eq!(index(&odd), i32::MIN, "a bogus episode number saturates");
    }

    #[test]
    fn normalize_api() {
        assert_eq!(
            normalize("  https://a.example/x/tok/  ").unwrap(),
            "https://a.example/x/tok"
        );
        assert_eq!(normalize("").unwrap(), "");
        assert!(normalize("ftp://a").is_err());
        assert!(normalize("a.example").is_err());
    }

    #[test]
    fn style_is_clamped() {
        let s = Style {
            opacity: 9.0,
            area: f64::NAN,
            speed: 0,
            border: "x".into(),
            ..Style::default()
        }
        .clamped();
        assert_eq!(
            (s.opacity, s.area, s.speed, s.border.as_str()),
            (1.0, 0.5, 1, "heavy")
        );
        // Fields missing from an old config take defaults
        let s: Style = serde_json::from_str(r#"{"on":false}"#).unwrap();
        assert!(!s.on && s.bold && s.speed == 3);
    }

    // Reply shapes taken from the real API (texts and ids replaced)
    #[test]
    fn parses_api_shapes() {
        let m = json!({"errorCode":0,"success":true,"isMatched":true,"matches":[
            {"episodeId":101,"animeId":7,"animeTitle":"某剧(2024)【电视剧】","episodeTitle":"【qq】 第3集"}]});
        assert_eq!(
            parse_match(&m),
            Some(Episode {
                id: 101,
                title: "某剧(2024)【电视剧】 【qq】 第3集".into()
            })
        );
        assert_eq!(parse_match(&json!({"success":true,"matches":[]})), None);
        let s = json!({"animes":[{"animeId":7,"animeTitle":"某剧","episodeCount":28},{"animeTitle":"缺 id"}]});
        assert_eq!(
            parse_search(&s),
            vec![Anime {
                id: 7,
                title: "某剧".into(),
                count: 28
            }]
        );
        let b = json!({"bangumi":{"animeTitle":"某剧","episodes":[
            {"episodeId":1,"episodeTitle":"第1集"},{"episodeId":2,"episodeTitle":"第2集"}]}});
        assert_eq!(
            parse_bangumi(&b)[1],
            Episode {
                id: 2,
                title: "某剧 第2集".into()
            }
        );
        let c = json!({"count":4,"comments":[
            {"cid":1,"p":"12.345,1,16777215,[qq]","m":"后\n一条"},
            {"cid":2,"p":"3.00,5,16711680,[src2]","m":"顶部红"},
            {"cid":3,"p":"4,4,255,[x]","m":"  "},
            {"cid":4,"p":"bad,1,1","m":"坏时间"},
            {"cid":5,"p":"5,6,99999999999","m":"怪类型"},
            {"cid":6,"p":"-1,1,1","m":"负时间"}]});
        assert_eq!(
            parse_comments(&c),
            vec![
                (3.0, 5, 0xFF0000, "顶部红".into(), 3.0),
                (5.0, 1, 0xFFFFFF, "怪类型".into(), 3.0),
                (12.35, 1, 0xFFFFFF, "后 一条".into(), 3.55),
            ]
        );
    }

    #[test]
    fn widths() {
        assert_eq!(em_width("a\u{e9}中😀"), 0.55 + 0.6 + 1.0 + 1.1);
    }

    #[test]
    fn judges_statuses() {
        let ok = json!({"success": true});
        assert_eq!(judge(200, Some(ok.clone())), Ok(ok));
        let busy = json!({"success":false,"errorCode":503,"errorMessage":"Storage or request capacity temporarily unavailable"});
        assert_eq!(
            judge(503, Some(busy)),
            Err(Fail::Busy(
                "弹幕接口：Storage or request capacity temporarily unavailable".into()
            ))
        );
        assert!(matches!(judge(401, None), Err(Fail::Setup(_))));
        assert!(matches!(judge(403, Some(json!({}))), Err(Fail::Setup(_))));
        assert!(matches!(judge(404, None), Err(Fail::Setup(_)))); // a proxy 404 page: wrong address
        assert!(matches!(judge(200, None), Err(Fail::Setup(_)))); // a firewall challenge
        assert!(matches!(judge(502, None), Err(Fail::Busy(_))));
        assert!(matches!(judge(500, None), Err(Fail::Busy(_))));
        assert_eq!(
            judge(304, Some(json!({"success": false}))),
            Err(Fail::Other("弹幕接口：请求失败".into()))
        );
        assert!(matches!(judge(429, None), Err(Fail::Busy(_))));
        assert_eq!(
            judge(
                404,
                Some(json!({"success":false,"errorMessage":"没有这一集"}))
            ),
            Err(Fail::Other("弹幕接口：没有这一集".into()))
        );
        assert_eq!(
            judge(200, Some(json!({"success":false}))),
            Err(Fail::Other("弹幕接口：请求失败".into()))
        );
    }

    /// Two writers of one episode (prefetch meets start): each uses its own temp file and both succeed.
    #[test]
    fn concurrent_writes() {
        let dir = std::env::temp_dir().join(format!("embymini-dm-w-{}", std::process::id()));
        let path = dir.join("x-1.json");
        let ts: Vec<_> = (0..4)
            .map(|_| {
                let path = path.clone();
                std::thread::spawn(move || (0..50).all(|_| write(&path, "[]").is_ok()))
            })
            .collect();
        assert!(ts.into_iter().all(|t| t.join().unwrap()));
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            1,
            "no temp files left"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    fn age(path: &Path, secs: u64) {
        let f = std::fs::File::options().write(true).open(path).unwrap();
        f.set_modified(SystemTime::now() - Duration::from_secs(secs))
            .unwrap();
    }

    /// Behaviour of `comment/101` on the fake API: 0 = comments, 1 = "no such episode", 2 = 401.
    static MODE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

    fn route(path: &str) -> (&'static str, String) {
        let ok = |v: Value| ("200 OK", v.to_string());
        let comments = |m: &str| ok(json!({"comments":[{"p":"1.5,1,16777215","m":m}]}));
        let gone = || {
            (
                "404 Not Found",
                json!({"success":false,"errorMessage":"没有这一集"}).to_string(),
            )
        };
        match path.trim_start_matches("/x/tok/api/v2/") {
            "match" => ok(
                json!({"matches":[{"episodeId":101,"animeTitle":"某剧","episodeTitle":"第3集"}]}),
            ),
            "bangumi/7" => ok(json!({"bangumi":{"animeTitle":"某剧","episodes":[
                {"episodeId":1,"episodeTitle":"第1集"},{"episodeId":2},{"episodeId":3,"episodeTitle":"第3集"}]}})),
            "comment/101" => match MODE.load(Ordering::SeqCst) {
                0 => comments("好"),
                1 => gone(),
                _ => ("401 Unauthorized", String::new()),
            },
            "comment/3" => comments("三"),
            _ => gone(),
        }
    }

    #[tokio::test]
    async fn load_matches_caches_and_falls_back() {
        let (port, heads) = emby::tests::fake(route).await;
        let api = Api::new(
            emby::http_client().unwrap(),
            &format!("http://127.0.0.1:{port}/x/tok/"),
        )
        .unwrap();
        let dir = std::env::temp_dir().join(format!("embymini-dm-load-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (old, recent) = (dir.join("old.json"), dir.join("recent.json"));
        std::fs::write(&old, "[]").unwrap();
        std::fs::write(&recent, "[]").unwrap();
        age(&old, 8 * 24 * 3600);
        age(&recent, 6 * 24 * 3600);
        let auto = Want {
            name: "某剧 S01E03".into(),
            index: 2,
            pick: None,
        };
        let list = |l: &Loaded| serde_json::from_str::<Value>(&l.list).unwrap()[0][3].clone();

        let l = load(&api, &dir, &auto).await.unwrap().unwrap();
        assert_eq!(
            (l.title.as_str(), l.manual, l.stale),
            ("某剧 第3集", false, false)
        );
        assert_eq!(list(&l), "好");
        let cached = dir.join(format!("{}-101.json", crate::config::server_tag(&api.base)));
        assert!(cached.is_file());
        assert!(
            !old.exists() && recent.exists(),
            "a successful fetch prunes week-old files only"
        );

        let n = heads.lock().unwrap().len();
        assert!(load(&api, &dir, &auto).await.unwrap().is_some());
        let sent = heads.lock().unwrap()[n..].to_vec();
        assert_eq!(sent.len(), 1, "a fresh cache only re-matches: {sent:?}");

        // Stale cache: used when the API fails softly, not when the address is wrong.
        age(&cached, 2 * 24 * 3600);
        MODE.store(1, Ordering::SeqCst);
        let l = load(&api, &dir, &auto).await.unwrap().unwrap();
        assert!(l.stale);
        assert_eq!(list(&l), "好");
        MODE.store(2, Ordering::SeqCst);
        assert!(matches!(load(&api, &dir, &auto).await, Err(Fail::Setup(_))));

        // Manual pick: API index = Emby index + offset.
        let pick = |offset| Want {
            pick: Some(Pick { anime: 7, offset }),
            index: 1,
            ..auto.clone()
        };
        let l = load(&api, &dir, &pick(1)).await.unwrap().unwrap();
        assert_eq!((l.title.as_str(), l.manual), ("某剧 第3集", true));
        assert_eq!(list(&l), "三");
        assert_eq!(load(&api, &dir, &pick(5)).await, Ok(None));
        assert_eq!(load(&api, &dir, &pick(i32::MAX)).await, Ok(None));
        // No cache to fall back on: the API's own error comes through.
        assert_eq!(
            load(&api, &dir, &pick(-1)).await,
            Err(Fail::Other("弹幕接口：没有这一集".into()))
        );

        // An unwritable cache still shows the danmaku.
        MODE.store(0, Ordering::SeqCst);
        let l = load(&api, &recent, &auto).await.unwrap().unwrap();
        assert_eq!(list(&l), "好");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Real API: `DANMU_API` holds the address (with token).
    #[tokio::test]
    #[ignore]
    async fn e2e_danmaku_api() {
        let api = Api::new(
            emby::http_client().unwrap(),
            &std::env::var("DANMU_API").unwrap(),
        )
        .expect("DANMU_API");
        let dir = std::env::temp_dir().join(format!("embymini-dm-{}", std::process::id()));
        let w = Want {
            name: "沙丘2 (2024)".into(),
            index: 0,
            pick: None,
        };
        let l = load(&api, &dir, &w).await.unwrap().expect("should match");
        let list: Vec<Value> = serde_json::from_str(&l.list).unwrap();
        println!("{}：{} 条", l.title, list.len());
        assert!(list.len() > 100);
        let found = api.search("沙丘2").await.unwrap();
        let eps = api.episodes(found[0].id).await.unwrap();
        assert!(!eps.is_empty());
        let w = Want {
            pick: Some(Pick {
                anime: found[0].id,
                offset: 0,
            }),
            ..w
        };
        assert!(
            load(&api, &dir, &w)
                .await
                .unwrap()
                .is_some_and(|l| l.manual)
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
