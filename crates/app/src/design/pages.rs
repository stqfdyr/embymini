//! The pages: home, search, favourites, a library grid, the detail page (film or series) and settings.

use gpui_kit::assets::IconName;
use gpui_kit::component::{h_flex, switch::Switch, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::fixture::Item;
use super::tokens::{BAR_H, PAD, POSTER_W, T};
use super::ui::*;
use super::{Gallery, Page};

/// Columns and card width that fill the content width exactly.
fn grid_columns(window: &Window, sidebar: f32, min_w: f32, gap: f32) -> (usize, f32) {
    let content = f32::from(window.viewport_size().width) - sidebar - 2. * PAD - 2.;
    let cols = ((content + gap) / (min_w + gap)).floor().max(2.);
    (
        cols as usize,
        ((content - gap * (cols - 1.)) / cols).floor(),
    )
}

/// Title block at the top of list pages.
fn page_head(title: impl Into<SharedString>, sub: impl Into<SharedString>, t: T) -> Div {
    v_flex()
        .gap(px(4.))
        .child(
            div()
                .text_size(px(28.))
                .line_height(px(34.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(t.fg)
                .child(title.into()),
        )
        .child(text(13., t.fg2, sub))
}

impl Gallery {
    fn poster_grid(&self, items: &[Item], window: &Window, cx: &mut Context<Self>) -> Div {
        let t = self.t();
        let (cols, w) = grid_columns(window, self.sidebar_w(window), POSTER_W, 20.);
        let mut grid = v_flex().px(px(PAD)).gap(px(28.));
        for (r, chunk) in items.chunks(cols).enumerate() {
            grid = grid.child(h_flex().gap(px(20.)).items_start().children(
                chunk.iter().enumerate().map(|(i, it)| {
                    let it2 = it.clone();
                    poster_card(("grid", r * cols + i), it, &self.fx, w, t)
                        .on_click(cx.listener(move |this, _, _, cx| this.open(&it2, cx)))
                }),
            ));
        }
        grid
    }

    fn poster_shelf(
        &self,
        name: impl Into<SharedString>,
        items: &[Item],
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let t = self.t();
        let name: SharedString = name.into();
        let cards = items
            .iter()
            .enumerate()
            .map(|(i, it)| {
                let it2 = it.clone();
                poster_card(
                    ElementId::named_usize(name.clone(), i),
                    it,
                    &self.fx,
                    POSTER_W,
                    t,
                )
                .on_click(cx.listener(move |this, _, _, cx| this.open(&it2, cx)))
                .into_any_element()
            })
            .collect();
        shelf(ElementId::Name(name), cards)
    }

    fn hero(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.t();
        let Some(it) = self.fx.hero.get(self.hero % self.fx.hero.len().max(1)) else {
            return div().into_any_element();
        };
        let white = hsla(0., 0., 1., 1.);
        let mut meta: Vec<AnyElement> = Vec::new();
        if let Some(y) = it.production_year {
            meta.push(text(13., white.opacity(0.78), y.to_string()).into_any_element());
        }
        if !it.genres.is_empty() {
            meta.push(
                text(
                    13.,
                    white.opacity(0.78),
                    it.genres
                        .iter()
                        .take(3)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(" / "),
                )
                .into_any_element(),
            );
        }
        if let Some(r) = it.community_rating {
            meta.push(
                h_flex()
                    .gap(px(4.))
                    .items_center()
                    .child(icon(IconName::Star, 12., white.opacity(0.78)))
                    .child(text(13., white.opacity(0.78), format!("{r:.1}")))
                    .into_any_element(),
            );
        }
        let n = self.fx.hero.len();
        let cur = self.hero % n.max(1);
        let it2 = it.clone();
        div()
            .px(px(PAD))
            .pt(px(BAR_H + 4.))
            .child(
                div()
                    .relative()
                    .w_full()
                    .h(px(440.))
                    .rounded(px(16.))
                    .overflow_hidden()
                    .bg(t.surface)
                    .when_some(self.fx.img(it.backdrop.as_ref()), |d, p| {
                        d.child(
                            img(p)
                                .absolute()
                                .inset_0()
                                .size_full()
                                .rounded(px(16.))
                                .object_fit(ObjectFit::Cover),
                        )
                    })
                    .child(
                        div()
                            .absolute()
                            .inset_0()
                            .rounded(px(16.))
                            .bg(linear_gradient(
                                90.,
                                linear_color_stop(hsla(0., 0., 0., 0.78), 0.),
                                linear_color_stop(hsla(0., 0., 0., 0.), 0.62),
                            )),
                    )
                    .child(
                        div()
                            .absolute()
                            .inset_0()
                            .rounded(px(16.))
                            .bg(linear_gradient(
                                90.,
                                linear_color_stop(hsla(0., 0., 0., 0.45), 0.),
                                linear_color_stop(hsla(0., 0., 0., 0.), 0.9),
                            )),
                    )
                    .child(
                        div()
                            .absolute()
                            .inset_0()
                            .rounded(px(16.))
                            .bg(linear_gradient(
                                0.,
                                linear_color_stop(hsla(0., 0., 0., 0.55), 0.),
                                linear_color_stop(hsla(0., 0., 0., 0.), 0.5),
                            )),
                    )
                    .child(
                        div()
                            .absolute()
                            .inset_0()
                            .rounded(px(16.))
                            .border_1()
                            .border_color(t.edge),
                    )
                    .child(
                        v_flex()
                            .absolute()
                            .left(px(48.))
                            .bottom(px(44.))
                            .w(px(520.))
                            .gap(px(14.))
                            .child(logo_or_title(&self.fx, it, 340., 96., white))
                            .child(meta_line(
                                meta,
                                T {
                                    fg3: white.opacity(0.5),
                                    ..t
                                },
                            ))
                            .when_some(it.overview.clone(), |d, o| {
                                d.child(
                                    div()
                                        .text_size(px(14.))
                                        .line_height(px(22.))
                                        .text_color(white.opacity(0.82))
                                        .line_clamp(3)
                                        .child(o),
                                )
                            })
                            .child(
                                h_flex()
                                    .pt(px(6.))
                                    .gap(px(10.))
                                    .child(primary_button(
                                        "hero-play",
                                        "播放",
                                        Some(IconName::Play),
                                        T {
                                            primary: white,
                                            on_primary: hsla(0., 0., 0.04, 1.),
                                            ..t
                                        },
                                    ))
                                    .child(
                                        secondary_button(
                                            "hero-more",
                                            "详情",
                                            Some(IconName::Info),
                                            white.opacity(0.16),
                                            white,
                                        )
                                        .on_click(
                                            cx.listener(move |this, _, _, cx| this.open(&it2, cx)),
                                        ),
                                    ),
                            ),
                    )
                    .child(
                        h_flex()
                            .absolute()
                            .right(px(32.))
                            .bottom(px(30.))
                            .gap(px(6.))
                            .items_center()
                            .children((0..n).map(|i| {
                                div()
                                    .id(("dot", i))
                                    .h(px(4.))
                                    .w(px(if i == cur { 22. } else { 8. }))
                                    .rounded_full()
                                    .cursor_pointer()
                                    .bg(white.opacity(if i == cur { 0.95 } else { 0.35 }))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.hero = i;
                                        cx.notify();
                                    }))
                            })),
                    ),
            )
            .into_any_element()
    }

    pub(super) fn home(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.t();
        let fx = self.fx.clone();
        let libs: Vec<AnyElement> = fx
            .views
            .iter()
            .enumerate()
            .map(|(i, v)| {
                v_flex()
                    .id(("my", i))
                    .w(px(232.))
                    .gap(px(9.))
                    .flex_shrink_0()
                    .cursor_pointer()
                    .group("card")
                    .child(
                        art(fx.img(v.poster.as_ref()), 232., 131., 8., t).child(
                            div()
                                .absolute()
                                .inset_0()
                                .rounded(px(8.))
                                .group_hover("card", |s| s.bg(hsla(0., 0., 0., 0.2))),
                        ),
                    )
                    .child(medium(text(13., t.fg, v.name.clone())))
                    .on_click(cx.listener(move |this, _, _, cx| this.go(Page::Library(i), cx)))
                    .into_any_element()
            })
            .collect();
        let resume: Vec<AnyElement> = fx
            .resume
            .iter()
            .enumerate()
            .map(|(i, it)| {
                let it2 = it.clone();
                wide_card(("resume", i), it, &fx, 300., t)
                    .on_click(cx.listener(move |this, _, _, cx| this.open(&it2, cx)))
                    .into_any_element()
            })
            .collect();
        let mut page = v_flex()
            .pb(px(56.))
            .gap(px(40.))
            .child(self.hero(cx))
            .child(
                v_flex()
                    .gap(px(14.))
                    .child(section_head("继续观看", false, t))
                    .child(shelf("resume", resume)),
            )
            .child(
                v_flex()
                    .gap(px(14.))
                    .child(section_head("我的媒体", false, t))
                    .child(shelf("my", libs)),
            );
        for (i, (name, items)) in fx.latest_rows().into_iter().enumerate() {
            page = page.child(
                v_flex()
                    .gap(px(14.))
                    .child(section_head(format!("最新 · {name}"), true, t))
                    .child(self.poster_shelf(format!("latest-{i}"), items, cx)),
            );
        }
        page
    }

    pub(super) fn library(
        &mut self,
        i: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let t = self.t();
        let fx = self.fx.clone();
        let name = fx.views.get(i).map(|v| v.name.clone()).unwrap_or_default();
        let total = if name == fx.library.name {
            fx.library.total
        } else {
            fx.library.items.len() as u32
        };
        let unit = match fx.views.get(i).and_then(|v| v.collection_type.as_deref()) {
            Some("tvshows") => "部剧集",
            _ => "部影片",
        };
        let filter = self.filter;
        v_flex()
            .pt(px(BAR_H + 20.))
            .pb(px(56.))
            .gap(px(24.))
            .child(
                h_flex()
                    .px(px(PAD))
                    .items_end()
                    .justify_between()
                    .child(page_head(
                        name,
                        format!("{} {unit}", group_digits(total)),
                        t,
                    ))
                    .child(
                        h_flex()
                            .gap(px(8.))
                            .items_center()
                            .child(segmented(
                                "lib-filter",
                                &["全部", "未看", "收藏"],
                                filter,
                                t,
                                {
                                    let e = cx.entity();
                                    move |i, _, cx| {
                                        e.update(cx, |this, cx| {
                                            this.filter = i;
                                            cx.notify();
                                        })
                                    }
                                },
                            ))
                            .child(
                                h_flex()
                                    .id("sort")
                                    .h(px(34.))
                                    .px(px(12.))
                                    .gap(px(8.))
                                    .items_center()
                                    .rounded(px(9.))
                                    .border_1()
                                    .border_color(t.border)
                                    .cursor_pointer()
                                    .hover(|s| s.bg(t.hover))
                                    .child(icon(IconName::ArrowDownWideNarrow, 15., t.fg2))
                                    .child(text(13., t.fg, "加入时间")),
                            )
                            .child(
                                h_flex()
                                    .h(px(34.))
                                    .p(px(3.))
                                    .gap(px(2.))
                                    .rounded(px(9.))
                                    .bg(t.surface)
                                    .child(
                                        div()
                                            .size(px(28.))
                                            .rounded(px(6.))
                                            .bg(t.raised)
                                            .shadow_sm()
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .child(icon(IconName::LayoutGrid, 15., t.fg)),
                                    )
                                    .child(
                                        div()
                                            .size(px(28.))
                                            .rounded(px(6.))
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .child(icon(IconName::List, 15., t.fg2)),
                                    ),
                            ),
                    ),
            )
            .child(self.poster_grid(&fx.library.items, window, cx))
    }

    pub(super) fn favorites(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let t = self.t();
        let fx = self.fx.clone();
        let items: Vec<Item> = fx
            .latest_rows()
            .iter()
            .flat_map(|(_, v)| v.iter().take(4).cloned())
            .collect();
        v_flex()
            .pt(px(BAR_H + 20.))
            .pb(px(56.))
            .gap(px(24.))
            .child(
                h_flex()
                    .px(px(PAD))
                    .items_end()
                    .justify_between()
                    .child(page_head("收藏", format!("{} 个条目", items.len()), t))
                    .child(segmented(
                        "fav-filter",
                        &["全部", "电影", "剧集", "人物"],
                        0,
                        t,
                        |_, _, _| {},
                    )),
            )
            .child(self.poster_grid(&items, window, cx))
    }

    pub(super) fn search(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let t = self.t();
        let fx = self.fx.clone();
        let chips = ["繁花", "诺兰", "漫威", "宫崎骏", "2026 新剧", "纪录片"];
        v_flex()
            .pt(px(BAR_H + 28.))
            .pb(px(56.))
            .gap(px(32.))
            .child(
                v_flex()
                    .px(px(PAD))
                    .gap(px(16.))
                    .child(
                        h_flex()
                            .h(px(52.))
                            .px(px(18.))
                            .gap(px(12.))
                            .items_center()
                            .rounded(px(12.))
                            .border_1()
                            .border_color(t.border)
                            .bg(t.surface.opacity(0.5))
                            .child(icon(IconName::Search, 20., t.fg2))
                            .child(text(17., t.fg, "繁"))
                            .child(div().w(px(1.5)).h(px(22.)).bg(t.fg).ml(px(-10.)))
                            .child(div().flex_1())
                            .child(text(12., t.fg3, "Esc 清除")),
                    )
                    .child(
                        h_flex()
                            .gap(px(8.))
                            .items_center()
                            .child(text(12.5, t.fg3, "最近搜索").mr(px(4.)))
                            .children(chips.iter().enumerate().map(|(i, c)| {
                                div()
                                    .id(("chip", i))
                                    .h(px(28.))
                                    .px(px(12.))
                                    .flex()
                                    .items_center()
                                    .rounded_full()
                                    .border_1()
                                    .border_color(t.border)
                                    .cursor_pointer()
                                    .hover(|s| s.bg(t.hover))
                                    .child(text(12.5, t.fg2, c.to_string()))
                            })),
                    ),
            )
            .child(
                v_flex()
                    .gap(px(14.))
                    .child(section_head("电影", false, t))
                    .child(self.poster_shelf(
                        "s-movies",
                        &fx.library.items[..fx.library.items.len().min(12)],
                        cx,
                    )),
            )
            .child(
                v_flex()
                    .gap(px(14.))
                    .child(section_head("剧集", false, t))
                    .child(self.poster_grid(
                        fx.latest_rows().get(1).map(|r| r.1).unwrap_or(&[]),
                        window,
                        cx,
                    )),
            )
    }

    pub(super) fn detail(
        &mut self,
        it: &Item,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let t = self.t();
        let fx = self.fx.clone();
        let backdrop = fx
            .img(it.backdrop.as_ref())
            .or_else(|| fx.img(it.thumb.as_ref()));
        // facts: year, rating, age rating, runtime or seasons, genres
        let mut meta: Vec<AnyElement> = Vec::new();
        if let Some(y) = it.production_year {
            meta.push(text(13., t.fg2, y.to_string()).into_any_element());
        }
        if let Some(r) = it.community_rating {
            meta.push(
                h_flex()
                    .gap(px(4.))
                    .items_center()
                    .child(icon(IconName::Star, 12., t.fg2))
                    .child(text(13., t.fg2, format!("{r:.1}")))
                    .into_any_element(),
            );
        }
        if it.is_series() {
            let n = if it.seasons.is_empty() {
                it.child_count.unwrap_or(0) as usize
            } else {
                it.seasons.len()
            };
            if n > 0 {
                meta.push(text(13., t.fg2, format!("{n} 季")).into_any_element());
            }
        } else if let Some(m) = it.minutes() {
            meta.push(text(13., t.fg2, minutes_label(m)).into_any_element());
        }
        if !it.genres.is_empty() {
            meta.push(
                text(
                    13.,
                    t.fg2,
                    it.genres
                        .iter()
                        .take(3)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(" / "),
                )
                .into_any_element(),
            );
        }
        let mut badges: Vec<Div> = Vec::new();
        if let Some(r) = &it.official_rating {
            badges.push(badge(r.clone(), t));
        }
        if let Some(v) = &it.video {
            if v.width.unwrap_or(0) >= 3800 {
                badges.push(badge("4K", t));
            }
            if let Some(r) = v.video_range.as_deref().filter(|r| *r != "SDR") {
                badges.push(badge(r.to_string(), t));
            }
        }
        if it
            .audio
            .iter()
            .any(|a| a.contains("5.1") || a.contains("7.1"))
        {
            badges.push(badge(
                if it.audio.iter().any(|a| a.contains("7.1")) {
                    "7.1"
                } else {
                    "5.1"
                },
                t,
            ));
        }
        let play_label = if it.is_series() && !it.episodes.is_empty() {
            "播放 第 1 集"
        } else {
            "播放"
        };
        let hero_h = 600.;
        let mut page = v_flex().pb(px(56.)).child(
            div()
                .relative()
                .w_full()
                .h(px(hero_h))
                .when_some(backdrop, |d, p| {
                    d.child(
                        img(p)
                            .absolute()
                            .top_0()
                            .left_0()
                            .w_full()
                            .h(px(hero_h))
                            .object_fit(ObjectFit::Cover),
                    )
                })
                .child(div().absolute().inset_0().bg(linear_gradient(
                    90.,
                    linear_color_stop(t.bg.opacity(0.92), 0.),
                    linear_color_stop(t.bg.opacity(0.), 0.6),
                )))
                .child(div().absolute().inset_0().bg(linear_gradient(
                    90.,
                    linear_color_stop(t.bg.opacity(0.6), 0.),
                    linear_color_stop(t.bg.opacity(0.), 0.9),
                )))
                .child(div().absolute().inset_0().bg(linear_gradient(
                    180.,
                    linear_color_stop(t.bg.opacity(0.), 0.38),
                    linear_color_stop(t.bg, 1.),
                )))
                .child(
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .right_0()
                        .h(px(140.))
                        .bg(linear_gradient(
                            180.,
                            linear_color_stop(t.bg.opacity(0.55), 0.),
                            linear_color_stop(t.bg.opacity(0.), 1.),
                        )),
                )
                .child(
                    v_flex()
                        .absolute()
                        .left(px(PAD))
                        .bottom(px(28.))
                        .w(px(600.))
                        .gap(px(14.))
                        .child(logo_or_title(&fx, it, 380., 110., t.fg))
                        .child(meta_line(meta, t))
                        .when(!badges.is_empty(), |d| {
                            d.child(h_flex().gap(px(6.)).children(badges))
                        })
                        .when_some(it.overview.clone(), |d, o| {
                            d.child(
                                div()
                                    .text_size(px(14.))
                                    .line_height(px(22.))
                                    .text_color(t.fg2)
                                    .line_clamp(3)
                                    .child(o),
                            )
                        })
                        .child(
                            h_flex()
                                .pt(px(6.))
                                .gap(px(10.))
                                .items_center()
                                .child(primary_button("play", play_label, Some(IconName::Play), t))
                                .child(round_button("fav", IconName::Heart, t))
                                .child(round_button("played", IconName::Check, t))
                                .child(round_button("more", IconName::Ellipsis, t)),
                        ),
                ),
        );
        let mut sections = v_flex().pt(px(20.)).gap(px(40.));
        if !it.episodes.is_empty() {
            let names: Vec<String> = it.seasons.iter().map(|s| s.name.clone()).collect();
            let names: Vec<&str> = names.iter().map(String::as_str).collect();
            let e = cx.entity();
            let mut list = v_flex().px(px(PAD - 12.)).gap(px(2.));
            for (i, ep) in it.episodes.iter().enumerate() {
                list = list.child(episode_row(i, ep, &fx, t));
            }
            sections = sections.child(
                v_flex()
                    .gap(px(16.))
                    .child(
                        h_flex()
                            .px(px(PAD))
                            .items_center()
                            .justify_between()
                            .child(segmented(
                                "seasons",
                                &names,
                                self.season,
                                t,
                                move |i, _, cx| {
                                    e.update(cx, |this, cx| {
                                        this.season = i;
                                        cx.notify();
                                    })
                                },
                            ))
                            .child(text(13., t.fg2, format!("{} 集", it.episodes.len()))),
                    )
                    .child(list),
            );
        }
        let cast: Vec<AnyElement> = it
            .people
            .iter()
            .filter(|p| p.kind.as_deref() != Some("Director") || p.img.is_some())
            .enumerate()
            .map(|(i, p)| {
                let role = match p.kind.as_deref() {
                    Some("Director") => "导演".to_string(),
                    _ => p.role.clone().unwrap_or_default(),
                };
                let initial: String = p.name.chars().next().map(String::from).unwrap_or_default();
                v_flex()
                    .id(("cast", i))
                    .w(px(100.))
                    .gap(px(8.))
                    .items_center()
                    .flex_shrink_0()
                    .cursor_pointer()
                    .group("card")
                    .child(
                        div()
                            .relative()
                            .size(px(88.))
                            .rounded_full()
                            .bg(t.surface)
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(semibold(text(22., t.fg3, initial)))
                            .when_some(fx.img(p.img.as_ref()), |d, path| {
                                d.child(
                                    img(path)
                                        .absolute()
                                        .inset_0()
                                        .size_full()
                                        .rounded_full()
                                        .object_fit(ObjectFit::Cover),
                                )
                            })
                            .child(
                                div()
                                    .absolute()
                                    .inset_0()
                                    .rounded_full()
                                    .border_1()
                                    .border_color(t.edge)
                                    .group_hover("card", |s| s.bg(hsla(0., 0., 0., 0.18))),
                            ),
                    )
                    .child(
                        v_flex()
                            .w_full()
                            .items_center()
                            .gap(px(1.))
                            .child(medium(text(13., t.fg, p.name.clone())).truncate())
                            .child(text(12., t.fg2, role).truncate()),
                    )
                    .into_any_element()
            })
            .collect();
        if !cast.is_empty() {
            sections = sections.child(
                v_flex()
                    .gap(px(16.))
                    .child(section_head("演职人员", false, t))
                    .child(shelf("cast", cast)),
            );
        }
        if let Some(v) = &it.video {
            let res = match (v.width, v.height) {
                (Some(w), Some(h)) => format!("{w} × {h}"),
                _ => String::new(),
            };
            let rows = [
                (
                    "视频",
                    format!(
                        "{} · {} · {}",
                        res,
                        v.codec.clone().unwrap_or_default().to_uppercase(),
                        v.video_range.clone().unwrap_or_default()
                    ),
                ),
                ("音频", it.audio.first().cloned().unwrap_or_default()),
                ("时长", it.minutes().map(minutes_label).unwrap_or_default()),
            ];
            sections = sections.child(
                v_flex()
                    .gap(px(16.))
                    .child(section_head("媒体信息", false, t))
                    .child(
                        h_flex()
                            .px(px(PAD))
                            .gap(px(48.))
                            .children(rows.into_iter().map(|(k, v)| {
                                v_flex()
                                    .gap(px(4.))
                                    .child(text(12., t.fg3, k))
                                    .child(text(13.5, t.fg, v))
                            })),
                    ),
            );
        }
        if !it.similar.is_empty() {
            sections = sections.child(
                v_flex()
                    .gap(px(14.))
                    .child(section_head("相似推荐", false, t))
                    .child(self.poster_shelf("similar", &it.similar, cx)),
            );
        } else if let Some((_, items)) = fx.latest_rows().first() {
            sections = sections.child(
                v_flex()
                    .gap(px(14.))
                    .child(section_head("更多推荐", false, t))
                    .child(self.poster_shelf("more", items, cx)),
            );
        }
        page = page.child(sections);
        page
    }

    pub(super) fn settings(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.t();
        let tabs = [
            (IconName::Server, "服务器"),
            (IconName::MonitorPlay, "播放"),
            (IconName::Captions, "字幕与弹幕"),
            (IconName::HardDrive, "缓存"),
            (IconName::Palette, "外观"),
            (IconName::Info, "关于"),
        ];
        let cur = self.settings_tab;
        let nav =
            v_flex()
                .w(px(184.))
                .flex_shrink_0()
                .gap(px(2.))
                .children(tabs.iter().enumerate().map(|(i, (ic, label))| {
                    let on = i == cur;
                    h_flex()
                        .id(("set-tab", i))
                        .h(px(34.))
                        .px(px(10.))
                        .gap(px(10.))
                        .items_center()
                        .rounded(px(8.))
                        .cursor_pointer()
                        .when(on, |d| d.bg(t.accent))
                        .when(!on, |d| d.hover(|s| s.bg(t.hover)))
                        .child(icon(*ic, 16., if on { t.fg } else { t.fg2 }))
                        .child(
                            text(13.5, if on { t.fg } else { t.fg2 }, label.to_string())
                                .font_weight(if on {
                                    FontWeight::MEDIUM
                                } else {
                                    FontWeight::NORMAL
                                }),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.settings_tab = i;
                            cx.notify();
                        }))
                }));
        let dark = self.dark;
        let e = cx.entity();
        let content = v_flex()
            .flex_1()
            .min_w_0()
            .max_w(px(760.))
            .gap(px(36.))
            .child(settings_group(
                "服务器",
                "添加多个服务器，随时在左下角切换",
                vec![
                    server_row(0, "家庭影院", "https://media.example.com", "admin", true, t),
                    server_row(
                        1,
                        "朋友的服务器",
                        "https://emby.example.net:8920",
                        "guest",
                        false,
                        t,
                    ),
                    h_flex()
                        .id("add-server")
                        .h(px(48.))
                        .px(px(16.))
                        .gap(px(8.))
                        .items_center()
                        .cursor_pointer()
                        .hover(|s| s.bg(t.hover))
                        .child(icon(IconName::Plus, 15., t.fg2))
                        .child(text(13.5, t.fg2, "添加服务器"))
                        .into_any_element(),
                ],
                t,
            ))
            .child(settings_group(
                "播放",
                "mpv 播放器的行为",
                vec![
                    setting_row(
                        "播放器",
                        "留空使用自带的 mpv",
                        h_flex()
                            .gap(px(8.))
                            .child(field("自带 mpv", 220., t))
                            .child(outline_button("browse", "浏览…", t))
                            .into_any_element(),
                        t,
                    ),
                    setting_row(
                        "预加载下一集",
                        "当前集播放到这个进度时，开始缓冲下一集",
                        select_like("90%", t),
                        t,
                    ),
                    setting_row(
                        "整片缓存",
                        "边播边把整部影片存到本地，来回拖动不用再等",
                        Switch::new("whole").checked(true).into_any_element(),
                        t,
                    ),
                    setting_row(
                        "记住音量",
                        "下次播放沿用上次的音量",
                        Switch::new("volume").checked(true).into_any_element(),
                        t,
                    ),
                ],
                t,
            ))
            .child(settings_group(
                "外观",
                "跟随系统，或固定浅色 / 深色",
                vec![setting_row(
                    "主题",
                    "",
                    segmented(
                        "theme-pick",
                        &["跟随系统", "浅色", "深色"],
                        if dark { 2 } else { 1 },
                        t,
                        move |i, window, cx| {
                            e.update(cx, |this, cx| this.set_dark(i != 1, window, cx))
                        },
                    )
                    .into_any_element(),
                    t,
                )],
                t,
            ));
        v_flex()
            .pt(px(BAR_H + 20.))
            .pb(px(56.))
            .px(px(PAD))
            .gap(px(28.))
            .child(page_head("设置", "服务器、播放、缓存与外观", t))
            .child(
                h_flex()
                    .gap(px(48.))
                    .items_start()
                    .child(nav)
                    .child(content),
            )
    }
}

