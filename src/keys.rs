//! Keyboard shortcuts: every action a key can trigger, its default keys, and
//! the user's own keys from the settings file.
//!
//! A few keys stay fixed because they depend on what is happening, not on a
//! single action: Esc (leave crop, trim, the tag bar or fullscreen), Enter
//! (apply a crop) and 1 to 5 (crop shape, while cropping).
//!
//! No default key types a character on its own: letters, digits and
//! punctuation take Alt (or Ctrl), so typing starts a tag instead.

use eframe::egui::{Key, Modifiers};
use std::collections::BTreeMap;
use std::fmt;

/// Something a key does.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Action {
    Previous,
    Next,
    First,
    Last,
    Trash,
    Rename,
    Open,
    PlayPause,
    Back,
    Forward,
    FrameBack,
    FrameForward,
    Mute,
    VolumeUp,
    VolumeDown,
    Trim,
    TrimStart,
    TrimEnd,
    /// Quick tag 1 to 9, as index 0 to 8.
    QuickTag(u8),
    /// Rating 0 (clear) to 5.
    Rate(u8),
    Reject,
    /// Colour label, as an index into `tags::LABELS` (red to blue).
    Label(u8),
    TagBar,
    /// Copy the file's tags, to paste onto others.
    CopyTags,
    /// Add the copied tags to the file's.
    PasteTags,
    /// Replace the file's tags with the copied ones.
    PasteTagsReplace,
    Filter,
    Crop,
    RotateRight,
    RotateLeft,
    Flip,
    ZoomIn,
    ZoomOut,
    Fit,
    ActualSize,
    Fullscreen,
    Help,
    Save,
    SaveCopy,
    Revert,
    Quit,
}

/// The groups of the Keybinds list, in order.
pub const GROUPS: [&str; 6] = ["Browse", "Video", "Tags", "Photo", "View", "Save"];

impl Action {
    /// Every action, in the order the Keybinds list shows them.
    pub fn all() -> Vec<Action> {
        use Action::*;
        let mut all = vec![
            Previous,
            Next,
            First,
            Last,
            Trash,
            Rename,
            Open,
            PlayPause,
            Back,
            Forward,
            FrameBack,
            FrameForward,
            Mute,
            VolumeUp,
            VolumeDown,
            Trim,
            TrimStart,
            TrimEnd,
        ];
        all.extend((0..9).map(QuickTag));
        all.extend((0..=5).map(Rate));
        all.push(Reject);
        all.extend((0..5).map(Label));
        all.extend([
            TagBar,
            CopyTags,
            PasteTags,
            PasteTagsReplace,
            Filter,
            Crop,
            RotateRight,
            RotateLeft,
            Flip,
            ZoomIn,
            ZoomOut,
            Fit,
            ActualSize,
            Fullscreen,
            Help,
            Save,
            SaveCopy,
            Revert,
            Quit,
        ]);
        all
    }

    /// Whether the action's keys can't be changed: saving and quitting stay
    /// where everyone expects them.
    pub fn is_fixed(self) -> bool {
        self.group() == "Save"
    }

    /// The fixed action `chord` belongs to, if any.
    pub fn fixed_for(chord: Chord) -> Option<Action> {
        Action::all()
            .into_iter()
            .filter(|action| action.is_fixed())
            .find(|action| action.default_keys().contains(&chord))
    }

