//! Building blocks shared by the design pages: artwork, cards with badges, rows with paging arrows, section
//! headers, buttons, segmented controls, menus.

use std::path::{Path, PathBuf};
use std::rc::Rc;

use gpui_kit::assets::IconName;
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::fixture::{Fixture, Item};
use super::tokens::{GAP, PAD, T};

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

pub fn light(a: f32) -> Hsla {
    hsla(0., 0., 1., a)
}

pub fn shade(a: f32) -> Hsla {
    hsla(0., 0., 0., a)
}

/// Artwork box: a placeholder surface (with a clapperboard when there is no image) and a hairline edge on top.
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
        None => d.flex().items_center().justify_center().child(icon(
            IconName::Clapperboard,
            (h / 4.).min(32.),
            t.fg3,
        )),
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

/// Darkens the artwork of the hovered card a little.
pub fn hover_veil(r: f32) -> Div {
    div()
        .absolute()
        .inset_0()
        .rounded(px(r))
        .group_hover("card", |s| s.bg(shade(0.18)))
}

/// Community rating in the lower right corner of a poster.
pub fn rating_badge(r: f32) -> Div {
    div()
        .absolute()
        .right(px(6.))
        .bottom(px(6.))
        .h(px(18.))
        .px(px(5.))
        .flex()
        .items_center()
        .rounded(px(5.))
        .bg(shade(0.55))
        .child(semibold(text(11., light(0.95), format!("{r:.1}"))))
}

/// Upper right corner: unwatched episodes, or a tick when watched.
pub fn corner_badge(it: &Item) -> Option<Div> {
    let base = || {
        div()
            .absolute()
            .top(px(6.))
            .right(px(6.))
            .h(px(18.))
            .min_w(px(18.))
            .flex()
            .items_center()
            .justify_center()
            .rounded_full()
            .bg(light(0.95))
            .shadow_sm()
    };
    if let Some(n) = it.unplayed.filter(|&n| n > 0) {
        return Some(
            base()
                .px(px(5.))
                .child(semibold(text(10.5, shade(0.9), n.to_string()))),
        );
    }
    it.watched
        .then(|| base().child(icon(IconName::Check, 11., shade(0.9))))
}

pub fn year(it: &Item) -> String {
    it.production_year
        .map(|y| y.to_string())
        .unwrap_or_default()
}

/// 2:3 poster with badges, title and year.
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
    v_flex()
        .id(id)
        .w(px(w))
        .gap(px(7.))
        .flex_shrink_0()
        .cursor_pointer()
        .group("card")
        .child(
            art(path, w, h, 10., t)
                .child(hover_veil(10.))
                .when_some(it.community_rating.filter(|&r| r > 0.), |d, r| {
                    d.child(rating_badge(r))
                })
                .when_some(corner_badge(it), |d, b| d.child(b))
                .when_some(it.played, |d, p| d.child(progress(p, 8.))),
        )
        .child(
            v_flex()
                .gap(px(1.))
                .child(medium(text(12.5, t.fg, it.name.clone())).truncate())
                .child(text(11.5, t.fg2, year(it)).truncate()),
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
        .bg(light(0.3))
        .child(
            div()
                .h_full()
                .w(relative(pct.clamp(0., 100.) / 100.))
                .rounded_full()
                .bg(light(0.95)),
        )
}

pub fn minutes_label(m: u64) -> String {
    if m >= 60 {
        format!("{} 小时 {} 分", m / 60, m % 60)
    } else {
        format!("{m} 分钟")
    }
}

