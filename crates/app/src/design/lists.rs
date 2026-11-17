//! Grid pages: all libraries, one library (films / genres / studios / folders), search, favourites, servers.

use gpui_kit::assets::IconName;
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::fixture::Item;
use super::tokens::{BAR_H, PAD, POSTER_W};
use super::ui::*;
use super::{Gallery, Page, SERVERS};

const GRID_GAP_X: f32 = 18.;
const GRID_GAP_Y: f32 = 22.;

impl Gallery {
    /// Columns and card width that fill the content width exactly.
    fn grid_columns(&self, window: &Window, min_w: f32) -> (usize, f32) {
        let content =
            f32::from(window.viewport_size().width) - self.sidebar_w(window) - 2. * PAD - 2.;
        let cols = ((content + GRID_GAP_X) / (min_w + GRID_GAP_X))
            .floor()
            .max(2.);
        (
            cols as usize,
            ((content - GRID_GAP_X * (cols - 1.)) / cols).floor(),
        )
    }

    fn grid(&self, cells: Vec<AnyElement>, cols: usize) -> Div {
        let mut grid = v_flex().px(px(PAD)).gap(px(GRID_GAP_Y));
        let mut cells = cells.into_iter().peekable();
        while cells.peek().is_some() {
            grid = grid.child(
                h_flex()
                    .gap(px(GRID_GAP_X))
                    .items_start()
                    .children(cells.by_ref().take(cols)),
            );
        }
        grid
    }

