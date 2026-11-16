//! Home page: title bar + rows of posters. In bench mode it also measures loading and scrolling.

use std::{path::PathBuf, sync::atomic::Ordering, time::Duration};

use gpui_kit::base::{Selectable as _, StyledExt as _};
use gpui_kit::component::{
    ActiveTheme as _, Sizable as _, TitleBar,
    button::{Button, ButtonVariants as _},
    h_flex, v_flex,
};
use gpui_kit::*;

use crate::{
    data::{self, Card, Kind, Row},
    poster,
};

/// When the rows were ready on the tokio side (f64 bits), to tell a slow load from a slow hand-over to the UI.
static LOADED_AT_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// When the UI-side load task first ran (f64 bits): a late start means the UI thread was busy.
static TASK_START_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

enum State {
    Loading,
    Ready(Vec<Row>),
    Failed(SharedString),
}

pub struct Home {
    state: State,
    scroll: ScrollHandle,
    bench: Option<Bench>,
    _load: Task<()>,
}

impl Home {
    pub fn new(
        source: data::Source,
        bench: Option<PathBuf>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let load = cx.spawn(async move |this, cx| {
            TASK_START_MS.store(crate::since_start_ms().to_bits(), Ordering::Relaxed);
            let r = crate::rt()
                .spawn(async move {
                    let r = data::load(source).await;
                    (r, crate::since_start_ms())
                })
                .await
                .map(|(r, at)| {
                    LOADED_AT_MS.store(at.to_bits(), Ordering::Relaxed);
                    r
                });
            let state = match r {
                Ok(Ok(rows)) => State::Ready(rows),
                Ok(Err(e)) => State::Failed(format!("{e:#}").into()),
                Err(e) => State::Failed(e.to_string().into()),
            };
            this.update(cx, |home, cx| {
                if let Some(b) = &mut home.bench {
                    b.data_ms = crate::since_start_ms();
                }
                home.state = state;
                cx.notify();
            })
            .ok();
        });
        let bench = bench.map(|out| {
            // Wake the view while images load (often enough to time them, rarely enough not to keep the UI
            // thread busy); the scroll phase then drives itself frame by frame.
            let poll = cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor()
                        .timer(Duration::from_millis(100))
                        .await;
                    if this.update(cx, |_, cx| cx.notify()).is_err() {
                        break;
                    }
                }
            });
            Bench::new(out, poll)
        });
        Self {
            state: State::Loading,
            scroll: ScrollHandle::new(),
            bench,
            _load: load,
        }
    }
}

impl Render for Home {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(b) = &mut self.bench {
            let total = match &self.state {
                // GPUI loads each (key, size) once, so a backdrop shared by two cards in a row counts once
                State::Ready(rows) => rows
                    .iter()
                    .flat_map(|r| r.cards.iter().map(move |c| (r.kind, c)))
                    .filter_map(|(kind, c)| Some((kind, c.image.as_ref()?)))
                    .collect::<std::collections::HashSet<_>>()
                    .len(),
                // Nothing to wait for: go straight to the scroll phase and write the report
                State::Failed(_) => 0,
                State::Loading => usize::MAX,
            };
            if b.step(total, &self.scroll, window) {
                cx.quit();
            }
        }
        let theme = cx.theme();
        let body = match &self.state {
            State::Loading => div()
                .p_5()
                .text_color(theme.muted_foreground)
                .child("加载中…")
                .into_any_element(),
            State::Failed(e) => div()
                .p_5()
                .text_color(theme.danger)
                .child(e.clone())
                .into_any_element(),
            State::Ready(rows) => v_flex()
                .p_5()
                .gap_6()
                .children(rows.iter().map(|r| row(r, window, cx)))
                .into_any_element(),
        };
        v_flex()
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(title_bar())
            .child(
                div()
                    .id("home")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .child(body),
            )
    }
}

fn title_bar() -> TitleBar {
    TitleBar::new().child(
        h_flex()
            .w_full()
            .pr_2()
            .gap_4()
            .child(div().font_semibold().child("EmbyMini"))
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Button::new("tab-media")
                            .label("媒体")
                            .small()
                            .ghost()
                            .selected(true),
                    )
                    .child(Button::new("tab-search").label("搜索").small().ghost())
                    .child(Button::new("tab-favorites").label("收藏").small().ghost()),
            ),
    )
}

fn row(r: &Row, window: &Window, cx: &App) -> impl IntoElement {
    v_flex()
        .gap_2()
        .child(div().text_lg().font_semibold().child(r.title.clone()))
        .child(
            div().id(r.id.clone()).overflow_x_scroll().child(
                h_flex()
                    .gap_3()
                    .pb_2()
                    .children(r.cards.iter().map(|c| card(c, r.kind, window, cx))),
            ),
        )
}

fn card(c: &Card, kind: Kind, window: &Window, cx: &App) -> impl IntoElement {
    let (w, h) = match kind {
        Kind::Poster => (160.0, 240.0),
        Kind::Wide => (288.0, 162.0),
    };
    let theme = cx.theme();
    let art = div()
        .w(px(w))
        .h(px(h))
        .rounded(theme.radius)
        .overflow_hidden()
        .bg(theme.muted);
    let art = match &c.image {
        Some(key) => art.child(
            img(poster::source(key.clone(), w, h, window))
                .size_full()
                .rounded(theme.radius)
                .object_fit(ObjectFit::Cover),
        ),
        None => art,
    };
    v_flex()
        .id(c.id.clone())
        .w(px(w))
        .gap_1()
        .flex_shrink_0()
        .cursor_pointer()
        .child(art)
        .child(div().text_sm().truncate().child(c.title.clone()))
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .truncate()
                .child(c.sub.clone()),
        )
}