fn group_digits(n: u32) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn episode_row(i: usize, ep: &Item, fx: &super::fixture::Fixture, t: T) -> Stateful<Div> {
    let thumb = fx
        .img(ep.poster.as_ref())
        .or_else(|| fx.img(ep.thumb.as_ref()));
    let n = ep.index_number.unwrap_or(i as u32 + 1);
    let played = if i == 0 {
        Some(100.)
    } else if i == 1 {
        Some(38.)
    } else {
        None
    };
    h_flex()
        .id(("ep", i))
        .p(px(12.))
        .gap(px(20.))
        .items_start()
        .rounded(px(12.))
        .cursor_pointer()
        .group("card")
        .hover(|s| s.bg(t.hover))
        .child(
            art(thumb, 224., 126., 8., t)
                .child(
                    div()
                        .absolute()
                        .inset_0()
                        .rounded(px(8.))
                        .group_hover("card", |s| s.bg(hsla(0., 0., 0., 0.22))),
                )
                .when_some(played, |d, p| d.child(progress(p, 8.))),
        )
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .pt(px(4.))
                .gap(px(6.))
                .child(
                    h_flex()
                        .gap(px(16.))
                        .justify_between()
                        .items_center()
                        .child(medium(text(14.5, t.fg, format!("{n}. {}", ep.name))).truncate())
                        .child(
                            h_flex()
                                .gap(px(8.))
                                .items_center()
                                .flex_shrink_0()
                                .when(played == Some(100.), |d| {
                                    d.child(icon(IconName::CircleCheck, 14., t.fg2))
                                })
                                .child(text(
                                    12.5,
                                    t.fg2,
                                    ep.minutes().map(minutes_label).unwrap_or_default(),
                                )),
                        ),
                )
                .when_some(ep.overview.clone(), |d, o| {
                    d.child(
                        div()
                            .text_size(px(13.))
                            .line_height(px(20.))
                            .text_color(t.fg2)
                            .line_clamp(2)
                            .child(o),
                    )
                }),
        )
}

