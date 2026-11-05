//! Updates from GitHub releases and problem reports to GitHub issues. Both go
//! through `curl` and `xdg-open`, which every desktop Linux has, so ommata
//! carries no HTTP or TLS code of its own.
//!
//! The newest version comes from where `releases/latest` redirects to, at
//! most once a day. An AppImage can replace itself: the new AppImage is
//! downloaded next to the running one, must start and report the expected
//! version, and only then takes its place.

use eframe::egui;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
const REPO: &str = "https://github.com/mmzim05/Ommata";
const CHECK_EVERY: Duration = Duration::from_secs(24 * 60 * 60);

/// Where the update stands.
#[derive(Clone, PartialEq, Debug)]
pub enum State {
    /// Not checked this session.
    Unknown,
    Checking,
    UpToDate,
    /// Checking failed, with this error.
    CheckFailed(String),
    /// This version is out.
    Available(String),
    /// Downloading this version.
    Downloading(String),
    /// This version replaced the AppImage; it runs from the next start.
    Installed(String),
    /// Installing this version failed, with this error.
    Failed(String, String),
}

/// Update state shared with the background thread.
#[derive(Clone)]
pub struct Updater {
    state: Arc<Mutex<State>>,
    ctx: egui::Context,
}

impl Updater {
    /// With `automatic`, check in the background when the last check is a
    /// day old.
    pub fn new(ctx: &egui::Context, automatic: bool) -> Self {
        let updater = Self {
            state: Arc::new(Mutex::new(State::Unknown)),
            ctx: ctx.clone(),
        };
        if automatic && check_mark().is_none_or(|mark| check_due(&mark)) {
            updater.check();
        }
        updater
    }

    /// Ask GitHub for the newest release now.
    pub fn check(&self) {
        self.set(State::Checking);
        let thread = self.clone();
        std::thread::spawn(move || {
            let state = match latest_version() {
                Ok(latest) => {
                    if let Some(mark) = check_mark() {
                        if let Some(dir) = mark.parent() {
                            let _ = std::fs::create_dir_all(dir);
                        }
                        let _ = std::fs::write(&mark, "");
                    }
                    if is_newer(&latest, VERSION) {
                        State::Available(latest)
                    } else {
                        State::UpToDate
                    }
                }
                Err(error) => State::CheckFailed(error),
            };
            thread.set(state);
        });
    }

    pub fn state(&self) -> State {
        crate::lock(&self.state).clone()
    }

    fn set(&self, state: State) {
        *crate::lock(&self.state) = state;
        self.ctx.request_repaint();
    }

    /// Download and install `version` over the running AppImage.
    pub fn install(&self, version: String) {
        let Some(appimage) = appimage() else {
            return;
        };
        self.set(State::Downloading(version.clone()));
        let thread = self.clone();
        std::thread::spawn(move || {
            let state = match replace_appimage(&appimage, &version) {
                Ok(()) => State::Installed(version),
                Err(error) => State::Failed(version, error),
            };
            thread.set(state);
        });
    }
}

/// The running AppImage, which the AppImage runtime names in `$APPIMAGE`.
pub fn appimage() -> Option<PathBuf> {
    std::env::var_os("APPIMAGE")
        .map(PathBuf::from)
        .filter(|path| path.is_file())
}

/// Start the installed AppImage on `file` and let this instance quit.
pub fn restart(file: Option<&Path>) -> Result<(), String> {
    let appimage = appimage().ok_or("not running as an AppImage")?;
    Command::new(appimage)
        .args(file)
        .spawn()
        .map(drop)
        .map_err(|error| error.to_string())
}

/// The release page of `version`.
pub fn release_url(version: &str) -> String {
    format!("{REPO}/releases/tag/v{version}")
}

/// Every release, with its notes.
pub fn releases_url() -> String {
    format!("{REPO}/releases")
}

/// Open `url` in the browser.
pub fn open_url(url: &str) -> Result<(), String> {
    Command::new("xdg-open")
        .arg(url)
        .spawn()
        .map(drop)
        .map_err(|error| format!("couldn't open the browser: {error}"))
}

/// How this copy was installed.
pub fn install_kind() -> &'static str {
    if appimage().is_some() {
        "AppImage"
    } else if is_system_package() {
        "system package"
    } else {
        "built from source"
    }
}

/// Whether this copy is a distribution package (installed under /usr by
/// pacman), which updates with the rest of the system.
pub fn is_system_package() -> bool {
    std::env::current_exe().is_ok_and(|path| path.starts_with("/usr/bin"))
}

/// A new GitHub issue from the bug report form, with the version and the
/// system details filled in.
pub fn report_url(details: &str) -> String {
    format!(
        "{REPO}/issues/new?template=bug_report.yml&version={}&environment={}",
        encode(&format!("{VERSION} ({})", install_kind())),
        encode(details)
    )
}

/// A new GitHub issue from the feature request form.
pub fn feature_url() -> String {
    format!("{REPO}/issues/new?template=feature_request.yml")
}

