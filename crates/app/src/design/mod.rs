//! Design pages (step 2½): the new look drawn with static data, to settle it before real pages are built.
//!
//! `embymini --design <fixture dir> [--page home|search|favorites|library|series|movie|settings|item:<title>] [--light]`
//! The sidebar, cards, back / forward and the theme switch work, so the pages can be walked through.

mod assets;
mod fixture;
mod pages;
mod tokens;
mod ui;

use std::{path::PathBuf, rc::Rc};

use gpui_kit::assets::IconName;
use gpui_kit::component::{Theme, ThemeMode, TitleBar, h_flex, tooltip::Tooltip, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use fixture::{Fixture, Item};
use tokens::{BAR_H, RAIL_W, SIDEBAR_W, T};
use ui::{ghost_button, icon, medium, semibold, text};

actions!(design, [ToggleSidebar]);

/// Below this window width the sidebar starts out as an icon rail (until the user toggles it).
const AUTO_RAIL_BELOW: f32 = 1200.;

#[derive(Clone)]
pub enum Page {
    Home,
    Search,
    Favorites,
    /// Index into the fixture's libraries.
    Library(usize),
    Detail(Rc<Item>),
    Settings,
}

/// The page a navigation item leads to is the one shown (detail pages belong to no item).
fn same_page(a: &Page, b: &Page) -> bool {
    match (a, b) {
        (Page::Library(i), Page::Library(j)) => i == j,
        (Page::Detail(_), _) | (_, Page::Detail(_)) => false,
        _ => std::mem::discriminant(a) == std::mem::discriminant(b),
    }
}

pub struct Gallery {
    fx: Rc<Fixture>,
    page: Page,
    back: Vec<Page>,
    forward: Vec<Page>,
    dark: bool,
    scroll: ScrollHandle,
    season: usize,
    hero: usize,
    settings_tab: usize,
    filter: usize,
    /// The user's choice; None follows the window width.
    collapsed: Option<bool>,
    focus: FocusHandle,
}

impl Gallery {
    /// Tokens for the page area: the detail page is always dark (artwork-led, like a cinema), whatever the theme.
    fn t(&self) -> T {
        T::new(self.dark || matches!(self.page, Page::Detail(_)))
    }

    /// Tokens for the sidebar, which follows the theme.
    fn chrome(&self) -> T {
        T::new(self.dark)
    }

    fn is_rail(&self, window: &Window) -> bool {
        self.collapsed
            .unwrap_or_else(|| f32::from(window.viewport_size().width) < AUTO_RAIL_BELOW)
    }

    fn sidebar_w(&self, window: &Window) -> f32 {
        if self.is_rail(window) {
            RAIL_W
        } else {
            SIDEBAR_W
        }
    }

    fn toggle_sidebar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.collapsed = Some(!self.is_rail(window));
        cx.notify();
    }

    fn go(&mut self, page: Page, cx: &mut Context<Self>) {
        let old = std::mem::replace(&mut self.page, page);
        self.back.push(old);
        self.forward.clear();
        self.reset_view(cx);
    }

    fn reset_view(&mut self, cx: &mut Context<Self>) {
        self.scroll.set_offset(point(px(0.), px(0.)));
        self.season = 0;
        cx.notify();
    }

    fn history(&mut self, back: bool, cx: &mut Context<Self>) {
        let (from, to) = if back {
            (&mut self.back, &mut self.forward)
        } else {
            (&mut self.forward, &mut self.back)
        };
        if let Some(p) = from.pop() {
            to.push(std::mem::replace(&mut self.page, p));
            self.reset_view(cx);
        }
    }

    /// Opens a card: an episode opens its series; the full detail when the fixture has it, else the card's own data.
    fn open(&mut self, it: &Item, cx: &mut Context<Self>) {
        let id = it.series_id.as_ref().unwrap_or(&it.id);
        let full = self
            .fx
            .details
            .get(id)
            .or_else(|| {
                [&self.fx.series, &self.fx.movie]
                    .into_iter()
                    .find(|f| &f.id == id)
            })
            .cloned()
            .unwrap_or_else(|| it.clone());
        self.go(Page::Detail(Rc::new(full)), cx);
    }

    fn set_dark(&mut self, dark: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.dark = dark;
        Theme::change(
            if dark {
                ThemeMode::Dark
            } else {
                ThemeMode::Light
            },
            Some(window),
            cx,
        );
        cx.notify();
    }

    fn nav_item(
        &self,
        id: impl Into<ElementId>,
        ic: IconName,
        label: impl Into<SharedString>,
        page: Page,
        rail: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let t = self.chrome();
        let on = same_page(&self.page, &page);
        let label: SharedString = label.into();
        h_flex()
            .id(id)
            .h(px(if rail { 40. } else { 34. }))
            .items_center()
            .rounded(px(8.))
            .cursor_pointer()
            .when(on, |d| d.bg(t.accent))
            .when(!on, |d| d.hover(|s| s.bg(t.hover)))
            .when(rail, |d| {
                let tip = label.clone();
                d.w(px(44.))
                    .justify_center()
                    .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
            })
            .when(!rail, |d| d.px(px(10.)).gap(px(10.)))
            .child(icon(
                ic,
                if rail { 18. } else { 16. },
                if on { t.fg } else { t.fg2 },
            ))
            .when(!rail, |d| {
                d.child(
                    text(13.5, if on { t.fg } else { t.fg2 }, label).font_weight(if on {
                        FontWeight::MEDIUM
                    } else {
                        FontWeight::NORMAL
                    }),
                )
            })
            .on_click(cx.listener(move |this, _, _, cx| this.go(page.clone(), cx)))
    }

    fn sidebar(&self, rail: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.chrome();
        let libs: Vec<_> = self
            .fx
            .views
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let ic = match v.collection_type.as_deref() {
                    Some("tvshows") => IconName::Tv,
                    Some("music") => IconName::Music,
                    _ => IconName::Clapperboard,
                };
                self.nav_item(("lib", i), ic, v.name.clone(), Page::Library(i), rail, cx)
                    .into_any_element()
            })
            .collect();
        v_flex()
            .w(px(if rail { RAIL_W } else { SIDEBAR_W }))
            .h_full()
            .flex_shrink_0()
            .bg(t.sidebar)
            .border_r_1()
            .border_color(t.border)
            .child(
                h_flex()
                    .id("brand")
                    .h(px(BAR_H))
                    .when(rail, |d| d.justify_center())
                    .when(!rail, |d| d.px(px(18.)).gap(px(10.)))
                    .items_center()
                    .window_control_area(WindowControlArea::Drag)
                    .on_mouse_down(MouseButton::Left, |_, window, _| window.start_window_move())
                    .child(
                        div()
                            .size(px(24.))
                            .rounded(px(7.))
                            .bg(t.fg)
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(icon(IconName::Play, 12., t.bg).ml(px(1.))),
                    )
                    .when(!rail, |d| d.child(semibold(text(14.5, t.fg, "EmbyMini")))),
            )
            .child(
                v_flex()
                    .id("side-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px(px(10.))
                    .pt(px(6.))
                    .gap(px(2.))
                    .when(rail, |d| d.items_center())
                    .child(self.nav_item("nav-home", IconName::House, "首页", Page::Home, rail, cx))
                    .child(self.nav_item(
                        "nav-search",
                        IconName::Search,
                        "搜索",
                        Page::Search,
                        rail,
                        cx,
                    ))
                    .child(self.nav_item(
                        "nav-fav",
                        IconName::Heart,
                        "收藏",
                        Page::Favorites,
                        rail,
                        cx,
                    ))
                    .child(if rail {
                        div().w(px(24.)).h(px(1.)).my(px(12.)).bg(t.border)
                    } else {
                        medium(text(12., t.fg3, "媒体库"))
                            .px(px(10.))
                            .pt(px(22.))
                            .pb(px(6.))
                    })
                    .children(libs),
            )
            .child(
                v_flex()
                    .px(px(10.))
                    .pb(px(12.))
                    .pt(px(8.))
                    .gap(px(2.))
                    .when(rail, |d| d.items_center())
                    .child(self.nav_item(
                        "nav-settings",
                        IconName::Settings,
                        "设置",
                        Page::Settings,
                        rail,
                        cx,
                    ))
                    .child(
                        h_flex()
                            .id("server")
                            .mt(px(6.))
                            .h(px(48.))
                            .items_center()
                            .rounded(px(10.))
                            .when(rail, |d| {
                                d.w(px(44.)).justify_center().tooltip(|window, cx| {
                                    Tooltip::new("家庭影院").build(window, cx)
                                })
                            })
                            .when(!rail, |d| {
                                d.px(px(8.)).gap(px(10.)).border_1().border_color(t.border)
                            })
                            .cursor_pointer()
                            .hover(|s| s.bg(t.hover))
                            .child(
                                div()
                                    .size(px(30.))
                                    .rounded_full()
                                    .bg(t.surface)
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(semibold(text(12., t.fg, "家"))),
                            )
                            .when(!rail, |d| {
                                d.child(
                                    v_flex()
                                        .flex_1()
                                        .min_w_0()
                                        .child(medium(text(13., t.fg, "家庭影院")).truncate())
                                        .child(
                                            text(
                                                11.5,
                                                t.fg3,
                                                format!("{} 个媒体库", self.fx.views.len()),
                                            )
                                            .truncate(),
                                        ),
                                )
                                .child(icon(
                                    IconName::ChevronsUpDown,
                                    14.,
                                    t.fg3,
                                ))
                            }),
                    ),
            )
    }

    fn top_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.t();
        let can_back = !self.back.is_empty();
        // Clear over the page top (artwork runs under it); solid once the page scrolls beneath
        let scrolled = self.scroll.offset().y < px(-8.);
        let can_fwd = !self.forward.is_empty();
        TitleBar::new()
            .h(px(BAR_H))
            .bg(if scrolled {
                t.bg.opacity(0.94)
            } else {
                transparent_black()
            })
            .border_0()
            .when(scrolled, |d| d.border_b_1().border_color(t.border))
            .child(
                h_flex()
                    .w_full()
                    .pl(px(12.))
                    .pr(px(8.))
                    .gap(px(4.))
                    .items_center()
                    .child(
                        ghost_button("toggle-sidebar", IconName::PanelLeft, t.fg2, t)
                            .tooltip(|window, cx| {
                                Tooltip::new("折叠侧栏 (Ctrl+B)").build(window, cx)
                            })
                            .on_click(
                                cx.listener(|this, _, window, cx| this.toggle_sidebar(window, cx)),
                            ),
                    )
                    .child(div().w(px(1.)).h(px(16.)).mx(px(6.)).bg(t.border))
                    .child(
                        ghost_button(
                            "back",
                            IconName::ChevronLeft,
                            if can_back { t.fg } else { t.fg3 },
                            t,
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.history(true, cx))),
                    )
                    .child(
                        ghost_button(
                            "forward",
                            IconName::ChevronRight,
                            if can_fwd { t.fg } else { t.fg3 },
                            t,
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.history(false, cx))),
                    )
                    .child(
                        h_flex()
                            .id("search-box")
                            .ml(px(8.))
                            .h(px(34.))
                            .w(px(320.))
                            .px(px(12.))
                            .gap(px(8.))
                            .items_center()
                            .rounded(px(9.))
                            .bg(if t.dark {
                                hsla(0., 0., 0.09, 0.72)
                            } else {
                                hsla(0., 0., 1., 0.78)
                            })
                            .border_1()
                            .border_color(t.border)
                            .cursor_text()
                            .child(icon(IconName::Search, 15., t.fg3))
                            .child(text(13., t.fg3, "搜索影片、剧集、人物").flex_1())
                            .child(
                                div()
                                    .px(px(5.))
                                    .h(px(18.))
                                    .flex()
                                    .items_center()
                                    .rounded(px(4.))
                                    .border_1()
                                    .border_color(t.border)
                                    .child(text(10.5, t.fg3, "Ctrl K")),
                            )
                            .on_click(cx.listener(|this, _, _, cx| this.go(Page::Search, cx))),
                    )
                    .child(div().flex_1())
                    .child(ghost_button("refresh", IconName::RefreshCw, t.fg2, t))
                    .child(
                        ghost_button(
                            "theme",
                            // what the theme is, not the always-dark detail page
                            if self.dark {
                                IconName::Sun
                            } else {
                                IconName::Moon
                            },
                            t.fg2,
                            t,
                        )
                        .on_click(
                            cx.listener(|this, _, window, cx| {
                                this.set_dark(!this.dark, window, cx)
                            }),
                        ),
                    ),
            )
    }
}