    /// The group the action is listed under, one of `GROUPS`.
    pub fn group(self) -> &'static str {
        use Action::*;
        match self {
            Previous | Next | First | Last | Trash | Rename | Open => "Browse",
            PlayPause | Back | Forward | FrameBack | FrameForward | Mute | VolumeUp
            | VolumeDown | Trim | TrimStart | TrimEnd => "Video",
            QuickTag(_) | Rate(_) | Reject | Label(_) | TagBar | CopyTags | PasteTags
            | PasteTagsReplace | Filter => "Tags",
            Crop | RotateRight | RotateLeft | Flip => "Photo",
            ZoomIn | ZoomOut | Fit | ActualSize | Fullscreen | Help => "View",
            Save | SaveCopy | Revert | Quit => "Save",
        }
    }

    /// What the action does, for the Keybinds list.
    pub fn label(self) -> String {
        use Action::*;
        let text = match self {
            Previous => "Previous file",
            Next => "Next file",
            First => "First file",
            Last => "Last file",
            Trash => "Move to trash",
            Rename => "Rename",
            Open => "Open a file",
            PlayPause => "Play / pause",
            Back => "Back 5 seconds",
            Forward => "Forward 5 seconds",
            FrameBack => "Back one frame",
            FrameForward => "Forward one frame",
            Mute => "Mute",
            VolumeUp => "Volume up",
            VolumeDown => "Volume down",
            Trim => "Trim",
            TrimStart => "Set trim start",
            TrimEnd => "Set trim end",
            QuickTag(index) => return format!("Quick tag {} on / off", index + 1),
            Rate(0) => "Clear rating",
            Rate(stars) => return format!("{stars} star{} (again: clear)", plural(stars)),
            Reject => "Reject",
            Label(index) => {
                let name = ["Red", "Yellow", "Green", "Blue", "Purple"][usize::from(index.min(4))];
                return format!("{name} label");
            }
            TagBar => "Tag bar",
            CopyTags => "Copy tags",
            PasteTags => "Paste tags (add)",
            PasteTagsReplace => "Paste tags (replace)",
            Filter => "Filter",
            Crop => "Crop",
            RotateRight => "Rotate right",
            RotateLeft => "Rotate left",
            Flip => "Flip",
            ZoomIn => "Zoom in",
            ZoomOut => "Zoom out",
            Fit => "Fit to window",
            ActualSize => "Actual size",
            Fullscreen => "Fullscreen",
            Help => "Help",
            Save => "Save over the original",
            SaveCopy => "Save a copy",
            Revert => "Undo all edits",
            Quit => "Quit",
        };
        text.to_string()
    }

    /// The name in the settings file, after `key_`.
    fn id(self) -> String {
        use Action::*;
        let id = match self {
            Previous => "previous",
            Next => "next",
            First => "first",
            Last => "last",
            Trash => "trash",
            Rename => "rename",
            Open => "open",
            PlayPause => "play_pause",
            Back => "back",
            Forward => "forward",
            FrameBack => "frame_back",
            FrameForward => "frame_forward",
            Mute => "mute",
            VolumeUp => "volume_up",
            VolumeDown => "volume_down",
            Trim => "trim",
            TrimStart => "trim_start",
            TrimEnd => "trim_end",
            QuickTag(index) => return format!("quick_tag_{}", index + 1),
            Rate(stars) => return format!("rate_{stars}"),
            Reject => "reject",
            Label(index) => return format!("label_{}", index + 1),
            TagBar => "tag_bar",
            CopyTags => "copy_tags",
            PasteTags => "paste_tags",
            PasteTagsReplace => "paste_tags_replace",
            Filter => "filter",
            Crop => "crop",
            RotateRight => "rotate_right",
            RotateLeft => "rotate_left",
            Flip => "flip",
            ZoomIn => "zoom_in",
            ZoomOut => "zoom_out",
            Fit => "fit",
            ActualSize => "actual_size",
            Fullscreen => "fullscreen",
            Help => "help",
            Save => "save",
            SaveCopy => "save_copy",
            Revert => "revert",
            Quit => "quit",
        };
        id.to_string()
    }

    fn from_id(id: &str) -> Option<Action> {
        Action::all().into_iter().find(|action| action.id() == id)
    }

    /// The keys the action has unless the user changed them.
    pub fn default_keys(self) -> Vec<Chord> {
        use Action::*;
        let plain = Chord::plain;
        let ctrl = Chord::ctrl;
        let shift = Chord::shift;
        let alt = Chord::alt;
        let alt_shift = |key| Chord {
            shift: true,
            ..alt(key)
        };
        match self {
            Previous => vec![plain(Key::ArrowLeft), plain(Key::PageUp)],
            Next => vec![plain(Key::ArrowRight), plain(Key::PageDown)],
            First => vec![plain(Key::Home)],
            Last => vec![plain(Key::End)],
            Trash => vec![plain(Key::Delete)],
            Rename => vec![plain(Key::F2)],
            Open => vec![ctrl(Key::O)],
            PlayPause => vec![plain(Key::Space)],
            Back => vec![alt(Key::J), shift(Key::ArrowLeft)],
            Forward => vec![alt(Key::L), shift(Key::ArrowRight)],
            FrameBack => vec![alt(Key::Comma)],
            FrameForward => vec![alt(Key::Period)],
            Mute => vec![alt(Key::M)],
            VolumeUp => vec![plain(Key::ArrowUp)],
            VolumeDown => vec![plain(Key::ArrowDown)],
            Trim => vec![alt(Key::T)],
            TrimStart => vec![alt(Key::I)],
            TrimEnd => vec![alt(Key::O)],
            QuickTag(index) => vec![alt(DIGITS[usize::from(index) + 1])],
            Rate(0) => vec![ctrl(Key::X)],
            Rate(stars) => vec![ctrl(DIGITS[usize::from(stars)])],
            Reject => vec![plain(Key::Backspace)],
            // Red to blue on Ctrl+6 to 9, purple on Ctrl+0.
            Label(index) => vec![ctrl(DIGITS[(usize::from(index) + 6) % 10])],
            TagBar => vec![ctrl(Key::K)],
            CopyTags => vec![ctrl(Key::C)],
            PasteTags => vec![ctrl(Key::V)],
            PasteTagsReplace => vec![Chord {
                shift: true,
                ..ctrl(Key::V)
            }],
            Filter => vec![Chord {
                shift: true,
                ..ctrl(Key::F)
            }],
            Crop => vec![alt(Key::C)],
            RotateRight => vec![alt(Key::R)],
            RotateLeft => vec![alt_shift(Key::R)],
            Flip => vec![alt(Key::H)],
            ZoomIn => vec![alt(Key::Plus), alt(Key::Equals)],
            ZoomOut => vec![alt(Key::Minus)],
            Fit => vec![alt(Key::Num0)],
            ActualSize => vec![alt(Key::Z)],
            Fullscreen => vec![ctrl(Key::F), plain(Key::F11)],
            Help => vec![alt(Key::Questionmark), plain(Key::F1)],
            Save => vec![ctrl(Key::S)],
            SaveCopy => vec![Chord {
                shift: true,
                ..ctrl(Key::S)
            }],
            Revert => vec![ctrl(Key::Z)],
            Quit => vec![ctrl(Key::Q)],
        }
    }
}