/// h:mm:ss or mm:ss.
pub fn clock(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if h > 0 {
        format!("{h:02}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

/// 16:9 card for "continue watching": artwork with progress and played / total time, the show on the first line,
/// season, episode and its name on the second.
pub fn wide_card(id: impl Into<ElementId>, it: &Item, fx: &Fixture, w: f32, t: T) -> Stateful<Div> {
    let h = (w * 9. / 16.).round();
    let path = fx
        .img(it.thumb.as_ref())
        .or_else(|| fx.img(it.poster.as_ref()))
        .or_else(|| fx.img(it.backdrop.as_ref()));
    let (title, sub) = match &it.series_name {
        Some(s) => {
            let code = match (it.parent_index_number, it.index_number) {
                (Some(s), Some(e)) => format!("S{s:02} · E{e:02} · "),
                _ => String::new(),
            };
            (s.clone(), format!("{code}{}", it.name))
        }
        None => (it.name.clone(), year(it)),
    };
    let time = match (it.run_time_ticks, it.position) {
        (Some(total), Some(pos)) => Some(format!(
            "{} / {}",
            clock(pos / 10_000_000),
            clock(total / 10_000_000)
        )),
        _ => None,
    };
    v_flex()
        .id(id)
        .w(px(w))
        .gap(px(7.))
        .flex_shrink_0()
        .cursor_pointer()
        .group("card")
        .child(
            art(path, w, h, 10., t)
                .child(
                    div()
                        .absolute()
                        .left_0()
                        .right_0()
                        .bottom_0()
                        .h(px(44.))
                        .rounded_b(px(10.))
                        .bg(linear_gradient(
                            180.,
                            linear_color_stop(shade(0.), 0.),
                            linear_color_stop(shade(0.55), 1.),
                        )),
                )
                .child(hover_veil(10.))
                .when_some(time, |d, s| {
                    d.child(
                        h_flex()
                            .absolute()
                            .right(px(10.))
                            .bottom(px(16.))
                            .gap(px(4.))
                            .items_center()
                            .child(icon(IconName::Play, 10., light(0.95)))
                            .child(medium(text(11., light(0.95), s))),
                    )
                })
                .when_some(it.played, |d, p| d.child(progress(p, 8.))),
        )
        .child(
            v_flex()
                .gap(px(1.))
                .child(medium(text(12.5, t.fg, title)).truncate())
                .child(text(11.5, t.fg2, sub).truncate()),
        )
}

/// Section title with the item count and a chevron on the right (opens the full list).
pub fn section_head(title: impl Into<SharedString>, count: Option<String>, t: T) -> Div {
    h_flex()
        .px(px(PAD))
        .items_center()
        .justify_between()
        .child(semibold(text(16., t.fg, title)))
        .when_some(count, |d, n| {
            d.child(
                h_flex()
                    .gap(px(2.))
                    .items_center()
                    .cursor_pointer()
                    .child(text(12.5, t.fg2, n))
                    .child(icon(IconName::ChevronRight, 13., t.fg2)),
            )
        })
}

/// Round paging arrow shown at a row's edge while the row is hovered.
fn row_arrow(group: SharedString, handle: ScrollHandle, right: bool, top: f32) -> Stateful<Div> {
    div()
        .id(ElementId::named_usize(
            format!("{group}-arrow"),
            usize::from(right),
        ))
        .absolute()
        .top(px(top - 16.))
        .when(right, |d| d.right(px(8.)))
        .when(!right, |d| d.left(px(8.)))
        .size(px(32.))
        .rounded_full()
        .flex()
        .items_center()
        .justify_center()
        .bg(hsla(0., 0., 0.98, 0.92))
        .shadow_md()
        .cursor_pointer()
        .opacity(0.)
        .group_hover(group, |s| s.opacity(1.))
        .child(icon(
            if right {
                IconName::ChevronRight
            } else {
                IconName::ChevronLeft
            },
            16.,
            shade(0.85),
        ))
        .on_click(move |_, window, _| {
            let page = f32::from(window.viewport_size().width) * 0.7;
            let max = f32::from(handle.max_offset().x);
            let x = -f32::from(handle.offset().x);
            let next = if right { x + page } else { x - page }.clamp(0., max);
            handle.set_offset(point(px(-next), px(0.)));
            window.refresh();
        })
}

/// A row that scrolls sideways, with paging arrows at both ends on hover; `art_h` centres the arrows on the
/// artwork.
pub fn shelf(name: SharedString, handle: &ScrollHandle, cards: Vec<AnyElement>, art_h: f32) -> Div {
    div()
        .relative()
        .w_full()
        .group(name.clone())
        .child(
            div()
                .id(name.clone())
                .w_full()
                .overflow_x_scroll()
                .track_scroll(handle)
                .child(
                    h_flex()
                        .px(px(PAD))
                        .gap(px(GAP))
                        .items_start()
                        .children(cards),
                ),
        )
        .child(row_arrow(name.clone(), handle.clone(), false, art_h / 2.))
        .child(row_arrow(name, handle.clone(), true, art_h / 2.))
}

/// Round translucent icon button (toolbar, detail page actions).
pub fn round_button(
    id: impl Into<ElementId>,
    ic: IconName,
    size: f32,
    bg: Hsla,
    fg: Hsla,
    border: Hsla,
) -> Stateful<Div> {
    div()
        .id(id)
        .size(px(size))
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .bg(bg)
        .border_1()
        .border_color(border)
        .cursor_pointer()
        .hover(|s| s.opacity(0.8))
        .child(icon(ic, size * 0.45, fg))
}

/// Square ghost icon button for toolbars.
pub fn ghost_button(id: impl Into<ElementId>, ic: IconName, color: Hsla, t: T) -> Stateful<Div> {
    div()
        .id(id)
        .size(px(30.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(8.))
        .cursor_pointer()
        .hover(|s| s.bg(t.hover))
        .child(icon(ic, 16., color))
}

/// Outlined tag for facts such as the age rating.
pub fn badge(s: impl Into<SharedString>, color: Hsla) -> Div {
    div()
        .h(px(17.))
        .px(px(5.))
        .flex()
        .items_center()
        .rounded(px(4.))
        .border_1()
        .border_color(color.opacity(0.6))
        .child(medium(text(10.5, color, s)))
}

/// Segmented control: a track with the selected segment raised.
pub fn segmented(
    id: &'static str,
    items: &[&str],
    selected: usize,
    t: T,
    on_pick: impl Fn(usize, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let on_pick = Rc::new(on_pick);
    h_flex()
        .id(id)
        .p(px(3.))
        .gap(px(2.))
        .rounded_full()
        .bg(t.surface)
        .border_1()
        .border_color(t.border)
        .children(items.iter().enumerate().map(|(i, s)| {
            let on_pick = on_pick.clone();
            let on = i == selected;
            div()
                .id((id, i))
                .h(px(26.))
                .px(px(14.))
                .flex()
                .items_center()
                .rounded_full()
                .cursor_pointer()
                .when(on, |d| d.bg(t.raised).shadow_sm())
                .when(!on, |d| d.hover(|s| s.bg(t.hover)))
                .child(
                    text(12.5, if on { t.fg } else { t.fg2 }, s.to_string()).font_weight(if on {
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

/// Title logo fitted into a box; falls back to the title in large type.
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
        .text_size(px(34.))
        .line_height(px(42.))
        .font_weight(FontWeight::BOLD)
        .text_color(color)
        .child(it.name.clone())
        .into_any_element()
}
