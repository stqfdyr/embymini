//! Detail page (film or series): the backdrop fills the first screen and dissolves into its own blurred copy, which
//! stays behind the rest of the page. Always dark.

use gpui_kit::assets::IconName;
use gpui_kit::component::{h_flex, tooltip::Tooltip, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::fixture::Item;
use super::tokens::PAD;
use super::ui::*;
use super::{Gallery, MenuItem};

const EP_W: f32 = 248.;
const PERSON_W: f32 = 92.;

/// "1080P" / "4K" from the video width.
fn res_label(w: u32) -> &'static str {
    match w {
        3800.. => "4K",
        2500.. => "2K",
        1900.. => "1080P",
        1200.. => "720P",
        _ => "SD",
    }
}

impl Gallery {
    fn actions_column(&self, it: &Item, cx: &mut Context<Self>) -> impl IntoElement {
        let (glass, border) = (light(0.14), light(0.12));
        let resume = it.episodes.iter().find_map(|e| e.position).or(it.position);
        let label = match resume {
            Some(p) => format!("播放  {}", clock(p / 10_000_000)),
            None => "播放".into(),
        };
        let mut buttons: Vec<(&'static str, IconName, &'static str)> = Vec::new();
        if resume.is_some() {
            buttons.push(("from-start", IconName::RotateCcw, "从头播放"));
        }
        buttons.extend([
            ("watched", IconName::Check, "标记为已观看"),
            ("favorite", IconName::Heart, "收藏"),
        ]);
        if it.is_series() {
            buttons.push(("version", IconName::Film, "版本"));
        }
        buttons.extend([
            ("audio", IconName::Headphones, "音轨"),
            ("subtitle", IconName::MessageSquareText, "字幕"),
        ]);
        let audio: Vec<String> = it
            .source
            .as_ref()
            .map(|s| {
                s.streams
                    .iter()
                    .filter(|x| x.kind == "音频")
                    .map(|x| x.rows.first().map(|r| r.1.clone()).unwrap_or_default())
                    .collect()
            })
            .unwrap_or_default();
        let subs: Vec<String> = it
            .source
            .as_ref()
            .map(|s| {
                s.streams
                    .iter()
                    .filter(|x| x.kind == "字幕")
                    .map(|x| x.rows.first().map(|r| r.1.clone()).unwrap_or_default())
                    .collect()
            })
            .unwrap_or_default();
        v_flex()
            .w(px(300.))
            .flex_shrink_0()
            .gap(px(14.))
            .child(
                h_flex()
                    .gap(px(10.))
                    .children(buttons.into_iter().map(|(id, ic, tip)| {
                        let (audio, subs) = (audio.clone(), subs.clone());
                        round_button(id, ic, 36., glass, light(0.95), border)
                            .tooltip(move |w, cx| Tooltip::new(tip).build(w, cx))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                let list = match id {
                                    "audio" => Some(("音频", audio.clone())),
                                    "subtitle" => Some(("字幕", subs.clone())),
                                    "version" => Some((
                                        "版本",
                                        vec!["2160p HEVC Dolby Vision".into(), "1080p H264".into()],
                                    )),
                                    _ => None,
                                };
                                if let Some((title, opts)) = list {
                                    let mut items: Vec<MenuItem> = opts
                                        .into_iter()
                                        .enumerate()
                                        .map(|(i, s)| MenuItem::new(s).checked(i == 0))
                                        .collect();
                                    if id != "version" {
                                        items.push(MenuItem::new("禁用").gap());
                                    }
                                    this.show_menu(Some(title), items, window, cx);
                                }
                            }))
                    })),
            )
            .child(
                h_flex()
                    .id("play")
                    .h(px(38.))
                    .w_full()
                    .gap(px(8.))
                    .items_center()
                    .justify_center()
                    .rounded_full()
                    .bg(light(0.95))
                    .cursor_pointer()
                    .hover(|s| s.bg(light(0.85)))
                    .child(icon(IconName::Play, 14., shade(0.9)))
                    .child(semibold(text(14., shade(0.9), label))),
            )
    }

    fn facts(&self, it: &Item) -> impl IntoElement {
        let t = self.t();
        let mut first = h_flex().gap(px(7.)).items_center();
        if let Some(r) = it.community_rating {
            first = first
                .child(icon(IconName::Star, 12., t.fg))
                .child(medium(text(13., t.fg, format!("{r:.1}"))));
        }
        if let Some(r) = &it.official_rating {
            first = first.child(badge(r.clone(), t.fg2));
        }
        if !it.genres.is_empty() {
            first = first.child(text(
                13.,
                t.fg2,
                it.genres
                    .iter()
                    .take(4)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(" · "),
            ));
        }
        // runtime, premiere, resolution, dynamic range, size
        let mut tech: Vec<String> = Vec::new();
        let runtime = it
            .minutes()
            .or_else(|| it.episodes.first().and_then(Item::minutes));
        if let Some(m) = runtime {
            tech.push(minutes_label(m));
        }
        if let Some(d) = &it.premiere_date {
            let mut p = d.split('-');
            if let (Some(y), Some(m), Some(dd)) = (p.next(), p.next(), p.next()) {
                tech.push(format!(
                    "{y} 年 {} 月 {} 日",
                    m.trim_start_matches('0'),
                    dd.trim_start_matches('0')
                ));
            }
        }
        if let Some(v) = it
            .source
            .as_ref()
            .and_then(|s| s.streams.iter().find(|x| x.kind == "视频"))
        {
            let get = |k: &str| v.rows.iter().find(|r| r.0 == k).map(|r| r.1.clone());
            if let Some(w) =
                get("分辨率").and_then(|r| r.split('x').next().and_then(|w| w.parse().ok()))
            {
                tech.push(res_label(w).into());
            }
            if let Some(r) = get("动态范围") {
                tech.push(r);
            }
        }
        if let Some(sz) = it.source.as_ref().and_then(|s| s.size) {
            tech.push(format!("{:.1}G", sz as f64 / 1_073_741_824.));
        }
        let prefix = match (it.is_series(), it.seasons.len()) {
            (true, n) if n > 0 => format!("[{} · 第 1/{n} 季 · 第 1 集] ", it.name),
            _ => String::new(),
        };
        v_flex()
            .flex_1()
            .min_w_0()
            .gap(px(6.))
            .child(first)
            .child(text(12.5, t.fg2, tech.join("  ")))
            .when_some(it.overview.clone(), |d, o| {
                d.child(
                    div()
                        .text_size(px(13.))
                        .line_height(px(20.))
                        .text_color(light(0.82))
                        .line_clamp(3)
                        .child(format!("{prefix}{o}")),
                )
            })
    }

    pub(super) fn detail(
        &mut self,
        it: &Item,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let t = self.t();
        let fx = self.fx.clone();
        let vh = f32::from(window.viewport_size().height);
        let hero_h = (vh - 150.).max(460.);
        let backdrop = fx
            .img(it.backdrop.as_ref())
            .or_else(|| fx.img(it.thumb.as_ref()));
        let mut page =
            v_flex().pb(px(36.)).child(
                div()
                    .relative()
                    .w_full()
                    .h(px(hero_h))
                    .when_some(backdrop, |d, p| {
                        d.child(
                            img(p)
                                .absolute()
                                .inset_0()
                                .size_full()
                                .object_fit(ObjectFit::Cover),
                        )
                    })
                    // the sharp picture fades into the same dark veil that covers the blurred copy behind the page
                    .child(div().absolute().inset_0().bg(linear_gradient(
                        180.,
                        linear_color_stop(shade(0.), 0.4),
                        linear_color_stop(shade(0.62), 1.),
                    )))
                    .child(div().absolute().inset_0().bg(linear_gradient(
                        90.,
                        linear_color_stop(shade(0.35), 0.),
                        linear_color_stop(shade(0.), 0.5),
                    )))
                    .child(div().absolute().top_0().left_0().right_0().h(px(110.)).bg(
                        linear_gradient(
                            180.,
                            linear_color_stop(shade(0.45), 0.),
                            linear_color_stop(shade(0.), 1.),
                        ),
                    ))
                    .child(
                        v_flex()
                            .absolute()
                            .left(px(PAD))
                            .right(px(PAD))
                            .bottom(px(14.))
                            .gap(px(16.))
                            .child(logo_or_title(&fx, it, 300., 110., t.fg))
                            .child(
                                h_flex()
                                    .gap(px(28.))
                                    .items_end()
                                    .child(self.actions_column(it, cx))
                                    .child(self.facts(it)),
                            ),
                    ),
            );
        let mut sections = v_flex().pt(px(12.)).gap(px(26.));
        // episodes of the chosen season
        let episodes: &[Item] = it
            .seasons
            .get(self.season)
            .filter(|s| !s.episodes.is_empty())
            .map_or(&it.episodes, |s| &s.episodes);
        if !episodes.is_empty() {
            let season_name = it
                .seasons
                .get(self.season)
                .map_or("第 1 季".to_string(), |s| s.name.clone());
            let names: Vec<String> = it.seasons.iter().map(|s| s.name.clone()).collect();
            let cur = self.season;
            let current = episodes
                .iter()
                .position(|e| e.played.is_some())
                .unwrap_or(0);
            let cards: Vec<AnyElement> = episodes
                .iter()
                .enumerate()
                .map(|(i, e)| episode_card(i, e, i == current, &fx, t).into_any_element())
                .collect();
            let handle = self.row(format!("eps-{cur}"));
            let jump = handle.clone();
            let jump2 = handle.clone();
            sections = sections.child(
                v_flex()
                    .gap(px(10.))
                    .child(
                        h_flex()
                            .px(px(PAD))
                            .items_center()
                            .justify_between()
                            .child(
                                h_flex()
                                    .id("season-pick")
                                    .gap(px(4.))
                                    .items_center()
                                    .cursor_pointer()
                                    .child(semibold(text(16., t.fg, season_name)))
                                    .child(icon(IconName::ChevronDown, 14., t.fg))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        let items = names
                                            .iter()
                                            .enumerate()
                                            .map(|(i, n)| {
                                                MenuItem::new(n.clone()).checked(i == cur).on(
                                                    move |this, cx| {
                                                        this.season = i;
                                                        cx.notify();
                                                    },
                                                )
                                            })
                                            .collect();
                                        this.show_menu(Some("季"), items, window, cx);
                                    })),
                            )
                            .child(
                                h_flex()
                                    .gap(px(4.))
                                    .items_center()
                                    .child(
                                        ghost_button(
                                            "to-first",
                                            IconName::ArrowLeftToLine,
                                            t.fg2,
                                            t,
                                        )
                                        .on_click(
                                            move |_, w, _| {
                                                jump.set_offset(point(px(0.), px(0.)));
                                                w.refresh();
                                            },
                                        ),
                                    )
                                    .child(
                                        ghost_button(
                                            "to-last",
                                            IconName::ArrowRightToLine,
                                            t.fg2,
                                            t,
                                        )
                                        .on_click(
                                            move |_, w, _| {
                                                let max = jump2.max_offset().x;
                                                jump2.set_offset(point(-max, px(0.)));
                                                w.refresh();
                                            },
                                        ),
                                    )
                                    .child(text(12.5, t.fg2, "更多").ml(px(4.))),
                            ),
                    )
                    .child(shelf(
                        format!("eps-{cur}").into(),
                        &handle,
                        cards,
                        EP_W * 9. / 16.,
                    )),
            );
        }
        // cast
        let people: Vec<AnyElement> = it
            .people
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let kind = match p.kind.as_deref() {
                    Some("Director") => "导演",
                    Some("Writer") => "编剧",
                    Some("Producer") => "制片",
                    Some("GuestStar") => "客串",
                    _ => "演员",
                };
                v_flex()
                    .id(("cast", i))
                    .w(px(PERSON_W))
                    .gap(px(6.))
                    .flex_shrink_0()
                    .cursor_pointer()
                    .group("card")
                    .child(
                        div()
                            .relative()
                            .size(px(PERSON_W))
                            .rounded(px(12.))
                            .bg(light(0.1))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(icon(IconName::User, 30., light(0.45)))
                            .when_some(fx.img(p.img.as_ref()), |d, path| {
                                d.child(
                                    img(path)
                                        .absolute()
                                        .inset_0()
                                        .size_full()
                                        .rounded(px(12.))
                                        .object_fit(ObjectFit::Cover),
                                )
                            })
                            .child(hover_veil(12.)),
                    )
                    .child(
                        v_flex()
                            .gap(px(1.))
                            .child(medium(text(12., t.fg, p.name.clone())).truncate())
                            .when_some(p.role.clone().filter(|r| !r.is_empty()), |d, r| {
                                d.child(text(11., t.fg2, r).truncate())
                            })
                            .child(text(11., t.fg3, kind)),
                    )
                    .into_any_element()
            })
            .collect();
        if !people.is_empty() {
            let h = self.row("cast");
            sections = sections.child(
                v_flex()
                    .gap(px(10.))
                    .child(section_head("演职人员", None, t))
                    .child(shelf("cast".into(), &h, people, PERSON_W)),
            );
        }
        // artwork
        let arts: Vec<_> = it
            .backdrops
            .iter()
            .filter_map(|b| fx.img(b.as_ref()))
            .collect();
        if !arts.is_empty() {
            let cards = arts
                .into_iter()
                .map(|p| art(Some(p), 300., 169., 12., t).into_any_element())
                .collect();
            let h = self.row("art");
            sections = sections.child(
                v_flex()
                    .gap(px(10.))
                    .child(section_head("艺术图", None, t))
                    .child(shelf("art".into(), &h, cards, 169.)),
            );
        }
        let similar = if it.similar.is_empty() {
            fx.latest_rows()
                .first()
                .map(|r| r.1.to_vec())
                .unwrap_or_default()
        } else {
            it.similar.clone()
        };
        if !similar.is_empty() {
            sections = sections.child(
                v_flex()
                    .gap(px(10.))
                    .child(section_head("类似作品", None, t))
                    .child(self.poster_shelf("similar", &similar, cx)),
            );
        }
        if !it.links.is_empty() {
            sections = sections.child(
                v_flex()
                    .gap(px(10.))
                    .child(section_head("链接", None, t))
                    .child(h_flex().px(px(PAD)).gap(px(8.)).flex_wrap().children(
                        it.links.iter().enumerate().map(|(i, l)| {
                            h_flex()
                                .id(("link", i))
                                .h(px(28.))
                                .px(px(12.))
                                .gap(px(6.))
                                .items_center()
                                .rounded_full()
                                .bg(light(0.1))
                                .border_1()
                                .border_color(light(0.1))
                                .cursor_pointer()
                                .hover(|s| s.bg(light(0.16)))
                                .child(icon(IconName::Link, 13., t.fg2))
                                .child(text(12.5, t.fg, l.name.clone()))
                        }),
                    )),
            );
        }
        if let Some(src) = &it.source {
            let cards: Vec<AnyElement> = src
                .streams
                .iter()
                .map(|s| {
                    let ic = match s.kind.as_str() {
                        "视频" => IconName::Video,
                        "音频" => IconName::Volume2,
                        _ => IconName::MessageSquareText,
                    };
                    v_flex()
                        .w(px(250.))
                        .min_h(px(260.))
                        .flex_shrink_0()
                        .p(px(14.))
                        .gap(px(6.))
                        .rounded(px(14.))
                        .bg(light(0.07))
                        .border_1()
                        .border_color(light(0.09))
                        .child(
                            h_flex()
                                .gap(px(8.))
                                .items_center()
                                .pb(px(4.))
                                .child(icon(ic, 15., t.fg))
                                .child(medium(text(13., t.fg, s.kind.clone())))
                                .when(s.default && s.kind != "视频", |d| {
                                    d.child(icon(IconName::Check, 13., t.fg))
                                }),
                        )
                        .children(s.rows.iter().map(|(k, v)| {
                            h_flex()
                                .gap(px(8.))
                                .items_start()
                                .child(text(11.5, t.fg3, k.clone()).w(px(52.)).flex_shrink_0())
                                .child(text(11.5, t.fg, v.clone()).flex_1().min_w_0().line_clamp(2))
                        }))
                        .into_any_element()
                })
                .collect();
            let h = self.row("info");
            sections = sections.child(
                v_flex()
                    .gap(px(10.))
                    .child(section_head("媒体信息", None, t))
                    .child(shelf("info".into(), &h, cards, 260.)),
            );
            let size = src
                .size
                .map(|s| format!("{:.2}G", s as f64 / 1_073_741_824.))
                .unwrap_or_default();
            sections = sections.child(
                v_flex()
                    .mx(px(PAD))
                    .py(px(10.))
                    .px(px(16.))
                    .gap(px(2.))
                    .items_center()
                    .rounded(px(12.))
                    .bg(light(0.07))
                    .border_1()
                    .border_color(light(0.09))
                    .when(!it.studios.is_empty(), |d| {
                        d.child(text(11.5, t.fg2, it.studios.join(" · ")))
                    })
                    .when_some(src.file.clone().filter(|f| !f.is_empty()), |d, f| {
                        d.child(text(11.5, t.fg2, f).truncate())
                    })
                    .child(text(
                        11.5,
                        t.fg3,
                        format!("{}  ·  {}", super::SERVERS[self.server].0, size),
                    )),
            );
        }
        page = page.child(sections);
        page
    }
}