/// A titled card of rows separated by hairlines.
fn settings_group(title: &str, desc: &str, rows: Vec<AnyElement>, t: T) -> Div {
    let mut card = v_flex()
        .rounded(px(12.))
        .border_1()
        .border_color(t.border)
        .overflow_hidden();
    for (i, r) in rows.into_iter().enumerate() {
        card = card.child(
            div()
                .when(i > 0, |d| d.border_t_1().border_color(t.border))
                .child(r),
        );
    }
    v_flex()
        .gap(px(12.))
        .child(
            v_flex()
                .gap(px(2.))
                .child(semibold(text(15., t.fg, title.to_string())))
                .child(text(12.5, t.fg2, desc.to_string())),
        )
        .child(card)
}

fn setting_row(label: &str, desc: &str, control: AnyElement, t: T) -> AnyElement {
    h_flex()
        .min_h(px(64.))
        .px(px(16.))
        .py(px(12.))
        .gap(px(24.))
        .items_center()
        .justify_between()
        .child(
            v_flex()
                .gap(px(2.))
                .min_w_0()
                .child(medium(text(13.5, t.fg, label.to_string())))
                .when(!desc.is_empty(), |d| {
                    d.child(text(12.5, t.fg2, desc.to_string()))
                }),
        )
        .child(control)
        .into_any_element()
}

