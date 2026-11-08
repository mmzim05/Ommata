//! What kind of media a path is, and the sorted list of media files in its folder.

use std::path::{Path, PathBuf};

/// The kind of media a file holds.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Image,
    Video,
    Audio,
}

impl Kind {
    /// Whether the file plays in the player (video, or audio alone).
    pub fn plays(self) -> bool {
        matches!(self, Self::Video | Self::Audio)
    }
}

const IMAGE_EXT: &[&str] = &[
    "jpg", "jpeg", "jpe", "png", "webp", "gif", "tif", "tiff", "bmp",
];
/// Camera RAW extensions, decoded through rawler.
pub const RAW_EXT: &[&str] = &[
    "rw2", "cr2", "cr3", "nef", "nrw", "arw", "srf", "sr2", "raf", "orf", "dng", "pef", "srw",
    "erf", "3fr", "iiq", "mos", "kdc", "dcr", "mrw",
];
const VIDEO_EXT: &[&str] = &[
    "mp4", "m4v", "mov", "mkv", "webm", "ivf", "avi", "ts", "mts", "m2ts", "flv", "ogv", "wmv",
    "mpg", "mpeg", "3gp",
];
/// Audio-only files, played through the video player with nothing to show.
const AUDIO_EXT: &[&str] = &[
    "mp3", "mp2", "mp1", "m4a", "m4b", "aac", "adts", "flac", "wav", "wave", "ogg", "oga", "opus",
    "spx", "weba", "mka", "wma", "aif", "aiff", "aifc", "ape", "wv", "tta", "tak", "amr", "awb",
    "au", "snd", "caf", "w64", "voc", "dsf", "dff", "mpc", "mp+", "ra", "rm", "ac3", "eac3", "dts",
    "thd", "mlp", "shn", "gsm", "qoa", "oma",
];

/// The media kind of `path`, from its extension.
pub fn kind(path: &Path) -> Option<Kind> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    if IMAGE_EXT.contains(&ext.as_str()) || RAW_EXT.contains(&ext.as_str()) {
        Some(Kind::Image)
    } else if VIDEO_EXT.contains(&ext.as_str()) {
        Some(Kind::Video)
    } else if AUDIO_EXT.contains(&ext.as_str()) {
        Some(Kind::Audio)
    } else {
        None
    }
}

/// Media files next to `path`, naturally sorted ("img2" before "img10").
pub fn siblings(path: &Path) -> Vec<PathBuf> {
    let dir = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|file| file.is_file() && kind(file).is_some())
                .collect()
        })
        .unwrap_or_default();
    files.sort_by(|a, b| natural_cmp(&a.to_string_lossy(), &b.to_string_lossy()));
    files
}

fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let (mut a, mut b) = (a.as_bytes(), b.as_bytes());
    loop {
        match (a.first(), b.first()) {
            (None, None) => return std::cmp::Ordering::Equal,
            (None, _) => return std::cmp::Ordering::Less,
            (_, None) => return std::cmp::Ordering::Greater,
            (Some(left), Some(right)) if left.is_ascii_digit() && right.is_ascii_digit() => {
                let left_len = a.iter().take_while(|byte| byte.is_ascii_digit()).count();
                let right_len = b.iter().take_while(|byte| byte.is_ascii_digit()).count();
                let left_digits = trim_zeros(&a[..left_len]);
                let right_digits = trim_zeros(&b[..right_len]);
                let order = left_digits
                    .len()
                    .cmp(&right_digits.len())
                    .then(left_digits.cmp(right_digits));
                if order.is_ne() {
                    return order;
                }
                a = &a[left_len..];
                b = &b[right_len..];
            }
            (Some(left), Some(right)) => {
                let order = left.to_ascii_lowercase().cmp(&right.to_ascii_lowercase());
                if order.is_ne() {
                    return order;
                }
                a = &a[1..];
                b = &b[1..];
            }
        }
    }
}

