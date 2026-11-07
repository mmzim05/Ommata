//! User settings, stored as `key = value` lines in
//! `$XDG_CONFIG_HOME/ommata/settings.conf`.

use crate::keys::Keymap;
use std::path::PathBuf;

/// User preferences.
#[derive(Clone, PartialEq, Debug)]
pub struct Settings {
    /// Tags toggled with 1 to 9 (by default), `/` between levels; empty
    /// when unset.
    pub quick_tags: [String; 9],
    /// Keyboard shortcuts.
    pub keys: Keymap,
    /// Look for a new release each time ommata starts.
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
            quick_tags: Default::default(),
            keys: Keymap::default(),
            check_updates: true,
            waveform: true,
            wave_filled: false,
            wave_min: 3.0,
            wave_max: 22.0,
            wave_smooth: 0.3,
        }
    }
}

/// `$XDG_CONFIG_HOME`, or `~/.config`.
pub fn config_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
}

/// `name` in Ommata's config folder.
pub fn config_file(name: &str) -> Option<PathBuf> {
    Some(config_dir()?.join("ommata").join(name))
}

fn path() -> Option<PathBuf> {
    config_file("settings.conf")
}

/// Where versions before the rename to Ommata kept the settings.
fn old_path() -> Option<PathBuf> {
    Some(config_dir()?.join("omalens/settings.conf"))
}

impl Settings {
    /// Read the settings file; missing files and bad values fall back to
    /// the defaults.
    pub fn load() -> Self {
        [path(), old_path()]
            .into_iter()
            .flatten()
            .find_map(|file| std::fs::read_to_string(file).ok())
            .map_or_else(Self::default, |text| Self::parse(&text))
    }

    /// Settings from the file's `key = value` lines.
    fn parse(text: &str) -> Self {
        let mut settings = Self::default();
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim();
            let key = key.trim();
            if settings.keys.read_setting(key, value) {
                continue;
            }
            if let Some(slot) = key
                .strip_prefix("quick_tag_")
                .and_then(|number| number.parse::<usize>().ok())
                .and_then(|number| number.checked_sub(1))
                .and_then(|index| settings.quick_tags.get_mut(index))
            {
                *slot = value.to_string();
                continue;
            }
            match key {
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
        std::fs::write(file, self.to_text())
    }

    /// The settings file's text.
    fn to_text(&self) -> String {
        let quick: String = self
            .quick_tags
            .iter()
            .enumerate()
            .map(|(index, tag)| format!("quick_tag_{} = {}\n", index + 1, tag.trim()))
            .collect();
        quick
            + &format!(
                "check_updates = {}\nwaveform = {}\nwave_style = {}\nwave_min = {}\n\
                 wave_max = {}\nwave_smooth = {}\n",
                self.check_updates,
                self.waveform,
                if self.wave_filled { "filled" } else { "bars" },
                self.wave_min,
                self.wave_max,
                self.wave_smooth
            )
            + &self.keys.to_settings()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_text() {
        let mut settings = Settings::default();
        settings.quick_tags[0] = "Shot/Establishing".into();
        settings.quick_tags[4] = "Shot/Motion".into();
        settings.check_updates = false;
        settings.wave_filled = true;
        settings.keys.set(crate::keys::Action::Mute, Vec::new());
        assert_eq!(Settings::parse(&settings.to_text()), settings);
    }

    #[test]
    fn reads_quick_tags_by_number() {
        let settings = Settings::parse(
            "quick_tag_2 = Shot/Action\nquick_tag_10 = ignored\nquick_tag_0 = no\n",
        );
        assert_eq!(settings.quick_tags[1], "Shot/Action");
        assert!(
            settings
                .quick_tags
                .iter()
                .enumerate()
                .all(|(index, tag)| index == 1 || tag.is_empty())
        );
    }
}
