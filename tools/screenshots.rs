//! Screenshots of Ommata's pages, drawn by the real app on a headless OpenGL
//! context, for checking layouts without a window:
//!
//!     OMMATA_SHOTS=dir cargo test --release page_screenshots -- --ignored
//!
//! A tool, not part of the app: it is compiled only for tests, as a child of
//! `src/app.rs` so it can open pages directly.

use super::*;
use eframe::egui_glow;
use eframe::glow::{self, HasContext};

fn shoot(
    app: &mut App,
    ctx: &egui::Context,
    painter: &mut egui_glow::Painter,
    gl: &glow::Context,
    size: [u32; 2],
    path: &Path,
) {
    let mut frame = eframe::Frame::_new_kittest();
    let mut output = None;
    // A few frames: layouts measure themselves on the first.
    for _ in 0..6 {
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(
                Pos2::ZERO,
                vec2(size[0] as f32, size[1] as f32),
            )),
            ..Default::default()
        };
        let mut full = ctx.run_ui(input, |ui| eframe::App::ui(app, ui, &mut frame));
        if let Some(previous) = output.take() {
            let previous: egui::TexturesDelta = previous;
            let mut delta = previous;
            delta.append(std::mem::take(&mut full.textures_delta));
            full.textures_delta = delta;
        }
        let primitives = ctx.tessellate(std::mem::take(&mut full.shapes), full.pixels_per_point);
        // SAFETY: test-only drawing into a framebuffer this test owns.
        unsafe {
            let fbo = gl.create_framebuffer().unwrap();
            let rb = gl.create_renderbuffer().unwrap();
            gl.bind_renderbuffer(glow::RENDERBUFFER, Some(rb));
            gl.renderbuffer_storage(
                glow::RENDERBUFFER,
                glow::RGBA8,
                size[0] as i32,
                size[1] as i32,
            );
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
            gl.framebuffer_renderbuffer(
                glow::FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::RENDERBUFFER,
                Some(rb),
            );
            gl.viewport(0, 0, size[0] as i32, size[1] as i32);
            gl.clear_color(0.0, 0.0, 0.0, 1.0);
            gl.clear(glow::COLOR_BUFFER_BIT);
            painter.paint_and_update_textures(
                size,
                full.pixels_per_point,
                &primitives,
                &mut full.textures_delta,
            );
            let mut pixels = vec![0u8; (size[0] * size[1] * 4) as usize];
            gl.read_pixels(
                0,
                0,
                size[0] as i32,
                size[1] as i32,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelPackData::Slice(Some(&mut pixels)),
            );
            let mut flipped = Vec::with_capacity(pixels.len());
            for row in pixels.chunks((size[0] * 4) as usize).rev() {
                flipped.extend_from_slice(row);
            }
            let image = image::RgbaImage::from_raw(size[0], size[1], flipped).unwrap();
            image.save(path).unwrap();
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            gl.delete_framebuffer(fbo);
            gl.delete_renderbuffer(rb);
        }
    }
}

#[test]
#[ignore]
fn page_screenshots() {
    let dir = PathBuf::from(std::env::var("OMMATA_SHOTS").unwrap());
    let (_context, gl) = crate::gpu::headless::context();
    let mut painter = egui_glow::Painter::new(gl.clone(), "", None, false).unwrap();
    let ctx = egui::Context::default();
    let creation = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = App::new(&creation, Loader::new(), None);
    // With a file open (OMMATA_SHOT_FILE), the top and tag bars show too.
    if let Some(file) = std::env::var_os("OMMATA_SHOT_FILE") {
        app.open_path(&ctx, Path::new(&file));
        app.tag_bar = true;
    }
    for size in [[1261, 1390], [1280, 720]] {
        for tab in [SettingsTab::General, SettingsTab::Tags, SettingsTab::Video] {
            app.page = Some(Page::Settings(tab));
            let name = format!("settings-{tab:?}-{}x{}.png", size[0], size[1]);
            shoot(&mut app, &ctx, &mut painter, &gl, size, &dir.join(name));
        }
        for group in ["Browse", "Tags"] {
            ctx.data_mut(|data| data.insert_temp(Id::unique("keybind group"), group));
            app.page = Some(Page::Settings(SettingsTab::Keybinds));
            let name = format!("settings-Keybinds-{group}-{}x{}.png", size[0], size[1]);
            shoot(&mut app, &ctx, &mut painter, &gl, size, &dir.join(name));
        }
    }
    for (query, name) in [("", "all"), ("lab", "lab"), ("ctrl f", "ctrl-f")] {
        app.help_search = query.into();
        app.page = Some(Page::Help(HelpTab::Keys));
        let path = dir.join(format!("help-{name}.png"));
        shoot(&mut app, &ctx, &mut painter, &gl, [1280, 720], &path);
    }
    // The player's bottom bar, once the file and its waveform have loaded.
    if std::env::var_os("OMMATA_SHOT_FILE").is_some() {
        app.page = None;
        app.tag_bar = false;
        app.settings.waveform = true;
        for _ in 0..12 {
            std::thread::sleep(std::time::Duration::from_millis(500));
            shoot(
                &mut app,
                &ctx,
                &mut painter,
                &gl,
                [1280, 720],
                &dir.join("player.png"),
            );
        }
        // A video turned a quarter clockwise, as the rotate button does.
        if app.current_kind() == Some(Kind::Video) {
            app.rotate(true);
            let path = dir.join("player-turned.png");
            shoot(&mut app, &ctx, &mut painter, &gl, [1280, 720], &path);
        }
    }
}
