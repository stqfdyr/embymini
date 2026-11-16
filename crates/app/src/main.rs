//! EmbyMini on GPUI (spike): one window with a custom title bar and the home rows.
//!
//! `--fake` draws generated posters (no server); otherwise the server comes from
//! EMBY_SERVER / PORT / EMBY_USER / EMBY_PASS. `--bench <file.json>` loads every image, scrolls the page for a
//! few seconds, writes timings and memory to the file and exits.

mod data;
mod design;
mod home;
mod poster;

use std::{path::PathBuf, sync::OnceLock, time::Instant};

use gpui_kit::component::{Theme, TitleBar};
use gpui_kit::*;

/// Process start, the zero point of every timing in the bench report.
static STARTED: OnceLock<Instant> = OnceLock::new();
/// Network and decoding run on tokio; GPUI tasks await its join handles.
static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

fn rt() -> &'static tokio::runtime::Runtime {
    RUNTIME.get().expect("runtime is set in main")
}

fn since_start_ms() -> f64 {
    STARTED
        .get()
        .map_or(0.0, |t| t.elapsed().as_secs_f64() * 1000.0)
}

fn main() {
    STARTED.set(Instant::now()).ok();
    let mut fake = false;
    let mut bench = None;
    let mut design: Option<PathBuf> = None;
    let mut page = String::new();
    let mut light = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--fake" => fake = true,
            "--bench" => bench = args.next().map(PathBuf::from),
            "--design" => design = args.next().map(PathBuf::from),
            "--page" => page = args.next().unwrap_or_default(),
            "--light" => light = true,
            other => eprintln!("unknown argument {other}"),
        }
    }
    if let Some(dir) = design {
        design::run(design::Options { dir, page, light });
        return;
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("starting tokio");
    RUNTIME.set(runtime).ok();
    let source = if fake {
        data::Source::Fake
    } else {
        data::Source::from_env().unwrap_or_else(|e| {
            eprintln!("{e}; falling back to --fake");
            data::Source::Fake
        })
    };

    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(1440.), px(900.)),
                    cx,
                ))),
                window_min_size: Some(size(px(960.), px(600.))),
                ..TitleBar::window_options()
            };
            gpui_kit::open_window(options, cx, |window, cx| {
                Theme::sync_system_appearance(Some(window), cx);
                cx.new(|cx| home::Home::new(source, bench, window, cx))
            })
            .expect("opening the main window");
        });
}
