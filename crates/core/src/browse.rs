//! Browsing data for the UI: the fields of Emby replies the pages read, the field lists they ask for, and one typed
//! function per request a page makes (home rows, detail, seasons and episodes, people, library pages, search,
//! favorites) plus the favorite / played / hide-from-resume writes. All go through [`Emby::call`], so the path guard
//! applies; the UI never builds a path or a query itself.
//!
//! Every field may be missing, and servers send `null` for some: both read as the default. Unknown fields are ignored.

use crate::emby::{Emby, Error, NO_QUERY};
use crate::pager::Fetch;
use reqwest::Method;
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::BTreeMap;

/// Missing and `null` both read as the default.
fn null_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

// ---- Fields the pages read (declared from real replies, see testdata/) ----

#[derive(Serialize, Deserialize, Default, Debug, Clone, PartialEq)]
#[serde(rename_all = "PascalCase", default)]
pub struct UserData {
    #[serde(deserialize_with = "null_default")]
    pub playback_position_ticks: i64,
    pub played_percentage: Option<f64>,
    #[serde(deserialize_with = "null_default")]
    pub played: bool,
    #[serde(deserialize_with = "null_default")]
    pub is_favorite: bool,
    /// Series: episodes left; some endpoints leave it out (see [`crate::fmt::series_badge`]).
    pub unplayed_item_count: Option<u32>,
}

#[derive(Serialize, Deserialize, Default, Debug, Clone, PartialEq)]
#[serde(rename_all = "PascalCase", default)]
pub struct Person {
    #[serde(deserialize_with = "null_default")]
    pub id: String,
    #[serde(deserialize_with = "null_default")]
    pub name: String,
    /// The character played (actors) or the job.
    pub role: Option<String>,
    /// Actor, Director, Writer, … ([`crate::fmt::role`]).
    #[serde(rename = "Type", deserialize_with = "null_default")]
    pub kind: String,
    pub primary_image_tag: Option<String>,
}

#[derive(Serialize, Deserialize, Default, Debug, Clone, PartialEq)]
#[serde(rename_all = "PascalCase", default)]
pub struct ExternalUrl {
    #[serde(deserialize_with = "null_default")]
    pub name: String,
    #[serde(deserialize_with = "null_default")]
    pub url: String,
}

/// One track of a version (the media info card).
#[derive(Serialize, Deserialize, Default, Debug, Clone, PartialEq)]
#[serde(rename_all = "PascalCase", default)]
pub struct MediaStream {
    #[serde(deserialize_with = "null_default")]
    pub index: i32,
    /// Video, Audio, Subtitle, …
    #[serde(rename = "Type", deserialize_with = "null_default")]
    pub kind: String,
    pub codec: Option<String>,
    pub language: Option<String>,
    pub display_title: Option<String>,
    pub title: Option<String>,
    #[serde(deserialize_with = "null_default")]
    pub is_external: bool,
    #[serde(deserialize_with = "null_default")]
    pub is_default: bool,
    #[serde(deserialize_with = "null_default")]
    pub is_interlaced: bool,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// SDR, HDR, HDR10, DolbyVision, …
    pub video_range: Option<String>,
    pub real_frame_rate: Option<f64>,
    pub average_frame_rate: Option<f64>,
    pub bit_rate: Option<u64>,
    pub bit_depth: Option<u32>,
    pub profile: Option<String>,
    pub level: Option<f64>,
    pub aspect_ratio: Option<String>,
    pub pixel_format: Option<String>,
    pub channels: Option<u32>,
    pub channel_layout: Option<String>,
    pub sample_rate: Option<u32>,
}

/// One version of a title (1080p, 4K, …).
#[derive(Serialize, Deserialize, Default, Debug, Clone, PartialEq)]
#[serde(rename_all = "PascalCase", default)]
pub struct MediaSource {
    #[serde(deserialize_with = "null_default")]
    pub id: String,
    pub name: Option<String>,
    pub container: Option<String>,
    /// Bytes.
    pub size: Option<u64>,
    pub path: Option<String>,
    #[serde(deserialize_with = "null_default")]
    pub media_streams: Vec<MediaStream>,
}

