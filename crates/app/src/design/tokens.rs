//! Design tokens: one neutral palette per mode, type scale and spacing. Monochrome on purpose: the posters bring
//! the colour, the chrome stays out of their way. The primary action is the foreground colour itself (white on
//! dark, near-black on light).

use gpui_kit::{Hsla, rgb, rgba};

/// Horizontal page padding.
pub const PAD: f32 = 40.;
/// Height of the top bar (drag area, history, search).
pub const BAR_H: f32 = 52.;
/// Sidebar width.
pub const SIDEBAR_W: f32 = 232.;
/// Width of the collapsed sidebar (icons only).
pub const RAIL_W: f32 = 64.;
/// Poster card width in rows; grids stretch it to fill the line.
pub const POSTER_W: f32 = 156.;
/// Gap between cards in a row.
pub const GAP: f32 = 16.;

#[derive(Clone, Copy)]
pub struct T {
    pub dark: bool,
    pub bg: Hsla,
    pub sidebar: Hsla,
    /// Image placeholders, segmented-control tracks.
    pub surface: Hsla,
    /// Raised element on a surface (the selected segment).
    pub raised: Hsla,
    pub border: Hsla,
    /// Hairline around artwork, so dark posters keep an edge on a dark page.
    pub edge: Hsla,
    pub fg: Hsla,
    /// Secondary text: metadata, descriptions.
    pub fg2: Hsla,
    /// Tertiary text: placeholders, captions.
    pub fg3: Hsla,
    /// Selected navigation item.
    pub accent: Hsla,
    pub hover: Hsla,
    pub primary: Hsla,
    pub on_primary: Hsla,
}

fn c(v: u32) -> Hsla {
    rgb(v).into()
}

fn ca(v: u32) -> Hsla {
    rgba(v).into()
}

impl T {
    pub fn new(dark: bool) -> Self {
        if dark {
            Self {
                dark,
                bg: c(0x0a0a0a),
                sidebar: c(0x0d0d0d),
                surface: c(0x171717),
                raised: c(0x2a2a2a),
                border: ca(0xffffff14),
                edge: ca(0xffffff0f),
                fg: c(0xfafafa),
                fg2: c(0xa3a3a3),
                fg3: c(0x6b6b6b),
                accent: ca(0xffffff12),
                hover: ca(0xffffff0b),
                primary: c(0xfafafa),
                on_primary: c(0x0a0a0a),
            }
        } else {
            Self {
                dark,
                bg: c(0xffffff),
                sidebar: c(0xfafafa),
                surface: c(0xf4f4f5),
                raised: c(0xffffff),
                border: c(0xe7e7e7),
                edge: ca(0x0000000f),
                fg: c(0x0a0a0a),
                fg2: c(0x6b6b6b),
                fg3: c(0xa1a1a1),
                accent: ca(0x0000000d),
                hover: ca(0x00000008),
                primary: c(0x171717),
                on_primary: c(0xfafafa),
            }
        }
    }
}
