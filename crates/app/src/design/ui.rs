//! Building blocks shared by the design pages: artwork, cards, section headers, buttons, badges.

use std::path::{Path, PathBuf};

use gpui_kit::assets::IconName;
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::fixture::{Fixture, Item};
use super::tokens::{PAD, T};

pub fn icon(name: IconName, size: f32, color: Hsla) -> Svg {
    svg()
        .path(name.path())
        .size(px(size))
        .flex_shrink_0()
        .text_color(color)
}

pub fn text(size: f32, color: Hsla, s: impl Into<SharedString>) -> Div {
    div().text_size(px(size)).text_color(color).child(s.into())
}

pub fn medium(d: Div) -> Div {
    d.font_weight(FontWeight::MEDIUM)
}

pub fn semibold(d: Div) -> Div {
    d.font_weight(FontWeight::SEMIBOLD)
}

/// Artwork box: a placeholder surface until (or unless) the image is there, and a hairline edge on top.
pub fn art(path: Option<PathBuf>, w: f32, h: f32, r: f32, t: T) -> Div {
    let d = div()
        .relative()
        .w(px(w))
        .h(px(h))
        .flex_shrink_0()
        .rounded(px(r))
        .bg(t.surface);
    let d = match path {
        Some(p) => d.child(
            img(p)
                .absolute()
                .inset_0()
                .size_full()
                .rounded(px(r))
                .object_fit(ObjectFit::Cover),
        ),
        None => d,
    };
    d.child(
        div()
            .absolute()
            .inset_0()
            .rounded(px(r))
            .border_1()
            .border_color(t.edge),
    )
}

/// Darkens the artwork of the hovered card a little and shows a play button in its corner.
fn hover_veil(r: f32, play: bool) -> Div {
    let veil = div()
        .absolute()
        .inset_0()
        .rounded(px(r))
        .group_hover("card", |s| s.bg(hsla(0., 0., 0., 0.22)));
    if !play {
        return veil;
    }
    veil.child(
        div()
            .absolute()
            .left(px(10.))
            .bottom(px(10.))
            .size(px(34.))
            .rounded_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(hsla(0., 0., 1., 0.95))
            .opacity(0.)
            .group_hover("card", |s| s.opacity(1.))
            .child(icon(IconName::Play, 15., hsla(0., 0., 0.04, 1.)).ml(px(2.))),
    )
}

pub fn year(it: &Item) -> String {
    it.production_year
        .map(|y| y.to_string())
        .unwrap_or_default()
}

/// 2:3 poster with title and year.
pub fn poster_card(
    id: impl Into<ElementId>,
    it: &Item,
    fx: &Fixture,
    w: f32,
    t: T,
) -> Stateful<Div> {
    let h = (w * 1.5).round();
    let path = fx
        .img(it.poster.as_ref())
        .or_else(|| fx.img(it.series_poster.as_ref()));
    let sub = match (it.is_series(), it.child_count) {
        (true, Some(n)) if n > 0 => format!("{} · {n} 季", year(it)),
        _ => year(it),
    };
    let played = it.played.unwrap_or(0.);
    v_flex()
        .id(id)
        .w(px(w))
        .gap(px(9.))
        .flex_shrink_0()
        .cursor_pointer()
        .group("card")
        .child(
            art(path, w, h, 8., t)
                .child(hover_veil(8., true))
                .when(played > 0., |d| d.child(progress(played, 8.))),
        )
        .child(
            v_flex()
                .gap(px(2.))
                .child(medium(text(13., t.fg, it.name.clone())).truncate())
                .child(text(12., t.fg2, sub).truncate()),
        )
}

/// Thin progress bar laid over the bottom of artwork.
pub fn progress(pct: f32, inset: f32) -> Div {
    div()
        .absolute()
        .left(px(inset))
        .right(px(inset))
        .bottom(px(inset))
        .h(px(3.))
        .rounded_full()
        .bg(hsla(0., 0., 1., 0.28))
        .child(
            div()
                .h_full()
                .w(relative(pct.clamp(0., 100.) / 100.))
                .rounded_full()
                .bg(hsla(0., 0., 1., 0.95)),
        )
}