/// A movie, series, season, episode, library (`CollectionFolder`) or person.
#[derive(Serialize, Deserialize, Default, Debug, Clone, PartialEq)]
#[serde(rename_all = "PascalCase", default)]
pub struct Item {
    #[serde(deserialize_with = "null_default")]
    pub id: String,
    #[serde(deserialize_with = "null_default")]
    pub name: String,
    #[serde(rename = "Type", deserialize_with = "null_default")]
    pub kind: String,
    /// Libraries: movies, tvshows, music, … ([`browsable`]).
    pub collection_type: Option<String>,
    pub series_id: Option<String>,
    pub series_name: Option<String>,
    pub season_id: Option<String>,
    pub index_number: Option<i32>,
    pub parent_index_number: Option<i32>,
    pub production_year: Option<i32>,
    /// ISO date; for a person the birth date.
    pub premiere_date: Option<String>,
    pub end_date: Option<String>,
    /// Person: birthplace.
    #[serde(deserialize_with = "null_default")]
    pub production_locations: Vec<String>,
    /// Series: Continuing / Ended.
    pub status: Option<String>,
    /// Series: episode count (seasons not counted).
    pub recursive_item_count: Option<u32>,
    pub community_rating: Option<f64>,
    pub official_rating: Option<String>,
    pub overview: Option<String>,
    #[serde(deserialize_with = "null_default")]
    pub genres: Vec<String>,
    pub run_time_ticks: Option<i64>,
    pub primary_image_aspect_ratio: Option<f64>,
    /// Image type (Primary, Logo, Thumb, …) → tag.
    #[serde(deserialize_with = "null_default")]
    pub image_tags: BTreeMap<String, String>,
    #[serde(deserialize_with = "null_default")]
    pub backdrop_image_tags: Vec<String>,
    pub parent_backdrop_item_id: Option<String>,
    #[serde(deserialize_with = "null_default")]
    pub parent_backdrop_image_tags: Vec<String>,
    #[serde(deserialize_with = "null_default")]
    pub user_data: UserData,
    #[serde(deserialize_with = "null_default")]
    pub people: Vec<Person>,
    #[serde(deserialize_with = "null_default")]
    pub external_urls: Vec<ExternalUrl>,
    /// Imdb, Tmdb, Douban, … → id.
    #[serde(deserialize_with = "null_default")]
    pub provider_ids: BTreeMap<String, String>,
    #[serde(deserialize_with = "null_default")]
    pub media_sources: Vec<MediaSource>,
}

/// An image of an item, for [`crate::images::Req`]: whose, which type, which tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageRef<'a> {
    pub item: &'a str,
    pub kind: &'static str,
    pub tag: &'a str,
}

impl Item {
    /// Movies and episodes play; series, collections and libraries open instead.
    pub fn playable(&self) -> bool {
        !matches!(self.kind.as_str(), "Series" | "BoxSet" | "CollectionFolder")
    }

    /// The item's own image of `kind` (Primary, Logo, …), if it has one.
    pub fn image(&self, kind: &'static str) -> Option<ImageRef<'_>> {
        let tag = self.image_tags.get(kind)?;
        Some(ImageRef {
            item: &self.id,
            kind,
            tag,
        })
    }

    /// Landscape image: own backdrop → the series' backdrop → own primary (an episode still).
    pub fn wide_image(&self) -> Option<ImageRef<'_>> {
        if let Some(tag) = self.backdrop_image_tags.first() {
            return Some(ImageRef {
                item: &self.id,
                kind: "Backdrop",
                tag,
            });
        }
        if let (Some(item), Some(tag)) = (
            self.parent_backdrop_item_id.as_deref(),
            self.parent_backdrop_image_tags.first(),
        ) {
            return Some(ImageRef {
                item,
                kind: "Backdrop",
                tag,
            });
        }
        self.image("Primary")
    }

    /// The detail page's links: the server's external links, Douban first when the item has its id.
    pub fn links(&self) -> Vec<ExternalUrl> {
        let douban = self.provider_ids.get("Douban").map(|id| ExternalUrl {
            name: "豆瓣".into(),
            url: format!("https://movie.douban.com/subject/{id}/"),
        });
        douban
            .into_iter()
            .chain(self.external_urls.iter().cloned())
            .collect()
    }
}

/// A list reply. `total_record_count` is not always the real size of the list (see [`crate::pager`]).
#[derive(Serialize, Deserialize, Default, Debug, Clone, PartialEq)]
#[serde(rename_all = "PascalCase", default)]
pub struct Page {
    #[serde(deserialize_with = "null_default")]
    pub items: Vec<Item>,
    #[serde(deserialize_with = "null_default")]
    pub total_record_count: usize,
}

// ---- Field lists and limits ----

macro_rules! list_fields {
    () => {
        "ProductionYear,EndDate,CommunityRating,UserData,PrimaryImageAspectRatio,RunTimeTicks,Overview,Genres,OfficialRating,RecursiveItemCount"
    };
}

/// Fields list cards need; the detail adds heavy ones such as People and MediaSources.
pub const LIST_FIELDS: &str = list_fields!();
pub const DETAIL_FIELDS: &str = concat!(
    list_fields!(),
    ",People,ExternalUrls,ProviderIds,MediaSources,Studios,PremiereDate,Status"
);
/// Fields episode cards need.
pub const EP_FIELDS: &str =
    "Overview,MediaSources,UserData,CommunityRating,PremiereDate,RunTimeTicks";
/// Home rows (continue watching, each library's latest) and similar titles.
pub const ROW_LIMIT: usize = 16;
/// Home carousel.
pub const HERO_LIMIT: usize = 8;
/// Search results (one request, not paged).
pub const SEARCH_LIMIT: usize = 60;
/// [`resume_count`] counts up to this many.
pub const RESUME_COUNT_MAX: usize = 1000;
/// Library types browsable as movies / series (music, live TV and playlists stay off the home page and the
/// library wall). A library without a type is a mixed one.
const BROWSABLE: [&str; 5] = ["movies", "tvshows", "homevideos", "boxsets", "mixed"];
const MOVIE_SERIES: &str = "Movie,Series";

