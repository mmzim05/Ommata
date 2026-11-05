//! User settings, stored as `key = value` lines in
//! `$XDG_CONFIG_HOME/omalens/settings.conf`.

use std::path::PathBuf;

/// User preferences.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Settings {
    /// Look for a new release once a day.
    pub check_updates: bool,
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
            check_updates: true,
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
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(base.join("omalens/settings.conf"))
}

impl Settings {
    /// Read the settings file; missing files and bad values fall back to
    /// the defaults.
    pub fn load() -> Self {
        let mut settings = Self::default();
        let Some(text) = path().and_then(|file| std::fs::read_to_string(file).ok()) else {
            return settings;
        };
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim();
            match key.trim() {
                "check_updates" => settings.check_updates = value != "false",
                "waveform" => settings.waveform = value != "false",
                "wave_style" => settings.wave_filled = value == "filled",
                "wave_min" => settings.wave_min = value.parse().unwrap_or(settings.wave_min),
                "wave_max" => settings.wave_max = value.parse().unwrap_or(settings.wave_max),
                "wave_smooth" => {
                    settings.wave_smooth = value.parse().unwrap_or(settings.wave_smooth);
                }
                _ => {}
            }
        }
        settings.clamp()
    }

    /// Keep every value inside the range the settings UI offers.
    pub fn clamp(mut self) -> Self {
        self.wave_max = self.wave_max.clamp(2.0, 40.0);
        self.wave_min = self.wave_min.clamp(1.0, self.wave_max);
        self.wave_smooth = self.wave_smooth.clamp(0.0, 1.0);
        self
    }

    /// Write the settings file, creating its directory if needed.
    pub fn save(&self) -> std::io::Result<()> {
        let Some(file) = path() else { return Ok(()) };
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(
            file,
            format!(
                "check_updates = {}\nwaveform = {}\nwave_style = {}\nwave_min = {}\n\
                 wave_max = {}\nwave_smooth = {}\n",
                self.check_updates,
                self.waveform,
                if self.wave_filled { "filled" } else { "bars" },
                self.wave_min,
                self.wave_max,
                self.wave_smooth
            ),
        )
    }
}
