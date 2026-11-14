//! 弹幕数据：社区通用的 `/api/v2` 弹幕接口（`match`、`search/anime`、`bangumi/{id}`、`comment/{id}`）。
//!
//! 接口地址由用户填写，路径里可能带令牌：地址不进日志，错误信息里也不带 URL。
//! 这里只负责匹配、拉取、按集缓存到磁盘（播放时经 IPC 整集发给脚本）；排版和渲染在 mpv 脚本 `mpv/scripts/danmaku.lua` 里按屏幕刷新率逐帧做
//! （mpv 的字幕只在视频帧变化时重绘，24 帧片源上滚动弹幕会一顿一顿，所以不走字幕轨）。

use crate::emby::{self, Item};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

/// 缓存在这段时间内直接用（还在播出的剧，弹幕会越来越多）。
const FRESH: Duration = Duration::from_secs(24 * 3600);
/// 超过这么久没用的缓存文件，下次拉取时删掉。
const KEEP: Duration = Duration::from_secs(7 * 24 * 3600);
/// 一集 4 万条的剧，接口冷启动接近 20 秒。
const TIMEOUT: Duration = Duration::from_secs(90);

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct Config {
    /// 接口地址（`/api/v2` 格式，可带令牌）；空串 = 不用弹幕。
    pub api: String,
    /// 屏蔽词：弹幕含其中任一个（不分大小写）就不显示。
    pub words: Vec<String>,
    pub style: Style,
    /// 手动匹配的结果，键见 [`key`]。
    pub picks: HashMap<String, Pick>,
}