/// Libraries the home page and the library wall show.
pub fn browsable(views: Vec<Item>) -> Vec<Item> {
    views
        .into_iter()
        .filter(|v| {
            v.collection_type
                .as_deref()
                .is_none_or(|t| BROWSABLE.contains(&t))
        })
        .collect()
}

// ---- Requests ----

type Query<'a> = [(&'a str, &'a str)];

/// `/Users/{uid}/Items` with `query`.
async fn items(c: &Emby, query: &Query<'_>) -> Result<Page, Error> {
    c.get(&format!("/Users/{}/Items", c.user_id), query).await
}

/// All browsable libraries (home page "my media", the library wall).
pub async fn views(c: &Emby) -> Result<Vec<Item>, Error> {
    let p: Page = c
        .get(&format!("/Users/{}/Views", c.user_id), NO_QUERY)
        .await?;
    Ok(browsable(p.items))
}

/// Home carousel: random movies and series that have a backdrop.
pub async fn hero(c: &Emby) -> Result<Vec<Item>, Error> {
    let limit = HERO_LIMIT.to_string();
    let q = [
        ("Recursive", "true"),
        ("IncludeItemTypes", MOVIE_SERIES),
        ("SortBy", "Random"),
        ("ImageTypes", "Backdrop"),
        ("Limit", limit.as_str()),
        ("Fields", LIST_FIELDS),
    ];
    Ok(items(c, &q).await?.items)
}

/// The home page's continue-watching row: the first [`ROW_LIMIT`] and their count.
#[derive(Serialize, Deserialize, Default, Debug, Clone, PartialEq)]
pub struct ResumeRow {
    pub items: Vec<Item>,
    pub total: usize,
    /// False when the count may be short: the server reported only the number returned as the total (some do),
    /// so [`resume_count`] counts the list afterwards.
    pub exact: bool,
}

pub async fn resume_row(c: &Emby) -> Result<ResumeRow, Error> {
    let limit = ROW_LIMIT.to_string();
    let q = [
        ("Limit", limit.as_str()),
        ("MediaTypes", "Video"),
        ("Fields", LIST_FIELDS),
    ];
    let p: Page = c
        .get(&format!("/Users/{}/Items/Resume", c.user_id), &q)
        .await?;
    Ok(resume_of(p))
}

fn resume_of(p: Page) -> ResumeRow {
    let n = p.items.len();
    ResumeRow {
        total: p.total_record_count.max(n),
        exact: p.total_record_count > n || n < ROW_LIMIT,
        items: p.items,
    }
}

/// A count that stops at a cap: [`resume_count`]. Displays as `n`, or `n+` when capped.
#[derive(Serialize, Deserialize, Default, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Count {
    pub n: usize,
    /// `n` is the cap: there may be more.
    pub capped: bool,
}

impl std::fmt::Display for Count {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}{}", self.n, if self.capped { "+" } else { "" })
    }
}

/// Counts continue watching with a lean request, after the home row is up (when [`ResumeRow::exact`] is false).
/// Shortcut: up to [`RESUME_COUNT_MAX`] (shown as "1000+"), far beyond real lists.
pub async fn resume_count(c: &Emby) -> Result<Count, Error> {
    let limit = RESUME_COUNT_MAX.to_string();
    let q = [
        ("Limit", limit.as_str()),
        ("MediaTypes", "Video"),
        ("EnableImages", "false"),
        ("EnableUserData", "false"),
    ];
    let p: Page = c
        .get(&format!("/Users/{}/Items/Resume", c.user_id), &q)
        .await?;
    let n = p.items.len();
    Ok(Count {
        n,
        capped: n >= RESUME_COUNT_MAX,
    })
}

/// The home page below the carousel: libraries and continue watching.
#[derive(Serialize, Deserialize, Default, Debug, Clone, PartialEq)]
pub struct Home {
    pub views: Vec<Item>,
    pub resume: ResumeRow,
}

/// Asks libraries and continue watching in parallel.
pub async fn home(c: &Emby) -> Result<Home, Error> {
    let (views, resume) = tokio::try_join!(views(c), resume_row(c))?;
    Ok(Home { views, resume })
}

/// A home row with one library's latest items and the library's size.
#[derive(Serialize, Deserialize, Default, Debug, Clone, PartialEq)]
pub struct LibraryRow {
    pub latest: Vec<Item>,
    pub total: usize,
}

pub async fn library_row(c: &Emby, view_id: &str) -> Result<LibraryRow, Error> {
    let limit = ROW_LIMIT.to_string();
    let latest_q = [
        ("ParentId", view_id),
        ("Limit", limit.as_str()),
        ("Fields", LIST_FIELDS),
    ];
    let count_q = [
        ("ParentId", view_id),
        ("Recursive", "true"),
        ("IncludeItemTypes", MOVIE_SERIES),
        ("Limit", "0"),
    ];
    let latest_path = format!("/Users/{}/Items/Latest", c.user_id);
    let (latest, count) = tokio::try_join!(
        c.get::<Vec<Item>, _>(&latest_path, &latest_q),
        items(c, &count_q)
    )?;
    Ok(LibraryRow {
        latest,
        total: count.total_record_count,
    })
}