pub fn minutes_label(m: u64) -> String {
    if m >= 60 {
        format!("{} 小时 {} 分", m / 60, m % 60)
    } else {
        format!("{m} 分钟")
    }
}

/// 16:9 card for "continue watching": artwork with progress, the show on the first line, where you are on the
/// second.
pub fn wide_card(id: impl Into<ElementId>, it: &Item, fx: &Fixture, w: f32, t: T) -> Stateful<Div> {
    let h = (w * 9. / 16.).round();
    let path = fx
        .img(it.thumb.as_ref())
        .or_else(|| fx.img(it.backdrop.as_ref()))
        .or_else(|| fx.img(it.poster.as_ref()));
    let (title, mut sub) = match &it.series_name {
        Some(s) => {
            let code = match (it.parent_index_number, it.index_number) {
                (Some(s), Some(e)) => format!("S{s}:E{e} · "),
                _ => String::new(),
            };
            (s.clone(), format!("{code}{}", it.name))
        }
        None => (it.name.clone(), year(it)),
    };
    if let (Some(m), Some(p)) = (it.minutes(), it.played) {
        let left = ((m as f32) * (1. - p / 100.)).round() as u64;
        sub = format!("{sub} · 剩余 {}", minutes_label(left.max(1)));
    }
    v_flex()
        .id(id)
        .w(px(w))
        .gap(px(9.))
        .flex_shrink_0()
        .cursor_pointer()
        .group("card")
        .child(
            art(path, w, h, 8., t)
                .child(hover_veil(8., true))
                .when_some(it.played, |d, p| d.child(progress(p, 10.))),
        )
        .child(
            v_flex()
                .gap(px(2.))
                .child(medium(text(13., t.fg, title)).truncate())
                .child(text(12., t.fg2, sub).truncate()),
        )
}

/// Section title with an optional "see all" on the right.
pub fn section_head(title: impl Into<SharedString>, more: bool, t: T) -> Div {
    h_flex()
        .px(px(PAD))
        .items_center()
        .justify_between()
        .child(semibold(text(17., t.fg, title)))
        .when(more, |d| {
            d.child(
                h_flex()
                    .gap(px(2.))
                    .items_center()
                    .cursor_pointer()
                    .child(text(13., t.fg2, "查看全部"))
                    .child(icon(IconName::ChevronRight, 14., t.fg2)),
            )
        })
}

/// A row that scrolls sideways; the first card lines up with the page padding.
pub fn shelf(id: impl Into<ElementId>, cards: Vec<AnyElement>) -> Stateful<Div> {
    div().id(id).w_full().overflow_x_scroll().child(
        h_flex()
            .px(px(PAD))
            .gap(px(super::tokens::GAP))
            .items_start()
            .children(cards),
    )
}

/// Filled button in the primary (foreground) colour.
pub fn primary_button(
    id: &'static str,
    label: impl Into<SharedString>,
    ic: Option<IconName>,
    t: T,
) -> Stateful<Div> {
    h_flex()
        .id(id)
        .h(px(40.))
        .px(px(20.))
        .gap(px(8.))
        .items_center()
        .rounded(px(8.))
        .bg(t.primary)
        .cursor_pointer()
        .hover(|s| s.opacity(0.88))
        .when_some(ic, |d, i| d.child(icon(i, 16., t.on_primary)))
        .child(semibold(text(14., t.on_primary, label)))
}

/// Tinted button next to a primary one.
pub fn secondary_button(
    id: &'static str,
    label: impl Into<SharedString>,
    ic: Option<IconName>,
    bg: Hsla,
    fg: Hsla,
) -> Stateful<Div> {
    h_flex()
        .id(id)
        .h(px(40.))
        .px(px(18.))
        .gap(px(8.))
        .items_center()
        .rounded(px(8.))
        .bg(bg)
        .cursor_pointer()
        .hover(|s| s.opacity(0.85))
        .when_some(ic, |d, i| d.child(icon(i, 16., fg)))
        .child(medium(text(14., fg, label)))
}

