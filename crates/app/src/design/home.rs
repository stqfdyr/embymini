//! Server home: full-bleed carousel, my media, continue watching, then one row per library.

use gpui_kit::assets::IconName;
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::tokens::{BAR_H, POSTER_W};
use super::ui::*;
use super::{Gallery, Page};

/// Library cover cards and continue-watching cards.
const COVER_W: f32 = 232.;
const WIDE_W: f32 = 264.;

impl Gallery {
    fn hero(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.t();
        let fx = self.fx.clone();
        let n = fx.hero.len().max(1);
        let cur = self.hero % n;
        let Some(it) = fx.hero.get(cur) else {
            return div().h(px(BAR_H)).into_any_element();
        };
        let vw = f32::from(window.viewport_size().width) - self.sidebar_w(window);
        let h = (f32::from(window.viewport_size().height) * 0.78)
            .min(vw * 0.6)
            .max(420.);
        let mut meta: Vec<AnyElement> = Vec::new();
        if let Some(r) = it.community_rating {
            meta.push(
                h_flex()
                    .gap(px(3.))
                    .items_center()
                    .child(icon(IconName::Star, 11., t.fg))
                    .child(medium(text(12.5, t.fg, format!("{r:.1}"))))
                    .into_any_element(),
            );
        }
        if let Some(y) = it.production_year {
            meta.push(text(12.5, t.fg2, y.to_string()).into_any_element());
        }
        if let Some(r) = &it.official_rating {
            meta.push(badge(r.clone(), t.fg2).into_any_element());
        }
        if !it.genres.is_empty() {
            meta.push(
                text(
                    12.5,
                    t.fg2,
                    it.genres
                        .iter()
                        .take(3)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(" · "),
                )
                .into_any_element(),
            );
        }
        let mut line = h_flex().gap(px(7.)).items_center().justify_center();
        for (i, p) in meta.into_iter().enumerate() {
            if i > 0 {
                line = line.child(text(12.5, t.fg3, "·"));
            }
            line = line.child(p);
        }
        let it2 = it.clone();
        let arrow = |right: bool| {
            div()
                .id(("hero-arrow", usize::from(right)))
                .absolute()
                .top(px(h * 0.42))
                .when(right, |d| d.right(px(18.)))
                .when(!right, |d| d.left(px(18.)))
                .size(px(34.))
                .rounded_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(t.glass)
                .border_1()
                .border_color(t.border)
                .cursor_pointer()
                .opacity(0.)
                .group_hover("hero", |s| s.opacity(1.))
                .child(icon(
                    if right {
                        IconName::ChevronRight
                    } else {
                        IconName::ChevronLeft
                    },
                    16.,
                    t.fg,
                ))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.hero = (this.hero + if right { 1 } else { n - 1 }) % n;
                    cx.notify();
                }))
        };
        div()
            .id("hero")
            .group("hero")
            .relative()
            .w_full()
            .h(px(h))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| this.open(&it2, cx)))
            .when_some(fx.img(it.backdrop.as_ref()), |d, p| {
                d.child(
                    img(p)
                        .absolute()
                        .inset_0()
                        .size_full()
                        .object_fit(ObjectFit::Cover),
                )
            })
            // top: keeps the toolbar readable; bottom: the picture dissolves into the page
            .child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .right_0()
                    .h(px(120.))
                    .bg(linear_gradient(
                        180.,
                        linear_color_stop(t.bg.opacity(0.5), 0.),
                        linear_color_stop(t.bg.opacity(0.), 1.),
                    )),
            )
            .child(div().absolute().inset_0().bg(linear_gradient(
                180.,
                linear_color_stop(t.bg.opacity(0.), 0.45),
                linear_color_stop(t.bg, 1.),
            )))
            .child(div().absolute().inset_0().bg(linear_gradient(
                180.,
                linear_color_stop(t.bg.opacity(0.), 0.7),
                linear_color_stop(t.bg.opacity(0.85), 0.92),
            )))
            .child(
                v_flex()
                    .absolute()
                    .left_0()
                    .right_0()
                    .bottom(px(18.))
                    .items_center()
                    .gap(px(10.))
                    .child(logo_or_title(&fx, it, 380., 110., t.fg))
                    .child(line)
                    .when_some(it.overview.clone(), |d, o| {
                        d.child(
                            div()
                                .max_w(px(760.))
                                .px(px(40.))
                                .text_center()
                                .text_size(px(12.5))
                                .line_height(px(19.))
                                .text_color(t.fg2)
                                .line_clamp(2)
                                .child(o),
                        )
                    })
                    .child(h_flex().pt(px(6.)).gap(px(7.)).children((0..n).map(|i| {
                        div()
                            .id(("dot", i))
                            .size(px(6.))
                            .rounded_full()
                            .cursor_pointer()
                            .bg(if i == cur { t.fg } else { t.fg3.opacity(0.6) })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.hero = i;
                                cx.notify();
                            }))
                    }))),
            )
            .child(arrow(false))
            .child(arrow(true))
            .into_any_element()
    }

    pub(super) fn home(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.t();
        let fx = self.fx.clone();
        let covers: Vec<AnyElement> = fx
            .views
            .iter()
            .enumerate()
            .map(|(i, v)| {
                v_flex()
                    .id(("my", i))
                    .w(px(COVER_W))
                    .gap(px(7.))
                    .flex_shrink_0()
                    .cursor_pointer()
                    .group("card")
                    .child(
                        art(
                            fx.img(v.poster.as_ref()),
                            COVER_W,
                            (COVER_W * 9. / 16.).round(),
                            10.,
                            t,
                        )
                        .child(hover_veil(10.)),
                    )
                    .child(medium(text(12.5, t.fg, v.name.clone())))
                    .on_click(cx.listener(move |this, _, _, cx| this.go(Page::Library(i), cx)))
                    .into_any_element()
            })
            .collect();
        let resume: Vec<AnyElement> = fx
            .resume
            .iter()
            .enumerate()
            .map(|(i, it)| {
                let (a, b) = (it.clone(), it.clone());
                wide_card(("resume", i), it, &fx, WIDE_W, t)
                    .on_click(cx.listener(move |this, _, _, cx| this.open(&a, cx)))
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, _, window, cx| {
                            this.card_menu(&b, true, window, cx)
                        }),
                    )
                    .into_any_element()
            })
            .collect();
        let my = self.row("my");
        let rs = self.row("resume");
        let mut page = v_flex()
            .pb(px(48.))
            .gap(px(26.))
            .child(self.hero(window, cx))
            .child(
                v_flex()
                    .gap(px(10.))
                    .child(
                        section_head("我的媒体", Some(fx.views.len().to_string()), t)
                            .id("my-head")
                            .on_click(cx.listener(|this, _, _, cx| this.go(Page::Media, cx))),
                    )
                    .child(shelf("my".into(), &my, covers, COVER_W * 9. / 16.)),
            )
            .child(
                v_flex()
                    .gap(px(10.))
                    .child(section_head(
                        "继续观看",
                        Some(fx.resume.len().to_string()),
                        t,
                    ))
                    .child(shelf("resume".into(), &rs, resume, WIDE_W * 9. / 16.)),
            );
        for (i, (name, items)) in fx.latest_rows().into_iter().enumerate() {
            let lib = fx.views.iter().position(|v| v.name == name).unwrap_or(0);
            let count = if name == fx.library.name {
                fx.library.total as usize
            } else {
                items.len()
            };
            page = page.child(
                v_flex()
                    .gap(px(10.))
                    .child(
                        section_head(name.to_string(), Some(count.to_string()), t)
                            .id(("lib-head", i))
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.go(Page::Library(lib), cx)),
                            ),
                    )
                    .child(self.poster_shelf(format!("latest-{i}"), items, cx)),
            );
        }
        page
    }

    pub(super) fn poster_shelf(
        &mut self,
        name: impl Into<SharedString>,
        items: &[super::fixture::Item],
        cx: &mut Context<Self>,
    ) -> Div {
        let t = self.t();
        let name: SharedString = name.into();
        let handle = self.row(name.clone());
        let cards = items
            .iter()
            .enumerate()
            .map(|(i, it)| {
                let (a, b) = (it.clone(), it.clone());
                poster_card(
                    ElementId::named_usize(name.clone(), i),
                    it,
                    &self.fx,
                    POSTER_W,
                    t,
                )
                .on_click(cx.listener(move |this, _, _, cx| this.open(&a, cx)))
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, _, window, cx| this.card_menu(&b, false, window, cx)),
                )
                .into_any_element()
            })
            .collect();
        shelf(name, &handle, cards, POSTER_W * 1.5)
    }
}