/// Item detail with every field the detail page shows; hover prefetch and the detail page ask the same.
pub async fn item(c: &Emby, id: &str) -> Result<Item, Error> {
    c.get(
        &format!("/Users/{}/Items/{id}", c.user_id),
        &[("Fields", DETAIL_FIELDS)],
    )
    .await
}

/// An item with the default fields only: a library's name, a person's photo, overview, birth date and place.
pub async fn item_brief(c: &Emby, id: &str) -> Result<Item, Error> {
    c.get(&format!("/Users/{}/Items/{id}", c.user_id), NO_QUERY)
        .await
}

/// A series' seasons.
pub async fn seasons(c: &Emby, series_id: &str) -> Result<Vec<Item>, Error> {
    let q = [("UserId", c.user_id.as_str()), ("Fields", "UserData")];
    let p: Page = c.get(&format!("/Shows/{series_id}/Seasons"), &q).await?;
    Ok(p.items)
}

/// The episode a series continues with (what its play button plays), if any.
pub async fn next_up(c: &Emby, series_id: &str) -> Result<Option<Item>, Error> {
    let q = [
        ("SeriesId", series_id),
        ("UserId", c.user_id.as_str()),
        ("Limit", "1"),
        ("Fields", DETAIL_FIELDS),
    ];
    let p: Page = c.get("/Shows/NextUp", &q).await?;
    Ok(p.items.into_iter().next())
}

pub async fn similar(c: &Emby, id: &str) -> Result<Vec<Item>, Error> {
    let limit = ROW_LIMIT.to_string();
    let q = [
        ("UserId", c.user_id.as_str()),
        ("Limit", limit.as_str()),
        ("Fields", LIST_FIELDS),
    ];
    let p: Page = c.get(&format!("/Items/{id}/Similar"), &q).await?;
    Ok(p.items)
}

/// One season's episodes, in order, with what episode cards show.
pub async fn episodes(c: &Emby, series_id: &str, season_id: &str) -> Result<Vec<Item>, Error> {
    let q = [
        ("UserId", c.user_id.as_str()),
        ("SeasonId", season_id),
        ("Fields", EP_FIELDS),
    ];
    let p: Page = c.get(&format!("/Shows/{series_id}/Episodes"), &q).await?;
    Ok(p.items)
}

/// The detail page: the item (an episode opens its series), and for a series its seasons and next up.
#[derive(Serialize, Deserialize, Default, Debug, Clone, PartialEq)]
pub struct Detail {
    pub item: Item,
    pub seasons: Vec<Item>,
    pub next: Option<Item>,
    /// Similar titles are decoration: `None` when they failed (upstream 429…), so the page keeps what it showed
    /// or leaves the row out instead of failing. Seasons and next up do fail the page (play must target the right
    /// episode).
    pub similar: Option<Vec<Item>>,
    /// The season to show: the episode's when opened from one, else next up's, else the first.
    pub season: Option<String>,
}

/// Seasons and next up of a series, asked together.
async fn extras(c: &Emby, series_id: &str) -> Result<(Vec<Item>, Option<Item>), Error> {
    tokio::try_join!(seasons(c, series_id), next_up(c, series_id))
}

/// Loads the detail page; seasons, next up and similar go out in parallel. `guess_series`: the series a list
/// already says this is (the item itself, or an episode's series); its extras are then asked along with the
/// detail instead of after it, and dropped if the guess was wrong.
pub async fn detail(c: &Emby, id: &str, guess_series: Option<&str>) -> Result<Detail, Error> {
    let (first, early) = match guess_series {
        Some(g) => {
            let (first, ex, sim) = tokio::join!(item(c, id), extras(c, g), similar(c, g));
            (first, Some((g, ex, sim)))
        }
        None => (item(c, id).await, None),
    };
    let mut it = first?;
    let mut season = None;
    if it.kind == "Episode"
        && let Some(series) = it.series_id.clone()
    {
        // Opened from an episode: show its series with that season selected
        season = it.season_id.take();
        it = item(c, &series).await?;
    }
    let is_series = it.kind == "Series";
    let (extras, similar) = match early {
        Some((g, ex, sim)) if is_series && g == it.id => (Some(ex?), sim.ok()),
        _ if is_series => {
            let (ex, sim) = tokio::join!(extras(c, &it.id), similar(c, &it.id));
            (Some(ex?), sim.ok())
        }
        _ => (None, similar(c, &it.id).await.ok()),
    };
    let (seasons, next) = extras.unwrap_or_default();
    let season = season
        .or_else(|| next.as_ref().and_then(|n| n.season_id.clone()))
        .or_else(|| seasons.first().map(|s| s.id.clone()));
    Ok(Detail {
        item: it,
        seasons,
        next,
        similar,
        season,
    })
}

/// The whole-season page: the series, its seasons (to switch) and this season's episodes.
#[derive(Serialize, Deserialize, Default, Debug, Clone, PartialEq)]
pub struct SeasonPage {
    pub series: Item,
    pub seasons: Vec<Item>,
    pub episodes: Vec<Item>,
}

