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
            danger: if dark {
                Color32::from_rgb(0xe0, 0x6c, 0x75)
            } else {
                Color32::from_rgb(0xc6, 0x28, 0x28)
            },
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
    parse_omarchy(&std::fs::read_to_string(state_dir()?.join("theme/colors.toml")).ok()?)
}

fn parse_omarchy(text: &str) -> Option<Palette> {
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
        // Some themes recolour their "red" slot (blue in a few), and danger
        // must read as red: take the theme's own red only if it is one.
        danger: readable(
            [color("red"), color("bright_red")]
                .into_iter()
                .flatten()
                .find(|red| is_red(*red))
                .unwrap_or(base.danger),
            bg,
            surface,
            dark,
        ),
    })
}

/// `color`, lightened on a dark theme or darkened on a light one just enough
/// to carry button text in the background colour (4.5:1) and stand out as an
/// icon on the bars (3:1). Most themes' reds already do and stay as they are.
fn readable(color: Color32, bg: Color32, surface: Color32, dark: bool) -> Color32 {
    let target = if dark { Color32::WHITE } else { Color32::BLACK };
    (0..=50)
        .map(|step| lerp(color, target, step as f32 / 50.0))
        .find(|shade| contrast(*shade, bg) >= 4.5 && contrast(*shade, surface) >= 3.0)
        .unwrap_or(target)
}

/// WCAG contrast ratio, from 1 (same) to 21 (black on white).
fn contrast(a: Color32, b: Color32) -> f32 {
    let luminance = |color: Color32| {
        let channel = |c: u8| {
            let c = f32::from(c) / 255.0;
            if c <= 0.039_28 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(color.r()) + 0.7152 * channel(color.g()) + 0.0722 * channel(color.b())
    };
    let (a, b) = (luminance(a), luminance(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

/// Whether `color` is recognisably red: a hue within 25° of red, and
/// saturated enough not to pass for grey.
fn is_red(color: Color32) -> bool {
    let [r, g, b] = [color.r(), color.g(), color.b()].map(|c| f32::from(c) / 255.0);
    let (max, min) = (r.max(g).max(b), r.min(g).min(b));
    if max == 0.0 || (max - min) / max < 0.3 || max != r {
        return false;
    }
    // Hue in degrees from red, towards yellow (g > b) or magenta (b > g).
    let hue = 60.0 * (g - b).abs() / (max - min);
    hue <= 25.0
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
    // egui applies this to plain buttons only; app.rs gives the other
    // clickable widgets the hand itself.
    visuals.interact_cursor = Some(egui::CursorIcon::PointingHand);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn red_is_readable_in_every_omarchy_theme() {
        let themes = std::fs::read_dir("/usr/share/omarchy/themes");
        let Ok(themes) = themes else {
            return;
        };
        for theme in themes.flatten() {
            let text = std::fs::read_to_string(theme.path().join("colors.toml"));
            let Some(palette) = text.ok().and_then(|text| parse_omarchy(&text)) else {
                continue;
            };
            let name = theme.file_name();
            assert!(contrast(palette.danger, palette.bg) >= 4.5, "{name:?}");
            assert!(contrast(palette.danger, palette.surface) >= 3.0, "{name:?}");
            assert!(is_red(palette.danger), "{name:?}");
        }
    }

    #[test]
    fn recognises_red() {
        assert!(is_red(Color32::from_rgb(0xd3, 0x5f, 0x5f)));
        assert!(is_red(Color32::from_rgb(0xb9, 0x1c, 0x1c)));
        assert!(!is_red(Color32::from_rgb(0x4d, 0x86, 0xb0)));
        assert!(!is_red(Color32::from_rgb(0xe0, 0xa0, 0x40)));
        assert!(!is_red(Color32::from_rgb(0x80, 0x70, 0x70)));
    }
}