fn server_row(i: usize, name: &str, url: &str, user: &str, current: bool, t: T) -> AnyElement {
    let initial: String = name.chars().next().map(String::from).unwrap_or_default();
    h_flex()
        .id(("srv", i))
        .h(px(64.))
        .px(px(16.))
        .gap(px(12.))
        .items_center()
        .child(
            div()
                .size(px(34.))
                .rounded_full()
                .bg(t.surface)
                .flex()
                .items_center()
                .justify_center()
                .child(semibold(text(13., t.fg, initial))),
        )
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap(px(2.))
                .child(
                    h_flex()
                        .gap(px(8.))
                        .items_center()
                        .child(medium(text(13.5, t.fg, name.to_string())))
                        .when(current, |d| {
                            d.child(
                                div()
                                    .h(px(18.))
                                    .px(px(6.))
                                    .flex()
                                    .items_center()
                                    .rounded(px(4.))
                                    .bg(t.accent)
                                    .child(medium(text(11., t.fg, "当前"))),
                            )
                        }),
                )
                .child(text(12.5, t.fg2, format!("{user} · {url}")).truncate()),
        )
        .child(ghost_button(("srv-more", i), IconName::Ellipsis, t.fg2, t))
        .into_any_element()
}

fn field(placeholder: &str, w: f32, t: T) -> Div {
    h_flex()
        .h(px(34.))
        .w(px(w))
        .px(px(12.))
        .items_center()
        .rounded(px(8.))
        .border_1()
        .border_color(t.border)
        .child(text(13., t.fg3, placeholder.to_string()))
}

fn outline_button(id: &'static str, label: &str, t: T) -> Stateful<Div> {
    h_flex()
        .id(id)
        .h(px(34.))
        .px(px(14.))
        .items_center()
        .rounded(px(8.))
        .border_1()
        .border_color(t.border)
        .cursor_pointer()
        .hover(|s| s.bg(t.hover))
        .child(medium(text(13., t.fg, label.to_string())))
}

fn select_like(value: &str, t: T) -> AnyElement {
    h_flex()
        .id("select")
        .h(px(34.))
        .w(px(120.))
        .px(px(12.))
        .items_center()
        .justify_between()
        .rounded(px(8.))
        .border_1()
        .border_color(t.border)
        .cursor_pointer()
        .hover(|s| s.bg(t.hover))
        .child(text(13., t.fg, value.to_string()))
        .child(icon(IconName::ChevronDown, 14., t.fg2))
        .into_any_element()
}