pub async fn season(c: &Emby, series_id: &str, season_id: &str) -> Result<SeasonPage, Error> {
    let (series, seasons, episodes) = tokio::try_join!(
        item(c, series_id),
        seasons(c, series_id),
        episodes(c, series_id, season_id)
    )?;
    Ok(SeasonPage {
        series,
        seasons,
        episodes,
    })
}

/// Favorite movies and series by name, in one request (favorites lists are short).
pub async fn favorites(c: &Emby) -> Result<Vec<Item>, Error> {
    let q = [
        ("Recursive", "true"),
        ("Filters", "IsFavorite"),
        ("IncludeItemTypes", MOVIE_SERIES),
        ("SortBy", "SortName"),
        ("Fields", LIST_FIELDS),
    ];
    Ok(items(c, &q).await?.items)
}

/// Movies and series matching `term`, the first [`SEARCH_LIMIT`].
pub async fn search(c: &Emby, term: &str) -> Result<Vec<Item>, Error> {
    let limit = SEARCH_LIMIT.to_string();
    let q = [
        ("Recursive", "true"),
        ("SearchTerm", term),
        ("IncludeItemTypes", MOVIE_SERIES),
        ("Limit", limit.as_str()),
        ("Fields", LIST_FIELDS),
    ];
    Ok(items(c, &q).await?.items)
}

/// Library sort orders.
#[derive(Serialize, Deserialize, Default, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Sort {
    #[default]
    DateCreated,
    SortName,
    ProductionYear,
    CommunityRating,
}

impl Sort {
    pub const ALL: [Sort; 4] = [
        Sort::DateCreated,
        Sort::SortName,
        Sort::ProductionYear,
        Sort::CommunityRating,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Sort::DateCreated => "最近加入",
            Sort::SortName => "名称",
            Sort::ProductionYear => "年份",
            Sort::CommunityRating => "评分",
        }
    }

    /// `SortBy` and `SortOrder`: by name ascending; the others newest / highest first, the name breaking ties.
    fn query(self) -> (&'static str, &'static str) {
        match self {
            Sort::DateCreated => ("DateCreated,SortName", "Descending"),
            Sort::SortName => ("SortName", "Ascending"),
            Sort::ProductionYear => ("ProductionYear,SortName", "Descending"),
            Sort::CommunityRating => ("CommunityRating,SortName", "Descending"),
        }
    }
}

/// A list read page by page with a [`crate::pager::Pager`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum List {
    /// A library's movies and series; `unplayed` filters server-side so the total stays right.
    Library {
        id: String,
        sort: Sort,
        unplayed: bool,
    },
    /// A person's movies and series, newest first.
    Person { id: String },
    /// Everything in continue watching (the wall behind the home row's count).
    Resume,
}

impl List {
    fn path(&self, user_id: &str) -> String {
        match self {
            List::Resume => format!("/Users/{user_id}/Items/Resume"),
            _ => format!("/Users/{user_id}/Items"),
        }
    }

    fn query(&self) -> Vec<(&'static str, &str)> {
        let mut q = Vec::new();
        match self {
            List::Library { id, sort, unplayed } => {
                let (by, order) = sort.query();
                q.extend([
                    ("ParentId", id.as_str()),
                    ("SortBy", by),
                    ("SortOrder", order),
                ]);
                if *unplayed {
                    q.push(("Filters", "IsUnplayed"));
                }
            }
            List::Person { id } => q.extend([
                ("PersonIds", id.as_str()),
                ("SortBy", "ProductionYear,PremiereDate,SortName"),
                ("SortOrder", "Descending"),
            ]),
            List::Resume => {
                q.extend([("MediaTypes", "Video"), ("Fields", LIST_FIELDS)]);
                return q;
            }
        }
        q.extend([
            ("Recursive", "true"),
            ("IncludeItemTypes", MOVIE_SERIES),
            ("Fields", LIST_FIELDS),
        ]);
        q
    }

    /// Snapshot key of the list's first page ([`crate::pager::Snap`]); the app scopes it to the server.
    pub fn key(&self) -> String {
        let q: Vec<String> = self
            .query()
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        format!("pager|{}|{}", self.path("-"), q.join("&"))
    }
}

/// One page of `list`, as the pager asked for it.
pub async fn page(c: &Emby, list: &List, at: Fetch) -> Result<Page, Error> {
    let (start, limit) = (at.index.to_string(), at.limit.to_string());
    let mut q = list.query();
    q.extend([("StartIndex", start.as_str()), ("Limit", limit.as_str())]);
    c.get(&list.path(&c.user_id), &q).await
}

async fn send(c: &Emby, method: Method, path: &str, query: &Query<'_>) -> Result<(), Error> {
    c.call(method, path, query, None).await.map(drop)
}

fn on_off(on: bool) -> Method {
    if on { Method::POST } else { Method::DELETE }
}

pub async fn set_favorite(c: &Emby, id: &str, on: bool) -> Result<(), Error> {
    send(
        c,
        on_off(on),
        &format!("/Users/{}/FavoriteItems/{id}", c.user_id),
        NO_QUERY,
    )
    .await
}

