//! Colors from the active Omarchy theme, falling back to egui's system-following
//! default. The theme file is watched so `omarchy theme set` restyles a running app.

use eframe::egui::{self, Color32, Visuals};
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

/// UI colors, from the Omarchy theme or egui's defaults.
#[derive(Clone, Copy, PartialEq)]
pub struct Palette {
    pub dark: bool,
    pub bg: Color32,
    pub surface: Color32,
    pub fg: Color32,
    pub muted: Color32,
    pub accent: Color32,
    pub selection: Color32,
    pub danger: Color32,
}

impl Palette {
    fn fallback(dark: bool) -> Self {
        let visuals = if dark {
            Visuals::dark()
        } else {
            Visuals::light()
        };
        Self {
            dark,
            bg: visuals.panel_fill,
            surface: visuals.window_fill,
            fg: visuals.text_color(),
            muted: visuals.weak_text_color(),
            accent: visuals.selection.stroke.color,
            selection: visuals.selection.bg_fill,
            danger: visuals.error_fg_color,
        }
    }
}

fn state_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state"))
        })?;
    Some(base.join("omarchy/current"))
}

/// `omarchy theme set` swaps the theme directory, then rewrites this file last.
fn stamp_path() -> Option<PathBuf> {
    state_dir().map(|dir| dir.join("theme.name"))
}

fn parse_hex(text: &str) -> Option<Color32> {
    let hex = text.trim().trim_start_matches('#');
    let rgb = u32::from_str_radix(hex.get(..6)?, 16).ok()?;
    Some(Color32::from_rgb(
        (rgb >> 16) as u8,
        (rgb >> 8) as u8,
        rgb as u8,
    ))
}

/// Reads the flat `key = "value"` lines of Omarchy's colors.toml.
fn load_omarchy() -> Option<Palette> {
    let text = std::fs::read_to_string(state_dir()?.join("theme/colors.toml")).ok()?;
    let get = |key: &str| {
        text.lines().find_map(|line| {
            let (name, value) = line.split_once('=')?;
            (name.trim() == key).then(|| value.trim().trim_matches('"').to_string())
        })
    };
    let color = |key: &str| get(key).and_then(|value| parse_hex(&value));
    let dark = get("mode").is_none_or(|mode| mode != "light");
    let bg = color("background")?;
    let fg = color("foreground")?;
    let base = Palette::fallback(dark);
    let surface = color("lighter_background")
        .filter(|surface| *surface != bg)
        .unwrap_or_else(|| lerp(bg, fg, 0.08));
    Some(Palette {
        dark,
        bg,
        surface,
        fg,
        muted: color("dark_foreground")
            .or(color("muted"))
            .unwrap_or(base.muted),
        accent: color("accent").unwrap_or(base.accent),
        selection: color("selection").unwrap_or(base.selection),
        danger: color("red").unwrap_or(base.danger),
    })
}

/// Blend from `a` (t = 0) to `b` (t = 1).
pub fn lerp(a: Color32, b: Color32, t: f32) -> Color32 {
    let mix = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(mix(a.r(), b.r()), mix(a.g(), b.g()), mix(a.b(), b.b()))
}

/// The active palette, reloaded when the Omarchy theme or system mode changes.
pub struct Theme {
    pub palette: Palette,
    omarchy: bool,
    stamp: Option<SystemTime>,
    last_check: f64,
}

impl Theme {
    /// Load the current theme and apply it to `ctx`.
    pub fn new(ctx: &egui::Context) -> Self {
        let mut theme = Self {
            palette: Palette::fallback(true),
            omarchy: false,
            stamp: None,
            last_check: 0.0,
        };
        theme.reload(ctx);
        theme
    }

    fn reload(&mut self, ctx: &egui::Context) {
        self.stamp = stamp_path().and_then(|file| file.metadata().ok()?.modified().ok());
        match load_omarchy() {
            Some(palette) => {
                self.omarchy = true;
                self.palette = palette;
            }
            None => {
                self.omarchy = false;
                self.palette = Palette::fallback(ctx.theme() == egui::Theme::Dark);
            }
        }
        apply(ctx, &self.palette);
    }

    /// Cheap per-frame check: one stat() at most every 2 seconds, piggybacking on
    /// repaints that happen anyway, so an idle window stays fully idle.
    pub fn poll(&mut self, ctx: &egui::Context) {
        let now = ctx.input(|input| input.time);
        if now - self.last_check < 2.0 {
            return;
        }
        self.last_check = now;
        let stamp = stamp_path().and_then(|file| file.metadata().ok()?.modified().ok());
        let system_dark = ctx.theme() == egui::Theme::Dark;
        if stamp != self.stamp || (!self.omarchy && system_dark != self.palette.dark) {
            self.reload(ctx);
        }
        ctx.request_repaint_after(Duration::from_secs(2));
    }
}

fn apply(ctx: &egui::Context, palette: &Palette) {
    let mut visuals = if palette.dark {
        Visuals::dark()
    } else {
        Visuals::light()
    };
    visuals.override_text_color = Some(palette.fg);
    visuals.panel_fill = palette.bg;
    visuals.window_fill = palette.surface;
    visuals.extreme_bg_color = palette.bg;
    visuals.faint_bg_color = palette.surface;
    visuals.window_stroke = egui::Stroke::new(1.0, lerp(palette.surface, palette.fg, 0.12));
    visuals.selection.bg_fill = palette.selection;
    visuals.selection.stroke = egui::Stroke::new(1.0, palette.accent);
    visuals.hyperlink_color = palette.accent;
    visuals.error_fg_color = palette.danger;
    let radius = egui::CornerRadius::same(8);
    visuals.window_corner_radius = egui::CornerRadius::same(12);
    let widgets = &mut visuals.widgets;
    for (state, shade) in [
        (&mut widgets.noninteractive, 0.0),
        (&mut widgets.inactive, 0.06),
        (&mut widgets.hovered, 0.14),
        (&mut widgets.active, 0.22),
        (&mut widgets.open, 0.10),
    ] {
        state.bg_fill = lerp(palette.surface, palette.fg, shade);
        state.weak_bg_fill = state.bg_fill;
        state.bg_stroke = egui::Stroke::NONE;
        state.fg_stroke.color = palette.fg;
        state.corner_radius = radius;
    }
    widgets.noninteractive.bg_fill = palette.surface;
    widgets.noninteractive.weak_bg_fill = palette.surface;
    widgets.active.fg_stroke.color = palette.accent;
    ctx.set_visuals(visuals);
}