/// Bench: time to first frame, to data, to every image; then scroll down and back for a fixed time and record
/// the interval between frames.
struct Bench {
    out: PathBuf,
    first_frame_ms: Option<f64>,
    /// Start time of the first renders, to spot a UI thread stalled between them
    early_frames_ms: Vec<f64>,
    data_ms: f64,
    images_ms: Option<f64>,
    mem_loaded_mb: f64,
    scroll_start: Option<std::time::Instant>,
    last_frame: Option<std::time::Instant>,
    intervals: Vec<f64>,
    down: bool,
    _poll: Option<Task<()>>,
}

/// Scroll speed in logical pixels per frame and how long the scroll phase lasts.
const SCROLL_STEP: f32 = 12.0;
const SCROLL_FOR: Duration = Duration::from_secs(8);
/// Give up waiting for data and images after this long (a slow server should not hang the bench).
const IMAGES_TIMEOUT_MS: f64 = 60_000.0;

impl Bench {
    fn new(out: PathBuf, poll: Task<()>) -> Self {
        Self {
            out,
            first_frame_ms: None,
            early_frames_ms: Vec::new(),
            data_ms: 0.0,
            images_ms: None,
            mem_loaded_mb: 0.0,
            scroll_start: None,
            last_frame: None,
            intervals: Vec::new(),
            down: true,
            _poll: Some(poll),
        }
    }

    /// One frame; true when the bench is done and the app should quit.
    fn step(&mut self, total: usize, scroll: &ScrollHandle, window: &mut Window) -> bool {
        let now_ms = crate::since_start_ms();
        self.first_frame_ms.get_or_insert(now_ms);
        if self.early_frames_ms.len() < 8 {
            self.early_frames_ms.push((now_ms * 10.0).round() / 10.0);
        }
        if self.images_ms.is_none() {
            let done =
                poster::LOADED.load(Ordering::Relaxed) + poster::FAILED.load(Ordering::Relaxed);
            if done >= total || now_ms > IMAGES_TIMEOUT_MS {
                self.images_ms = Some(now_ms);
                self.mem_loaded_mb = memory_mb();
                self._poll = None; // from here on every frame asks for the next one
                window.request_animation_frame();
            }
            return false;
        }
        let now = std::time::Instant::now();
        let start = *self.scroll_start.get_or_insert(now);
        if let Some(last) = self.last_frame {
            self.intervals.push((now - last).as_secs_f64() * 1000.0);
        }
        self.last_frame = Some(now);
        if now - start >= SCROLL_FOR {
            self.write();
            return true;
        }
        let max = scroll.max_offset().y;
        let y = -scroll.offset().y;
        let next = if self.down {
            y + px(SCROLL_STEP)
        } else {
            y - px(SCROLL_STEP)
        };
        if next >= max {
            self.down = false;
        } else if next <= px(0.) {
            self.down = true;
        }
        scroll.set_offset(point(px(0.), -next.clamp(px(0.), max)));
        window.request_animation_frame();
        false
    }

    fn write(&self) {
        let mut v = self.intervals.clone();
        v.sort_by(f64::total_cmp);
        let pct = |p: f64| {
            v.get(((v.len() as f64 - 1.0) * p).round() as usize)
                .copied()
                .unwrap_or(0.0)
        };
        let report = serde_json::json!({
            "first_frame_ms": self.first_frame_ms,
            "early_frames_ms": self.early_frames_ms,
            "task_start_ms": f64::from_bits(TASK_START_MS.load(Ordering::Relaxed)),
            "data_ready_ms": f64::from_bits(LOADED_AT_MS.load(Ordering::Relaxed)),
            "data_ms": self.data_ms,
            "images_ms": self.images_ms,
            "images_loaded": poster::LOADED.load(Ordering::Relaxed),
            "images_failed": poster::FAILED.load(Ordering::Relaxed),
            "frames": v.len(),
            "frame_ms_p50": pct(0.5),
            "frame_ms_p95": pct(0.95),
            "frame_ms_p99": pct(0.99),
            "frame_ms_max": v.last().copied().unwrap_or(0.0),
            "frames_over_33ms": v.iter().filter(|&&x| x > 33.4).count(),
            "mem_mb_images_loaded": self.mem_loaded_mb,
            "mem_mb_end": memory_mb(),
        });
        let text = serde_json::to_string_pretty(&report).unwrap_or_default();
        eprintln!("{text}");
        if let Err(e) = std::fs::write(&self.out, text) {
            eprintln!("writing {}: {e}", self.out.display());
        }
    }
}

/// Resident memory of this process (working set on Windows).
fn memory_mb() -> f64 {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};
    let Ok(pid) = sysinfo::get_current_pid() else {
        return 0.0;
    };
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing().with_memory(),
    );
    sys.process(pid)
        .map_or(0.0, |p| p.memory() as f64 / 1_048_576.0)
}
