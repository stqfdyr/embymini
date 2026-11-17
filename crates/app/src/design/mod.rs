//! Design pages (step 2½): the new look drawn with static data, to settle it before real pages are built.
//!
//! `embymini --design <fixture dir> [--page home|search|favorites|media|library|series|movie|servers|settings|item:<title>] [--light]`
//! Navigation, menus, the sidebar, row paging and the settings dialog work, so the pages can be walked through.

mod assets;
mod detail;
mod fixture;
mod home;
mod lists;
mod settings;
mod tokens;
mod ui;

use std::{collections::HashMap, path::PathBuf, rc::Rc};

use gpui_kit::assets::IconName;
use gpui_kit::component::{Theme, ThemeMode, TitleBar, h_flex, tooltip::Tooltip, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use fixture::{Fixture, Item};
use tokens::{BAR_H, SIDEBAR_W, T};
use ui::{ghost_button, icon, medium, semibold, text};

actions!(design, [ToggleSidebar, CloseOverlay]);

/// Below this window width the sidebar starts out hidden (until the user toggles it).
const AUTO_HIDE_BELOW: f32 = 1100.;

#[derive(Clone)]
pub enum Page {
    Home,
    Search,
    Favorites,
    /// All libraries ("我的媒体 ›").
    Media,
    /// Index into the fixture's libraries.
    Library(usize),
    Detail(Rc<Item>),
    Servers,
}

/// A small popup menu: an optional grey heading, then items (✓ marks the current one).
pub struct Menu {
    pos: Point<Pixels>,
    title: Option<SharedString>,
    items: Vec<MenuItem>,
}

/// What a menu item does when picked.
type MenuAction = Rc<dyn Fn(&mut Gallery, &mut Context<Gallery>)>;

pub struct MenuItem {
    label: SharedString,
    checked: bool,
    /// Separator line above this item.
    gap: bool,
    action: Option<MenuAction>,
}

impl MenuItem {
    pub fn new(label: impl Into<SharedString>) -> Self {
        Self {
            label: label.into(),
            checked: false,
            gap: false,
            action: None,
        }
    }

    pub fn checked(mut self, on: bool) -> Self {
        self.checked = on;
        self
    }

    pub fn gap(mut self) -> Self {
        self.gap = true;
        self
    }

    pub fn on(mut self, f: impl Fn(&mut Gallery, &mut Context<Gallery>) + 'static) -> Self {
        self.action = Some(Rc::new(f));
        self
    }
}

#[derive(Clone, Copy, PartialEq)]
pub enum Modal {
    Settings,
    AddServer,
}

pub struct Gallery {
    fx: Rc<Fixture>,
    page: Page,
    back: Vec<Page>,
    forward: Vec<Page>,
    dark: bool,
    scroll: ScrollHandle,
    /// Paging state of the sideways rows, by row name.
    rows: HashMap<SharedString, ScrollHandle>,
    season: usize,
    hero: usize,
    settings_tab: usize,
    /// Library page segment (films / genres / studios / folders).
    lib_seg: usize,
    /// Sort field index and direction (descending).
    sort: (usize, bool),
    /// Search page: whether a query has been run (results) or not (empty state).
    searched: bool,
    server: usize,
    /// The user's choice; None follows the window width.
    sidebar: Option<bool>,
    menu: Option<Menu>,
    modal: Option<Modal>,
    focus: FocusHandle,
}

pub const SORTS: [&str; 16] = [
    "名称",
    "添加日期",
    "发行日期",
    "播放日期",
    "年份",
    "IMDb 评分",
    "影评人评分",
    "播放次数",
    "票房",
    "随机",
    "分级",
    "分辨率",
    "码率",
    "格式",
    "时长",
    "大小",
];

pub const SERVERS: [(&str, &str); 2] = [("家庭影院", "admin"), ("朋友的服务器", "guest")];

impl Gallery {
    /// Tokens for the page area: the detail page is always dark (artwork-led, like a cinema), whatever the theme.
    fn t(&self) -> T {
        T::new(self.dark || matches!(self.page, Page::Detail(_)))
    }

    /// Tokens for the sidebar and dialogs, which follow the theme.
    fn chrome(&self) -> T {
        T::new(self.dark)
    }

    fn sidebar_open(&self, window: &Window) -> bool {
        self.sidebar
            .unwrap_or_else(|| f32::from(window.viewport_size().width) >= AUTO_HIDE_BELOW)
    }

    fn sidebar_w(&self, window: &Window) -> f32 {
        if self.sidebar_open(window) {
            SIDEBAR_W
        } else {
            0.
        }
    }

    fn toggle_sidebar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sidebar = Some(!self.sidebar_open(window));
        cx.notify();
    }

    fn row(&mut self, name: impl Into<SharedString>) -> ScrollHandle {
        self.rows.entry(name.into()).or_default().clone()
    }

    fn go(&mut self, page: Page, cx: &mut Context<Self>) {
        let old = std::mem::replace(&mut self.page, page);
        self.back.push(old);
        self.forward.clear();
        self.reset_view(cx);
    }

    fn reset_view(&mut self, cx: &mut Context<Self>) {
        self.scroll.set_offset(point(px(0.), px(0.)));
        self.rows.clear();
        self.season = 0;
        self.menu = None;
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

    /// Opens a menu at the pointer.
    fn show_menu(
        &mut self,
        title: Option<&str>,
        items: Vec<MenuItem>,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        self.menu = Some(Menu {
            pos: window.mouse_position(),
            title: title.map(|s| SharedString::from(s.to_string())),
            items,
        });
        cx.notify();
    }

    /// Right-click menu of a card.
    fn card_menu(&mut self, it: &Item, resume: bool, window: &Window, cx: &mut Context<Self>) {
        let it2 = it.clone();
        let mut items = vec![
            MenuItem::new("播放"),
            MenuItem::new("打开详情").on(move |this, cx| this.open(&it2, cx)),
            MenuItem::new("添加到收藏夹").gap(),
            MenuItem::new(if it.watched {
                "标记为未观看"
            } else {
                "标记为已观看"
            }),
        ];
        if resume {
            items.push(MenuItem::new("移除观看记录"));
        }
        self.show_menu(None, items, window, cx);
    }

    fn close_overlay(&mut self, cx: &mut Context<Self>) {
        if self.menu.take().is_none() {
            self.modal = None;
        }
        cx.notify();
    }

    fn sidebar_item(
        &self,
        id: impl Into<ElementId>,
        lead: AnyElement,
        label: impl Into<SharedString>,
        on: bool,
        cx: &mut Context<Self>,
        click: impl Fn(&mut Self, &mut Context<Self>) + 'static,
    ) -> impl IntoElement {
        let t = self.chrome();
        h_flex()
            .id(id)
            .h(px(32.))
            .px(px(10.))
            .gap(px(10.))
            .items_center()
            .rounded(px(8.))
            .cursor_pointer()
            .when(on, |d| d.bg(t.accent))
            .when(!on, |d| d.hover(|s| s.bg(t.hover)))
            .child(lead)
            .child(text(13., t.fg, label).font_weight(if on {
                FontWeight::MEDIUM
            } else {
                FontWeight::NORMAL
            }))
            .on_click(cx.listener(move |this, _, _, cx| click(this, cx)))
    }

    fn sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.chrome();
        let on_servers = matches!(self.page, Page::Servers);
        let in_server = !on_servers;
        let servers: Vec<AnyElement> = SERVERS
            .iter()
            .enumerate()
            .map(|(i, (name, _))| {
                let initial: String = name.chars().next().map(String::from).unwrap_or_default();
                let lead = div()
                    .size(px(20.))
                    .rounded(px(6.))
                    .bg(if i == self.server { t.fg } else { t.surface })
                    .border_1()
                    .border_color(t.border)
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(semibold(text(
                        10.5,
                        if i == self.server { t.bg } else { t.fg2 },
                        initial,
                    )))
                    .into_any_element();
                self.sidebar_item(
                    ("server", i),
                    lead,
                    *name,
                    in_server && i == self.server,
                    cx,
                    move |this, cx| {
                        this.server = i;
                        this.go(Page::Home, cx);
                    },
                )
                .into_any_element()
            })
            .collect();
        v_flex()
            .w(px(SIDEBAR_W))
            .h_full()
            .flex_shrink_0()
            .bg(t.sidebar)
            .border_r_1()
            .border_color(t.border)
            .child(
                h_flex()
                    .id("brand")
                    .h(px(BAR_H))
                    .px(px(16.))
                    .gap(px(10.))
                    .items_center()
                    .window_control_area(WindowControlArea::Drag)
                    .on_mouse_down(MouseButton::Left, |_, window, _| window.start_window_move())
                    .child(
                        div()
                            .size(px(22.))
                            .rounded(px(6.))
                            .bg(t.fg)
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(icon(IconName::Play, 11., t.bg).ml(px(1.))),
                    )
                    .child(semibold(text(14., t.fg, "EmbyMini"))),
            )
            .child(
                v_flex()
                    .id("side-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px(px(10.))
                    .gap(px(2.))
                    .child(self.sidebar_item(
                        "nav-servers",
                        icon(IconName::Server, 16., t.fg2).into_any_element(),
                        "服务器",
                        on_servers,
                        cx,
                        |this, cx| this.go(Page::Servers, cx),
                    ))
                    .child(self.sidebar_item(
                        "nav-settings",
                        icon(IconName::Settings, 16., t.fg2).into_any_element(),
                        "设置",
                        false,
                        cx,
                        |this, cx| {
                            this.modal = Some(Modal::Settings);
                            cx.notify();
                        },
                    ))
                    .child(
                        medium(text(11.5, t.fg3, "媒体服务器"))
                            .px(px(10.))
                            .pt(px(18.))
                            .pb(px(6.)),
                    )
                    .children(servers),
            )
    }

    fn top_bar(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.t();
        let on_detail = matches!(self.page, Page::Detail(_));
        let over_art = on_detail || matches!(self.page, Page::Home);
        let scrolled = self.scroll.offset().y < px(-8.);
        let solid = !over_art || scrolled;
        let sidebar_shown = self.sidebar_open(window);
        let server = SERVERS[self.server].0;
        let (title, center): (SharedString, Option<AnyElement>) = match &self.page {
            Page::Home | Page::Search | Page::Favorites => {
                let sel = match self.page {
                    Page::Search => 1,
                    Page::Favorites => 2,
                    _ => 0,
                };
                let e = cx.entity();
                (
                    match self.page {
                        Page::Search => "搜索".into(),
                        Page::Favorites => "收藏".into(),
                        _ => server.into(),
                    },
                    Some(
                        ui::segmented(
                            "tabs",
                            &["媒体", "搜索", "收藏"],
                            sel,
                            t,
                            move |i, _, cx| {
                                e.update(cx, |this, cx| {
                                    let p = [Page::Home, Page::Search, Page::Favorites][i].clone();
                                    this.go(p, cx);
                                })
                            },
                        )
                        .into_any_element(),
                    ),
                )
            }
            Page::Library(i) => {
                let tv = self
                    .fx
                    .views
                    .get(*i)
                    .and_then(|v| v.collection_type.as_deref())
                    == Some("tvshows");
                let e = cx.entity();
                (
                    self.fx
                        .views
                        .get(*i)
                        .map(|v| v.name.clone())
                        .unwrap_or_default()
                        .into(),
                    Some(
                        ui::segmented(
                            "lib-seg",
                            &[if tv { "节目" } else { "电影" }, "风格", "工作室", "文件夹"],
                            self.lib_seg,
                            t,
                            move |i, _, cx| {
                                e.update(cx, |this, cx| {
                                    this.lib_seg = i;
                                    cx.notify();
                                })
                            },
                        )
                        .into_any_element(),
                    ),
                )
            }
            Page::Media => ("我的媒体".into(), None),
            Page::Servers => ("服务器".into(), None),
            Page::Detail(_) => ("".into(), None),
        };
        let nested = !matches!(
            self.page,
            Page::Home | Page::Search | Page::Favorites | Page::Servers
        );
        // Back / home as one pill, like a browser
        let nav_pill = h_flex()
            .h(px(30.))
            .px(px(2.))
            .gap(px(2.))
            .items_center()
            .rounded_full()
            .bg(t.glass)
            .border_1()
            .border_color(t.border)
            .child(
                ghost_button("back", IconName::ChevronLeft, t.fg, t)
                    .rounded_full()
                    .size(px(26.))
                    .on_click(cx.listener(|this, _, _, cx| this.history(true, cx))),
            )
            .child(
                ghost_button("home", IconName::House, t.fg, t)
                    .rounded_full()
                    .size(px(26.))
                    .on_click(cx.listener(|this, _, _, cx| this.go(Page::Home, cx))),
            );
        let right: AnyElement = match &self.page {
            Page::Detail(_) => h_flex()
                .gap(px(8.))
                .child(
                    ui::round_button("versions", IconName::Search, 30., t.glass, t.fg, t.border)
                        .tooltip(|w, cx| Tooltip::new("其它版本").build(w, cx)),
                )
                .child(
                    ui::round_button(
                        "detail-more",
                        IconName::Ellipsis,
                        30.,
                        t.glass,
                        t.fg,
                        t.border,
                    )
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.show_menu(
                            None,
                            vec![
                                MenuItem::new("在浏览器中打开"),
                                MenuItem::new("复制链接"),
                                MenuItem::new("刷新元数据").gap(),
                            ],
                            window,
                            cx,
                        )
                    })),
                )
                .into_any_element(),
            Page::Library(_) | Page::Media => {
                let (cur, desc) = self.sort;
                h_flex()
                    .id("sort")
                    .h(px(30.))
                    .px(px(10.))
                    .gap(px(4.))
                    .items_center()
                    .rounded_full()
                    .bg(t.glass)
                    .border_1()
                    .border_color(t.border)
                    .cursor_pointer()
                    .child(icon(IconName::ArrowDownUp, 15., t.fg))
                    .child(icon(IconName::ChevronDown, 12., t.fg2))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        let items = SORTS
                            .iter()
                            .enumerate()
                            .map(|(i, s)| {
                                let label = if i == cur {
                                    format!("{s} {}", if desc { "↓" } else { "↑" })
                                } else {
                                    s.to_string()
                                };
                                MenuItem::new(label).checked(i == cur).on(move |this, cx| {
                                    this.sort = if this.sort.0 == i {
                                        (i, !this.sort.1)
                                    } else {
                                        (i, true)
                                    };
                                    cx.notify();
                                })
                            })
                            .collect();
                        this.show_menu(Some("排序"), items, window, cx)
                    }))
                    .into_any_element()
            }
            Page::Servers => h_flex()
                .gap(px(8.))
                .child(
                    ui::round_button("add-server", IconName::Plus, 30., t.glass, t.fg, t.border)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.modal = Some(Modal::AddServer);
                            cx.notify();
                        })),
                )
                .child(
                    h_flex()
                        .h(px(30.))
                        .w(px(220.))
                        .px(px(10.))
                        .gap(px(6.))
                        .items_center()
                        .rounded_full()
                        .bg(t.surface)
                        .border_1()
                        .border_color(t.border)
                        .child(icon(IconName::Search, 14., t.fg3))
                        .child(text(12.5, t.fg3, "搜索")),
                )
                .into_any_element(),
            _ => h_flex()
                .gap(px(8.))
                .child(ui::round_button(
                    "refresh",
                    IconName::RefreshCw,
                    30.,
                    t.glass,
                    t.fg,
                    t.border,
                ))
                .child(
                    ui::round_button("more", IconName::Ellipsis, 30., t.glass, t.fg, t.border)
                        .on_click(cx.listener(|this, _, window, cx| {
                            let dark = this.dark;
                            this.show_menu(
                                None,
                                vec![
                                    MenuItem::new("媒体管理"),
                                    MenuItem::new("设置…").on(|this, cx| {
                                        this.modal = Some(Modal::Settings);
                                        cx.notify();
                                    }),
                                    MenuItem::new("深色外观")
                                        .checked(dark)
                                        .gap()
                                        .on(|this, cx| {
                                            this.dark = !this.dark;
                                            cx.notify();
                                        }),
                                ],
                                window,
                                cx,
                            )
                        })),
                )
                .into_any_element(),
        };
        TitleBar::new()
            .h(px(BAR_H))
            .bg(if solid {
                t.bg.opacity(0.94)
            } else {
                transparent_black()
            })
            .border_0()
            .when(solid && !on_detail, |d| {
                d.border_b_1().border_color(t.border)
            })
            .child(
                div()
                    .relative()
                    .w_full()
                    .h_full()
                    .child(
                        h_flex()
                            .h_full()
                            .pl(px(10.))
                            .pr(px(10.))
                            .gap(px(8.))
                            .items_center()
                            .child(
                                ghost_button(
                                    "toggle-sidebar",
                                    IconName::PanelLeft,
                                    if sidebar_shown { t.fg } else { t.fg2 },
                                    t,
                                )
                                .tooltip(|w, cx| {
                                    Tooltip::new("显示 / 隐藏侧栏 (Ctrl+B)").build(w, cx)
                                })
                                .on_click(cx.listener(
                                    |this, _, window, cx| this.toggle_sidebar(window, cx),
                                )),
                            )
                            .when(nested, |d| d.child(nav_pill))
                            .when(!title.is_empty(), |d| {
                                d.child(semibold(text(14., t.fg, title)))
                            })
                            .child(div().flex_1())
                            .child(right),
                    )
                    .when_some(center, |d, c| {
                        // centred on the window content, independent of the left / right groups
                        d.child(
                            h_flex()
                                .absolute()
                                .inset_0()
                                .justify_center()
                                .items_center()
                                .child(c),
                        )
                    }),
            )
    }

    fn menu_view(&self, menu: &Menu, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.chrome();
        let mut list = v_flex()
            .min_w(px(180.))
            .p(px(5.))
            .rounded(px(12.))
            .bg(t.raised)
            .border_1()
            .border_color(t.border)
            .shadow_lg()
            .when_some(menu.title.clone(), |d, s| {
                d.child(
                    medium(text(11.5, t.fg3, s))
                        .px(px(10.))
                        .pt(px(4.))
                        .pb(px(4.)),
                )
            });
        for (i, it) in menu.items.iter().enumerate() {
            if it.gap {
                list = list.child(div().h(px(1.)).mx(px(6.)).my(px(4.)).bg(t.border));
            }
            let action = it.action.clone();
            list = list.child(
                h_flex()
                    .id(("menu", i))
                    .h(px(28.))
                    .pl(px(6.))
                    .pr(px(14.))
                    .gap(px(6.))
                    .items_center()
                    .rounded(px(7.))
                    .cursor_pointer()
                    .hover(|s| s.bg(t.accent))
                    .child(
                        div()
                            .w(px(16.))
                            .when(it.checked, |d| d.child(icon(IconName::Check, 13., t.fg))),
                    )
                    .child(text(13., t.fg, it.label.clone()))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.menu = None;
                        if let Some(a) = &action {
                            a(this, cx);
                        }
                        cx.notify();
                    })),
            );
        }
        list
    }
}