/// 显示设置，在播放器的弹幕设置面板里改（字段和面板一一对应，见 danmaku.lua）。
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct Style {
    pub on: bool,
    /// 不透明度 0.1–1。
    pub opacity: f64,
    /// 滚动和顶部弹幕可用的高度占比：0.25 / 0.5 / 0.75 / 1。
    pub area: f64,
    /// 字号倍数 0.5–2。
    pub scale: f64,
    /// 速度档 1（极慢）– 5（极快）。
    pub speed: u8,
    pub bold: bool,
    /// 描边：`heavy` 重墨 / `outline` 描边 / `shadow` 45° 投影。
    pub border: String,
    /// 合并短时间内的重复弹幕。
    pub merge: bool,
    pub no_scroll: bool,
    pub no_top: bool,
    pub no_bottom: bool,
    /// 屏蔽彩色弹幕（只留白色）。
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
    /// 面板发来的值来自脚本，越界的拉回范围内。
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

/// 手动匹配：这部剧（这一季）对应接口里的哪部番剧，集数差几集。
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Pick {
    pub anime: i64,
    /// 接口里的集序号（从 0 起）= Emby 集号 - 1 + offset。
    pub offset: i32,
}

/// 校验并规范化接口地址：只认 http / https，去掉末尾的 `/`。空串表示关闭。
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

/// 手动匹配的键：剧集按「上游 + 剧 + 季」（同一季的集共用一次匹配），其余按「上游 + 条目」。
pub fn key(tag: &str, item: &Item) -> String {
    match (&item.series_id, item.kind.as_str()) {
        (Some(series), "Episode") => {
            // 没有季号的按第 1 季，和 match_name 一致
            format!("{tag}:{series}:{}", item.parent_index_number.unwrap_or(1))
        }
        _ => format!("{tag}:{}", item.id),
    }
}

/// 自动匹配用的名字：剧集「剧名 SxxEyy」（没有季号的按第 1 季），电影「片名 (年份)」。接口按文件名的习惯解析这两种写法。
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

/// Emby 集号对应的接口集序号（从 0 起，未加偏移）。电影等没有集号的算第 0 集。
pub fn index(item: &Item) -> i32 {
    match item.kind.as_str() {
        "Episode" => item.index_number.unwrap_or(1) - 1,
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

/// 弹幕接口出错的三种后果（状态码分类见 [`crate::status`]）。
#[derive(Debug, Clone, PartialEq)]
pub enum Fail {
    /// 地址或令牌不对（401 / 403、地址下回的不是 JSON）：这次播放不再请求，提示到设置里检查地址。
    Setup(String),
    /// 暂时取不到（限流、5xx、连不上，已重试过）：有旧缓存先用旧的，弹幕面板里可以重试。
    Busy(String),
    /// 接口明确说失败（如这一集没有弹幕）。
    Other(String),
}

impl std::fmt::Display for Fail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (Self::Setup(m) | Self::Busy(m) | Self::Other(m)) = self;
        f.write_str(m)
    }
}

/// 回包 → 结果。`json` 为 None 表示正文不是 JSON。
fn judge(code: u16, json: Option<Value>) -> Result<Value, Fail> {
    use crate::status::{Class, classify, describe};
    let msg = json
        .as_ref()
        .and_then(|v| v["errorMessage"].as_str())
        .filter(|m| !m.is_empty())
        .map(str::to_owned);
    let fallback = || {
        if code < 300 {
            "请求失败".to_owned()
        } else {
            describe(code, "")
        }
    };
    let text = |m: Option<String>| format!("弹幕接口：{}", m.unwrap_or_else(fallback));
    match (classify(code), json) {
        (Class::Ok, Some(v)) if v.is_object() && v["success"] != false => Ok(v),
        (Class::Ok, Some(_)) => Err(Fail::Other(text(msg))),
        (Class::Auth | Class::Denied, _) => Err(Fail::Setup(format!(
            "弹幕接口拒绝访问（{code}），令牌可能已失效，请在设置里检查弹幕接口地址"
        ))),
        // 地址对的话，接口出错也回 JSON；回 HTML（如反代的 404 页、防火墙验证页）说明地址不对或被拦
        (_, None) if code != 429 && code < 500 => Err(Fail::Setup(format!(
            "弹幕接口地址不对或被拦截（{code}，回的不是 JSON），请在设置里检查弹幕接口地址"
        ))),
        (Class::Busy | Class::Failed, _) | (_, None) => Err(Fail::Busy(text(msg))),
        _ => Err(Fail::Other(text(msg))),
    }
}

#[derive(Clone)]
pub struct Api {
    base: String,
    http: Client,
}

impl Api {
    /// 地址为空或不合法时返回 None（设置页保存时已校验，这里只是兜底）。
    pub fn new(http: Client, url: &str) -> Option<Self> {
        normalize(url)
            .ok()
            .filter(|b| !b.is_empty())
            .map(|base| Self { base, http })
    }

    /// 发请求（暂时性失败按 [`crate::status`] 重试），按 [`judge`] 定结果。错误里不带 URL：地址里可能有令牌。
    async fn send(&self, build: impl Fn() -> reqwest::RequestBuilder) -> Result<Value, Fail> {
        // 接口全是查询（match 也只是用 POST 传参），重发无副作用
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

    /// 按名字自动匹配，取第一个结果。
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

    /// 一部番剧的全部集（按接口给的顺序）。
    pub async fn episodes(&self, anime: i64) -> Result<Vec<Episode>, Fail> {
        let v = self.get(&format!("bangumi/{anime}"), &[]).await?;
        Ok(parse_bangumi(&v))
    }

    /// 一集的弹幕（含关联来源、转成简体），规范化成按时间排好的 `[秒, 类型, 颜色, 文字, 字宽]`（见 [`parse_comments`]）。
    pub async fn comments(&self, episode: i64) -> Result<Vec<Value>, Fail> {
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

/// 文字宽度估算（单位 em，脚本排版用）：按 UTF-8 字节数分类，中日韩全角 1、半角 0.55。
/// 在这里算好：脚本里逐条算，4 万条要一两秒。
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

/// `p` = `秒,类型,颜色(十进制 RGB),[来源]`；类型 4 底部、5 顶部，其余都按滚动。
/// 输出按时间排好的 `[秒, 类型, 颜色, 文字, 字宽(em)]`。
fn parse_comments(v: &Value) -> Vec<Value> {
    let mut out: Vec<(f64, Value)> = v["comments"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| {
            // 换行换成空格；去掉表情后面的变体选择符（U+FE0F，部分字体会画成方框）
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
            Some((t, json!([t, mode, color, text, em_width(text)])))
        })
        .collect();
    out.sort_by(|a, b| a.0.total_cmp(&b.0));
    out.into_iter().map(|(_, v)| v).collect()
}

/// 一次播放用到的弹幕环境（起播时从配置取一份）。
#[derive(Clone)]
pub struct Ctx {
    /// 没填接口地址时为 None：面板里提示到设置里填。
    pub api: Option<Api>,
    /// 缓存目录（每集一个 `<接口地址哈希>-<episodeId>.json`）。
    pub dir: PathBuf,
    pub cfg: Config,
}

/// 找一集弹幕要的东西（取自 Emby 条目，在播放循环里组好，交给后台任务）。
#[derive(Clone, Debug)]
pub struct Want {
    pub name: String,
    pub index: i32,
    pub pick: Option<Pick>,
}

/// 拉好的一集：弹幕（缓存文件的原文，JSON 数组）和给人看的出处。
#[derive(Serialize, Debug, PartialEq)]
pub struct Loaded {
    pub list: String,
    pub title: String,
    pub manual: bool,
    /// 接口暂时取不到，用的是旧缓存。
    pub stale: bool,
}

/// 找到对应的集、取弹幕（缓存新鲜就直接用）。没匹配上返回 Ok(None)。
/// 接口暂时取不到（[`Fail::Busy`] / [`Fail::Other`]）而本地有这一集的旧缓存时用旧的。
pub async fn load(api: &Api, dir: &Path, w: &Want) -> Result<Option<Loaded>, Fail> {
    let ep = match &w.pick {
        Some(p) => {
            let i = w.index + p.offset;
            let eps = api.episodes(p.anime).await?;
            usize::try_from(i).ok().and_then(|i| eps.into_iter().nth(i))
        }
        None => api.matched(&w.name).await?,
    };
    let Some(ep) = ep else { return Ok(None) };
    // 集 id 只在同一个接口里唯一：文件名带上接口地址的哈希，换了接口不会读到别家的缓存
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
                let text = Value::Array(v).to_string();
                write(&path, &text).map_err(Fail::Other)?;
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

/// 先写临时文件再改名。同一集可能同时在取（预取下一集还没完就开播了、连点重新加载），临时文件名各不相同。
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

/// 删掉一周没更新的缓存。
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
        // 旧配置里没有的字段取默认值
        let s: Style = serde_json::from_str(r#"{"on":false}"#).unwrap();
        assert!(!s.on && s.bold && s.speed == 3);
    }

    // 回包形状取自真实接口（文字、id 已替换）
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
            {"cid":5,"p":"5,6,99999999999","m":"怪类型"}]});
        assert_eq!(
            parse_comments(&c),
            vec![
                json!([3.0, 5, 0xFF0000, "顶部红", 3.0]),
                json!([5.0, 1, 0xFFFFFF, "怪类型", 3.0]),
                json!([12.35, 1, 0xFFFFFF, "后 一条", 3.55]),
            ]
        );
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
        assert!(matches!(judge(404, None), Err(Fail::Setup(_)))); // 反代的 404 页：地址不对
        assert!(matches!(judge(200, None), Err(Fail::Setup(_)))); // 防火墙验证页
        assert!(matches!(judge(502, None), Err(Fail::Busy(_))));
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

    /// 同一集同时写两份（预取和开播撞上）：各写各的临时文件，都成功。
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
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1, "不留临时文件");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 真接口：环境变量 `DANMU_API` 为接口地址（含令牌）。
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
        let l = load(&api, &dir, &w).await.unwrap().expect("应能匹配");
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
