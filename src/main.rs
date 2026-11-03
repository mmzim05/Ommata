mod app;
mod library;
mod picture;
mod preview;
mod settings;
mod theme;
mod trim;
mod video;
mod waveform;

use eframe::egui;

fn main() -> eframe::Result {
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    if args.first().is_some_and(|a| a == "--thumbnail") {
        std::process::exit(thumbnail(&args[1..]));
    }
    let arg = args.first().map(std::path::PathBuf::from);
    if let Err(e) = ffmpeg_next::init() {
        eprintln!("anyview: ffmpeg init failed: {e}");
    }
    ffmpeg_next::util::log::set_level(ffmpeg_next::util::log::Level::Error);

    // Start decoding the first image before the window and GL context exist.
    let loader = app::Loader::new();
    let start = arg.map(|p| app::resolve_start(&p, &loader));

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
        Box::new(move |cc| Ok(Box::new(app::App::new(cc, loader, start)))),
    )
}

/// `anyview --thumbnail INPUT OUTPUT SIZE`, called by file managers through
/// the freedesktop thumbnailer spec (see assets/anyview.thumbnailer).
fn thumbnail(args: &[std::ffi::OsString]) -> i32 {
    let [input, output, size] = args else {
        eprintln!("usage: anyview --thumbnail INPUT OUTPUT SIZE");
        return 2;
    };
    let size = size.to_str().and_then(|s| s.parse().ok()).unwrap_or(256);
    match picture::thumbnail(input.as_ref(), output.as_ref(), size) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("anyview: thumbnail: {e}");
            1
        }
    }
}
