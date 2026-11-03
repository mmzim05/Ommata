//! User settings, stored as `key = value` lines in
//! `$XDG_CONFIG_HOME/anyview/settings.conf`.

use std::path::PathBuf;

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Settings {
    /// Draw the seek bar as the audio waveform (falls back to a plain bar
    /// when off or when the video has no audio).
    pub waveform: bool,
    /// One continuous filled shape instead of separate bars.
    pub wave_filled: bool,
    /// Bar height in px for silence.
    pub wave_min: f32,
    /// Bar height in px for the loudest part.
    pub wave_max: f32,
    /// 0 = raw levels, 1 = heavily smoothed.
    pub wave_smooth: f32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            waveform: true,
            wave_filled: false,
            wave_min: 3.0,
            wave_max: 22.0,
            wave_smooth: 0.3,
        }
    }
}

fn path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("anyview/settings.conf"))
}

impl Settings {
    pub fn load() -> Self {
        let mut s = Self::default();
        let Some(text) = path().and_then(|p| std::fs::read_to_string(p).ok()) else {
            return s;
        };
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let v = v.trim();
            match k.trim() {
                "waveform" => s.waveform = v != "false",
                "wave_style" => s.wave_filled = v == "filled",
                "wave_min" => s.wave_min = v.parse().unwrap_or(s.wave_min),
                "wave_max" => s.wave_max = v.parse().unwrap_or(s.wave_max),
                "wave_smooth" => s.wave_smooth = v.parse().unwrap_or(s.wave_smooth),
                _ => {}
            }
        }
        s.clamp()
    }

    pub fn clamp(mut self) -> Self {
        self.wave_max = self.wave_max.clamp(2.0, 40.0);
        self.wave_min = self.wave_min.clamp(1.0, self.wave_max);
        self.wave_smooth = self.wave_smooth.clamp(0.0, 1.0);
        self
    }

    pub fn save(&self) -> std::io::Result<()> {
        let Some(p) = path() else { return Ok(()) };
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(
            p,
            format!(
                "waveform = {}\nwave_style = {}\nwave_min = {}\nwave_max = {}\nwave_smooth = {}\n",
                self.waveform,
                if self.wave_filled { "filled" } else { "bars" },
                self.wave_min,
                self.wave_max,
                self.wave_smooth
            ),
        )
    }
}