    pub(super) fn poster_grid(
        &self,
        items: &[Item],
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let t = self.t();
        let (cols, w) = self.grid_columns(window, POSTER_W - 10.);
        let cells = items
            .iter()
            .enumerate()
            .map(|(i, it)| {
                let (a, b) = (it.clone(), it.clone());
                poster_card(("grid", i), it, &self.fx, w, t)
                    .on_click(cx.listener(move |this, _, _, cx| this.open(&a, cx)))
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, _, window, cx| {
                            this.card_menu(&b, false, window, cx)
                        }),
                    )
                    .into_any_element()
            })
            .collect();
        self.grid(cells, cols)
    }

    pub(super) fn media(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let t = self.t();
        let fx = self.fx.clone();
        let (cols, w) = self.grid_columns(window, 220.);
        let cells = fx
            .views
            .iter()
            .enumerate()
            .map(|(i, v)| {
                v_flex()
                    .id(("lib", i))
                    .w(px(w))
                    .gap(px(7.))
                    .cursor_pointer()
                    .group("card")
                    .child(
                        art(fx.img(v.poster.as_ref()), w, (w * 9. / 16.).round(), 10., t)
                            .child(hover_veil(10.)),
                    )
                    .child(medium(text(12.5, t.fg, v.name.clone())))
                    .on_click(cx.listener(move |this, _, _, cx| this.go(Page::Library(i), cx)))
                    .into_any_element()
            })
            .collect();
        v_flex()
            .pt(px(BAR_H + 16.))
            .pb(px(40.))
            .child(self.grid(cells, cols))
    }

    pub(super) fn library(
        &mut self,
        _i: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let t = self.t();
        let fx = self.fx.clone();
        let body = match self.lib_seg {
            1 | 2 => {
                // genres or studios: name tiles over a poster from that group
                let mut names: Vec<String> = Vec::new();
                for it in fx.details.values() {
                    let pool = if self.lib_seg == 1 {
                        &it.genres
                    } else {
                        &it.studios
                    };
                    for g in pool {
                        if !names.contains(g) {
                            names.push(g.clone());
                        }
                    }
                }
                names.sort();
                let (cols, w) = self.grid_columns(window, 200.);
                let cells = names
                    .iter()
                    .enumerate()
                    .map(|(i, n)| {
                        let cover = fx
                            .details
                            .values()
                            .find(|d| {
                                if self.lib_seg == 1 {
                                    d.genres.contains(n)
                                } else {
                                    d.studios.contains(n)
                                }
                            })
                            .and_then(|d| fx.img(d.backdrop.as_ref()));
                        div()
                            .id(("group", i))
                            .relative()
                            .w(px(w))
                            .h(px((w * 9. / 16.).round()))
                            .rounded(px(10.))
                            .overflow_hidden()
                            .bg(t.surface)
                            .cursor_pointer()
                            .group("card")
                            .when_some(cover, |d, p| {
                                d.child(
                                    img(p)
                                        .absolute()
                                        .inset_0()
                                        .size_full()
                                        .rounded(px(10.))
                                        .object_fit(ObjectFit::Cover),
                                )
                            })
                            .child(
                                div()
                                    .absolute()
                                    .inset_0()
                                    .rounded(px(10.))
                                    .bg(shade(0.5))
                                    .group_hover("card", |s| s.bg(shade(0.38))),
                            )
                            .child(
                                h_flex()
                                    .absolute()
                                    .inset_0()
                                    .items_center()
                                    .justify_center()
                                    .child(semibold(text(15., light(0.95), n.clone()))),
                            )
                            .into_any_element()
                    })
                    .collect();
                self.grid(cells, cols)
            }
            3 => {
                // folders: a plain list
                let rows = [
                    "华语电影 2026",
                    "华语电影 2025",
                    "华语电影 2024",
                    "经典修复",
                    "4K HDR",
                    "合集",
                ]
                .iter()
                .enumerate()
                .map(|(i, n)| {
                    h_flex()
                        .id(("folder", i))
                        .h(px(48.))
                        .px(px(14.))
                        .gap(px(12.))
                        .items_center()
                        .rounded(px(10.))
                        .cursor_pointer()
                        .hover(|s| s.bg(t.hover))
                        .child(icon(IconName::Folder, 18., t.fg2))
                        .child(text(13.5, t.fg, n.to_string()).flex_1())
                        .child(text(12., t.fg3, format!("{} 项", 120 - i * 17)))
                        .child(icon(IconName::ChevronRight, 14., t.fg3))
                        .into_any_element()
                });
                v_flex().px(px(PAD - 14.)).gap(px(2.)).children(rows)
            }
            _ => self.poster_grid(&fx.library.items, window, cx),
        };
        v_flex().pt(px(BAR_H + 16.)).pb(px(40.)).child(body)
    }

    pub(super) fn search(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let t = self.t();
        let fx = self.fx.clone();
        let history = ["繁花", "哪吒", "乘风破浪"];
        let searched = self.searched;
        let field = h_flex()
            .id("search-field")
            .mx(px(PAD))
            .h(px(38.))
            .px(px(14.))
            .gap(px(10.))
            .items_center()
            .rounded_full()
            .bg(t.surface)
            .border_1()
            .border_color(t.border)
            .cursor_text()
            .child(icon(IconName::Search, 16., t.fg3))
            .child(if searched {
                text(13.5, t.fg, "哪吒")
            } else {
                text(13.5, t.fg3, "搜索电影、节目、单集、人物")
            })
            .on_click(cx.listener(|this, _, _, cx| {
                this.searched = !this.searched;
                cx.notify();
            }));
        let chips = h_flex()
            .px(px(PAD))
            .gap(px(8.))
            .items_center()
            .children(history.iter().enumerate().map(|(i, c)| {
                div()
                    .id(("history", i))
                    .h(px(28.))
                    .px(px(12.))
                    .flex()
                    .items_center()
                    .rounded_full()
                    .bg(t.surface)
                    .border_1()
                    .border_color(t.border)
                    .cursor_pointer()
                    .hover(|s| s.bg(t.accent))
                    .child(text(12.5, t.fg, c.to_string()))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.searched = true;
                        cx.notify();
                    }))
            }))
            .child(div().flex_1())
            .child(ghost_button("clear-history", IconName::Trash, t.fg3, t));
        let body = if searched {
            let items: Vec<Item> = fx.library.items.iter().take(14).cloned().collect();
            self.poster_grid(&items, window, cx).into_any_element()
        } else {
            v_flex()
                .pt(px(140.))
                .items_center()
                .gap(px(14.))
                .child(icon(IconName::Search, 56., t.fg3))
                .child(text(
                    13.,
                    t.fg2,
                    format!("搜索「{}」中的内容", SERVERS[self.server].0),
                ))
                .into_any_element()
        };
        v_flex()
            .pt(px(BAR_H + 16.))
            .pb(px(40.))
            .gap(px(16.))
            .child(field)
            .child(chips)
            .child(body)
    }

    pub(super) fn favorites(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let t = self.t();
        let fx = self.fx.clone();
        let shows: Vec<Item> = fx
            .details
            .values()
            .filter(|d| d.is_series())
            .take(9)
            .cloned()
            .collect();
        let films: Vec<Item> = fx
            .details
            .values()
            .filter(|d| !d.is_series())
            .take(9)
            .cloned()
            .collect();
        v_flex()
            .pt(px(BAR_H + 16.))
            .pb(px(40.))
            .gap(px(26.))
            .child(
                v_flex()
                    .gap(px(10.))
                    .child(section_head("节目", Some("更多".into()), t))
                    .child(self.poster_shelf("fav-shows", &shows, cx)),
            )
            .child(
                v_flex()
                    .gap(px(10.))
                    .child(section_head("电影", Some("更多".into()), t))
                    .child(self.poster_grid(&films, window, cx)),
            )
    }

    pub(super) fn servers(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let t = self.t();
        let (cols, w) = self.grid_columns(window, 220.);
        let stats = [
            ("12,480", "8,211", "主线路", "1 分钟前"),
            ("3,021", "1,507", "备用线路", "3 天前"),
        ];
        let cells = SERVERS
            .iter()
            .zip(stats)
            .enumerate()
            .map(|(i, ((name, user), (films, eps, line, sync)))| {
                let initial: String = name.chars().next().map(String::from).unwrap_or_default();
                v_flex()
                    .id(("srv-card", i))
                    .w(px(w))
                    .h(px(120.))
                    .p(px(14.))
                    .justify_between()
                    .rounded(px(14.))
                    .bg(linear_gradient(
                        150.,
                        linear_color_stop(t.surface, 0.),
                        linear_color_stop(t.bg, 1.),
                    ))
                    .border_1()
                    .border_color(t.border)
                    .cursor_pointer()
                    .hover(|s| s.border_color(t.fg3))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.server = i;
                        this.go(Page::Home, cx);
                    }))
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(|this, _, window, cx| {
                            this.show_menu(
                                None,
                                vec![
                                    super::MenuItem::new("编辑"),
                                    super::MenuItem::new("重新登录"),
                                    super::MenuItem::new("设为默认首页"),
                                    super::MenuItem::new("删除").gap(),
                                ],
                                window,
                                cx,
                            )
                        }),
                    )
                    .child(
                        h_flex()
                            .items_start()
                            .justify_between()
                            .child(
                                h_flex()
                                    .gap(px(6.))
                                    .items_center()
                                    .child(semibold(text(14., t.fg, name.to_string())))
                                    .child(div().size(px(6.)).rounded_full().bg(rgb(0x22c55e))),
                            )
                            .child(
                                div()
                                    .size(px(28.))
                                    .rounded(px(8.))
                                    .bg(t.fg)
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(semibold(text(12., t.bg, initial))),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap(px(14.))
                            .child(
                                h_flex()
                                    .gap(px(4.))
                                    .items_center()
                                    .child(icon(IconName::Clapperboard, 13., t.fg2))
                                    .child(text(12., t.fg2, films)),
                            )
                            .child(
                                h_flex()
                                    .gap(px(4.))
                                    .items_center()
                                    .child(icon(IconName::Tv, 13., t.fg2))
                                    .child(text(12., t.fg2, eps)),
                            )
                            .child(text(12., t.fg3, user.to_string())),
                    )
                    .child(
                        h_flex()
                            .justify_between()
                            .child(text(11.5, t.fg3, line))
                            .child(text(11.5, t.fg3, sync)),
                    )
                    .into_any_element()
            })
            .collect();
        v_flex()
            .pt(px(BAR_H + 16.))
            .pb(px(40.))
            .child(self.grid(cells, cols))
    }
}