fn plural(count: u8) -> &'static str {
    if count == 1 { "" } else { "s" }
}

/// The number keys, 0 to 9.
pub const DIGITS: [Key; 10] = [
    Key::Num0,
    Key::Num1,
    Key::Num2,
    Key::Num3,
    Key::Num4,
    Key::Num5,
    Key::Num6,
    Key::Num7,
    Key::Num8,
    Key::Num9,
];

/// A key with the modifiers held down with it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Chord {
    pub key: Key,
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
}

impl Chord {
    pub fn plain(key: Key) -> Self {
        Self {
            key,
            ctrl: false,
            shift: false,
            alt: false,
        }
    }

    pub fn ctrl(key: Key) -> Self {
        Self {
            ctrl: true,
            ..Self::plain(key)
        }
    }

    pub fn shift(key: Key) -> Self {
        Self {
            shift: true,
            ..Self::plain(key)
        }
    }

    pub fn alt(key: Key) -> Self {
        Self {
            alt: true,
            ..Self::plain(key)
        }
    }

    /// `key` pressed with `modifiers`.
    pub fn pressed(key: Key, modifiers: Modifiers) -> Self {
        Self {
            key,
            ctrl: modifiers.command,
            shift: modifiers.shift,
            alt: modifiers.alt,
        }
    }

    /// The modifiers held down for this chord.
    pub fn modifiers(self) -> Modifiers {
        Modifiers {
            alt: self.alt,
            ctrl: self.ctrl,
            shift: self.shift,
            mac_cmd: false,
            command: self.ctrl,
        }
    }

    /// Read `Ctrl+Shift+S`; modifier names in any case.
    pub fn parse(text: &str) -> Option<Self> {
        let mut parts: Vec<&str> = text.split('+').map(str::trim).collect();
        // `+` itself, alone or after a modifier (`Ctrl++`).
        if text.trim().ends_with("++") || text.trim() == "+" {
            parts.retain(|part| !part.is_empty());
            parts.push("+");
        }
        let (key, modifiers) = parts.split_last()?;
        let mut chord = Self::plain(Key::from_name(key)?);
        for modifier in modifiers {
            match modifier.to_ascii_lowercase().as_str() {
                "ctrl" => chord.ctrl = true,
                "shift" => chord.shift = true,
                "alt" => chord.alt = true,
                _ => return None,
            }
        }
        Some(chord)
    }

    /// As written in the settings file: `Ctrl+Shift+S`.
    fn to_setting(self) -> String {
        self.join(self.key.name())
    }