pub async fn set_played(c: &Emby, id: &str, on: bool) -> Result<(), Error> {
    send(
        c,
        on_off(on),
        &format!("/Users/{}/PlayedItems/{id}", c.user_id),
        NO_QUERY,
    )
    .await
}

/// Removes an item from continue watching (server side, progress kept); `hide = false` undoes it.
pub async fn hide_from_resume(c: &Emby, id: &str, hide: bool) -> Result<(), Error> {
    let path = format!("/Users/{}/Items/{id}/HideFromResume", c.user_id);
    send(
        c,
        Method::POST,
        &path,
        &[("Hide", if hide { "true" } else { "false" })],
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emby::tests::{fake, fixture, local};

    /// Real replies parse; fields missing or `null` take their defaults.
    #[test]
    fn fixtures_parse() {
        let views: Page = fixture("views.json");
        assert!(!views.items.is_empty(), "views");
        assert!(
            views
                .items
                .iter()
                .all(|v| v.kind == "CollectionFolder" || v.kind == "UserView"),
            "libraries"
        );
        assert!(!browsable(views.items).is_empty(), "browsable libraries");

        let list: Page = fixture("list.json");
        assert!(
            list.total_record_count >= list.items.len() && !list.items.is_empty(),
            "list"
        );
        assert!(
            list.items
                .iter()
                .all(|i| !i.id.is_empty() && !i.name.is_empty() && i.image("Primary").is_some()),
            "cards"
        );

        let latest: Vec<Item> = fixture("latest.json");
        assert!(!latest.is_empty(), "latest is a bare array");

        let d: Item = fixture("detail.json");
        assert_eq!(d.kind, "Movie", "detail");
        assert!(
            !d.people.is_empty() && d.people.iter().all(|p| !p.kind.is_empty()),
            "people"
        );
        let src = d.media_sources.first().expect("a version");
        assert!(src.size.is_some(), "size");
        let v = src
            .media_streams
            .iter()
            .find(|s| s.kind == "Video")
            .expect("a video track");
        assert!(v.width.is_some() && v.height.is_some(), "resolution");
        assert!(!crate::fmt::tech_line(Some(src)).is_empty(), "tech line");
        assert!(
            d.run_time_ticks.is_some() && d.production_year.is_some(),
            "runtime and year"
        );

        let eps: Page = fixture("episodes.json");
        assert!(
            eps.items
                .iter()
                .all(|e| e.kind == "Episode" && e.index_number.is_some()),
            "episodes"
        );
        let series: Item = fixture("series.json");
        assert_eq!(series.kind, "Series", "series");

        let nulls: Item = serde_json::from_str(
            r#"{"Id":"1","Name":null,"Genres":null,"UserData":null,"CommunityRating":null,"People":[{"Id":"p","Role":null}],"Unknown":1}"#,
        )
        .unwrap();
        assert!(
            nulls.name.is_empty() && nulls.genres.is_empty() && nulls.community_rating.is_none(),
            "nulls"
        );
        assert_eq!(nulls.people[0].role, None, "null role");
    }

    /// Snapshots store the same shapes: an item survives a round trip.
    #[test]
    fn items_round_trip() {
        let d: Item = fixture("detail.json");
        let back: Item = serde_json::from_str(&serde_json::to_string(&d).unwrap()).unwrap();
        assert_eq!(back, d, "round trip");
    }

    #[test]
    fn browsable_and_images() {
        let view = |t: Option<&str>| Item {
            collection_type: t.map(Into::into),
            ..Default::default()
        };
        let kept = browsable(vec![
            view(Some("movies")),
            view(Some("music")),
            view(None),
            view(Some("livetv")),
        ]);
        assert_eq!(kept.len(), 2, "movies and the untyped (mixed) library");

        let mut it = Item {
            id: "e".into(),
            kind: "Episode".into(),
            ..Default::default()
        };
        assert_eq!(it.wide_image(), None, "no image");
        it.image_tags.insert("Primary".into(), "p".into());
        assert_eq!(
            it.wide_image(),
            Some(ImageRef {
                item: "e",
                kind: "Primary",
                tag: "p"
            }),
            "still"
        );
        it.parent_backdrop_item_id = Some("s".into());
        it.parent_backdrop_image_tags = vec!["sb".into()];
        assert_eq!(
            it.wide_image(),
            Some(ImageRef {
                item: "s",
                kind: "Backdrop",
                tag: "sb"
            }),
            "series backdrop"
        );
        it.backdrop_image_tags = vec!["b".into()];
        assert_eq!(
            it.wide_image(),
            Some(ImageRef {
                item: "e",
                kind: "Backdrop",
                tag: "b"
            }),
            "own backdrop"
        );
        assert!(it.playable(), "episodes play");
        assert!(
            !Item {
                kind: "Series".into(),
                ..Default::default()
            }
            .playable(),
            "series open"
        );

        it.external_urls = vec![ExternalUrl {
            name: "IMDb".into(),
            url: "https://imdb.example/t".into(),
        }];
        it.provider_ids.insert("Douban".into(), "42".into());
        let names: Vec<String> = it.links().into_iter().map(|l| l.name).collect();
        assert_eq!(names, ["豆瓣", "IMDb"], "Douban first");
    }

    /// The continue-watching row: a reported count equal to the number returned may be short.
    #[test]
    fn resume_row_exactness() {
        let items = |n: usize| vec![Item::default(); n];
        let r = resume_of(Page {
            items: items(16),
            total_record_count: 16,
        });
        assert!(!r.exact && r.total == 16, "full row, reported count");
        let r = resume_of(Page {
            items: items(16),
            total_record_count: 40,
        });
        assert!(r.exact && r.total == 40, "real total");
        let r = resume_of(Page {
            items: items(5),
            total_record_count: 5,
        });
        assert!(r.exact && r.total == 5, "short row is all of it");
        assert_eq!(
            Count {
                n: 1000,
                capped: true
            }
            .to_string(),
            "1000+",
            "capped count"
        );
    }

    /// Paged lists ask with their own query plus the pager's StartIndex / Limit; keys differ per query.
    #[test]
    fn list_queries_and_keys() {
        let lib = |sort, unplayed| List::Library {
            id: "v".into(),
            sort,
            unplayed,
        };
        let by_name = lib(Sort::SortName, true);
        let q = by_name.query();
        for want in [
            ("ParentId", "v"),
            ("SortBy", "SortName"),
            ("SortOrder", "Ascending"),
            ("Filters", "IsUnplayed"),
        ] {
            assert!(q.contains(&want), "{want:?} in {q:?}");
        }
        let by_rating = lib(Sort::CommunityRating, false);
        let q = by_rating.query();
        assert!(
            q.contains(&("SortBy", "CommunityRating,SortName"))
                && !q.iter().any(|(k, _)| *k == "Filters"),
            "{q:?}"
        );
        let keys: std::collections::HashSet<String> = [
            lib(Sort::DateCreated, false),
            lib(Sort::DateCreated, true),
            lib(Sort::SortName, false),
            List::Person { id: "v".into() },
            List::Resume,
        ]
        .iter()
        .map(List::key)
        .collect();
        assert_eq!(keys.len(), 5, "one key per query");
        assert!(
            List::Resume.path("u").ends_with("/Items/Resume"),
            "resume path"
        );
    }

    /// Requests go to the expected paths with the expected queries and decode the replies.
    #[tokio::test]
    async fn requests_against_a_fake_server() {
        let (port, heads) = fake(|path| match path {
            "/emby/users/uid/views" => ("200 OK", crate::emby::tests::testdata("views.json")),
            "/emby/users/uid/items/latest" => {
                ("200 OK", crate::emby::tests::testdata("latest.json"))
            }
            "/emby/users/uid/items" | "/emby/users/uid/items/resume" | "/emby/shows/nextup" => {
                ("200 OK", crate::emby::tests::testdata("list.json"))
            }
            "/emby/users/uid/items/m" => ("200 OK", crate::emby::tests::testdata("detail.json")),
            // series.json's id
            "/emby/users/uid/items/1354590" => {
                ("200 OK", crate::emby::tests::testdata("series.json"))
            }
            "/emby/shows/1354590/seasons" => (
                "200 OK",
                r#"{"Items":[{"Id":"s1","Type":"Season"}]}"#.into(),
            ),
            "/emby/shows/1354590/episodes" => {
                ("200 OK", crate::emby::tests::testdata("episodes.json"))
            }
            "/emby/items/1354590/similar" => ("200 OK", crate::emby::tests::testdata("list.json")),
            "/emby/users/uid/favoriteitems/m" | "/emby/users/uid/items/m/hidefromresume" => {
                ("204 No Content", String::new())
            }
            _ => ("404 Not Found", String::new()),
        })
        .await;
        let c = local(port);
        let last = || heads.lock().unwrap().last().unwrap().clone();

        assert!(!home(&c).await.unwrap().views.is_empty(), "home");
        let row = library_row(&c, "v").await.unwrap();
        assert!(!row.latest.is_empty() && row.total > 0, "library row");
        let d = detail(&c, "m", None).await.unwrap();
        assert!(
            d.item.kind == "Movie" && d.seasons.is_empty() && d.similar.is_none(),
            "movie detail, similar failed quietly"
        );
        assert!(
            last().contains("/Similar?"),
            "similar asked (and got a 404)"
        );
        let d = detail(&c, "1354590", Some("1354590")).await.unwrap();
        assert_eq!(d.seasons.len(), 1, "seasons");
        assert!(
            d.next.is_some() && d.similar.is_some(),
            "next up and similar"
        );
        assert_eq!(
            d.season.as_deref(),
            Some("s1"),
            "next up has no season: the first one"
        );
        // A wrong guess (say a stale list) is dropped and the right series asked
        let d = detail(&c, "1354590", Some("m")).await.unwrap();
        assert_eq!(d.seasons.len(), 1, "seasons of the real series");
        let s = season(&c, "1354590", "s1").await.unwrap();
        assert!(
            s.episodes.len() > 1 && s.series.kind == "Series",
            "season page"
        );

        let pg = page(
            &c,
            &List::Resume,
            crate::pager::Pager::new(None).next_page().unwrap(),
        )
        .await
        .unwrap();
        assert!(!pg.items.is_empty(), "page");
        let head = last();
        assert!(
            head.contains("StartIndex=0")
                && head.contains("Limit=60")
                && head.contains("MediaTypes=Video"),
            "{head}"
        );

        search(&c, "a b").await.unwrap();
        assert!(last().contains("SearchTerm=a+b"), "search term encoded");
        set_favorite(&c, "m", true).await.unwrap();
        assert!(
            last().starts_with("POST /emby/Users/uid/FavoriteItems/m"),
            "favorite"
        );
        hide_from_resume(&c, "m", false).await.unwrap();
        assert!(last().contains("HideFromResume?Hide=false"), "undo hide");
        assert!(
            matches!(set_played(&c, "m", false).await, Err(Error::Status(404, _))),
            "errors pass through"
        );
        assert!(last().starts_with("DELETE "), "unplayed is a DELETE");
        assert!(
            matches!(item(&c, "x?y=1").await, Err(Error::Other(ref m)) if m.starts_with("非法路径")),
            "path guard"
        );
    }

    /// Every browse function against a real server parses its reply. Favorite is toggled and put back.
    #[tokio::test]
    #[ignore = "需要真服账号（EMBY_SERVER / PORT / EMBY_USER / EMBY_PASS）"]
    async fn browse_real_server() {
        let c = crate::player::tests::login_from_env().await;
        let h = home(&c).await.expect("home");
        let view = h.views.first().expect("a browsable library").id.clone();
        assert!(!hero(&c).await.expect("hero").is_empty(), "carousel");
        let n = resume_count(&c).await.expect("resume count");
        assert!(n.n >= h.resume.items.len(), "count covers the row");
        let row = library_row(&c, &view).await.expect("library row");
        assert!(row.total > 0, "library has items");
        assert_eq!(
            item_brief(&c, &view).await.expect("library").id,
            view,
            "library by id"
        );

        let mut pg = crate::pager::Pager::new(None);
        let list = List::Library {
            id: view,
            sort: Sort::DateCreated,
            unplayed: false,
        };
        let f = pg.next_page().expect("first page");
        assert!(
            matches!(
                pg.apply(f, page(&c, &list, f).await.expect("page")),
                crate::pager::Applied::Merged(Some(_))
            ),
            "first page"
        );
        if let Some(f) = pg.next_page() {
            pg.apply(f, page(&c, &list, f).await.expect("page 2"));
        }
        assert!(!pg.items().is_empty(), "library pages");

        let series = items(
            &c,
            &[
                ("Recursive", "true"),
                ("IncludeItemTypes", "Series"),
                ("Limit", "1"),
                ("Fields", LIST_FIELDS),
            ],
        )
        .await
        .expect("a series")
        .items;
        let series = series.first().expect("the server has a series");
        let d = detail(&c, &series.id, Some(&series.id))
            .await
            .expect("series detail");
        let season_id = d.season.clone().expect("a season");
        let eps = episodes(&c, &series.id, &season_id)
            .await
            .expect("episodes");
        assert!(!eps.is_empty(), "episodes");
        let s = season(&c, &series.id, &season_id)
            .await
            .expect("season page");
        assert_eq!(s.episodes.len(), eps.len(), "same episodes");
        let from_ep = detail(&c, &eps[0].id, None)
            .await
            .expect("detail from an episode");
        assert_eq!(
            (from_ep.item.id.as_str(), from_ep.season.as_deref()),
            (series.id.as_str(), Some(season_id.as_str())),
            "episode opens its series"
        );

        let movie = items(
            &c,
            &[
                ("Recursive", "true"),
                ("IncludeItemTypes", "Movie"),
                ("Limit", "1"),
            ],
        )
        .await
        .expect("a movie")
        .items;
        let movie = movie.first().expect("the server has a movie");
        let m = detail(&c, &movie.id, None).await.expect("movie detail");
        if let Some(p) = m.item.people.first().or(d.item.people.first()) {
            let person = item_brief(&c, &p.id).await.expect("person");
            assert_eq!(person.id, p.id, "person by id");
            let mut works = crate::pager::Pager::new(None);
            let list = List::Person { id: p.id.clone() };
            let f = works.next_page().expect("works");
            works.apply(f, page(&c, &list, f).await.expect("works page"));
        }
        let mut resume = crate::pager::Pager::new(None);
        let f = resume.next_page().expect("resume");
        resume.apply(f, page(&c, &List::Resume, f).await.expect("resume wall"));

        let term: String = movie.name.chars().take(2).collect();
        assert!(
            !search(&c, &term).await.expect("search").is_empty(),
            "search finds the movie"
        );

        let was = item(&c, &movie.id)
            .await
            .expect("item")
            .user_data
            .is_favorite;
        set_favorite(&c, &movie.id, !was)
            .await
            .expect("toggle favorite");
        let now = item(&c, &movie.id)
            .await
            .expect("item")
            .user_data
            .is_favorite;
        let listed = favorites(&c)
            .await
            .expect("favorites")
            .iter()
            .any(|f| f.id == movie.id);
        set_favorite(&c, &movie.id, was)
            .await
            .expect("put favorite back");
        assert_eq!((now, listed), (!was, !was), "favorite toggled and listed");
    }
}