/// What a problem report says about the system: distro, desktop, the GPU as
/// GL names it, and how video decodes.
pub fn system_details(renderer: &str, video: &str) -> String {
    let desktop = [
        std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default(),
        std::env::var("XDG_SESSION_TYPE").unwrap_or_default(),
    ]
    .join(" ");
    format!(
        "- System: {}\n- Desktop: {}\n- GPU: {renderer}\n- Video decoding: {video}",
        os_name(),
        desktop.trim()
    )
}

fn os_name() -> String {
    std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|text| {
            text.lines()
                .find_map(|line| line.strip_prefix("PRETTY_NAME="))
                .map(|name| name.trim_matches('"').to_string())
        })
        .unwrap_or_else(|| "Linux".into())
}

/// Percent-encode for a URL query value.
fn encode(text: &str) -> String {
    text.bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                char::from(byte).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

/// The file whose modification time is the last successful check.
fn check_mark() -> Option<PathBuf> {
    let dir = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))?;
    Some(dir.join("ommata/update-check"))
}

/// Whether a day has passed since the last successful check.
fn check_due(mark: &Path) -> bool {
    std::fs::metadata(mark)
        .and_then(|meta| meta.modified())
        .map_or(true, |last| {
            SystemTime::now()
                .duration_since(last)
                .map_or(true, |age| age >= CHECK_EVERY)
        })
}

/// The newest release's version, from where `releases/latest` redirects
/// (`.../releases/tag/vX.Y.Z`).
fn latest_version() -> Result<String, String> {
    let output = Command::new("curl")
        .args(["-fsS", "--max-time", "15", "-o", "/dev/null"])
        .args(["-w", "%{redirect_url}"])
        .arg(format!("{REPO}/releases/latest"))
        .output()
        .map_err(|error| format!("couldn't run curl: {error}"))?;
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr);
        return Err(error.trim().trim_start_matches("curl: ").to_string());
    }
    let url = String::from_utf8_lossy(&output.stdout);
    url.trim()
        .rsplit_once("/tag/v")
        .map(|(_, version)| version)
        .filter(|version| parse(version).is_some())
        .map(str::to_string)
        .ok_or_else(|| "no release found".into())
}

fn parse(version: &str) -> Option<(u64, u64, u64)> {
    let mut parts = version.split('.').map(|part| part.parse().ok());
    let parsed = (parts.next()??, parts.next()??, parts.next()??);
    parts.next().is_none().then_some(parsed)
}

fn is_newer(version: &str, than: &str) -> bool {
    parse(version)
        .zip(parse(than))
        .is_some_and(|(version, than)| version > than)
}

/// Download `version` next to `appimage`, check it starts, then rename it
/// over `appimage`. The running instance keeps its open copy until it quits.
fn replace_appimage(appimage: &Path, version: &str) -> Result<(), String> {
    let url = format!("{REPO}/releases/download/v{version}/ommata-{version}-x86_64.AppImage");
    let mut download = appimage.as_os_str().to_owned();
    download.push(".new");
    let download = PathBuf::from(download);
    let result = (|| {
        let status = Command::new("curl")
            .args(["-fsSL", "--max-time", "600", "-o"])
            .arg(&download)
            .arg(&url)
            .status()
            .map_err(|error| format!("couldn't run curl: {error}"))?;
        if !status.success() {
            return Err(format!("download failed (curl exit code {status})"));
        }
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&download, std::fs::Permissions::from_mode(0o755))
            .map_err(|error| error.to_string())?;
        let output = Command::new(&download)
            .arg("--version")
            .output()
            .map_err(|error| format!("the new version doesn't start: {error}"))?;
        let reported = String::from_utf8_lossy(&output.stdout);
        if reported.trim() != format!("ommata {version}") {
            return Err(format!(
                "the download reports {:?}, not ommata {version}",
                reported.trim()
            ));
        }
        std::fs::rename(&download, appimage).map_err(|error| error.to_string())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&download);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions() {
        assert!(is_newer("0.1.1", "0.1.0"));
        assert!(is_newer("0.10.0", "0.9.9"));
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.0.9", "0.1.0"));
        assert!(!is_newer("0.2", "0.1.0"));
        assert!(!is_newer("0.2.0-rc1", "0.1.0"));
    }

    #[test]
    fn encodes_query_values() {
        assert_eq!(encode("0.1.0 (AppImage)"), "0.1.0%20%28AppImage%29");
        assert_eq!(encode("- GPU: a/b\n"), "-%20GPU%3A%20a%2Fb%0A");
    }
}

#[cfg(test)]
mod network_tests {
    /// Downloads a real release over a scratch file:
    /// OMMATA_TEST_UPDATE=/tmp/x.AppImage cargo test -- --ignored replaces
    #[test]
    #[ignore]
    fn replaces_appimage_with_release() {
        let path = std::path::PathBuf::from(std::env::var("OMMATA_TEST_UPDATE").unwrap());
        std::fs::write(&path, "old").unwrap();
        let latest = super::latest_version().unwrap();
        super::replace_appimage(&path, &latest).unwrap();
        assert!(std::fs::metadata(&path).unwrap().len() > 1_000_000);
    }
}