    fn join(self, key: &str) -> String {
        let mut text = String::new();
        for (held, name) in [
            (self.ctrl, "Ctrl+"),
            (self.shift, "Shift+"),
            (self.alt, "Alt+"),
        ] {
            if held {
                text.push_str(name);
            }
        }
        text + key
    }
}

/// As shown to the user: `Ctrl+S`, `←`.
impl fmt::Display for Chord {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        let key = match self.key {
            Key::ArrowLeft => "←",
            Key::ArrowRight => "→",
            Key::ArrowUp => "↑",
            Key::ArrowDown => "↓",
            Key::Delete => "Del",
            Key::Space => "Space",
            key => key.symbol_or_name(),
        };
        formatter.write_str(&self.join(key))
    }
}

/// The keys of every action: the defaults, with the user's changes.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct Keymap {
    /// Actions whose keys the user changed; an empty list means no key.
    changed: BTreeMap<Action, Vec<Chord>>,
}

impl Keymap {
    pub fn keys(&self, action: Action) -> Vec<Chord> {
        self.changed
            .get(&action)
            .cloned()
            .unwrap_or_else(|| action.default_keys())
    }

    /// The action `chord` triggers. Without an exact match, Shift is ignored,
    /// since some keys (`?`, `+`) need it on many keyboard layouts.
    pub fn action(&self, chord: Chord) -> Option<Action> {
        let find = |chord: Chord| {
            Action::all()
                .into_iter()
                .find(|action| self.keys(*action).contains(&chord))
        };
        find(chord).or_else(|| {
            chord
                .shift
                .then(|| {
                    find(Chord {
                        shift: false,
                        ..chord
                    })
                })
                .flatten()
        })
    }

    /// Give `action` the keys `keys`, taking each of them away from any other
    /// action. Returns the actions that lost a key.
    /// Keys of fixed actions are left out: they can't be taken.
    pub fn set(&mut self, action: Action, mut keys: Vec<Chord>) -> Vec<Action> {
        keys.retain(|key| Action::fixed_for(*key).is_none());
        let mut lost = Vec::new();
        for other in Action::all() {
            if other == action {
                continue;
            }
            let own = self.keys(other);
            let kept: Vec<Chord> = own
                .iter()
                .copied()
                .filter(|key| !keys.contains(key))
                .collect();
            if kept.len() != own.len() {
                self.store(other, kept);
                lost.push(other);
            }
        }
        self.store(action, keys);
        lost
    }

    fn store(&mut self, action: Action, keys: Vec<Chord>) {
        if keys == action.default_keys() || action.is_fixed() {
            self.changed.remove(&action);
        } else {
            self.changed.insert(action, keys);
        }
    }

    pub fn is_default(&self) -> bool {
        self.changed.is_empty()
    }

    /// Read a `key_<action> = <keys>` setting; false when `name` isn't one.
    pub fn read_setting(&mut self, name: &str, value: &str) -> bool {
        let Some(action) = name.strip_prefix("key_").and_then(Action::from_id) else {
            return false;
        };
        let keys: Vec<Chord> = value
            .split(',')
            .filter(|key| !key.trim().is_empty())
            .filter_map(Chord::parse)
            .collect();
        self.store(action, keys);
        true
    }

