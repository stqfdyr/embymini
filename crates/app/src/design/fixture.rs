//! Static data for the design pages: a fixture.json captured from a real server plus its images, kept outside the
//! repository (the images are the server's). Every field is optional; a page shows what the fixture has.

use std::{collections::HashMap, path::PathBuf, rc::Rc};

use serde::Deserialize;

#[derive(Deserialize, Default, Clone)]
#[serde(rename_all = "PascalCase", default)]
pub struct Item {
    pub id: String,
    pub name: String,
    #[serde(rename = "Type")]
    pub kind: String,
    pub series_name: Option<String>,
    pub series_id: Option<String>,
    pub production_year: Option<u32>,
    pub official_rating: Option<String>,
    pub community_rating: Option<f32>,
    pub run_time_ticks: Option<u64>,
    pub genres: Vec<String>,
    pub overview: Option<String>,
    pub index_number: Option<u32>,
    pub parent_index_number: Option<u32>,
    pub child_count: Option<u32>,
    pub collection_type: Option<String>,
    pub played: Option<f32>,
    #[serde(rename = "poster")]
    pub poster: Option<String>,
    #[serde(rename = "backdrop")]
    pub backdrop: Option<String>,
    #[serde(rename = "thumb")]
    pub thumb: Option<String>,
    #[serde(rename = "logo")]
    pub logo: Option<String>,
    #[serde(rename = "seriesPoster")]
    pub series_poster: Option<String>,
    #[serde(rename = "video")]
    pub video: Option<Video>,
    #[serde(rename = "audio")]
    pub audio: Vec<String>,
    pub people: Vec<Person>,
    #[serde(rename = "seasons")]
    pub seasons: Vec<Item>,
    #[serde(rename = "episodes")]
    pub episodes: Vec<Item>,
    #[serde(rename = "similar")]
    pub similar: Vec<Item>,
}

impl Item {
    /// Minutes of running time.
    pub fn minutes(&self) -> Option<u64> {
        self.run_time_ticks
            .map(|t| t / 600_000_000)
            .filter(|&m| m > 0)
    }

    pub fn is_series(&self) -> bool {
        self.kind == "Series"
    }
}

#[derive(Deserialize, Default, Clone)]
#[serde(rename_all = "PascalCase", default)]
pub struct Video {
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub codec: Option<String>,
    pub video_range: Option<String>,
}

#[derive(Deserialize, Default, Clone)]
#[serde(rename_all = "PascalCase", default)]
pub struct Person {
    pub name: String,
    pub role: Option<String>,
    #[serde(rename = "Type")]
    pub kind: Option<String>,
    #[serde(rename = "img")]
    pub img: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase", default)]
pub struct Library {
    pub name: String,
    pub total: u32,
    #[serde(rename = "items")]
    pub items: Vec<Item>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct Fixture {
    pub views: Vec<Item>,
    pub resume: Vec<Item>,
    pub latest: HashMap<String, Vec<Item>>,
    pub hero: Vec<Item>,
    pub series: Item,
    pub movie: Item,
    pub library: Library,
    /// Full detail (people, seasons with their episodes, similar) of every title a card leads to.
    #[serde(rename = "details")]
    pub details: HashMap<String, Item>,
    #[serde(skip)]
    dir: PathBuf,
}

impl Fixture {
    pub fn load(dir: PathBuf) -> anyhow::Result<Rc<Self>> {
        let text = std::fs::read_to_string(dir.join("fixture.json"))?;
        let mut f: Self = serde_json::from_str(&text)?;
        f.dir = dir;
        Ok(Rc::new(f))
    }

    /// Path of a fixture image, if it was downloaded.
    pub fn img(&self, name: Option<&String>) -> Option<PathBuf> {
        let p = self.dir.join("img").join(name?);
        p.is_file().then_some(p)
    }

    /// The "latest" rows in library order.
    pub fn latest_rows(&self) -> Vec<(&str, &[Item])> {
        self.views
            .iter()
            .filter_map(|v| Some((v.name.as_str(), self.latest.get(&v.name)?.as_slice())))
            .collect()
    }
}