/// Round outlined icon button (favourite, played, more).
pub fn round_button(id: impl Into<ElementId>, ic: IconName, t: T) -> Stateful<Div> {
    div()
        .id(id)
        .size(px(40.))
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .border_1()
        .border_color(t.border)
        .cursor_pointer()
        .hover(|s| s.bg(t.hover))
        .child(icon(ic, 17., t.fg))
}

/// Square ghost icon button for toolbars.
pub fn ghost_button(id: impl Into<ElementId>, ic: IconName, color: Hsla, t: T) -> Stateful<Div> {
    div()
        .id(id)
        .size(px(30.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(7.))
        .cursor_pointer()
        .hover(|s| s.bg(t.hover))
        .child(icon(ic, 16., color))
}

/// Outlined tag for technical facts (4K, Dolby Vision, 5.1).
pub fn badge(s: impl Into<SharedString>, t: T) -> Div {
    div()
        .h(px(20.))
        .px(px(6.))
        .flex()
        .items_center()
        .rounded(px(4.))
        .border_1()
        .border_color(t.fg3.opacity(0.6))
        .child(medium(text(11., t.fg2, s)))
}

/// "a · b · c" with dimmed separators.
pub fn meta_line(parts: Vec<AnyElement>, t: T) -> Div {
    let mut row = h_flex().gap(px(8.)).items_center().flex_wrap();
    for (i, p) in parts.into_iter().enumerate() {
        if i > 0 {
            row = row.child(text(13., t.fg3, "·"));
        }
        row = row.child(p);
    }
    row
}

/// Segmented control: a track with the selected segment raised.
pub fn segmented(
    id: &'static str,
    items: &[&str],
    selected: usize,
    t: T,
    on_pick: impl Fn(usize, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let on_pick = std::rc::Rc::new(on_pick);
    h_flex()
        .id(id)
        .p(px(3.))
        .gap(px(2.))
        .rounded(px(9.))
        .bg(t.surface)
        .children(items.iter().enumerate().map(|(i, s)| {
            let on_pick = on_pick.clone();
            let on = i == selected;
            div()
                .id((id, i))
                .h(px(28.))
                .px(px(14.))
                .flex()
                .items_center()
                .rounded(px(6.))
                .cursor_pointer()
                .when(on, |d| d.bg(t.raised).shadow_sm())
                .when(!on, |d| d.hover(|s| s.bg(t.hover)))
                .child(
                    text(13., if on { t.fg } else { t.fg2 }, s.to_string()).font_weight(if on {
                        FontWeight::MEDIUM
                    } else {
                        FontWeight::NORMAL
                    }),
                )
                .on_click(move |_, w, cx| on_pick(i, w, cx))
        }))
}

/// Width and height of a PNG from its header, to lay out a logo at its own aspect ratio.
pub fn png_size(p: &Path) -> Option<(f32, f32)> {
    let mut head = [0u8; 24];
    std::io::Read::read_exact(&mut std::fs::File::open(p).ok()?, &mut head).ok()?;
    let w = u32::from_be_bytes(head[16..20].try_into().ok()?);
    let h = u32::from_be_bytes(head[20..24].try_into().ok()?);
    (w > 0 && h > 0).then_some((w as f32, h as f32))
}

/// Title logo fitted into a box, left-aligned; falls back to the title in large type.
pub fn logo_or_title(fx: &Fixture, it: &Item, max_w: f32, max_h: f32, color: Hsla) -> AnyElement {
    if let Some(p) = fx.img(it.logo.as_ref())
        && let Some((w, h)) = png_size(&p)
    {
        let s = (max_w / w).min(max_h / h);
        return img(p)
            .w(px(w * s))
            .h(px(h * s))
            .object_fit(ObjectFit::Contain)
            .into_any_element();
    }
    div()
        .text_size(px(40.))
        .line_height(px(48.))
        .font_weight(FontWeight::BOLD)
        .text_color(color)
        .child(it.name.clone())
        .into_any_element()
}