fn episode_card(
    i: usize,
    e: &Item,
    current: bool,
    fx: &super::fixture::Fixture,
    t: super::tokens::T,
) -> Stateful<Div> {
    let h = (EP_W * 9. / 16.).round();
    let thumb = fx
        .img(e.poster.as_ref())
        .or_else(|| fx.img(e.thumb.as_ref()));
    let n = e.index_number.unwrap_or(i as u32 + 1);
    let title = if e.name.is_empty() {
        format!("E{n:02}")
    } else {
        format!("E{n:02} · {}", e.name)
    };
    v_flex()
        .id(("ep", i))
        .w(px(EP_W))
        .gap(px(6.))
        .flex_shrink_0()
        .cursor_pointer()
        .group("card")
        .child(
            art(thumb, EP_W, h, 10., t)
                .child(hover_veil(10.))
                .when(current, |d| {
                    d.child(
                        div()
                            .absolute()
                            .inset_0()
                            .rounded(px(10.))
                            .border_2()
                            .border_color(light(0.95)),
                    )
                })
                .when_some(e.community_rating.filter(|&r| r > 0.), |d, r| {
                    d.child(rating_badge(r))
                })
                .when_some(corner_badge(e), |d, b| d.child(b))
                .when_some(e.played, |d, p| d.child(progress(p, 8.))),
        )
        .child(medium(text(12.5, t.fg, title)).truncate())
        .when_some(e.overview.clone(), |d, o| {
            d.child(
                div()
                    .text_size(px(11.5))
                    .line_height(px(17.))
                    .text_color(t.fg2)
                    .line_clamp(3)
                    .child(o),
            )
        })
        .when(e.overview.is_none(), |d| {
            d.child(text(
                11.5,
                t.fg3,
                e.minutes().map(minutes_label).unwrap_or_default(),
            ))
        })
}
