mod app;
mod gpu;
mod library;
mod picture;
mod preview;
mod settings;
mod theme;
mod trim;
mod video;
mod waveform;

use eframe::egui;
use std::sync::{Mutex, MutexGuard, PoisonError};

fn main() -> eframe::Result {
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    if args.first().is_some_and(|arg| arg == "--thumbnail") {
        std::process::exit(thumbnail(&args[1..]));
    }
    let start_path = args.first().map(std::path::PathBuf::from);
    if let Err(error) = ffmpeg_next::init() {
        eprintln!("anyview: ffmpeg init failed: {error}");
    }
    ffmpeg_next::util::log::set_level(ffmpeg_next::util::log::Level::Error);

    // Start decoding the first image before the window and GL context exist.
    let loader = app::Loader::new();
    let start = start_path.map(|path| app::resolve_start(&path, &loader));

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_app_id("anyview")
            .with_title("anyview")
            .with_inner_size([1100.0, 720.0])
            .with_min_inner_size([360.0, 240.0])
            .with_drag_and_drop(true),
        multisampling: 0,
        depth_buffer: 0,
        stencil_buffer: 0,
        ..Default::default()
    };
    eframe::run_native(
        "anyview",
        options,
        Box::new(move |creation| Ok(Box::new(app::App::new(creation, loader, start)))),
    )
}

/// `anyview --thumbnail INPUT OUTPUT SIZE`, called by file managers through
/// the freedesktop thumbnailer spec (see assets/anyview.thumbnailer).
fn thumbnail(args: &[std::ffi::OsString]) -> i32 {
    let [input, output, size] = args else {
        eprintln!("usage: anyview --thumbnail INPUT OUTPUT SIZE");
        return 2;
    };
    let size = size
        .to_str()
        .and_then(|text| text.parse().ok())
        .unwrap_or(256);
    match picture::thumbnail(input.as_ref(), output.as_ref(), size) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("anyview: thumbnail: {error}");
            1
        }
    }
}

/// Lock `mutex`, recovering the data if another thread panicked while holding
/// it: every shared value here stays consistent between statements, so a
/// panic elsewhere never leaves it half-written.
pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