    /// The changed keys as `key_<action> = <keys>` lines.
    pub fn to_settings(&self) -> String {
        self.changed
            .iter()
            .map(|(action, keys)| {
                let keys: Vec<String> = keys.iter().map(|key| key.to_setting()).collect();
                format!("key_{} = {}\n", action.id(), keys.join(", "))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_keys_are_fixed() {
        let mut keys = Keymap::default();
        assert!(keys.read_setting("key_save", "Ctrl+W"));
        assert_eq!(keys.keys(Action::Save), Action::Save.default_keys());
        let lost = keys.set(
            Action::Next,
            vec![Chord::plain(Key::N), Chord::ctrl(Key::S)],
        );
        assert!(lost.is_empty());
        assert_eq!(keys.keys(Action::Save), vec![Chord::ctrl(Key::S)]);
        assert_eq!(keys.keys(Action::Next), vec![Chord::plain(Key::N)]);
    }

    #[test]
    fn chords_round_trip_through_text() {
        for action in Action::all() {
            for chord in action.default_keys() {
                assert_eq!(Chord::parse(&chord.to_setting()), Some(chord), "{action:?}");
            }
        }
        assert_eq!(
            Chord::parse("ctrl+shift+s"),
            Some(Chord {
                shift: true,
                ..Chord::ctrl(Key::S)
            })
        );
        assert_eq!(Chord::parse("Ctrl++"), Some(Chord::ctrl(Key::Plus)));
        assert_eq!(Chord::parse("Hyper+S"), None);
    }

    #[test]
    fn every_action_has_its_own_settings_name() {
        let all = Action::all();
        for action in &all {
            assert_eq!(Action::from_id(&action.id()), Some(*action));
        }
    }

    #[test]
    fn default_keys_do_not_clash() {
        let mut seen = Vec::new();
        for action in Action::all() {
            for chord in action.default_keys() {
                assert!(!seen.contains(&chord), "{chord} is bound twice");
                seen.push(chord);
            }
        }
    }

    #[test]
    fn alt_numbers_toggle_quick_tags() {
        let keys = Keymap::default();
        assert_eq!(
            keys.action(Chord::alt(Key::Num1)),
            Some(Action::QuickTag(0))
        );
        assert_eq!(
            keys.action(Chord::alt(Key::Num9)),
            Some(Action::QuickTag(8))
        );
        assert_eq!(keys.action(Chord::ctrl(Key::Num3)), Some(Action::Rate(3)));
        assert_eq!(keys.action(Chord::ctrl(Key::Num6)), Some(Action::Label(0)));
        assert_eq!(keys.action(Chord::alt(Key::Num0)), Some(Action::Fit));
        assert_eq!(keys.action(Chord::plain(Key::Num1)), None);
    }

    /// Typing goes to the tag field, so no default key is a character alone.
    #[test]
    fn default_keys_never_type_a_character() {
        // Space is left plain: a blank never starts a tag.
        let silent = |key: Key| {
            use Key::*;
            matches!(
                key,
                ArrowLeft
                    | ArrowRight
                    | ArrowUp
                    | ArrowDown
                    | Space
                    | Delete
                    | Home
                    | End
                    | PageUp
                    | PageDown
                    | Escape
                    | Enter
                    | Tab
                    | Backspace
                    | Insert
            ) || key.name().starts_with('F')
                && key.name().len() > 1
                && key.name()[1..].chars().all(|c| c.is_ascii_digit())
        };
        let types = |key: Key| !silent(key);
        for action in Action::all() {
            for chord in action.default_keys() {
                let modified = chord.ctrl || chord.alt;
                assert!(modified || !types(chord.key), "{chord} types a character");
            }
        }
    }

    #[test]
    fn shift_is_ignored_only_without_an_exact_match() {
        let keys = Keymap::default();
        let alt_shift = |key| Chord {
            shift: true,
            ..Chord::alt(key)
        };
        assert_eq!(keys.action(alt_shift(Key::R)), Some(Action::RotateLeft));
        assert_eq!(
            keys.action(alt_shift(Key::Questionmark)),
            Some(Action::Help)
        );
        assert_eq!(keys.action(Chord::plain(Key::Y)), None);
    }

    #[test]
    fn rebinding_takes_the_key_from_its_old_action() {
        let mut keys = Keymap::default();
        let lost = keys.set(Action::Crop, vec![Chord::alt(Key::H)]);
        assert_eq!(lost, vec![Action::Flip]);
        assert_eq!(keys.action(Chord::alt(Key::H)), Some(Action::Crop));
        assert_eq!(keys.keys(Action::Flip), Vec::new());
        assert_eq!(keys.action(Chord::alt(Key::C)), None);
    }

    #[test]
    fn settings_store_only_changes() {
        let mut keys = Keymap::default();
        assert_eq!(keys.to_settings(), "");
        keys.set(
            Action::QuickTag(0),
            vec![Chord {
                shift: true,
                ..Chord::alt(Key::Num1)
            }],
        );
        keys.set(Action::Mute, Vec::new());
        let text = keys.to_settings();
        assert_eq!(text, "key_mute = \nkey_quick_tag_1 = Shift+Alt+1\n");
        let mut read = Keymap::default();
        for line in text.lines() {
            let (name, value) = line.split_once('=').unwrap_or_default();
            assert!(read.read_setting(name.trim(), value.trim()));
        }
        assert_eq!(read, keys);
        // Back to the default: nothing to store.
        keys.set(Action::QuickTag(0), Action::QuickTag(0).default_keys());
        keys.set(Action::Mute, Action::Mute.default_keys());
        assert!(keys.is_default());
    }
}