impl Render for Gallery {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.t();
        let sidebar = self.sidebar_open(window);
        let page = match self.page.clone() {
            Page::Home => self.home(window, cx).into_any_element(),
            Page::Search => self.search(window, cx).into_any_element(),
            Page::Favorites => self.favorites(window, cx).into_any_element(),
            Page::Media => self.media(window, cx).into_any_element(),
            Page::Library(i) => self.library(i, window, cx).into_any_element(),
            Page::Detail(it) => self.detail(&it, window, cx).into_any_element(),
            Page::Servers => self.servers(window, cx).into_any_element(),
        };
        // The detail page sits on its backdrop, blurred, behind everything that scrolls
        let blurred = match &self.page {
            Page::Detail(it) => self.fx.blurred(it.backdrop.as_ref()),
            _ => None,
        };
        let menu = self
            .menu
            .as_ref()
            .map(|m| (m.pos, self.menu_view(m, cx).into_any_element()));
        let modal = self
            .modal
            .map(|m| self.modal_view(m, window, cx).into_any_element());
        h_flex()
            .relative()
            .size_full()
            .track_focus(&self.focus)
            .on_action(
                cx.listener(|this, _: &ToggleSidebar, window, cx| this.toggle_sidebar(window, cx)),
            )
            .on_action(cx.listener(|this, _: &CloseOverlay, _, cx| this.close_overlay(cx)))
            .bg(t.bg)
            .text_color(t.fg)
            .when(sidebar, |d| d.child(self.sidebar(cx)))
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .bg(t.bg)
                    .when_some(blurred, |d, p| {
                        d.child(
                            img(p)
                                .absolute()
                                .inset_0()
                                .size_full()
                                .object_fit(ObjectFit::Cover),
                        )
                        .child(div().absolute().inset_0().bg(ui::shade(0.62)))
                    })
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
                            .child(self.top_bar(window, cx)),
                    ),
            )
            .when_some(modal, |d, m| {
                d.child(deferred(
                    div()
                        .id("modal-scrim")
                        .absolute()
                        .inset_0()
                        .bg(ui::shade(0.45))
                        .flex()
                        .items_center()
                        .justify_center()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| {
                                this.modal = None;
                                cx.notify();
                            }),
                        )
                        .child(m),
                ))
            })
            .when_some(menu, |d, (pos, m)| {
                d.child(
                    deferred(
                        div()
                            .id("menu-scrim")
                            .absolute()
                            .inset_0()
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| {
                                    this.menu = None;
                                    cx.notify();
                                }),
                            )
                            .on_mouse_down(
                                MouseButton::Right,
                                cx.listener(|this, _, _, cx| {
                                    this.menu = None;
                                    cx.notify();
                                }),
                            ),
                    )
                    .priority(1),
                )
                .child(
                    deferred(
                        anchored()
                            .position(pos)
                            .snap_to_window_with_margin(px(8.))
                            .child(div().occlude().child(m)),
                    )
                    .priority(2),
                )
            })
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
            cx.bind_keys([
                KeyBinding::new("ctrl-b", ToggleSidebar, None),
                KeyBinding::new("escape", CloseOverlay, None),
            ]);
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
                let mut modal = None;
                let page = match opts.page.as_str() {
                    "search" => Page::Search,
                    "favorites" => Page::Favorites,
                    "media" => Page::Media,
                    "library" => Page::Library(0),
                    "series" => Page::Detail(Rc::new(fx.series.clone())),
                    "movie" => Page::Detail(Rc::new(fx.movie.clone())),
                    "servers" => Page::Servers,
                    "settings" => {
                        modal = Some(Modal::Settings);
                        Page::Home
                    }
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
                // DESIGN_SCROLL=<px> starts the page scrolled down, for screenshots of its lower part
                let scroll = ScrollHandle::new();
                if let Some(y) = std::env::var("DESIGN_SCROLL")
                    .ok()
                    .and_then(|s| s.parse::<f32>().ok())
                {
                    scroll.set_offset(point(px(0.), px(-y)));
                }
                let focus = cx.focus_handle();
                window.focus(&focus, cx);
                cx.new(|_| Gallery {
                    fx,
                    page,
                    back: Vec::new(),
                    forward: Vec::new(),
                    dark,
                    scroll,
                    rows: HashMap::new(),
                    season: 0,
                    hero: 0,
                    settings_tab: 0,
                    lib_seg: 0,
                    sort: (1, true),
                    searched: false,
                    server: 0,
                    sidebar: None,
                    menu: None,
                    modal,
                    focus,
                })
            })
            .expect("opening the design window");
        });
}