fn trim_zeros(digits: &[u8]) -> &[u8] {
    let zeros = digits.iter().take_while(|&&byte| byte == b'0').count();
    &digits[zeros.min(digits.len().saturating_sub(1))..]
}

/// `photo.jpg` -> `photo-edit.jpg` (with extension `ext`, or the source's own
/// when `None`); never clobbers an existing file (`photo-edit-2.jpg`, ...).
pub fn edit_copy_path(path: &Path, ext: Option<&str>) -> PathBuf {
    suffixed_path(path, "edit", ext)
}

/// `clip.mp4` -> `clip-original.mp4`, the name an overwritten file's original
/// goes to the trash under.
pub fn original_path(path: &Path) -> PathBuf {
    suffixed_path(path, "original", None)
}

fn suffixed_path(path: &Path, tag: &str, ext: Option<&str>) -> PathBuf {
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    let ext = ext
        .map(|extension| format!(".{extension}"))
        .or_else(|| {
            path.extension()
                .map(|extension| format!(".{}", extension.to_string_lossy()))
        })
        .unwrap_or_default();
    let dir = path.parent().unwrap_or(Path::new(""));
    let mut number = 1;
    loop {
        let suffix = if number == 1 {
            String::new()
        } else {
            format!("-{number}")
        };
        let candidate = dir.join(format!("{stem}-{tag}{suffix}{ext}"));
        if !candidate.exists() {
            return candidate;
        }
        number += 1;
    }
}

/// Temp file in the same directory, so the final rename is atomic.
pub fn temp_path(target: &Path) -> PathBuf {
    let name = target.file_name().unwrap_or_default().to_string_lossy();
    target.with_file_name(format!(".{name}.ommata-tmp"))
}

/// Write via a temp file in the target's directory, then rename over the target,
/// so a failed or cancelled save never leaves a half-written file behind.
pub fn write_atomic(
    dst: &Path,
    write: impl FnOnce(&Path) -> Result<(), String>,
) -> Result<(), String> {
    let temp = temp_path(dst);
    let result =
        write(&temp).and_then(|()| std::fs::rename(&temp, dst).map_err(|error| error.to_string()));
    if result.is_err() {
        remove_temp(&temp);
    }
    result
}

/// Clean up a temp file after a failed save. The save error is what the user
/// needs to see; a leftover temp file is only clutter, so it is just logged.
pub fn remove_temp(temp: &Path) {
    if let Err(error) = std::fs::remove_file(temp)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        eprintln!("ommata: remove {}: {error}", temp.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_by_extension() {
        assert_eq!(kind(Path::new("a.JPG")), Some(Kind::Image));
        assert_eq!(kind(Path::new("a.mkv")), Some(Kind::Video));
        assert_eq!(kind(Path::new("a.FLAC")), Some(Kind::Audio));
        assert_eq!(kind(Path::new("a.mp+")), Some(Kind::Audio));
        assert_eq!(kind(Path::new("a.txt")), None);
        assert!(Kind::Audio.plays() && Kind::Video.plays() && !Kind::Image.plays());
    }

    #[test]
    fn natural_order() {
        let mut names = vec!["img10.jpg", "img2.jpg", "IMG1.jpg", "img02b.jpg"];
        names.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(names, ["IMG1.jpg", "img2.jpg", "img02b.jpg", "img10.jpg"]);
    }

    #[test]
    fn edit_name() {
        let dir = std::env::temp_dir().join(format!("ommata-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("clip.mp4");
        assert_eq!(edit_copy_path(&src, None), dir.join("clip-edit.mp4"));
        std::fs::write(dir.join("clip-edit.mp4"), b"").unwrap();
        assert_eq!(edit_copy_path(&src, None), dir.join("clip-edit-2.mp4"));
        assert_eq!(
            edit_copy_path(&dir.join("a.RW2"), Some("jpg")),
            dir.join("a-edit.jpg")
        );
        assert_eq!(original_path(&src), dir.join("clip-original.mp4"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