impl Render for Gallery {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.t();
        let rail = self.is_rail(window);
        let page = match self.page.clone() {
            Page::Home => self.home(window, cx).into_any_element(),
            Page::Search => self.search(window, cx).into_any_element(),
            Page::Favorites => self.favorites(window, cx).into_any_element(),
            Page::Library(i) => self.library(i, window, cx).into_any_element(),
            Page::Detail(it) => self.detail(&it, window, cx).into_any_element(),
            Page::Settings => self.settings(window, cx).into_any_element(),
        };
        h_flex()
            .size_full()
            .track_focus(&self.focus)
            .on_action(
                cx.listener(|this, _: &ToggleSidebar, window, cx| this.toggle_sidebar(window, cx)),
            )
            .bg(t.bg)
            .text_color(t.fg)
            .child(self.sidebar(rail, cx))
            .child(
                div()
                    .relative()
                    .bg(t.bg)
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .child(
                        div()
                            .id("page")
                            .size_full()
                            .overflow_y_scroll()
                            .track_scroll(&self.scroll)
                            .child(page),
                    )
                    .child(
                        div()
                            .absolute()
                            .top_0()
                            .left_0()
                            .right_0()
                            .child(self.top_bar(cx)),
                    ),
            )
    }
}

pub struct Options {
    pub dir: PathBuf,
    pub page: String,
    pub light: bool,
}

