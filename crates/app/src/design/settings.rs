//! Dialogs: settings (icon tabs on top, grouped rows) and adding a server.

use gpui_kit::assets::IconName;
use gpui_kit::component::{h_flex, switch::Switch, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::tokens::T;
use super::ui::*;
use super::{Gallery, Modal};

const TABS: [(IconName, &str); 7] = [
    (IconName::Settings, "通用"),
    (IconName::LibraryBig, "媒体库"),
    (IconName::CirclePlay, "播放"),
    (IconName::Captions, "字幕"),
    (IconName::MessageSquareText, "弹幕"),
    (IconName::HardDrive, "缓存"),
    (IconName::Info, "关于"),
];

/// One control on the right of a row.
enum Ctl {
    Switch(bool),
    /// A pop-up value (value, then the up/down chevrons).
    Pick(&'static str),
    Button(&'static str),
    Value(&'static str),
    Field(&'static str),
}

/// A grouped card of rows, with an optional heading above and a grey note below.
fn group(title: Option<&str>, rows: Vec<(&'static str, Ctl)>, note: Option<&str>, t: T) -> Div {
    let mut card = v_flex()
        .rounded(px(12.))
        .bg(t.surface)
        .border_1()
        .border_color(t.border);
    for (i, (label, ctl)) in rows.into_iter().enumerate() {
        let control = match ctl {
            Ctl::Switch(on) => Switch::new(SharedString::from(format!("sw-{label}")))
                .checked(on)
                .into_any_element(),
            Ctl::Pick(v) => h_flex()
                .gap(px(6.))
                .items_center()
                .child(text(13., t.fg2, v))
                .child(icon(IconName::ChevronsUpDown, 13., t.fg3))
                .into_any_element(),
            Ctl::Button(v) => div()
                .h(px(26.))
                .px(px(12.))
                .flex()
                .items_center()
                .rounded(px(7.))
                .bg(t.raised)
                .border_1()
                .border_color(t.border)
                .child(medium(text(12.5, t.fg, v)))
                .into_any_element(),
            Ctl::Value(v) => text(13., t.fg3, v).into_any_element(),
            Ctl::Field(v) => div()
                .w(px(260.))
                .h(px(28.))
                .px(px(10.))
                .flex()
                .items_center()
                .rounded(px(7.))
                .bg(t.bg)
                .border_1()
                .border_color(t.border)
                .child(text(12.5, t.fg3, v))
                .into_any_element(),
        };
        card = card.child(
            h_flex()
                .h(px(44.))
                .px(px(14.))
                .gap(px(16.))
                .items_center()
                .justify_between()
                .when(i > 0, |d| d.border_t_1().border_color(t.border))
                .child(text(13.5, t.fg, label))
                .child(control),
        );
    }
    v_flex()
        .gap(px(8.))
        .when_some(title, |d, s| {
            d.child(semibold(text(13.5, t.fg, s.to_string())).px(px(2.)))
        })
        .child(card)
        .when_some(note, |d, s| {
            d.child(
                text(12., t.fg3, s.to_string())
                    .px(px(2.))
                    .line_height(px(18.)),
            )
        })
}

impl Gallery {
    fn settings_body(&self, t: T, cx: &mut Context<Self>) -> Div {
        let dark = self.dark;
        let body = v_flex().gap(px(22.));
        match self.settings_tab {
            0 => body
                .child(
                    v_flex().gap(px(8.)).child(semibold(text(13.5, t.fg, "外观")).px(px(2.))).child(
                        h_flex()
                            .p(px(14.))
                            .rounded(px(12.))
                            .bg(t.surface)
                            .border_1()
                            .border_color(t.border)
                            .justify_between()
                            .items_center()
                            .child(text(13.5, t.fg, "主题"))
                            .child(segmented("theme", &["跟随系统", "浅色", "深色"], if dark { 2 } else { 1 }, t, {
                                let e = cx.entity();
                                move |i, window, cx| e.update(cx, |this, cx| this.set_dark(i != 1, window, cx))
                            })),
                    ),
                )
                .child(group(
                    Some("启动"),
                    vec![("启动时打开", Ctl::Pick("上次的服务器")), ("关闭窗口时", Ctl::Pick("退出程序"))],
                    None,
                    t,
                ))
                .child(group(
                    Some("更新"),
                    vec![("自动检查更新", Ctl::Switch(true)), ("更新代理", Ctl::Field("留空则直连"))],
                    Some("下载更新安装包时使用的代理，支持 http / socks5。"),
                    t,
                )),
            1 => body
                .child(group(
                    Some("首页"),
                    vec![
                        ("显示轮播图片", Ctl::Switch(true)),
                        ("显示我的媒体", Ctl::Switch(true)),
                        ("显示继续观看", Ctl::Switch(true)),
                        ("隐藏已观看项目", Ctl::Switch(false)),
                    ],
                    None,
                    t,
                ))
                .child(group(
                    Some("详情页与卡片"),
                    vec![
                        ("社区评分", Ctl::Switch(true)),
                        ("已播放标记", Ctl::Switch(true)),
                        ("剩余集数", Ctl::Switch(true)),
                        ("演职人员", Ctl::Switch(true)),
                        ("艺术图", Ctl::Switch(true)),
                        ("类似作品", Ctl::Switch(true)),
                        ("媒体信息", Ctl::Switch(true)),
                    ],
                    None,
                    t,
                ))
                .child(group(None, vec![("视频首选版本", Ctl::Pick("最佳分辨率"))], Some("一个条目有多个版本时，默认播放哪一个。"), t)),
            2 => body
                .child(group(
                    None,
                    vec![("播放器", Ctl::Field("自带 mpv")), ("从上次进度播放", Ctl::Switch(true)), ("记住音量", Ctl::Switch(true))],
                    Some("播放器留空使用安装包自带的 mpv；个人 mpv.conf 放在数据目录里，升级不会覆盖。"),
                    t,
                ))
                .child(group(None, vec![("预加载下一集", Ctl::Pick("90% 进度开始"))], Some("播放到这个进度时开始缓冲下一集，切集不用等。"), t))
                .child(group(None, vec![("整片缓存", Ctl::Switch(false))], Some("边播边把整部影片存到本地，来回拖动不用再等；关闭时只缓存播放位置前方。"), t)),
            3 => body
                .child(group(
                    None,
                    vec![("记住所选字幕", Ctl::Switch(true)), ("字幕首选语言", Ctl::Pick("简体中文")), ("加载外挂字幕", Ctl::Switch(true))],
                    None,
                    t,
                ))
                .child(group(None, vec![("换集沿用音轨与字幕", Ctl::Switch(true))], Some("下一集按语言和标题找回同一条音轨和字幕。"), t)),
            4 => body
                .child(group(None, vec![("启用弹幕", Ctl::Switch(true))], Some("关闭后不显示弹幕按钮，也不加载弹幕。"), t))
                .child(group(
                    Some("弹幕源"),
                    vec![("接口地址", Ctl::Field("https://api.example.com")), ("屏蔽词", Ctl::Value("3 个"))],
                    Some("字号、透明度、速度等显示设置在播放器的弹幕面板里调，实时生效。"),
                    t,
                )),
            5 => body
                .child(group(
                    None,
                    vec![
                        ("视频缓存", Ctl::Button("清理")),
                        ("图片缓存", Ctl::Button("清理")),
                        ("字幕与弹幕缓存", Ctl::Button("清理")),
                        ("媒体库缓存", Ctl::Button("清理")),
                        ("临时缓存", Ctl::Button("清理")),
                    ],
                    None,
                    t,
                ))
                .child(group(None, vec![("缓存大小", Ctl::Value("2.31 GB")), ("一键清理", Ctl::Button("清理全部"))], None, t)),
            _ => body
                .child(
                    v_flex()
                        .py(px(18.))
                        .items_center()
                        .gap(px(8.))
                        .child(
                            div()
                                .size(px(56.))
                                .rounded(px(14.))
                                .bg(t.fg)
                                .flex()
                                .items_center()
                                .justify_center()
                                .child(icon(IconName::Play, 26., t.bg).ml(px(3.))),
                        )
                        .child(semibold(text(16., t.fg, "EmbyMini")))
                        .child(text(12.5, t.fg2, "版本 0.6.0")),
                )
                .child(group(None, vec![("检查更新", Ctl::Button("检查")), ("日志", Ctl::Button("打开文件夹")), ("数据目录", Ctl::Button("打开文件夹"))], None, t)),
        }
    }

    pub(super) fn modal_view(
        &self,
        m: Modal,
        _window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let t = self.chrome();
        let dialog = v_flex()
            .id("dialog")
            .rounded(px(16.))
            .bg(if t.dark {
                rgb(0x141414).into()
            } else {
                t.raised
            })
            .border_1()
            .border_color(t.border)
            .shadow_lg()
            .overflow_hidden()
            .occlude()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation());
        match m {
            Modal::Settings => {
                let cur = self.settings_tab;
                let tabs =
                    h_flex()
                        .justify_center()
                        .gap(px(4.))
                        .children(TABS.iter().enumerate().map(|(i, (ic, label))| {
                            let on = i == cur;
                            v_flex()
                                .id(("tab", i))
                                .w(px(64.))
                                .py(px(6.))
                                .gap(px(4.))
                                .items_center()
                                .rounded(px(10.))
                                .cursor_pointer()
                                .when(on, |d| d.bg(t.accent))
                                .when(!on, |d| d.hover(|s| s.bg(t.hover)))
                                .child(icon(*ic, 20., if on { t.fg } else { t.fg2 }))
                                .child(
                                    text(11.5, if on { t.fg } else { t.fg2 }, label.to_string())
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
                dialog
                    .w(px(720.))
                    .h(px(560.))
                    .child(
                        v_flex()
                            .pt(px(14.))
                            .pb(px(10.))
                            .gap(px(10.))
                            .border_b_1()
                            .border_color(t.border)
                            .child(
                                h_flex()
                                    .px(px(14.))
                                    .justify_between()
                                    .items_center()
                                    .child(div().w(px(28.)))
                                    .child(semibold(text(
                                        13.5,
                                        t.fg,
                                        format!("{}设置", TABS[cur].1),
                                    )))
                                    .child(
                                        ghost_button("close-settings", IconName::X, t.fg2, t)
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.modal = None;
                                                cx.notify();
                                            })),
                                    ),
                            )
                            .child(tabs),
                    )
                    .child(
                        div()
                            .id("settings-scroll")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .child(
                                div()
                                    .px(px(28.))
                                    .py(px(22.))
                                    .child(self.settings_body(t, cx)),
                            ),
                    )
                    .into_any_element()
            }
            Modal::AddServer => {
                let field = |label: &'static str, hint: &'static str, last: bool| {
                    h_flex()
                        .h(px(40.))
                        .px(px(14.))
                        .gap(px(12.))
                        .items_center()
                        .when(!last, |d| d.border_b_1().border_color(t.border))
                        .child(text(13., t.fg, label).w(px(64.)))
                        .child(text(13., t.fg3, hint).flex_1())
                };
                let section = |title: Option<&'static str>, rows: Vec<Div>| {
                    v_flex()
                        .gap(px(8.))
                        .when_some(title, |d, s| {
                            d.child(semibold(text(12.5, t.fg2, s)).px(px(2.)))
                        })
                        .child(
                            v_flex()
                                .rounded(px(12.))
                                .bg(t.surface)
                                .border_1()
                                .border_color(t.border)
                                .children(rows),
                        )
                };
                dialog
                    .w(px(480.))
                    .child(
                        h_flex()
                            .h(px(48.))
                            .px(px(18.))
                            .items_center()
                            .border_b_1()
                            .border_color(t.border)
                            .child(semibold(text(14., t.fg, "添加 Emby 服务器"))),
                    )
                    .child(
                        v_flex()
                            .p(px(18.))
                            .gap(px(16.))
                            .child(section(
                                None,
                                vec![field("名称", "可选，默认用服务器上的名称", true)],
                            ))
                            .child(section(
                                Some("服务器地址"),
                                vec![
                                    field("地址", "https://media.example.com 或 IP:端口", false),
                                    field("路径", "可选，例如 /emby", true),
                                ],
                            ))
                            .child(section(
                                Some("登录信息"),
                                vec![
                                    field("用户名", "必填", false),
                                    field("密码", "可留空", true).child(icon(
                                        IconName::Eye,
                                        15.,
                                        t.fg3,
                                    )),
                                ],
                            )),
                    )
                    .child(
                        h_flex()
                            .px(px(18.))
                            .py(px(14.))
                            .gap(px(8.))
                            .justify_end()
                            .border_t_1()
                            .border_color(t.border)
                            .child(
                                div()
                                    .id("cancel")
                                    .h(px(32.))
                                    .px(px(16.))
                                    .flex()
                                    .items_center()
                                    .rounded(px(8.))
                                    .border_1()
                                    .border_color(t.border)
                                    .cursor_pointer()
                                    .hover(|s| s.bg(t.hover))
                                    .child(medium(text(13., t.fg, "取消")))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.modal = None;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                div()
                                    .id("save")
                                    .h(px(32.))
                                    .px(px(16.))
                                    .flex()
                                    .items_center()
                                    .rounded(px(8.))
                                    .bg(t.primary)
                                    .cursor_pointer()
                                    .hover(|s| s.opacity(0.88))
                                    .child(medium(text(13., t.on_primary, "登录并保存"))),
                            ),
                    )
                    .into_any_element()
            }
        }
    }
}
