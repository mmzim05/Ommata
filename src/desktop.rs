//! Making Ommata the app that opens photos and videos, through `xdg-mime`
//! (xdg-utils, on every desktop Linux) and the desktop entry the system
//! knows Ommata by.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The desktop entry that ships with Ommata.
const ENTRY: &str = include_str!("../assets/ommata.desktop");
/// Its icon, for an entry written here.
const ICON: &str = include_str!("../assets/ommata.svg");

/// Every type the desktop entry opens: photos, RAW files and videos.
pub fn mime_types() -> Vec<&'static str> {
    ENTRY
        .lines()
        .find_map(|line| line.strip_prefix("MimeType="))
        .unwrap_or_default()
        .split(';')
        .filter(|kind| !kind.is_empty())
        .collect()
}

/// Make Ommata the default for every type in `mime_types`. A copy no menu
/// knows yet (an AppImage nobody integrated) first gets its own entry in
/// `~/.local/share/applications`. Returns how many types it now opens.
pub fn make_default() -> Result<usize, String> {
    let program = program().ok_or("can't tell where Ommata is installed")?;
    let entry = match find_entry(&program) {
        Some(name) => name,
        None => write_entry(&program)?,
    };
    let types = mime_types();
    let output = Command::new("xdg-mime")
        .arg("default")
        .arg(&entry)
        .args(&types)
        .output()
        .map_err(|error| format!("couldn't run xdg-mime: {error}"))?;
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr);
        return Err(format!("xdg-mime failed: {}", error.trim()));
    }
    Ok(types.len())
}

/// The file that starts this copy: the AppImage, not the binary inside it.
fn program() -> Option<PathBuf> {
    crate::update::appimage().or_else(|| std::env::current_exe().ok())
}

/// Where desktop entries live, most personal first.
fn application_dirs() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| home.map(|home| home.join(".local/share")));
    let data_dirs = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|dirs| !dirs.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    data_home
        .into_iter()
        .chain(std::env::split_paths(&data_dirs))
        .map(|dir| dir.join("applications"))
        .collect()
}

/// The name of the installed desktop entry that starts `program`, or
/// failing that, the one called `ommata.desktop`.
fn find_entry(program: &Path) -> Option<String> {
    let entries: Vec<PathBuf> = application_dirs()
        .iter()
        .filter_map(|dir| std::fs::read_dir(dir).ok())
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "desktop"))
        .collect();
    let starts_program = |path: &PathBuf| {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| exec_program(&text))
            .is_some_and(|exec| Path::new(&exec) == program)
    };
    let found = entries
        .iter()
        .find(|path| starts_program(path))
        .or_else(|| entries.iter().find(|path| path.ends_with("ommata.desktop")))?;
    Some(found.file_name()?.to_string_lossy().into_owned())
}

/// The program an entry's `Exec=` line starts, without its arguments.
fn exec_program(entry: &str) -> Option<String> {
    let exec = entry
        .lines()
        .find_map(|line| line.strip_prefix("Exec="))?
        .trim();
    let program = match exec.strip_prefix('"') {
        Some(quoted) => quoted.split('"').next()?,
        None => exec.split_whitespace().next()?,
    };
    Some(program.to_string())
}

/// Write `ommata.desktop` and its icon for `program` into the user's
/// folders; returns the entry's name.
fn write_entry(program: &Path) -> Result<String, String> {
    let data = application_dirs()
        .into_iter()
        .next()
        .and_then(|dir| dir.parent().map(Path::to_path_buf))
        .ok_or("no home folder")?;
    let icon = data.join("icons/hicolor/scalable/apps/ommata.svg");
    let entry_path = data.join("applications/ommata.desktop");
    let exec = format!("Exec=\"{}\" %f", program.display());
    let text: String = ENTRY
        .lines()
        .map(|line| {
            if line.starts_with("Exec=") {
                exec.as_str()
            } else {
                line
            }
        })
        .map(|line| format!("{line}\n"))
        .collect();
    for (path, contents) in [(&icon, ICON), (&entry_path, text.as_str())] {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|error| error.to_string())?;
        }
        std::fs::write(path, contents).map_err(|error| error.to_string())?;
    }
    // Optional: menus read the folder again on their own sooner or later.
    let _ = Command::new("update-desktop-database")
        .arg(data.join("applications"))
        .status();
    Ok("ommata.desktop".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_photos_raw_and_video() {
        let types = mime_types();
        for kind in [
            "image/jpeg",
            "image/x-sony-arw",
            "video/mp4",
            "video/x-matroska",
        ] {
            assert!(types.contains(&kind), "{kind}");
        }
    }

    #[test]
    fn reads_the_program_of_an_entry() {
        let entry = "[Desktop Entry]\nExec=/opt/app/ommata %f\n";
        assert_eq!(exec_program(entry).as_deref(), Some("/opt/app/ommata"));
        let quoted = "Exec=\"/home/me/My Apps/ommata.AppImage\" %f\n";
        assert_eq!(
            exec_program(quoted).as_deref(),
            Some("/home/me/My Apps/ommata.AppImage")
        );
    }
}
