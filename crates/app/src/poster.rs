//! Poster images: fetched (or generated), downscaled to the size drawn and handed to GPUI as decoded frames.
//!
//! The loader is a GPUI `Asset`, so one key loads once and every view drawing it is redrawn when it arrives.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use anyhow::{Context as _, Result, bail};
use gpui_kit::{App, Asset, ImageCacheError, ImageSource, RenderImage, Window};
use image::{DynamicImage, ImageFormat, RgbImage, imageops::FilterType};

/// Where an image comes from. The pixel size is added per draw, so one key can be drawn at several sizes.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub enum Key {
    /// Emby image URL without a width (servers may ignore `maxWidth` and send the original, so we scale locally).
    Url(String),
    /// Generated poster, original-sized like a real one.
    Fake { seed: u32, wide: bool },
}

/// Images decoded so far (bench progress).
pub static LOADED: AtomicUsize = AtomicUsize::new(0);
/// Images that failed.
pub static FAILED: AtomicUsize = AtomicUsize::new(0);

/// At most this many images download at once.
const PARALLEL: usize = 8;

/// Decoding and scaling run on at most half the cores, so the UI thread (and a software renderer) always has CPU
/// left; with every core busy decoding, the window stops drawing until the whole page of posters is done.
fn decode_slots() -> &'static tokio::sync::Semaphore {
    static S: std::sync::OnceLock<tokio::sync::Semaphore> = std::sync::OnceLock::new();
    S.get_or_init(|| {
        let cores = std::thread::available_parallelism().map_or(2, |n| n.get());
        tokio::sync::Semaphore::new((cores / 2).max(1))
    })
}

pub struct Poster;

impl Asset for Poster {
    /// Key plus the target width and height in device pixels.
    type Source = (Key, u32, u32);
    type Output = Result<Arc<RenderImage>, ImageCacheError>;

    fn load(
        source: Self::Source,
        _: &mut App,
    ) -> impl Future<Output = Self::Output> + Send + 'static {
        let rt = crate::rt().handle().clone();
        async move {
            let r = match rt.spawn(load(source)).await {
                Ok(r) => r,
                Err(e) => Err(anyhow::anyhow!("image task: {e}")),
            };
            match r {
                Ok(img) => {
                    LOADED.fetch_add(1, Ordering::Relaxed);
                    Ok(img)
                }
                Err(e) => {
                    FAILED.fetch_add(1, Ordering::Relaxed);
                    log::warn!("image: {e:#}");
                    Err(ImageCacheError::Other(Arc::new(e)))
                }
            }
        }
    }
}

/// Image source for `img()` drawing `key` at `w` × `h` logical pixels.
pub fn source(key: Key, w: f32, h: f32, window: &Window) -> ImageSource {
    let s = window.scale_factor();
    let src = (key, (w * s).round() as u32, (h * s).round() as u32);
    ImageSource::Custom(Arc::new(move |window, cx| {
        window.use_asset::<Poster>(&src, cx)
    }))
}

async fn load((key, w, h): (Key, u32, u32)) -> Result<Arc<RenderImage>> {
    static SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(PARALLEL);
    let _slot = SLOTS.acquire().await?;
    let bytes = match &key {
        Key::Url(url) => {
            let r = crate::data::client().get(url).send().await?;
            if !r.status().is_success() {
                bail!("{} → {}", url.split('?').next().unwrap_or(url), r.status());
            }
            r.bytes().await?.to_vec()
        }
        &Key::Fake { seed, wide } => {
            let _cpu = decode_slots().acquire().await?;
            tokio::task::spawn_blocking(move || generate(seed, wide)).await??
        }
    };
    drop(_slot);
    let _cpu = decode_slots().acquire().await?;
    tokio::task::spawn_blocking(move || decode(&bytes, w, h)).await?
}

/// Decode, cover-crop to the target aspect, scale to the target size, RGBA → BGRA (GPUI's texture order).
fn decode(bytes: &[u8], w: u32, h: u32) -> Result<Arc<RenderImage>> {
    let img = image::load_from_memory(bytes).context("decoding")?;
    let img = img.resize_to_fill(w.max(1), h.max(1), FilterType::Triangle);
    let mut rgba = img.into_rgba8();
    for px in rgba.as_chunks_mut::<4>().0 {
        px.swap(0, 2);
    }
    Ok(Arc::new(RenderImage::new(vec![image::Frame::new(rgba)])))
}

/// A 1000×1500 (or 1920×1080) JPEG, the size servers that ignore `maxWidth` send.
fn generate(seed: u32, wide: bool) -> Result<Vec<u8>> {
    let (w, h) = if wide { (1920, 1080) } else { (1000, 1500) };
    let hue = (seed.wrapping_mul(47) % 360) as f32;
    let [r0, g0, b0] = hsl(hue, 0.55, 0.45);
    let [r1, g1, b1] = hsl((hue + 40.0) % 360.0, 0.6, 0.2);
    let img = RgbImage::from_fn(w, h, |x, y| {
        let t = (x + y) as f32 / (w + h) as f32;
        let n = ((x ^ y).wrapping_mul(2654435761) >> 28) as f32; // grain, so JPEG has work to do
        let mix = |a: u8, b: u8| (a as f32 * (1.0 - t) + b as f32 * t + n).min(255.0) as u8;
        image::Rgb([mix(r0, r1), mix(g0, g1), mix(b0, b1)])
    });
    let mut out = std::io::Cursor::new(Vec::new());
    DynamicImage::ImageRgb8(img).write_to(&mut out, ImageFormat::Jpeg)?;
    Ok(out.into_inner())
}

fn hsl(h: f32, s: f32, l: f32) -> [u8; 3] {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let m = l - c / 2.0;
    let (r, g, b) = match (h / 60.0) as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    [r, g, b].map(|v| ((v + m) * 255.0).round() as u8)
}
