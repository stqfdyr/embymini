//! Home rows: from an Emby server, or generated for runs without one.

use anyhow::{Context as _, Result, bail};
use gpui_kit::SharedString;
use serde::Deserialize;

use crate::poster::Key;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    /// 2:3 poster
    Poster,
    /// 16:9 card
    Wide,
}

pub struct Card {
    pub id: SharedString,
    pub title: SharedString,
    pub sub: SharedString,
    pub image: Option<Key>,
}

pub struct Row {
    pub id: SharedString,
    pub title: SharedString,
    pub kind: Kind,
    pub cards: Vec<Card>,
}

pub enum Source {
    Fake,
    Emby {
        base: String,
        user: String,
        pass: String,
    },
}

impl Source {
    pub fn from_env() -> Result<Self> {
        let var = |k: &str| std::env::var(k).with_context(|| format!("{k} is not set"));
        let server = var("EMBY_SERVER")?;
        let server = server.trim_end_matches('/');
        let base = match std::env::var("PORT") {
            Ok(p) if !p.is_empty() => format!("{server}:{p}/emby"),
            _ => format!("{server}/emby"),
        };
        Ok(Self::Emby {
            base,
            user: var("EMBY_USER")?,
            pass: var("EMBY_PASS")?,
        })
    }
}

pub async fn load(source: Source) -> Result<Vec<Row>> {
    match source {
        Source::Fake => Ok(fake()),
        Source::Emby { base, user, pass } => emby(&base, &user, &pass).await,
    }
}

const NAMES: [&str; 12] = [
    "长安十二时辰",
    "漫长的季节",
    "繁花",
    "隐秘的角落",
    "三体",
    "山海情",
    "Dune: Part Two",
    "Oppenheimer",
    "The Bear",
    "Severance",
    "Arcane",
    "Shōgun",
];