pub fn run(opts: Options) {
    let fx = match Fixture::load(opts.dir.clone()) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("reading {}/fixture.json: {e:#}", opts.dir.display());
            std::process::exit(2);
        }
    };
    gpui_kit::application()
        .with_assets(assets::AppAssets)
        .run(move |cx| {
            gpui_kit::init(cx);
            cx.bind_keys([KeyBinding::new("ctrl-b", ToggleSidebar, None)]);
            // DESIGN_SIZE=WxH opens a taller window for whole-page screenshots
            let (w, h) = std::env::var("DESIGN_SIZE")
                .ok()
                .and_then(|s| {
                    let (w, h) = s.split_once('x')?;
                    Some((w.parse().ok()?, h.parse().ok()?))
                })
                .unwrap_or((1440., 900.));
            let window = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(w), px(h)),
                    cx,
                ))),
                window_min_size: Some(size(px(960.), px(600.))),
                ..TitleBar::window_options()
            };
            gpui_kit::open_window(window, cx, move |window, cx| {
                let dark = !opts.light;
                Theme::change(
                    if dark {
                        ThemeMode::Dark
                    } else {
                        ThemeMode::Light
                    },
                    Some(window),
                    cx,
                );
                let page = match opts.page.as_str() {
                    "search" => Page::Search,
                    "favorites" => Page::Favorites,
                    "library" => Page::Library(0),
                    "series" => Page::Detail(Rc::new(fx.series.clone())),
                    "movie" => Page::Detail(Rc::new(fx.movie.clone())),
                    "settings" => Page::Settings,
                    p => match p.strip_prefix("item:") {
                        // a title by name, for screenshots of its detail page
                        Some(name) => fx
                            .details
                            .values()
                            .find(|d| d.name == name)
                            .map_or(Page::Home, |d| Page::Detail(Rc::new(d.clone()))),
                        None => Page::Home,
                    },
                };
                let focus = cx.focus_handle();
                window.focus(&focus, cx);
                cx.new(|_| Gallery {
                    fx,
                    page,
                    back: Vec::new(),
                    forward: Vec::new(),
                    dark,
                    scroll: ScrollHandle::new(),
                    season: 0,
                    hero: 0,
                    settings_tab: 0,
                    filter: 0,
                    collapsed: None,
                    focus,
                })
            })
            .expect("opening the design window");
        });
}