fn fake() -> Vec<Row> {
    let mut rows = Vec::new();
    let titles = [
        "继续观看",
        "最新 电影",
        "最新 剧集",
        "最新 纪录片",
        "最新 动画",
        "最新 综艺",
        "最新 演唱会",
        "最新 4K",
    ];
    for (r, title) in titles.iter().enumerate() {
        let kind = if r == 0 { Kind::Wide } else { Kind::Poster };
        let cards = (0..24u32)
            .map(|i| {
                let seed = r as u32 * 100 + i;
                Card {
                    id: format!("fake-{seed}").into(),
                    title: NAMES[(seed as usize) % NAMES.len()].into(),
                    sub: format!("{} · 第 {} 集", 2000 + seed % 25, i + 1).into(),
                    image: Some(Key::Fake {
                        seed,
                        wide: kind == Kind::Wide,
                    }),
                }
            })
            .collect();
        rows.push(Row {
            id: format!("row-{r}").into(),
            title: (*title).into(),
            kind,
            cards,
        });
    }
    rows
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Auth {
    access_token: String,
    user: User,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct User {
    id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Items {
    items: Vec<Item>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase", default)]
struct Item {
    id: String,
    name: String,
    collection_type: Option<String>,
    series_name: Option<String>,
    production_year: Option<u32>,
    // Emby sends null instead of an empty list or map on some items
    image_tags: Option<std::collections::HashMap<String, String>>,
    backdrop_image_tags: Option<Vec<String>>,
    parent_backdrop_item_id: Option<String>,
    parent_backdrop_image_tags: Option<Vec<String>>,
}

fn auth_header(token: &str) -> String {
    let mut h = format!(
        r#"Emby UserId="", Client="EmbyMini", Device="spike", DeviceId="embymini-gpui-spike", Version="{}""#,
        env!("CARGO_PKG_VERSION")
    );
    if !token.is_empty() {
        h.push_str(&format!(r#", Token="{token}""#));
    }
    h
}

pub fn client() -> &'static reqwest::Client {
    static C: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    C.get_or_init(|| {
        reqwest::Client::builder()
            .user_agent(concat!("EmbyMini/", env!("CARGO_PKG_VERSION")))
            .no_proxy()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("building the HTTP client")
    })
}

async fn get<T: for<'de> Deserialize<'de>>(url: String, token: String) -> Result<T> {
    let r = client()
        .get(&url)
        .header("X-Emby-Authorization", auth_header(&token))
        .send()
        .await?;
    if !r.status().is_success() {
        bail!("{} → {}", url.split('?').next().unwrap_or(&url), r.status());
    }
    Ok(r.json().await?)
}

async fn emby(base: &str, user: &str, pass: &str) -> Result<Vec<Row>> {
    let r = client()
        .post(format!("{base}/Users/AuthenticateByName"))
        .header("X-Emby-Authorization", auth_header(""))
        .json(&serde_json::json!({ "Username": user, "Pw": pass }))
        .send()
        .await?;
    if !r.status().is_success() {
        bail!("login failed: {}", r.status());
    }
    let auth: Auth = r.json().await?;
    let (t, u) = (auth.access_token, auth.user.id);
    let fields = "Fields=ProductionYear,ParentBackdropItemId,ParentBackdropImageTags";

    let mut rows = Vec::new();
    let resume: Items = get(
        format!("{base}/Users/{u}/Items/Resume?MediaTypes=Video&Limit=16&{fields}"),
        t.clone(),
    )
    .await?;
    rows.push(Row {
        id: "resume".into(),
        title: "继续观看".into(),
        kind: Kind::Wide,
        cards: resume
            .items
            .into_iter()
            .map(|i| card(base, i, Kind::Wide))
            .collect(),
    });
    let views: Items = get(format!("{base}/Users/{u}/Views"), t.clone()).await?;
    let views: Vec<Item> = views
        .items
        .into_iter()
        .filter(|v| {
            matches!(
                v.collection_type.as_deref(),
                Some("movies" | "tvshows") | None
            )
        })
        .take(8)
        .collect();
    let latest = views.iter().map(|v| {
        get::<Vec<Item>>(
            format!(
                "{base}/Users/{u}/Items/Latest?ParentId={}&Limit=24&{fields}",
                v.id
            ),
            t.clone(),
        )
    });
    for (v, items) in views.iter().zip(futures_join(latest).await) {
        // One broken library should not take the whole page down
        let items = match items {
            Ok(items) => items,
            Err(e) => {
                log::warn!("latest in {}: {e:#}", v.name);
                continue;
            }
        };
        rows.push(Row {
            id: format!("latest-{}", v.id).into(),
            title: format!("最新 {}", v.name).into(),
            kind: Kind::Poster,
            cards: items
                .into_iter()
                .map(|i| card(base, i, Kind::Poster))
                .collect(),
        });
    }
    Ok(rows)
}

/// Runs the requests at the same time, keeping their order.
async fn futures_join<F>(fs: impl Iterator<Item = F>) -> Vec<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let handles: Vec<_> = fs.map(tokio::spawn).collect();
    let mut out = Vec::with_capacity(handles.len());
    for h in handles {
        out.push(h.await.expect("request task panicked"));
    }
    out
}

fn card(base: &str, i: Item, kind: Kind) -> Card {
    let primary = || {
        let tag = i.image_tags.as_ref()?.get("Primary")?;
        Some((i.id.clone(), "Primary", tag.clone()))
    };
    let image = match kind {
        Kind::Poster => primary(),
        Kind::Wide => i
            .backdrop_image_tags
            .as_deref()
            .and_then(<[String]>::first)
            .map(|tag| (i.id.clone(), "Backdrop", tag.clone()))
            .or_else(|| {
                let id = i.parent_backdrop_item_id.clone()?;
                Some((
                    id,
                    "Backdrop",
                    i.parent_backdrop_image_tags.as_deref()?.first()?.clone(),
                ))
            })
            .or_else(primary),
    };
    let sub = match (&i.series_name, i.production_year) {
        (Some(s), _) => s.clone(),
        (None, Some(y)) => y.to_string(),
        _ => String::new(),
    };
    Card {
        id: i.id.clone().into(),
        title: i.name.into(),
        sub: sub.into(),
        image: image.map(|(id, ty, tag)| {
            Key::Url(format!(
                "{base}/Items/{id}/Images/{ty}?tag={tag}&quality=90"
            ))
        }),
    }
}
