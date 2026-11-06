//! Window, input, and the save flow. Edits stay in memory per file until the user
//! saves (overwrite in place) or saves a copy (`name-edit.ext`).

use crate::gpu::Gpu;
use crate::library::{self, Kind};
use crate::picture::{self, Decoded, Edit};
use crate::settings::Settings;
use crate::sidecar::{self, Meta};
use crate::tags::{self, Filter};
use crate::theme::{Theme, lerp};
use crate::trim::{self, Progress};
use crate::update::{self, Updater};
use crate::video::Player;
use crate::waveform::Waveform;
use eframe::egui::{
    self, Align2, Color32, CornerRadius, CursorIcon, FontId, Id, Key, Mesh, Pos2, Rect, Sense,
    Shape, Stroke, TextureHandle, TextureOptions, Vec2, pos2, vec2,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;
use std::time::Duration;

/// Idle time after which the bars fade out and the cursor hides.
const CHROME_SECS: f64 = 3.0;

/// A titled group of (keys, action) rows in the Keybinds tab.
type ShortcutGroup = (&'static str, &'static [(&'static str, &'static str)]);

/// The keyboard shortcuts listed in Help.
const SHORTCUTS: [ShortcutGroup; 6] = [
    (
        "Browse",
        &[
            ("← →", "Previous / next file"),
            ("Home  End", "First / last file"),
            ("Del", "Move to trash"),
            ("F2", "Rename"),
            ("Ctrl+O", "Open a file"),
        ],
    ),
    (
        "Video",
        &[
            ("Space", "Play / pause"),
            ("J  L", "Back / forward 5 seconds"),
            (",  .", "Back / forward one frame"),
            ("M", "Mute"),
            ("↑ ↓", "Volume"),
            ("T", "Trim"),
            ("I  O", "Set trim start / end"),
            ("Esc", "Leave trim"),
        ],
    ),
    (
        "Tags",
        &[
            ("Alt+1 – 9", "Quick tag on / off"),
            ("Ctrl+1 – 5", "Rating (again: clear)"),
            ("Ctrl+0", "Clear rating"),
            ("X", "Reject"),
            ("Ctrl+6 – 9", "Red, yellow, green, blue label"),
            ("Ctrl+K", "Tag bar"),
            ("Ctrl+F", "Filter"),
        ],
    ),
    (
        "Photo",
        &[
            ("C", "Crop"),
            ("1 – 5", "Crop shape, while cropping"),
            ("Enter", "Apply crop"),
            ("R  Shift+R", "Rotate right / left"),
            ("H", "Flip"),
            ("Esc", "Cancel crop"),
        ],
    ),
    (
        "View",
        &[
            ("Scroll  + -", "Zoom"),
            ("0  1", "Fit to window / actual size"),
            ("F", "Fullscreen"),
            ("?", "Help"),
        ],
    ),
    (
        "Save",
        &[
            ("Ctrl+S", "Save over the original"),
            ("Ctrl+Shift+S", "Save a copy"),
            ("Ctrl+Z", "Undo all edits"),
            ("Ctrl+Q", "Quit"),
        ],
    ),
];

type LoadResult = (PathBuf, Result<Decoded, String>);

/// Background image decoding. Created before the window so the first image
/// decodes while the GL context is still starting up.
pub struct Loader {
    sender: Sender<LoadResult>,
    receiver: Receiver<LoadResult>,
    ctx: Arc<OnceLock<egui::Context>>,
    max_side: Arc<AtomicU32>,
}

impl Loader {
    /// A loader with no window yet; `App::new` connects it to the context.
    pub fn new() -> Self {
        let (sender, receiver) = channel();
        Self {
            sender,
            receiver,
            ctx: Default::default(),
            max_side: Arc::new(AtomicU32::new(8192)),
        }
    }

    fn spawn(&self, path: PathBuf) {
        let sender = self.sender.clone();
        let (ctx, max_side) = (self.ctx.clone(), self.max_side.clone());
        std::thread::spawn(move || {
            let result = picture::decode(&path, max_side.load(Relaxed));
            // The receiver lives as long as the app; a closed one means exit.
            let _ = sender.send((path, result));
            if let Some(ctx) = ctx.get() {
                ctx.request_repaint();
            }
        });
    }
}

/// The file list the app opens with, and which file to show first.
pub struct Start {
    files: Vec<PathBuf>,
    index: usize,
}

/// Turn a CLI path (file or folder) into a file list and start decoding.
pub fn resolve_start(path: &Path, loader: &Loader) -> Start {
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let (files, index) = if path.is_dir() {
        (library::siblings(&path.join("x")), 0)
    } else {
        let files = library::siblings(&path);
        match files.iter().position(|file| *file == path) {
            Some(index) => (files, index),
            None => (vec![path], 0),
        }
    };
    if let Some(first) = files
        .get(index)
        .filter(|file| library::kind(file) == Some(Kind::Image))
    {
        loader.spawn(first.clone());
    }
    Start { files, index }
}

enum Slot {
    Loading,
    Ready {
        texture: TextureHandle,
        size: [usize; 2],
    },
    Failed(String),
}

#[derive(Clone, Copy, PartialEq)]
struct Trim {
    start: f64,
    end: f64,
}

#[derive(Clone, Copy, PartialEq)]
enum Handle {
    Start,
    End,
    Seek,
}

#[derive(Clone, Copy, PartialEq)]
enum CropDrag {
    Move,
    Edge {
        left: bool,
        right: bool,
        top: bool,
        bottom: bool,
    },
    New(Pos2),
}

/// A press that hits no edge of the crop rect.
const NO_EDGE: CropDrag = CropDrag::Edge {
    left: false,
    right: false,
    top: false,
    bottom: false,
};

struct CropTool {
    rect: Rect,
    aspect: Option<f32>,
    /// Active drag: what it does, the rect when it began, and where it began
    /// (normalized).
    drag: Option<(CropDrag, Rect, Pos2)>,
}

/// Zoom out stops a little past fitting the window; zoom in at 50x.
fn clamp_zoom(zoom: f32, fit_scale: f32) -> f32 {
    zoom.clamp(fit_scale * 0.85, 50.0)
}

/// Padding inside the file name box, shown or edited.
const NAME_MARGIN: Vec2 = vec2(6.0, 4.0);

/// Renaming the current file in the top bar.
struct Rename {
    name: String,
    /// Focus the field and select the name without its extension.
    fresh: bool,
}

/// A window over everything, the same from the start screen and a file.
#[derive(Clone, Copy, PartialEq)]
enum Page {
    Settings,
    Help(HelpTab),
    /// Edit tags: remove the current file's tags, or pick the folder's.
    Tags,
    /// Which files browsing stops on.
    Filter,
}

#[derive(Clone, Copy, PartialEq)]
enum HelpTab {
    Keybinds,
    Version,
    Report,
}

#[derive(Clone, Copy, PartialEq)]
enum SaveMode {
    Overwrite,
    Copy,
}

struct Job {
    src: PathBuf,
    mode: SaveMode,
    progress: Arc<Progress>,
    /// Saved path, plus a note for the user if something needs their attention.
    handle: JoinHandle<Result<(PathBuf, Option<String>), String>>,
}

/// The ommata window.
pub struct App {
    theme: Theme,
    loader: Loader,
    files: Vec<PathBuf>,
    index: usize,
    images: HashMap<PathBuf, Slot>,
    player: Option<(PathBuf, Player)>,
    edits: HashMap<PathBuf, Edit>,
    trims: HashMap<PathBuf, Trim>,
    trim_mode: bool,
    snap_sender: Sender<(PathBuf, f64)>,
    snap_receiver: Receiver<(PathBuf, f64)>,
    drag: Option<Handle>,
    zoom: Option<f32>,
    pan: Vec2,
    /// The scale that fits the current image in the window, from the last frame.
    fit_scale: f32,
    crop: Option<CropTool>,
    job: Option<Job>,
    toast: Option<(String, f64)>,
    chrome_until: f64,
    confirm_quit: bool,
    /// The trash confirmation is open, with its "don't ask again" box.
    confirm_trash: Option<bool>,
    /// Trash without asking until ommata closes, for clearing out a folder.
    trash_without_asking: bool,
    /// Help or Settings, open over everything.
    page: Option<Page>,
    /// The new name being typed for the current file.
    rename: Option<Rename>,
    /// Every file in the folder; `files` is the part the filter lets through.
    listing: Vec<PathBuf>,
    /// Tags, rating and label per file, from the sidecars.
    meta: HashMap<PathBuf, Meta>,
    /// Files tagged this session: a folder scan still running must not
    /// overwrite them with what their sidecar said before.
    meta_written: std::collections::HashSet<PathBuf>,
    meta_sender: Sender<Vec<(PathBuf, Meta)>>,
    meta_receiver: Receiver<Vec<(PathBuf, Meta)>>,
    filter: Filter,
    /// The new tag being typed in the tag bar or Edit tags.
    tag_input: String,
    /// The tag bar under the top bar is shown.
    tag_bar: bool,
    /// Put the cursor in the tag bar's field on the next frame.
    focus_tag_input: bool,
    /// The top bar is too wide for the window, so its controls take a
    /// second row.
    top_bar_wrap: bool,
    /// Natural widths of the top bar's name and controls, last frame.
    top_bar_widths: (f32, f32),
    quitting: bool,
    settings: Settings,
    /// Set while the playhead is dragged: whether to resume playing after.
    resume_after_drag: Option<bool>,
    wave: Option<(PathBuf, Arc<Waveform>)>,
    /// Seek-bar columns, cached by (columns, smoothing, levels decoded so far).
    wave_cache: Option<((usize, u32, usize), Vec<f32>)>,
    /// Zero-copy video on the window's GL context, when available.
    gpu: Option<Gpu>,
    updater: Updater,
    /// The GPU, as GL names it, for problem reports.
    renderer: String,
    /// How video decodes, and why not on the GPU, for problem reports.
    video_decoding: String,
}

impl App {
    /// Set up the window and open `start`, if given.
    pub fn new(creation: &eframe::CreationContext, loader: Loader, start: Option<Start>) -> Self {
        let ctx = &creation.egui_ctx;
        // Set only here, once per loader.
        let _ = loader.ctx.set(ctx.clone());
        // egui reports the GPU's texture limit only from the first frame on
        // (2048 until then), so ask GL directly.
        if let Some(gl) = &creation.gl {
            use eframe::glow::{self, HasContext};
            let side = unsafe { gl.get_parameter_i32(glow::MAX_TEXTURE_SIZE) };
            loader.max_side.store(side.max(2048) as u32, Relaxed);
        }
        ctx.all_styles_mut(|style| {
            style.spacing.button_padding = vec2(10.0, 6.0);
            style.spacing.item_spacing = vec2(6.0, 6.0);
            style.spacing.interact_size.y = 28.0;
            style.interaction.selectable_labels = false;
        });
        // Ctrl with + - 0 would scale the whole interface; Ctrl+0 to 5 rate.
        ctx.options_mut(|options| options.zoom_with_keyboard = false);
        let renderer = creation.gl.as_ref().map_or("no OpenGL".into(), |gl| {
            use eframe::glow::{self, HasContext};
            unsafe { gl.get_parameter_string(glow::RENDERER) }
        });
        let mut video_decoding = "CPU (no OpenGL)".to_string();
        let gpu = creation.gl.clone().and_then(|gl| {
            Gpu::new(gl)
                .inspect(|_| video_decoding = "GPU (VA-API)".into())
                .inspect_err(|error| {
                    eprintln!("ommata: video decodes on the CPU: {error}");
                    video_decoding = format!("CPU ({error})");
                })
                .ok()
        });
        let (snap_sender, snap_receiver) = channel();
        let (meta_sender, meta_receiver) = channel();
        let settings = Settings::load();
        let check_updates = settings.check_updates;
        let mut app = Self {
            theme: Theme::new(ctx),
            loader,
            files: Vec::new(),
            index: 0,
            images: HashMap::new(),
            player: None,
            edits: HashMap::new(),
            trims: HashMap::new(),
            trim_mode: false,
            snap_sender,
            snap_receiver,
            drag: None,
            zoom: None,
            pan: Vec2::ZERO,
            fit_scale: 1.0,
            crop: None,
            job: None,
            toast: None,
            chrome_until: CHROME_SECS,
            confirm_quit: false,
            confirm_trash: None,
            trash_without_asking: false,
            page: None,
            rename: None,
            listing: Vec::new(),
            meta: HashMap::new(),
            meta_written: Default::default(),
            meta_sender,
            meta_receiver,
            filter: Filter::default(),
            tag_input: String::new(),
            tag_bar: false,
            focus_tag_input: false,
            top_bar_wrap: false,
            top_bar_widths: (0.0, 0.0),
            quitting: false,
            settings,
            resume_after_drag: None,
            wave: None,
            wave_cache: None,
            gpu,
            updater: Updater::new(ctx, check_updates),
            renderer,
            video_decoding,
        };
        if let Some(start) = start {
            app.open_start(ctx, start);
        }
        app
    }

    /// Show the file list of `start`, whose first image is already decoding.
    fn open_start(&mut self, ctx: &egui::Context, start: Start) {
        if let Some(first) = start.files.get(start.index)
            && library::kind(first) == Some(Kind::Image)
        {
            self.images.insert(first.clone(), Slot::Loading);
        }
        let focus = start.files.get(start.index).cloned();
        self.set_listing(ctx, start.files, focus.as_deref());
    }

    /// Show the files of a folder, `focus` first, through the filter, and
    /// read their sidecars in the background.
    fn set_listing(&mut self, ctx: &egui::Context, listing: Vec<PathBuf>, focus: Option<&Path>) {
        self.listing = listing;
        self.load_meta(ctx);
        self.apply_filter(ctx, focus.map(Path::to_path_buf));
    }

    /// Read the sidecars of every file in the listing on a background thread.
    fn load_meta(&mut self, ctx: &egui::Context) {
        let (files, sender, ctx) = (self.listing.clone(), self.meta_sender.clone(), ctx.clone());
        std::thread::spawn(move || {
            let read = files
                .into_iter()
                .map(|file| {
                    let meta = sidecar::read(&file);
                    (file, meta)
                })
                .collect();
            // The receiver lives as long as the app; a closed one means exit.
            let _ = sender.send(read);
            ctx.request_repaint();
        });
    }

    fn meta_of(&self, path: &Path) -> Meta {
        self.meta.get(path).cloned().unwrap_or_default()
    }

    /// Every tag in the folder's sidecars.
    fn folder_tags(&self) -> std::collections::BTreeSet<String> {
        self.listing
            .iter()
            .filter_map(|path| self.meta.get(path))
            .flat_map(|meta| meta.tags.iter().cloned())
            .collect()
    }

    /// How many files the filter lets through, of how many.
    fn filter_count(&self) -> (usize, usize) {
        let matching = self
            .listing
            .iter()
            .filter(|path| self.filter.matches(&self.meta_of(path)))
            .count();
        (matching, self.listing.len())
    }

    /// Narrow `files` to the listing's matches, staying on `keep` (or the
    /// current file): it stays in the list even when it no longer matches,
    /// so tagging a file never jumps away from it. With no match at all the
    /// whole folder stays browsable.
    fn apply_filter(&mut self, ctx: &egui::Context, keep: Option<PathBuf>) {
        let keep = keep.or_else(|| self.current().cloned());
        let (files, index) = tags::narrow(&self.listing, keep.as_deref(), &self.filter, |path| {
            self.meta_of(path)
        });
        let same = files.get(index) == self.current() && !self.files.is_empty();
        self.files = files;
        if same {
            self.index = index;
        } else {
            self.go(ctx, index);
        }
    }

    /// Store new tags for `path` in its sidecar.
    fn set_meta(&mut self, ctx: &egui::Context, path: &Path, meta: Meta) {
        match sidecar::write(path, &meta) {
            Ok(()) => {
                self.meta_written.insert(path.to_path_buf());
                self.meta.insert(path.to_path_buf(), meta);
            }
            Err(error) => self.notify(ctx, format!("Couldn't save tags: {error}")),
        }
    }

    /// Change the current file's tags with `change`.
    fn edit_meta(&mut self, ctx: &egui::Context, change: impl FnOnce(&mut Meta)) {
        let Some(path) = self.current().cloned() else {
            return;
        };
        let mut meta = self.meta_of(&path);
        change(&mut meta);
        self.set_meta(ctx, &path, meta);
    }

    /// Alt+1 to 9: add or remove quick tag `index`.
    fn toggle_quick_tag(&mut self, ctx: &egui::Context, index: usize) {
        let tag = tags::normalize(&self.settings.quick_tags[index]);
        if tag.is_empty() {
            self.notify(ctx, format!("Quick tag {} isn't set yet (Settings)", index + 1));
            return;
        }
        let mut added = false;
        self.edit_meta(ctx, |meta| {
            added = meta.tags.insert(tag.clone());
            if !added {
                meta.tags.remove(&tag);
            }
        });
        self.notify(ctx, if added { format!("Tagged {tag}") } else { format!("Removed {tag}") });
    }

    /// Ctrl+0 to 5: set the rating; the same rating again clears it.
    fn rate(&mut self, ctx: &egui::Context, rating: i8) {
        let mut now = 0;
        self.edit_meta(ctx, |meta| {
            meta.rating = if meta.rating == rating { 0 } else { rating };
            now = meta.rating;
        });
        self.notify(ctx, match now {
            -1 => "Rejected".to_string(),
            0 => "No rating".to_string(),
            stars => format!("{stars} star{}", if stars == 1 { "" } else { "s" }),
        });
    }

    /// Ctrl+6 to 9: set a colour label; the same one again clears it.
    fn set_label(&mut self, ctx: &egui::Context, label: &str) {
        let mut now = None;
        self.edit_meta(ctx, |meta| {
            let same = meta.label.as_deref().is_some_and(|own| own.eq_ignore_ascii_case(label));
            meta.label = (!same).then(|| label.to_string());
            now = meta.label.clone();
        });
        self.notify(ctx, match now {
            Some(label) => format!("{label} label"),
            None => "No label".to_string(),
        });
    }

    fn current(&self) -> Option<&PathBuf> {
        self.files.get(self.index)
    }

    fn current_kind(&self) -> Option<Kind> {
        self.current().and_then(|path| library::kind(path))
    }

    fn open_path(&mut self, ctx: &egui::Context, path: &Path) {
        let start = resolve_start(path, &self.loader);
        self.open_start(ctx, start);
    }

    /// Show file `index`: start or reuse decodes, preload neighbours, drop the rest.
    fn go(&mut self, ctx: &egui::Context, index: usize) {
        if self.files.is_empty() {
            self.player = None;
            ctx.send_viewport_cmd(egui::ViewportCommand::Title("Ommata".into()));
            return;
        }
        self.index = index.min(self.files.len() - 1);
        self.zoom = None;
        self.pan = Vec2::ZERO;
        self.crop = None;
        self.drag = None;
        let path = self.files[self.index].clone();
        self.trim_mode = self.trims.contains_key(&path);

        let count = self.files.len();
        let keep: Vec<PathBuf> = [count - 1, 0, 1]
            .iter()
            .map(|offset| self.files[(self.index + offset) % count].clone())
            .collect();
        self.images.retain(|image, _| keep.contains(image));
        for image in &keep {
            if library::kind(image) == Some(Kind::Image) && !self.images.contains_key(image) {
                self.images.insert(image.clone(), Slot::Loading);
                self.loader.spawn(image.clone());
            }
        }

        match library::kind(&path) {
            Some(Kind::Video) => {
                if self.player.as_ref().is_none_or(|(open, _)| *open != path) {
                    let gpu = self.gpu.as_ref().filter(|gpu| !gpu.failed);
                    self.player = Some((path.clone(), Player::open(path.clone(), ctx, gpu)));
                }
            }
            _ => self.player = None,
        }
        self.sync_waveform(ctx);
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        ctx.send_viewport_cmd(egui::ViewportCommand::Title(format!("{name} — Ommata")));
    }

    /// Keep a waveform scan running for the current video while the setting is on.
    fn sync_waveform(&mut self, ctx: &egui::Context) {
        let want = self
            .player
            .as_ref()
            .filter(|_| self.settings.waveform)
            .map(|(path, _)| path.clone());
        if self.wave.as_ref().map(|(path, _)| path) != want.as_ref() {
            self.wave = want.map(|path| (path.clone(), Waveform::spawn(path, ctx.clone())));
            self.wave_cache = None;
        }
    }

    fn wave_columns(&mut self, columns: usize, duration: f64) -> Option<&[f32]> {
        let (_, waveform) = self.wave.as_ref()?;
        if waveform.none.load(Relaxed) {
            return None;
        }
        let smooth = self.settings.wave_smooth;
        let key = (columns, smooth.to_bits(), waveform.len());
        if self
            .wave_cache
            .as_ref()
            .is_none_or(|(cached, _)| *cached != key)
        {
            self.wave_cache = Some((key, waveform.columns(columns, duration, smooth)));
        }
        self.wave_cache
            .as_ref()
            .map(|(_, values)| values.as_slice())
    }

    fn step(&mut self, ctx: &egui::Context, delta: isize) {
        // Tags may have changed since the filter last ran.
        if self.filter.is_active() {
            self.apply_filter(ctx, None);
        }
        if self.files.len() > 1 {
            let count = self.files.len() as isize;
            self.go(
                ctx,
                (self.index as isize + delta).rem_euclid(count) as usize,
            );
        }
    }

    fn reload_listing(&mut self, ctx: &egui::Context, focus: &Path) {
        self.set_listing(ctx, library::siblings(focus), Some(focus));
    }

    fn is_dirty(&self, path: &Path) -> bool {
        self.edits.get(path).is_some_and(|edit| !edit.is_identity())
            || self
                .trims
                .get(path)
                .is_some_and(|trim| self.trim_is_change(trim))
    }

    fn trim_is_change(&self, trim: &Trim) -> bool {
        let duration = self
            .player
            .as_ref()
            .map_or(f64::INFINITY, |(_, player)| player.duration());
        trim.start > 0.01 || trim.end < duration - 0.01
    }

    fn any_dirty(&self) -> bool {
        self.edits.values().any(|edit| !edit.is_identity()) || !self.trims.is_empty()
    }

    fn notify(&mut self, ctx: &egui::Context, message: impl Into<String>) {
        self.toast = Some((message.into(), ctx.input(|input| input.time) + 2.5));
    }

    // ---- background results -------------------------------------------------

    fn poll(&mut self, ctx: &egui::Context) {
        let mut scanned = false;
        while let Ok(read) = self.meta_receiver.try_recv() {
            for (path, meta) in read {
                if !self.meta_written.contains(&path) {
                    self.meta.insert(path, meta);
                }
            }
            scanned = true;
        }
        if scanned && self.filter.is_active() {
            self.apply_filter(ctx, None);
        }
        while let Ok((path, result)) = self.loader.receiver.try_recv() {
            if !self.images.contains_key(&path) {
                continue;
            }
            let slot = match result {
                Ok(decoded) => Slot::Ready {
                    texture: ctx.load_texture(
                        path.to_string_lossy(),
                        decoded.image,
                        TextureOptions::LINEAR,
                    ),
                    size: decoded.size,
                },
                Err(error) => Slot::Failed(error),
            };
            self.images.insert(path, slot);
        }
        while let Ok((path, cut)) = self.snap_receiver.try_recv() {
            if let Some(trim) = self.trims.get_mut(&path) {
                trim.start = cut.min(trim.end);
                // Show the frame the trimmed video will actually start on.
                if let Some((open, player)) = &mut self.player
                    && *open == path
                    && player.paused
                {
                    player.seek(trim.start, true);
                }
            }
        }
        let finished = self
            .job
            .as_ref()
            .is_some_and(|job| job.handle.is_finished());
        if let Some(job) = self.job.take_if(|_| finished) {
            match job
                .handle
                .join()
                .unwrap_or_else(|_| Err("save crashed".into()))
            {
                Ok((out, note)) => {
                    // A RAW saved over as name.dng: its tags move to the DNG.
                    if job.mode == SaveMode::Overwrite && out != job.src {
                        if let Err(error) = sidecar::follow_rename(&job.src, &out) {
                            eprintln!("ommata: tags of {}: {error}", job.src.display());
                        }
                        if let Some(meta) = self.meta.remove(&job.src) {
                            self.meta.insert(out.clone(), meta);
                        }
                    }
                    self.edits.remove(&job.src);
                    self.trims.remove(&job.src);
                    self.images.remove(&job.src);
                    if self
                        .player
                        .as_ref()
                        .is_some_and(|(open, _)| *open == job.src)
                    {
                        self.player = None;
                    }
                    let name = out
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string();
                    let message = match (job.mode, note) {
                        (_, Some(note)) => note,
                        (SaveMode::Overwrite, None) => format!("Saved {name}"),
                        (SaveMode::Copy, None) => format!("Saved copy {name}"),
                    };
                    self.notify(ctx, message);
                    self.reload_listing(ctx, &out);
                }
                Err(error) => self.notify(ctx, format!("Save failed: {error}")),
            }
        } else if self.job.is_some() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }

    // ---- saving -------------------------------------------------------------

    fn save(&mut self, ctx: &egui::Context, mode: SaveMode) {
        if self.job.is_some() {
            return;
        }
        if self.crop.is_some() {
            self.apply_crop();
        }
        let Some(src) = self.current().cloned() else {
            return;
        };
        if !self.is_dirty(&src) {
            self.notify(ctx, "No changes to save");
            return;
        }
        // Camera RAW saves as DNG: over the original, `name.dng` replaces it
        // (the original goes to the trash).
        let raw = picture::is_raw(&src);
        let dst = match mode {
            SaveMode::Overwrite if raw => src.with_extension("dng"),
            SaveMode::Overwrite => src.clone(),
            SaveMode::Copy => library::edit_copy_path(&src, raw.then_some("dng")),
        };
        if dst != src && mode == SaveMode::Overwrite && dst.exists() {
            let name = dst.file_name().unwrap_or_default().to_string_lossy();
            self.notify(ctx, format!("{name} already exists. Use Save copy"));
            return;
        }
        let progress = Arc::new(Progress::default());
        let handle = match library::kind(&src) {
            Some(Kind::Image) => {
                if !picture::can_save(&src) {
                    self.notify(ctx, "This format can't be saved");
                    return;
                }
                let edit = self.edits.get(&src).copied().unwrap_or_default();
                let (source, thread_ctx) = (src.clone(), ctx.clone());
                std::thread::spawn(move || {
                    let result =
                        save_to(&source, &dst, mode, |temp| picture::save(&source, temp, &edit));
                    thread_ctx.request_repaint();
                    result.map(|note| (dst, note))
                })
            }
            Some(Kind::Video) => {
                let Some(trim) = self.trims.get(&src).copied() else {
                    return;
                };
                let (source, thread_ctx) = (src.clone(), ctx.clone());
                let job_progress = progress.clone();
                std::thread::spawn(move || {
                    let result = save_to(&source, &dst, mode, |temp| {
                        trim::export(&source, temp, trim.start, trim.end, job_progress)
                    });
                    thread_ctx.request_repaint();
                    result.map(|note| (dst, note))
                })
            }
            None => return,
        };
        self.job = Some(Job {
            src,
            mode,
            progress,
            handle,
        });
    }

    fn revert(&mut self, ctx: &egui::Context) {
        if let Some(path) = self.current().cloned() {
            let had = self.is_dirty(&path) || self.crop.is_some();
            self.edits.remove(&path);
            self.trims.remove(&path);
            self.crop = None;
            self.trim_mode = false;
            if had {
                self.notify(ctx, "Edits reverted");
            }
        }
    }

    fn start_rename(&mut self) {
        if self.job.is_some() {
            return;
        }
        if let Some(path) = self.current() {
            let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
            self.rename = Some(Rename { name, fresh: true });
        }
    }

    /// The name field that replaces the file name while renaming: Enter
    /// renames, Esc or clicking away cancels.
    fn rename_field(&mut self, ctx: &egui::Context, ui: &mut egui::Ui, size: Vec2) {
        let Some(rename) = &mut self.rename else {
            return;
        };
        let id = Id::unique("rename");
        let field = egui::TextEdit::singleline(&mut rename.name)
            .id(id)
            .margin(NAME_MARGIN)
            .desired_width(size.x - 2.0 * NAME_MARGIN.x)
            .min_size(size)
            // Centred like the name it replaces, so the text doesn't move.
            .align(Align2::LEFT_CENTER);
        let response = ui.add(field);
        if rename.fresh {
            rename.fresh = false;
            response.request_focus();
            // Select the stem so typing replaces the name but keeps the extension.
            let stem = Path::new(&rename.name)
                .file_stem()
                .map_or(0, |stem| stem.to_string_lossy().chars().count());
            if let Some(mut state) = egui::TextEdit::load_state(ctx, id) {
                let range = egui::text::CCursorRange::two(
                    egui::text::CCursor::new(0),
                    egui::text::CCursor::new(stem),
                );
                state.cursor.set_char_range(Some(range));
                state.store(ctx, id);
            }
        } else if response.lost_focus() {
            let name = self.rename.take().map(|rename| rename.name);
            if let Some(name) = name.filter(|_| ctx.input(|input| input.key_pressed(Key::Enter))) {
                self.rename_current(ctx, &name);
            }
        }
    }

    /// Rename the current file within its folder, keeping its place, its
    /// unsaved edits and, for a video, the playback position.
    fn rename_current(&mut self, ctx: &egui::Context, name: &str) {
        let Some(old) = self.current().cloned() else {
            return;
        };
        let name = name.trim();
        if name.is_empty() || Some(name) == old.file_name().and_then(|old| old.to_str()) {
            return;
        }
        if name.contains('/') || name == "." || name == ".." {
            self.notify(ctx, "A file name can't contain /");
            return;
        }
        let new = old.with_file_name(name);
        if new.exists() {
            self.notify(ctx, format!("{name} already exists"));
            return;
        }
        if let Err(error) = std::fs::rename(&old, &new) {
            self.notify(ctx, format!("Couldn't rename: {error}"));
            return;
        }
        if let Err(error) = sidecar::follow_rename(&old, &new) {
            self.notify(ctx, format!("Renamed, but its tags stayed behind: {error}"));
        }
        if let Some(meta) = self.meta.remove(&old) {
            self.meta.insert(new.clone(), meta);
        }
        if let Some(slot) = self.images.remove(&old) {
            self.images.insert(new.clone(), slot);
        }
        if let Some(edit) = self.edits.remove(&old) {
            self.edits.insert(new.clone(), edit);
        }
        if let Some(trim) = self.trims.remove(&old) {
            self.trims.insert(new.clone(), trim);
        }
        // The player reads its file by path (previews, waveform), so it
        // reopens on the new name where it was.
        let resume = self
            .player
            .take()
            .filter(|(open, _)| *open == old)
            .map(|(_, player)| (player.position, player.paused));
        self.wave = None;
        self.reload_listing(ctx, &new);
        if let (Some((position, paused)), Some((_, player))) = (resume, &mut self.player) {
            player.seek(position, true);
            player.set_paused(paused);
        }
        self.notify(ctx, format!("Renamed to {name}"));
    }

    /// Trash the current file, after asking unless the user chose not to be
    /// asked again this session.
    fn ask_trash(&mut self, ctx: &egui::Context) {
        if self.current().is_none() {
            return;
        }
        if self.trash_without_asking {
            self.trash_current(ctx);
        } else {
            self.confirm_trash = Some(false);
        }
    }

    fn trash_current(&mut self, ctx: &egui::Context) {
        let Some(path) = self.current().cloned() else {
            return;
        };
        if self.player.as_ref().is_some_and(|(open, _)| *open == path) {
            self.player = None;
        }
        match trash::delete(&path) {
            Ok(()) => {
                // Its tags go to the trash with it, to come back together.
                if let Some(sidecar) = sidecar::find(&path)
                    && let Err(error) = trash::delete(&sidecar)
                {
                    eprintln!("ommata: trash {}: {error}", sidecar.display());
                }
                self.edits.remove(&path);
                self.trims.remove(&path);
                self.images.remove(&path);
                self.meta.remove(&path);
                self.listing.retain(|file| *file != path);
                self.files.remove(self.index);
                let name = path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                self.notify(ctx, format!("Moved {name} to trash"));
                let index = self.index.min(self.files.len().saturating_sub(1));
                self.go(ctx, index);
            }
            Err(error) => {
                self.notify(ctx, format!("Couldn't trash: {error}"));
                self.go(ctx, self.index);
            }
        }
    }

    fn open_dialog(&mut self, ctx: &egui::Context) {
        let start = self
            .current()
            .and_then(|path| path.parent().map(Path::to_path_buf));
        let mut dialog = rfd::FileDialog::new().set_title("Open");
        if let Some(dir) = start {
            dialog = dialog.set_directory(dir);
        }
        if let Some(path) = dialog.pick_file() {
            self.open_path(ctx, &path);
        }
    }

    // ---- crop ---------------------------------------------------------------

    fn start_crop(&mut self) {
        let Some(path) = self.current().cloned() else {
            return;
        };
        if self.current_kind() != Some(Kind::Image) {
            return;
        }
        let edit = self.edits.entry(path).or_default();
        let rect = edit
            .crop
            .take()
            .unwrap_or(Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0)));
        self.crop = Some(CropTool {
            rect,
            aspect: None,
            drag: None,
        });
        self.zoom = None;
        self.pan = Vec2::ZERO;
    }

    /// Pixel size of what the crop rect is normalized to.
    fn crop_space_size(&self) -> Option<[usize; 2]> {
        let path = self.current()?;
        let Slot::Ready { size, .. } = self.images.get(path)? else {
            return None;
        };
        Some(
            self.edits
                .get(path)
                .copied()
                .unwrap_or_default()
                .rotated_size(*size),
        )
    }

    fn set_crop_aspect(&mut self, aspect: Option<f32>) {
        let size = self.crop_space_size();
        let Some(tool) = &mut self.crop else { return };
        tool.aspect = aspect;
        if let (Some(aspect), Some(size)) = (aspect, size) {
            tool.rect = fit_aspect_centered(tool.rect, aspect, size);
        }
    }

    fn apply_crop(&mut self) {
        let (Some(tool), Some(path)) = (self.crop.take(), self.current().cloned()) else {
            return;
        };
        let full = tool.rect.width() > 0.999 && tool.rect.height() > 0.999;
        self.edits.entry(path).or_default().crop = (!full).then_some(tool.rect);
    }

    fn edit_mut(&mut self) -> Option<&mut Edit> {
        let path = self.current().cloned()?;
        (library::kind(&path) == Some(Kind::Image)).then(|| self.edits.entry(path).or_default())
    }

    /// Apply `change` to the current edit, moving the in-progress crop rect
    /// along with the image.
    fn transform(&mut self, change: impl Fn(&mut Edit)) {
        let tool = self.crop.take();
        let Some(edit) = self.edit_mut() else {
            return;
        };
        let mut moved = Edit {
            crop: tool.as_ref().map(|tool| tool.rect),
            ..*edit
        };
        change(&mut moved);
        change(edit);
        self.crop = tool
            .zip(moved.crop)
            .map(|(tool, rect)| CropTool { rect, ..tool });
    }

    fn rotate(&mut self, clockwise: bool) {
        self.transform(|edit| edit.rotate(clockwise));
        // A quarter turn swaps width and height.
        if let Some(tool) = &mut self.crop {
            tool.aspect = tool.aspect.map(|aspect| 1.0 / aspect);
            tool.drag = None;
        }
    }

    fn flip(&mut self) {
        self.transform(Edit::flip_h);
    }

    // ---- trim ---------------------------------------------------------------

    fn toggle_trim(&mut self) {
        let Some(path) = self.current().cloned() else {
            return;
        };
        let Some((_, player)) = &self.player else {
            return;
        };
        if self.trim_mode {
            self.trim_mode = false;
            if self
                .trims
                .get(&path)
                .is_some_and(|trim| !self.trim_is_change(trim))
            {
                self.trims.remove(&path);
            }
        } else {
            let duration = player.duration();
            self.trims.entry(path).or_insert(Trim {
                start: 0.0,
                end: duration,
            });
            self.trim_mode = true;
        }
    }

    /// Move a trim point. Dragging pauses and scrubs to the exact frame; `done`
    /// (end of a drag, or the I/O keys) moves the start to where the export
    /// will really cut: the frame itself for H.264/HEVC, else the keyframe at
    /// or before it. Keys set during playback don't interrupt it.
    fn set_trim_point(&mut self, start: bool, time: f64, done: bool) {
        let Some(path) = self.current().cloned() else {
            return;
        };
        let Some((_, player)) = &mut self.player else {
            return;
        };
        let duration = player.duration();
        if !done {
            player.set_paused(true);
        }
        self.trim_mode = true;
        let trim = self.trims.entry(path.clone()).or_insert(Trim {
            start: 0.0,
            end: duration,
        });
        let shown = if start {
            trim.start = time.min(trim.end - 0.05).max(0.0);
            if done {
                let (sender, requested) = (self.snap_sender.clone(), trim.start);
                std::thread::spawn(move || {
                    if let Some(cut) = trim::cut_point(&path, requested) {
                        // The app may have quit meanwhile; nothing to update.
                        let _ = sender.send((path, cut));
                    }
                });
            }
            trim.start
        } else {
            trim.end = time.max(trim.start + 0.05).min(duration);
            trim.end
        };
        if player.paused {
            if done {
                // Releasing: refine to the full-resolution exact frame.
                player.seek(shown, true);
            } else {
                player.scrub(shown);
            }
        }
    }

    // ---- input --------------------------------------------------------------

    fn keys(&mut self, ctx: &egui::Context) {
        // Ctrl+K closes the tag bar even while its field has the cursor.
        if self.tag_bar
            && self.page.is_none()
            && ctx.input_mut(|input| input.consume_key(egui::Modifiers::COMMAND, Key::K))
        {
            self.tag_bar = false;
            return;
        }
        if ctx.egui_wants_keyboard_input()
            || self.confirm_quit
            || self.confirm_trash.is_some()
            || self.page.is_some()
        {
            return;
        }
        let (pressed, modifiers) = ctx.input(|input| {
            let keys: Vec<Key> = input
                .events
                .iter()
                .filter_map(|event| match event {
                    egui::Event::Key {
                        key, pressed: true, ..
                    } => Some(*key),
                    _ => None,
                })
                .collect();
            (keys, input.modifiers)
        });
        for key in pressed {
            let command = modifiers.command;
            let digit = [
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
            ]
            .iter()
            .position(|number| *number == key);
            let has_file = self.current().is_some();
            match key {
                _ if has_file
                    && modifiers.alt
                    && !command
                    && digit.is_some_and(|digit| digit > 0) =>
                {
                    self.toggle_quick_tag(ctx, digit.unwrap_or(1) - 1);
                }
                _ if has_file && command && digit.is_some_and(|digit| digit <= 5) => {
                    self.rate(ctx, digit.unwrap_or(0) as i8);
                }
                _ if has_file && command && digit.is_some_and(|digit| digit >= 6) => {
                    let label = tags::LABELS[digit.unwrap_or(6) - 6].0;
                    self.set_label(ctx, label);
                }
                Key::X if has_file && !command => self.rate(ctx, -1),
                Key::K if has_file && command => self.toggle_tag_bar(),
                Key::F if command => self.page = Some(Page::Filter),
                Key::S if command && modifiers.shift => self.save(ctx, SaveMode::Copy),
                Key::S if command => self.save(ctx, SaveMode::Overwrite),
                Key::O if command => self.open_dialog(ctx),
                Key::Z if command => self.revert(ctx),
                Key::Q if command => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
                Key::ArrowLeft if modifiers.shift => self.seek_by(-5.0),
                Key::ArrowRight if modifiers.shift => self.seek_by(5.0),
                Key::ArrowLeft | Key::PageUp => self.step(ctx, -1),
                Key::ArrowRight | Key::PageDown => self.step(ctx, 1),
                Key::Home => self.go(ctx, 0),
                Key::End => self.go(ctx, usize::MAX),
                Key::Space => self.toggle_play(),
                Key::Period => self.frame_step(1),
                Key::Comma => self.frame_step(-1),
                Key::J => self.seek_by(-5.0),
                Key::L => self.seek_by(5.0),
                Key::M => {
                    if let Some((_, player)) = &mut self.player {
                        player.muted = !player.muted;
                    }
                }
                Key::ArrowUp | Key::ArrowDown => {
                    if let Some((_, player)) = &mut self.player {
                        let change = if key == Key::ArrowUp { 0.05 } else { -0.05 };
                        player.volume = (player.volume + change).clamp(0.0, 1.0);
                        player.muted = false;
                    }
                }
                Key::F | Key::F11 => toggle_fullscreen(ctx),
                Key::Questionmark | Key::F1 => self.page = Some(Page::Help(HelpTab::Keybinds)),
                Key::Escape => {
                    if self.crop.is_some() {
                        self.crop = None;
                    } else if self.trim_mode {
                        self.toggle_trim();
                    } else if self.tag_bar {
                        self.tag_bar = false;
                    } else if ctx.input(|input| input.viewport().fullscreen.unwrap_or(false)) {
                        toggle_fullscreen(ctx);
                    }
                }
                Key::Enter if self.crop.is_some() => self.apply_crop(),
                Key::Num1 | Key::Num2 | Key::Num3 | Key::Num4 | Key::Num5
                    if self.crop.is_some() =>
                {
                    let index = [Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5]
                        .iter()
                        .position(|number| *number == key)
                        .unwrap_or(0);
                    self.set_crop_aspect(ASPECTS[index].1);
                }
                Key::Delete => self.ask_trash(ctx),
                Key::F2 => self.start_rename(),
                Key::C if self.crop.is_some() => self.apply_crop(),
                Key::C => self.start_crop(),
                Key::R => self.rotate(!modifiers.shift),
                Key::H => self.flip(),
                Key::T => self.toggle_trim(),
                Key::I | Key::O => {
                    if let Some((_, player)) = &self.player {
                        let position = player.position;
                        self.set_trim_point(key == Key::I, position, true);
                    }
                }
                Key::Num0 => {
                    self.zoom = None;
                    self.pan = Vec2::ZERO;
                }
                Key::Num1 => {
                    self.zoom = Some(1.0);
                    self.pan = Vec2::ZERO;
                }
                Key::Plus | Key::Equals => self.zoom_by(1.25),
                Key::Minus => self.zoom_by(0.8),
                _ => {}
            }
        }
    }

    fn zoom_by(&mut self, factor: f32) {
        if self.crop.is_none() {
            let zoom = self.zoom.unwrap_or(self.fit_scale);
            self.zoom = Some(clamp_zoom(zoom * factor, self.fit_scale));
        }
    }

    fn toggle_play(&mut self) {
        let trim = self
            .current()
            .and_then(|path| self.trims.get(path).copied());
        let Some((_, player)) = &mut self.player else {
            return;
        };
        if player.paused {
            let position = player.position;
            let outside_trim =
                trim.filter(|trim| position < trim.start - 0.05 || position >= trim.end - 0.02);
            if let Some(trim) = outside_trim {
                player.seek(trim.start, true);
            } else if player.at_end() {
                player.seek(0.0, true);
            }
        }
        let paused = !player.paused;
        player.set_paused(paused);
    }

    fn seek_by(&mut self, offset: f64) {
        if let Some((_, player)) = &mut self.player {
            let time = player.position + offset;
            player.seek(time, true);
        }
    }

    fn frame_step(&mut self, frames: i32) {
        if let Some((_, player)) = &mut self.player {
            player.step(frames);
        }
    }

    // ---- drawing ------------------------------------------------------------

    fn canvas(&mut self, ui: &mut egui::Ui, rect: Rect) {
        let palette = self.theme.palette;
        let response = ui.interact(rect, Id::unique("canvas"), Sense::click_and_drag());
        if response.double_clicked() && self.crop.is_none() {
            toggle_fullscreen(ui.ctx());
        }
        let Some(path) = self.current().cloned() else {
            self.empty_state(ui, rect);
            return;
        };
        match library::kind(&path) {
            Some(Kind::Image) => self.image_canvas(ui, rect, &response, &path),
            Some(Kind::Video) => {
                let Some((_, player)) = &self.player else {
                    return;
                };
                if let Some(error) = &player.error {
                    centered_text(
                        ui,
                        rect,
                        &format!("Can't play this video\n{error}"),
                        palette.muted,
                    );
                    return;
                }
                let (Some(texture), Some(info)) = (player.texture(), &player.info) else {
                    return;
                };
                let size = vec2(info.size[0] as f32, info.size[1] as f32);
                let frame_rect = fit(rect, size, f32::INFINITY);
                let scale = ui.ctx().pixels_per_point();
                player.set_target(
                    (frame_rect.width() * scale) as u32,
                    (frame_rect.height() * scale) as u32,
                );
                let uv = Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0));
                ui.painter().image(texture, frame_rect, uv, Color32::WHITE);
                if response.clicked() {
                    self.toggle_play();
                }
            }
            None => {}
        }
    }

    fn empty_state(&mut self, ui: &mut egui::Ui, rect: Rect) {
        let palette = self.theme.palette;
        let center = rect.center();
        ui.painter().text(
            center - vec2(0.0, 30.0),
            Align2::CENTER_CENTER,
            "Ommata",
            FontId::proportional(28.0),
            palette.fg,
        );
        ui.painter().text(
            center + vec2(0.0, 4.0),
            Align2::CENTER_CENTER,
            "Drop a photo or video here",
            FontId::proportional(14.0),
            palette.muted,
        );
        let button = Rect::from_center_size(center + vec2(0.0, 48.0), vec2(120.0, 32.0));
        if ui.put(button, egui::Button::new("Open…")).clicked() {
            self.open_dialog(ui.ctx());
        }
        let settings = Rect::from_center_size(center + vec2(-56.0, 92.0), vec2(104.0, 32.0));
        if icon_label_button(ui, settings, Icon::Settings, "Settings", &palette).clicked() {
            self.page = Some(Page::Settings);
        }
        let help = Rect::from_center_size(center + vec2(56.0, 92.0), vec2(104.0, 32.0));
        if icon_label_button(ui, help, Icon::Help, "Help", &palette).clicked() {
            self.page = Some(Page::Help(HelpTab::Keybinds));
        }
    }

    fn image_canvas(
        &mut self,
        ui: &mut egui::Ui,
        rect: Rect,
        response: &egui::Response,
        path: &Path,
    ) {
        let palette = self.theme.palette;
        let (texture, size) = match self.images.get(path) {
            Some(Slot::Ready { texture, size }) => (texture.clone(), *size),
            Some(Slot::Failed(error)) => {
                let error = error.clone();
                centered_text(
                    ui,
                    rect,
                    &format!("Can't open this image\n{error}"),
                    palette.muted,
                );
                return;
            }
            _ => return,
        };
        let edit = self.edits.get(path).copied().unwrap_or_default();
        // While cropping, show the whole rotated image with the crop overlaid.
        let shown = Edit {
            crop: if self.crop.is_some() { None } else { edit.crop },
            ..edit
        };
        let out = shown.output_size(size);
        let out = vec2(out[0] as f32, out[1] as f32);
        let area = rect.shrink(if self.crop.is_some() { 48.0 } else { 0.0 });

        let fit_scale = (area.width() / out.x).min(area.height() / out.y).min(1.0);
        self.fit_scale = fit_scale;
        // A window grown since the zoom was set may now need more.
        self.zoom = self.zoom.map(|zoom| clamp_zoom(zoom, fit_scale));
        let scale = self.zoom.unwrap_or(fit_scale);

        // How far the image may move on each axis: only as far as it is larger
        // than the view, so it can't be dragged off screen or away from the
        // centre when it fits.
        let overflow = |scale: f32| ((out * scale - area.size()) / 2.0).max(Vec2::ZERO);

        // Zoom at the cursor, pan by dragging an image larger than the view.
        if self.crop.is_none() {
            let (scroll, pinch, hover) = ui.input(|input| {
                (
                    input.smooth_scroll_delta.y,
                    input.zoom_delta(),
                    input.pointer.hover_pos(),
                )
            });
            let factor = (scroll * 0.0025).exp() * pinch;
            if (factor - 1.0).abs() > 1e-4
                && let Some(pointer) = hover.filter(|pointer| rect.contains(*pointer))
            {
                let zoom = clamp_zoom(scale * factor, fit_scale);
                let anchor = pointer - rect.center() - self.pan;
                self.pan -= anchor * (zoom / scale - 1.0);
                self.zoom = Some(zoom);
            }
            let scale = self.zoom.unwrap_or(fit_scale);
            if overflow(scale) != Vec2::ZERO {
                if response.dragged() {
                    self.pan += response.drag_delta();
                    self.zoom.get_or_insert(scale);
                    ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
                } else if response.hovered() {
                    ui.ctx().set_cursor_icon(CursorIcon::Grab);
                }
            }
        }
        let scale = self.zoom.unwrap_or(fit_scale);
        let limit = overflow(scale);
        self.pan = self.pan.clamp(-limit, limit);
        let image_rect = Rect::from_center_size(area.center() + self.pan, out * scale);

        let crop = shown
            .crop
            .unwrap_or(Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0)));
        let mut mesh = Mesh::with_texture(texture.id());
        let corners = [
            image_rect.left_top(),
            image_rect.right_top(),
            image_rect.right_bottom(),
            image_rect.left_bottom(),
        ];
        let normalized = [
            crop.left_top(),
            crop.right_top(),
            crop.right_bottom(),
            crop.left_bottom(),
        ];
        for (corner, point) in corners.into_iter().zip(normalized) {
            mesh.vertices.push(egui::epaint::Vertex {
                pos: corner,
                uv: shown.to_uv(point),
                color: Color32::WHITE,
            });
        }
        mesh.indices.extend([0, 1, 2, 0, 2, 3]);
        ui.painter().with_clip_rect(rect).add(Shape::mesh(mesh));

        if self.crop.is_some() {
            self.crop_overlay(ui, image_rect, response, size, &edit);
        }
    }

    fn crop_overlay(
        &mut self,
        ui: &mut egui::Ui,
        image: Rect,
        response: &egui::Response,
        size: [usize; 2],
        edit: &Edit,
    ) {
        let palette = self.theme.palette;
        let Some(tool) = self.crop.as_mut() else {
            return;
        };
        let rotated = edit.rotated_size(size);
        let pixel_aspect = rotated[0] as f32 / rotated[1] as f32;
        let to_screen =
            |point: Pos2| image.min + vec2(point.x * image.width(), point.y * image.height());
        let to_norm = |point: Pos2| {
            pos2(
                (point.x - image.min.x) / image.width(),
                (point.y - image.min.y) / image.height(),
            )
        };
        let screen = Rect::from_min_max(to_screen(tool.rect.min), to_screen(tool.rect.max));

        let hit = 12.0;
        // egui reports a drag only after the pointer has moved a few pixels;
        // decide what it grabs from where the button went down, which is
        // where the hover cursor was showing.
        let press = ui.input(|input| input.pointer.press_origin());
        if response.drag_started()
            && let Some(pointer) = press.or(response.interact_pointer_pos())
        {
            let edges = crop_edges(screen, pointer, hit);
            let kind = if edges != NO_EDGE {
                edges
            } else if screen.contains(pointer) {
                CropDrag::Move
            } else {
                CropDrag::New(to_norm(pointer))
            };
            tool.drag = Some((kind, tool.rect, to_norm(pointer)));
        }
        if let (Some((kind, original, origin)), Some(pointer)) =
            (tool.drag, response.interact_pointer_pos())
        {
            let unit = Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0));
            let point = to_norm(pointer);
            let point = pos2(point.x.clamp(0.0, 1.0), point.y.clamp(0.0, 1.0));
            let mut moved = match kind {
                CropDrag::Move => {
                    let delta = point - origin;
                    let delta = vec2(
                        delta.x.clamp(-original.min.x, 1.0 - original.max.x),
                        delta.y.clamp(-original.min.y, 1.0 - original.max.y),
                    );
                    original.translate(delta)
                }
                CropDrag::New(anchor) => Rect::from_two_pos(anchor, point),
                CropDrag::Edge {
                    left,
                    right,
                    top,
                    bottom,
                } => {
                    let mut resized = original;
                    if left {
                        resized.min.x = point.x.min(resized.max.x - 0.01);
                    }
                    if right {
                        resized.max.x = point.x.max(resized.min.x + 0.01);
                    }
                    if top {
                        resized.min.y = point.y.min(resized.max.y - 0.01);
                    }
                    if bottom {
                        resized.max.y = point.y.max(resized.min.y + 0.01);
                    }
                    resized
                }
            };
            if let (Some(aspect), false) = (tool.aspect, kind == CropDrag::Move) {
                moved = constrain_aspect(moved, aspect / pixel_aspect, kind);
            }
            tool.rect = moved.intersect(unit);
        }
        if response.drag_stopped() {
            tool.drag = None;
        }

        // Dim outside, frame, thirds, handles.
        let screen = Rect::from_min_max(to_screen(tool.rect.min), to_screen(tool.rect.max));
        let dim = Color32::from_black_alpha(150);
        let clipped = ui.painter().with_clip_rect(image);
        let outside = [
            Rect::from_min_max(image.min, pos2(image.max.x, screen.min.y)),
            Rect::from_min_max(pos2(image.min.x, screen.max.y), image.max),
            Rect::from_min_max(
                pos2(image.min.x, screen.min.y),
                pos2(screen.min.x, screen.max.y),
            ),
            Rect::from_min_max(
                pos2(screen.max.x, screen.min.y),
                pos2(image.max.x, screen.max.y),
            ),
        ];
        for band in outside {
            clipped.rect_filled(band, 0.0, dim);
        }
        let thin = Stroke::new(1.0, Color32::from_white_alpha(70));
        for third in [1.0 / 3.0, 2.0 / 3.0] {
            let x = screen.min.x + screen.width() * third;
            let y = screen.min.y + screen.height() * third;
            clipped.line_segment([pos2(x, screen.min.y), pos2(x, screen.max.y)], thin);
            clipped.line_segment([pos2(screen.min.x, y), pos2(screen.max.x, y)], thin);
        }
        let painter = ui.painter();
        painter.rect_stroke(
            screen,
            0.0,
            Stroke::new(1.5, palette.accent),
            egui::StrokeKind::Middle,
        );
        for corner in [
            screen.left_top(),
            screen.right_top(),
            screen.left_bottom(),
            screen.right_bottom(),
        ] {
            painter.circle_filled(corner, 5.0, palette.accent);
        }
        let label = format!(
            "{} × {}",
            (tool.rect.width() * rotated[0] as f32).round(),
            (tool.rect.height() * rotated[1] as f32).round()
        );
        painter.text(
            screen.center_bottom() + vec2(0.0, 14.0),
            Align2::CENTER_CENTER,
            label,
            FontId::proportional(12.0),
            palette.fg,
        );
        // While dragging, keep the cursor of what the drag does.
        let hover = match tool.drag {
            Some((kind, ..)) => Some(kind),
            None => response.hover_pos().map(|pointer| {
                let edges = crop_edges(screen, pointer, hit);
                if edges != NO_EDGE {
                    edges
                } else if screen.contains(pointer) {
                    CropDrag::Move
                } else {
                    CropDrag::New(Pos2::ZERO)
                }
            }),
        };
        if let Some(kind) = hover {
            let icon = match kind {
                CropDrag::Move => CursorIcon::Grab,
                CropDrag::New(_) => CursorIcon::Crosshair,
                CropDrag::Edge {
                    left,
                    right,
                    top,
                    bottom,
                } => match (left || right, top || bottom) {
                    (true, true) if (left && top) || (right && bottom) => CursorIcon::ResizeNwSe,
                    (true, true) => CursorIcon::ResizeNeSw,
                    (true, false) => CursorIcon::ResizeHorizontal,
                    _ => CursorIcon::ResizeVertical,
                },
            };
            ui.ctx().set_cursor_icon(icon);
        }
    }

    /// The top bar; returns its bottom edge, or `None` with no file open.
    fn top_bar(&mut self, ctx: &egui::Context, opacity: f32) -> Option<f32> {
        let path = self.current().cloned()?;
        let kind = library::kind(&path);
        let dirty = self.is_dirty(&path);
        let palette = self.theme.palette;
        let wrap = self.top_bar_wrap;
        let last = self.top_bar_widths;
        // Wrapped rows are centred on each other.
        let pads = if wrap {
            let widest = last.0.max(last.1);
            ((widest - last.0) / 2.0, (widest - last.1) / 2.0)
        } else {
            (0.0, 0.0)
        };
        let mut widths = (0.0, 0.0);
        let area = egui::Area::new(Id::unique("top"))
            .anchor(Align2::CENTER_TOP, vec2(0.0, 12.0))
            .show(ctx, |ui| {
                ui.set_opacity(opacity);
                bar_frame(&palette).show(ui, |ui| {
                    let mut name = |this: &mut Self, ui: &mut egui::Ui| {
                        ui.add_space(pads.0);
                        let row = ui.horizontal(|ui| {
                            this.top_name(ctx, ui, &path, dirty, &palette);
                        });
                        widths.0 = row.response.rect.width();
                    };
                    let mut controls = |this: &mut Self, ui: &mut egui::Ui| {
                        ui.add_space(pads.1);
                        let row = ui.horizontal(|ui| {
                            this.top_controls(ctx, ui, kind, dirty, &palette);
                        });
                        widths.1 = row.response.rect.width();
                    };
                    if wrap {
                        // Too wide for the window: the controls go on a second row.
                        ui.vertical(|ui| {
                            ui.horizontal(|ui| name(self, ui));
                            ui.horizontal(|ui| controls(self, ui));
                        });
                    } else {
                        ui.horizontal(|ui| {
                            name(self, ui);
                            ui.separator();
                            controls(self, ui);
                        });
                    }
                });
            });
        // Each part is measured on its own, so the choice doesn't depend on
        // the layout it made; redo the frame when it changes. The bar is
        // centred on last frame's size, so a frame drawn at a new width would
        // sit off-centre and visibly jump back on the next one.
        let spacing = ctx.global_style().spacing.item_spacing.x * 2.0 + 8.0;
        let room = ctx.content_rect().width() - 24.0 - 2.0 * BAR_MARGIN;
        let wrap_now = widths.0 + spacing + widths.1 > room;
        let moved = (widths.0 - last.0).abs() > 0.5 || (widths.1 - last.1).abs() > 0.5;
        self.top_bar_widths = widths;
        if wrap_now != wrap || moved {
            self.top_bar_wrap = wrap_now;
            ctx.request_discard("top bar layout");
        }
        Some(area.response.rect.bottom())
    }

    /// Show or hide the tag bar; shown, its field takes the cursor.
    fn toggle_tag_bar(&mut self) {
        self.tag_bar = !self.tag_bar;
        self.focus_tag_input = self.tag_bar;
    }

    /// The tag bar, just under the top bar at `top`: rating, label, a field
    /// to add tags, and Edit tags for removing them.
    fn tag_bar(&mut self, ctx: &egui::Context, top: f32, opacity: f32) {
        let Some(path) = self.current().cloned() else {
            return;
        };
        let palette = self.theme.palette;
        let mut meta = self.meta_of(&path);
        let mut input = std::mem::take(&mut self.tag_input);
        let focus = std::mem::take(&mut self.focus_tag_input);
        let mut result = (false, false);
        egui::Area::new(Id::unique("tag bar"))
            .anchor(Align2::CENTER_TOP, vec2(0.0, top + 6.0))
            .show(ctx, |ui| {
                ui.set_opacity(opacity);
                bar_frame(&palette).show(ui, |ui| {
                    result = tags::bar(ui, &mut meta, &mut input, focus, &palette);
                });
            });
        self.tag_input = input;
        let (changed, edit) = result;
        if changed {
            self.set_meta(ctx, &path, meta);
        }
        if edit {
            self.page = Some(Page::Tags);
        }
    }

    /// The file name (click to rename), the unsaved dot and the position.
    fn top_name(
        &mut self,
        ctx: &egui::Context,
        ui: &mut egui::Ui,
        path: &Path,
        dirty: bool,
        palette: &crate::theme::Palette,
    ) {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let short = ellipsize(&name, 48);
        let text = egui::RichText::new(short).strong();
        let galley = egui::WidgetText::from(text).into_galley(
            ui,
            Some(egui::TextWrapMode::Extend),
            f32::INFINITY,
            egui::TextStyle::Body,
        );
        // The name and the rename field share one box, so the
        // whole box is the click target and nothing shifts.
        let size = vec2(
            (galley.size().x + 2.0 * NAME_MARGIN.x).max(120.0),
            ui.spacing().interact_size.y,
        );
        if self.rename.is_some() {
            self.rename_field(ctx, ui, size);
        } else {
            let (rect, response) = ui.allocate_exact_size(size, Sense::click());
            if response.hovered() {
                let widget = &ui.visuals().widgets.hovered;
                ui.painter().rect(
                    rect,
                    widget.corner_radius,
                    ui.visuals().extreme_bg_color,
                    widget.bg_stroke,
                    egui::StrokeKind::Inside,
                );
            }
            let text_pos = pos2(
                rect.left() + NAME_MARGIN.x,
                rect.center().y - galley.size().y / 2.0,
            );
            ui.painter().galley(text_pos, galley, palette.fg);
            focus_ring(ui, &response);
            if hand(response).on_hover_text("Rename (F2)").clicked() {
                self.start_rename();
            }
        }
        if dirty {
            let (dot, response) =
                ui.allocate_exact_size(vec2(8.0, 8.0), Sense::hover());
            ui.painter()
                .circle_filled(dot.center(), 3.5, palette.accent);
            response.on_hover_text("Unsaved edits");
        }
        if self.files.len() > 1 {
            ui.label(
                egui::RichText::new(format!(
                    "{} / {}",
                    self.index + 1,
                    self.files.len()
                ))
                .color(palette.muted),
            );
        }
        if self.filter.is_active() {
            let (matching, total) = self.filter_count();
            let text =
                egui::RichText::new(format!("Filter {matching}/{total}")).color(palette.accent);
            if hand(ui.selectable_label(true, text))
                .on_hover_text("Filter (Ctrl+F)")
                .clicked()
            {
                self.page = Some(Page::Filter);
            }
        }
        let meta = self.meta_of(path);
        match tags::summary(ui, &meta, palette) {
            Some(tags::SummaryClick::TagBar) => self.toggle_tag_bar(),
            Some(tags::SummaryClick::EditTags) => self.page = Some(Page::Tags),
            None => {}
        }
    }

    /// The tools for the current file, saving, and the window buttons.
    fn top_controls(
        &mut self,
        ctx: &egui::Context,
        ui: &mut egui::Ui,
        kind: Option<Kind>,
        dirty: bool,
        palette: &crate::theme::Palette,
    ) {
        if let Some(current) = self.crop.as_ref().map(|tool| tool.aspect) {
            for (label, aspect) in ASPECTS {
                if hand(ui.selectable_label(current == aspect, label)).clicked() {
                    self.set_crop_aspect(aspect);
                }
            }
            ui.separator();
            if ui.button("Cancel").clicked() {
                self.crop = None;
            }
            if filled_button(ui, "Done", palette.accent, &palette).clicked() {
                self.apply_crop();
            }
            return;
        }
        match kind {
            Some(Kind::Image) => {
                if icon(ui, Icon::RotateLeft, "Rotate left (Shift+R)").clicked() {
                    self.rotate(false);
                }
                if icon(ui, Icon::RotateRight, "Rotate right (R)").clicked() {
                    self.rotate(true);
                }
                if icon(ui, Icon::Flip, "Flip (H)").clicked() {
                    self.flip();
                }
                if icon(ui, Icon::Crop, "Crop (C)").clicked() {
                    self.start_crop();
                }
            }
            Some(Kind::Video) => {
                let tip = "Trim (T), set in/out with I / O";
                if icon_toggle(ui, Icon::Trim, self.trim_mode, tip).clicked() {
                    self.toggle_trim();
                }
            }
            None => {}
        }
        if icon_toggle(ui, Icon::Tag, self.tag_bar, "Tags and rating (Ctrl+K)").clicked() {
            self.toggle_tag_bar();
        }
        if dirty || self.job.is_some() {
            ui.separator();
            if let Some(job) = &self.job {
                ui.add(egui::Spinner::new());
                let fraction = job.progress.fraction();
                ui.label(if fraction > 0.0 {
                    format!("Saving {:.0}%", fraction * 100.0)
                } else {
                    "Saving…".into()
                });
                if kind == Some(Kind::Video) && ui.button("Cancel").clicked() {
                    job.progress.cancel.store(true, Relaxed);
                }
            } else {
                if ui
                    .button("Revert")
                    .on_hover_text("Discard edits (Ctrl+Z)")
                    .clicked()
                {
                    self.revert(ctx);
                }
                if ui
                    .button("Save copy")
                    .on_hover_text("Save as name-edit (Ctrl+Shift+S)")
                    .clicked()
                {
                    self.save(ctx, SaveMode::Copy);
                }
                if filled_button(ui, "Save", palette.accent, &palette)
                    .on_hover_text("Overwrite the original (Ctrl+S)")
                    .clicked()
                {
                    self.save(ctx, SaveMode::Overwrite);
                }
            }
        }
        ui.separator();
        if self.update_controls(ctx, ui, &palette, false) {
            ui.separator();
        }
        let trash = if self.trash_without_asking {
            icon_button(
                ui,
                Icon::Trash,
                false,
                Some(palette.danger),
                "Move to trash without asking (Del)",
            )
        } else {
            icon(ui, Icon::Trash, "Move to trash (Del)")
        };
        if trash.clicked() {
            self.ask_trash(ctx);
        }
        let fullscreen = ctx.input(|input| input.viewport().fullscreen.unwrap_or(false));
        let (screen_icon, screen_tip) = if fullscreen {
            (Icon::ExitFullscreen, "Leave fullscreen (F)")
        } else {
            (Icon::Fullscreen, "Fullscreen (F)")
        };
        if icon(ui, screen_icon, screen_tip).clicked() {
            toggle_fullscreen(ctx);
        }
        if icon(ui, Icon::Settings, "Settings").clicked() {
            self.page = Some(Page::Settings);
        }
        if icon(ui, Icon::Help, "Help (?)").clicked() {
            self.page = Some(Page::Help(HelpTab::Keybinds));
        }
    }

    fn bottom_bar(&mut self, ctx: &egui::Context, opacity: f32) {
        let Some(path) = self.current().cloned() else {
            return;
        };
        let trim = self
            .trim_mode
            .then(|| self.trims.get(&path).copied())
            .flatten();
        let palette = self.theme.palette;
        let Some(duration) = self
            .player
            .as_ref()
            .filter(|(_, player)| player.info.is_some())
            .map(|(_, player)| player.duration())
        else {
            return;
        };
        let width = (ctx.content_rect().width() - 32.0).min(900.0);
        // Fixed layout: play, time, mute and volume take ~256 px.
        let timeline_width = (width - 24.0 - 256.0).max(40.0);
        let step = wave_step(ctx.pixels_per_point());
        let columns = ((timeline_width - 12.0) / step).max(1.0) as usize;
        let wave = self.wave_columns(columns, duration).map(<[f32]>::to_vec);
        let settings = self.settings.clone();
        let Some((_, player)) = &mut self.player else {
            return;
        };
        let mut action = None;
        egui::Area::new(Id::unique("bottom"))
            .anchor(Align2::CENTER_BOTTOM, vec2(0.0, -12.0))
            .show(ctx, |ui| {
                ui.set_opacity(opacity);
                bar_frame(&palette).show(ui, |ui| {
                    ui.set_width(width - 24.0);
                    ui.horizontal(|ui| {
                        let play = if player.paused {
                            Icon::Play
                        } else {
                            Icon::Pause
                        };
                        if icon(ui, play, "Play/pause (Space)").clicked() {
                            action = Some(BarAction::TogglePlay);
                        }
                        ui.label(
                            egui::RichText::new(format!(
                                "{} / {}",
                                fmt_time(player.position),
                                fmt_time(duration)
                            ))
                            .monospace()
                            .size(12.0),
                        );
                        let volume_width = 90.0;
                        let wave =
                            wave.as_deref()
                                .filter(|_| settings.waveform)
                                .map(|levels| Wave {
                                    levels,
                                    min: settings.wave_min,
                                    max: settings.wave_max,
                                    filled: settings.wave_filled,
                                });
                        let (timeline_action, hover) = timeline(
                            ui,
                            timeline_width,
                            player.position,
                            duration,
                            trim,
                            wave,
                            &mut self.drag,
                            &palette,
                        );
                        if timeline_action.is_some() {
                            action = timeline_action;
                        }
                        if let Some((time, at)) = hover {
                            hover_preview(ctx, player, time, at, &palette);
                        }
                        let volume_icon = if player.muted || player.volume == 0.0 {
                            Icon::Muted
                        } else {
                            Icon::Volume
                        };
                        if icon(ui, volume_icon, "Mute (M)").clicked() {
                            player.muted = !player.muted;
                        }
                        let muted = player.muted;
                        let bar_width = volume_width - 10.0;
                        if volume_bar(ui, bar_width, &mut player.volume, muted, &palette) {
                            player.muted = false;
                        }
                    });
                });
            });
        match action {
            Some(BarAction::TogglePlay) => self.toggle_play(),
            // Dragging the playhead pauses so each frame under the pointer
            // shows; playback resumes on release if it was running.
            Some(BarAction::Scrub(time)) => {
                if let Some((_, player)) = &mut self.player {
                    if self.resume_after_drag.is_none() {
                        self.resume_after_drag = Some(!player.paused);
                        player.set_paused(true);
                    }
                    player.scrub(time);
                }
            }
            Some(BarAction::Seek(time, exact)) => {
                if let Some((_, player)) = &mut self.player {
                    // Ending an active drag: the drag preview stays on screen
                    // until the full-resolution exact frame replaces it.
                    player.seek(time, exact);
                    if self.resume_after_drag.take() == Some(true) {
                        player.set_paused(false);
                    }
                }
            }
            Some(BarAction::Trim { start, time, done }) => {
                self.set_trim_point(start, time, done);
            }
            None => {}
        }
    }

    /// Update status and its action. `full` (the Version tab) shows every
    /// state; the top bar shows only an update on offer or in progress.
    /// Returns whether anything was drawn.
    fn update_controls(
        &mut self,
        ctx: &egui::Context,
        ui: &mut egui::Ui,
        palette: &crate::theme::Palette,
        full: bool,
    ) -> bool {
        let accent = |ui: &mut egui::Ui, text: String| {
            filled_button(ui, text, palette.accent, palette)
        };
        let muted = |text: &str| egui::RichText::new(text).color(palette.muted);
        match self.updater.state() {
            update::State::Unknown if full => {
                if ui.button("Check for updates").clicked() {
                    self.updater.check();
                }
            }
            update::State::Checking if full => {
                ui.add(egui::Spinner::new());
                ui.label(muted("Checking for updates…"));
            }
            update::State::UpToDate if full => {
                ui.label(muted("You have the latest version."));
                if ui.button("Check again").clicked() {
                    self.updater.check();
                }
            }
            update::State::CheckFailed(error) if full => {
                ui.label(muted("Couldn't check for updates.")).on_hover_text(error);
                if ui.button("Try again").clicked() {
                    self.updater.check();
                }
            }
            update::State::Available(version) => {
                if update::appimage().is_none() {
                    let hint = if update::is_system_package() {
                        "Arrives with your next system update; click for the release notes"
                    } else {
                        "Open the release page"
                    };
                    let available = ui
                        .button(format!("v{version} available"))
                        .on_hover_text(hint);
                    if available.clicked()
                        && let Err(error) = update::open_url(&update::release_url(&version))
                    {
                        self.notify(ctx, error);
                    }
                } else if accent(ui, format!("Update to v{version}"))
                    .on_hover_text("Download and install the new version")
                    .clicked()
                {
                    self.updater.install(version);
                }
            }
            update::State::Downloading(version) => {
                ui.add(egui::Spinner::new());
                ui.label(format!("Updating to v{version}…"));
            }
            update::State::Installed(version) => {
                if accent(ui, "Restart to update".into())
                    .on_hover_text(format!("Ommata {version} is installed"))
                    .clicked()
                {
                    self.restart(ctx);
                }
            }
            update::State::Failed(version, error) => {
                if ui
                    .button("Update failed, retry")
                    .on_hover_text(error)
                    .clicked()
                {
                    self.updater.install(version);
                }
            }
            _ => return false,
        }
        true
    }

    /// Help or Settings: the same window from the start screen and from a
    /// file. Esc or a click outside closes it.
    fn page(&mut self, ctx: &egui::Context) {
        let Some(page) = self.page else {
            return;
        };
        let palette = self.theme.palette;
        let settings = self.settings.clone();
        let mut new_settings = settings.clone();
        let mut next = Some(page);
        let current = self.current().cloned();
        let known = self.folder_tags();
        let mut meta = current.as_deref().map(|path| self.meta_of(path));
        let mut meta_changed = false;
        let mut filter = self.filter.clone();
        let mut filter_changed = false;
        let count = self.filter_count();
        let mut tag_input = std::mem::take(&mut self.tag_input);
        let modal = modal("page", ctx, &palette).show(ctx, |ui| {
            ui.set_width(520.0);
            ui.horizontal(|ui| {
                let title = match page {
                    Page::Settings => "Settings",
                    Page::Help(_) => "Help",
                    Page::Tags => "Edit tags",
                    Page::Filter => "Filter",
                };
                ui.heading(title);
                if page == Page::Tags
                    && let Some(name) = current.as_deref().and_then(Path::file_name)
                {
                    let name = ellipsize(&name.to_string_lossy(), 40);
                    ui.label(egui::RichText::new(name).color(palette.muted));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Close").clicked() {
                        next = None;
                    }
                });
            });
            ui.add_space(4.0);
            match page {
                Page::Settings => settings_ui(ui, &mut new_settings, &palette),
                Page::Tags => match meta.as_mut() {
                    Some(meta) => {
                        meta_changed = tags::panel(
                            ui,
                            meta,
                            &known,
                            &settings.quick_tags,
                            &mut tag_input,
                            &palette,
                        );
                    }
                    None => {
                        ui.label("Open a photo or video to tag it.");
                    }
                },
                Page::Filter => {
                    filter_changed = tags::filter_panel(ui, &mut filter, &known, count, &palette);
                }
                Page::Help(tab) => {
                    ui.horizontal(|ui| {
                        for (label, option) in [
                            ("Keybinds", HelpTab::Keybinds),
                            ("Version", HelpTab::Version),
                            ("Report an issue", HelpTab::Report),
                        ] {
                            if hand(ui.selectable_label(tab == option, label)).clicked() {
                                next = Some(Page::Help(option));
                            }
                        }
                    });
                    ui.separator();
                    match tab {
                        HelpTab::Keybinds => shortcuts_ui(ui, &palette),
                        HelpTab::Version => self.version_ui(ctx, ui, &palette),
                        HelpTab::Report => self.report_ui(ctx, ui, &palette),
                    }
                }
            }
        });
        self.page = if modal.should_close() { None } else { next };
        self.tag_input = tag_input;
        if meta_changed && let (Some(path), Some(meta)) = (current, meta) {
            self.set_meta(ctx, &path, meta);
        }
        if filter_changed {
            self.filter = filter;
            self.apply_filter(ctx, None);
        }
        if new_settings != settings {
            let check_turned_on = new_settings.check_updates && !settings.check_updates;
            self.settings = new_settings.clamp();
            if let Err(error) = self.settings.save() {
                self.notify(ctx, format!("Couldn't save settings: {error}"));
            }
            if check_turned_on && self.updater.state() == update::State::Unknown {
                self.updater.check();
            }
            self.sync_waveform(ctx);
        }
    }

    fn version_ui(
        &mut self,
        ctx: &egui::Context,
        ui: &mut egui::Ui,
        palette: &crate::theme::Palette,
    ) {
        let version = format!("Ommata {}", update::VERSION);
        ui.label(egui::RichText::new(version).size(18.0).strong());
        ui.label(egui::RichText::new(update::install_kind()).color(palette.muted));
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            self.update_controls(ctx, ui, palette, true);
        });
        ui.add_space(8.0);
        if ui.link("What's new in each release").clicked()
            && let Err(error) = update::open_url(&update::releases_url())
        {
            self.notify(ctx, error);
        }
    }

    fn report_ui(
        &mut self,
        ctx: &egui::Context,
        ui: &mut egui::Ui,
        palette: &crate::theme::Palette,
    ) {
        let details = update::system_details(&self.renderer, &self.video_decoding);
        ui.label("Something not working? Report it on GitHub, with these details filled in:");
        ui.add_space(4.0);
        egui::Frame::new()
            .fill(palette.surface)
            .corner_radius(6.0)
            .inner_margin(8.0)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(
                    egui::RichText::new(format!(
                        "Ommata {} ({})\n{details}",
                        update::VERSION,
                        update::install_kind()
                    ))
                    .monospace()
                    .size(12.0)
                    .color(palette.muted),
                );
            });
        ui.add_space(8.0);
        let mut url = None;
        ui.horizontal(|ui| {
            if filled_button(ui, "Report an issue", palette.accent, palette).clicked() {
                url = Some(update::report_url(&details));
            }
            if ui.button("Suggest a feature").clicked() {
                url = Some(update::feature_url());
            }
        });
        if let Some(url) = url
            && let Err(error) = update::open_url(&url)
        {
            self.notify(ctx, error);
        }
    }

    /// Quit and start the newly installed version on the same file.
    fn restart(&mut self, ctx: &egui::Context) {
        if self.any_dirty() || self.job.is_some() {
            self.notify(ctx, "Save or revert your edits first");
            return;
        }
        match update::restart(self.current().map(PathBuf::as_path)) {
            Ok(()) => {
                self.quitting = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            Err(error) => self.notify(ctx, format!("Couldn't restart: {error}")),
        }
    }

    fn toast(&mut self, ctx: &egui::Context) {
        let Some((message, until)) = &self.toast else {
            return;
        };
        let now = ctx.input(|input| input.time);
        if now > *until {
            self.toast = None;
            return;
        }
        ctx.request_repaint_after(Duration::from_secs_f64(until - now));
        let palette = self.theme.palette;
        let offset = if self.player.is_some() { -84.0 } else { -24.0 };
        egui::Area::new(Id::unique("toast"))
            .anchor(Align2::CENTER_BOTTOM, vec2(0.0, offset))
            .interactable(false)
            .show(ctx, |ui| {
                bar_frame(&palette).show(ui, |ui| ui.label(message.as_str()));
            });
    }

    fn trash_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut skip) = self.confirm_trash else {
            return;
        };
        let Some(path) = self.current() else {
            self.confirm_trash = None;
            return;
        };
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let name = ellipsize(&name, 40);
        let palette = self.theme.palette;
        // Consumed here, so Enter confirms instead of also toggling a focused
        // checkbox.
        let enter = ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, Key::Enter));
        let escape = ctx.input(|input| input.key_pressed(Key::Escape));
        let mut confirmed = enter;
        let mut cancelled = escape;
        modal("trash", ctx, &palette).show(ctx, |ui| {
            ui.set_width(320.0);
            ui.heading(format!("Move {name} to the trash?"));
            ui.label(
                egui::RichText::new("You can restore it from the trash.").color(palette.muted),
            );
            ui.add_space(4.0);
            hand(ui.checkbox(&mut skip, "Don't ask again until Ommata closes"));
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Cancel").clicked() {
                    cancelled = true;
                }
                if filled_button(ui, "Move to trash", palette.danger, &palette).clicked() {
                    confirmed = true;
                }
            });
        });
        if cancelled {
            self.confirm_trash = None;
        } else if confirmed {
            self.confirm_trash = None;
            self.trash_without_asking = skip;
            self.trash_current(ctx);
        } else {
            self.confirm_trash = Some(skip);
        }
    }

    fn quit_dialog(&mut self, ctx: &egui::Context) {
        let close_requested = ctx.input(|input| input.viewport().close_requested());
        if close_requested && !self.quitting && self.any_dirty() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.confirm_quit = true;
        }
        if !self.confirm_quit {
            return;
        }
        let palette = self.theme.palette;
        modal("quit", ctx, &palette).show(ctx, |ui| {
            ui.set_width(300.0);
            ui.heading("Discard unsaved edits?");
            ui.label(
                egui::RichText::new("Your crops, rotations and trims haven't been saved.")
                    .color(palette.muted),
            );
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Cancel").clicked() {
                    self.confirm_quit = false;
                }
                if filled_button(ui, "Discard", palette.danger, &palette).clicked() {
                    self.quitting = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
        });
    }
}

enum BarAction {
    TogglePlay,
    Seek(f64, bool),
    /// Playhead drag in progress: show the exact frame under the pointer.
    Scrub(f64),
    /// Set a trim point (`start` or end). `done` moves the start to the real
    /// cut point; before that (mid-drag) it only scrubs.
    Trim {
        start: bool,
        time: f64,
        done: bool,
    },
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.theme.poll(&ctx);
        self.poll(&ctx);
        if let Some(gpu) = &mut self.gpu {
            gpu.register(|texture| frame.register_native_glow_texture(texture));
            gpu.collect_garbage();
        }

        let dropped = ctx.input(|input| {
            input
                .raw
                .dropped_files
                .first()
                .map(|file| file.path().to_path_buf())
                .filter(|path| !path.as_os_str().is_empty())
        });
        if let Some(path) = dropped {
            self.open_path(&ctx, &path);
        }
        self.keys(&ctx);

        // Video clock and trim-range preview.
        let trim = self
            .current()
            .and_then(|path| self.trims.get(path).copied())
            .filter(|_| self.trim_mode);
        let mut next = None;
        if let Some((_, player)) = &mut self.player {
            next = player.update(&ctx, self.gpu.as_mut());
            if let Some(trim) = trim
                && !player.paused
                && player.position >= trim.end
            {
                player.set_paused(true);
                player.seek(trim.end, true);
            }
            if !player.paused && player.at_end() {
                player.set_paused(true);
            }
        }

        let rect = ui.max_rect();
        ui.painter().rect_filled(rect, 0.0, self.theme.palette.bg);
        self.canvas(ui, rect);

        // Auto-hiding chrome: after CHROME_SECS without input the bars fade
        // out and the cursor hides, in every mode; any input brings them back.
        // They stay while the pointer rests on a bar or a dialog is open.
        let now = ctx.input(|input| input.time);
        let active = ctx.input(|input| {
            input.pointer.is_moving() || !input.events.is_empty() || input.pointer.any_down()
        });
        if active {
            self.chrome_until = now + CHROME_SECS;
        }
        let pinned = self.confirm_quit
            || self.confirm_trash.is_some()
            || self.page.is_some()
            || self.rename.is_some()
            || ctx.egui_wants_keyboard_input()
            || ctx.is_pointer_over_egui();
        let visible = pinned || now < self.chrome_until;
        if !visible {
            ctx.set_cursor_icon(CursorIcon::None);
        } else if !pinned {
            let left = (self.chrome_until - now).max(0.0);
            ctx.request_repaint_after(Duration::from_secs_f64(left));
        }
        let opacity = ctx.animate_bool_with_time(Id::unique("chrome"), visible, 0.15);
        if opacity > 0.0 {
            if let Some(bottom) = self.top_bar(&ctx, opacity)
                && self.tag_bar
            {
                self.tag_bar(&ctx, bottom, opacity);
            }
            self.bottom_bar(&ctx, opacity);
        }
        self.toast(&ctx);
        self.quit_dialog(&ctx);
        self.trash_dialog(&ctx);
        self.page(&ctx);

        if let Some(wait) = next {
            ctx.request_repaint_after(wait);
        }
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        self.theme.palette.bg.to_normalized_gamma_f32()
    }
}



/// Write a copy atomically, or overwrite while keeping the original in the trash.
fn save_to(
    src: &Path,
    dst: &Path,
    mode: SaveMode,
    write: impl FnOnce(&Path) -> Result<(), String>,
) -> Result<Option<String>, String> {
    match mode {
        SaveMode::Overwrite if dst == src => replace_trashing_original(dst, write),
        SaveMode::Overwrite => replace_with_new(src, dst, write),
        SaveMode::Copy => library::write_atomic(dst, write).map(|_| None),
    }
}

/// Overwrite `src` with a file of another type at `dst` (a RAW saved as DNG):
/// write `dst`, then send `src` to the trash so it can be restored.
fn replace_with_new(
    src: &Path,
    dst: &Path,
    write: impl FnOnce(&Path) -> Result<(), String>,
) -> Result<Option<String>, String> {
    library::write_atomic(dst, write)?;
    let name = src.file_name().unwrap_or_default().to_string_lossy().to_string();
    Ok(Some(match trash::delete(src) {
        Ok(()) => format!("Saved. Original {name} is in the trash"),
        Err(error) => format!("Saved, but couldn't trash {name}: {error}"),
    }))
}

/// Overwrite `dst` without destroying the original: write the new file to a
/// temp path, rename the original to `name-original.ext`, move the new file
/// into place, then send the original to the trash so it can be restored.
/// Returns a note for the user saying where the original went.
fn replace_trashing_original(
    dst: &Path,
    write: impl FnOnce(&Path) -> Result<(), String>,
) -> Result<Option<String>, String> {
    let temp = library::temp_path(dst);
    if let Err(error) = write(&temp) {
        library::remove_temp(&temp);
        return Err(error);
    }
    let original = library::original_path(dst);
    if let Err(error) = std::fs::rename(dst, &original) {
        library::remove_temp(&temp);
        return Err(format!("couldn't move the original aside: {error}"));
    }
    if let Err(error) = std::fs::rename(&temp, dst) {
        // Put the original back; nothing has changed for the user.
        if let Err(restore) = std::fs::rename(&original, dst) {
            library::remove_temp(&temp);
            return Err(format!(
                "{error}; the original is kept as {}: {restore}",
                original.display()
            ));
        }
        library::remove_temp(&temp);
        return Err(error.to_string());
    }
    let name = original
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    Ok(Some(match trash::delete(&original) {
        Ok(()) => format!("Saved. Original is in the trash as {name}"),
        Err(error) => format!("Saved. Couldn't trash the original ({error}); kept as {name}"),
    }))
}

/// Seek-bar waveform: column levels and the bar heights they map to.
#[derive(Clone, Copy)]
struct Wave<'a> {
    levels: &'a [f32],
    min: f32,
    max: f32,
    filled: bool,
}

/// The seek bar. Returns what the pointer did, and the hovered time with the
/// point above the bar where its preview goes.
#[allow(clippy::too_many_arguments)]
fn timeline(
    ui: &mut egui::Ui,
    width: f32,
    position: f64,
    duration: f64,
    trim: Option<Trim>,
    wave: Option<Wave>,
    drag: &mut Option<Handle>,
    palette: &crate::theme::Palette,
) -> (Option<BarAction>, Option<(f64, Pos2)>) {
    let height = wave.map_or(24.0, |wave| (wave.max + 8.0).max(24.0));
    let (rect, response) =
        ui.allocate_exact_size(vec2(width.max(40.0), height), Sense::click_and_drag());
    let track = Rect::from_center_size(rect.center(), vec2(rect.width() - 12.0, 4.0));
    let x_of = |time: f64| {
        track.left() + (time / duration.max(1e-6)).clamp(0.0, 1.0) as f32 * track.width()
    };
    let time_of = |x: f32| ((x - track.left()) / track.width()).clamp(0.0, 1.0) as f64 * duration;
    let mut action = None;

    // Trim handle under `x`, if any: the nearer one within the grab zone.
    const HANDLE_HIT: f32 = 14.0;
    let handle_at = |x: f32| {
        let trim = trim?;
        let to_start = (x - x_of(trim.start)).abs();
        let to_end = (x - x_of(trim.end)).abs();
        if to_start.min(to_end) > HANDLE_HIT {
            None
        } else if to_start <= to_end {
            Some(Handle::Start)
        } else {
            Some(Handle::End)
        }
    };
    // Decide from where the button went down (a drag registers only after
    // the pointer has moved a few pixels), matching the hover cursor.
    let press = ui.input(|input| input.pointer.press_origin());
    if (response.drag_started() || response.clicked())
        && let Some(pointer) = press.or(response.interact_pointer_pos())
    {
        *drag = Some(handle_at(pointer.x).unwrap_or(Handle::Seek));
    }
    if let (Some(handle), Some(pointer)) = (*drag, response.interact_pointer_pos()) {
        let time = time_of(pointer.x);
        let released = response.drag_stopped() || response.clicked();
        action = Some(match handle {
            Handle::Seek if released => BarAction::Seek(time, true),
            Handle::Seek => BarAction::Scrub(time),
            Handle::Start | Handle::End => BarAction::Trim {
                start: handle == Handle::Start,
                time,
                done: released,
            },
        });
        if released {
            *drag = None;
        }
    }
    let over_handle = match *drag {
        Some(handle) => handle != Handle::Seek,
        None => response
            .hover_pos()
            .and_then(|pointer| handle_at(pointer.x))
            .is_some(),
    };
    if over_handle {
        ui.ctx().set_cursor_icon(CursorIcon::ResizeColumn);
    } else if response.hovered() || drag.is_some() {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }

    // Hovered time and the point above the bar where its preview goes.
    // Hidden while any drag is underway (or about to start) and over a trim
    // handle, where the main view already shows the exact frame.
    let pressed = ui.input(|input| input.pointer.any_down());
    let hover = response
        .hover_pos()
        .filter(|pointer| drag.is_none() && !pressed && handle_at(pointer.x).is_none())
        .map(|pointer| {
            (
                time_of(pointer.x),
                pos2(pointer.x.clamp(track.left(), track.right()), rect.top()),
            )
        });
    let painter = ui.painter();
    let base = lerp(palette.surface, palette.fg, 0.18);
    // Grey out the parts the trim will cut, over whatever the bar shows.
    let shade_clipped = || {
        let Some(trim) = trim else {
            return;
        };
        let veil = palette.surface.gamma_multiply(0.8);
        let (start, end) = (x_of(trim.start), x_of(trim.end));
        for (left, right) in [(rect.left(), start), (end, rect.right())] {
            if right > left {
                let band = Rect::from_x_y_ranges(left..=right, rect.y_range());
                painter.rect_filled(band, 0.0, veil);
            }
        }
    };
    let trim_handles = |width: f32, active_width: f32, height: f32, color: Color32| {
        let Some(trim) = trim else {
            return;
        };
        for (x, active) in [
            (x_of(trim.start), *drag == Some(Handle::Start)),
            (x_of(trim.end), *drag == Some(Handle::End)),
        ] {
            let size = vec2(if active { active_width } else { width }, height);
            let handle = Rect::from_center_size(pos2(x, rect.center().y), size);
            painter.rect_filled(handle, CornerRadius::same(2), color);
        }
    };
    let playhead_x = x_of(position);
    if let Some(wave) = wave {
        // Waveform, accent up to the playhead.
        let unplayed = lerp(palette.surface, palette.fg, 0.32);
        let color_at = |x: f32| {
            if x <= playhead_x {
                palette.accent
            } else {
                unplayed
            }
        };
        let center_y = rect.center().y;
        // Columns start on a whole pixel and are a whole number of pixels
        // apart, so every bar comes out the same width on any display scale.
        let pixels = ui.ctx().pixels_per_point();
        let step = wave_step(pixels);
        let left = (track.left() * pixels).round() / pixels;
        let column = |index: usize| {
            let x = left + (index as f32 + 0.5) * step;
            (x, wave.min + (wave.max - wave.min) * wave.levels[index])
        };
        if wave.filled {
            // One continuous shape: a quad between each pair of columns,
            // coloured per segment so the playhead split stays sharp.
            let mut mesh = Mesh::default();
            for index in 1..wave.levels.len() {
                let ((left_x, left_height), (right_x, right_height)) =
                    (column(index - 1), column(index));
                let color = color_at((left_x + right_x) * 0.5);
                let first = mesh.vertices.len() as u32;
                for (x, bar_height) in [(left_x, left_height), (right_x, right_height)] {
                    mesh.colored_vertex(pos2(x, center_y - bar_height * 0.5), color);
                    mesh.colored_vertex(pos2(x, center_y + bar_height * 0.5), color);
                }
                mesh.add_triangle(first, first + 1, first + 2);
                mesh.add_triangle(first + 1, first + 3, first + 2);
            }
            painter.add(Shape::mesh(mesh));
        } else {
            // A third of each step (at least a pixel) is the gap.
            let step_pixels = (step * pixels).round();
            let bar_width = (step_pixels - (step_pixels / 3.0).round().max(1.0)) / pixels;
            for index in 0..wave.levels.len() {
                let (x, bar_height) = column(index);
                let bar_left = left + index as f32 * step;
                let bar = Rect::from_x_y_ranges(
                    bar_left..=bar_left + bar_width,
                    center_y - bar_height * 0.5..=center_y + bar_height * 0.5,
                );
                painter.rect_filled(bar, CornerRadius::same(1), color_at(x));
            }
        }
        shade_clipped();
        trim_handles(3.0, 4.0, height, palette.fg);
        painter.rect_filled(
            Rect::from_center_size(pos2(playhead_x, rect.center().y), vec2(2.0, height)),
            CornerRadius::same(1),
            palette.fg,
        );
        return (action, hover);
    }
    painter.rect_filled(track, CornerRadius::same(2), base);
    let played = Rect::from_x_y_ranges(track.left()..=playhead_x, track.y_range());
    painter.rect_filled(played, CornerRadius::same(2), palette.accent);
    shade_clipped();
    trim_handles(4.0, 6.0, 18.0, palette.accent);
    painter.circle_filled(
        pos2(playhead_x, track.center().y),
        if response.hovered() { 6.0 } else { 5.0 },
        palette.fg,
    );
    (action, hover)
}

const WAVE_STEP: f32 = 3.0;

/// Distance between waveform columns in points: WAVE_STEP rounded to whole
/// pixels at `pixels_per_point`.
fn wave_step(pixels_per_point: f32) -> f32 {
    (WAVE_STEP * pixels_per_point).round().max(2.0) / pixels_per_point
}

/// YouTube-style thumbnail above the hovered timeline point: the nearest
/// keyframe at or before `time`, with the hovered time under it.
fn hover_preview(
    ctx: &egui::Context,
    player: &mut Player,
    time: f64,
    at: Pos2,
    palette: &crate::theme::Palette,
) {
    let thumbnail = player
        .preview_at(time, ctx)
        .map(|texture| (texture.id(), texture.size_vec2()));
    let size = thumbnail.map_or(vec2(0.0, 0.0), |(_, size)| size * 0.75);
    egui::Area::new(Id::unique("hover-preview"))
        .order(egui::Order::Tooltip)
        .interactable(false)
        .pivot(Align2::CENTER_BOTTOM)
        .fixed_pos(at - vec2(0.0, 18.0))
        .show(ctx, |ui| {
            egui::Frame::new()
                .fill(palette.surface)
                .stroke(Stroke::new(1.0, lerp(palette.surface, palette.fg, 0.15)))
                .corner_radius(CornerRadius::same(10))
                .inner_margin(egui::Margin::same(4))
                .show(ui, |ui| {
                    ui.vertical_centered(|ui| {
                        if let Some((id, _)) = thumbnail {
                            let image =
                                egui::Image::new((id, size)).corner_radius(CornerRadius::same(7));
                            ui.add(image);
                        }
                        ui.label(egui::RichText::new(fmt_time(time)).monospace().size(11.0));
                    });
                });
        });
}

fn settings_ui(ui: &mut egui::Ui, settings: &mut Settings, palette: &crate::theme::Palette) {
    ui.spacing_mut().interact_size.y = 20.0;
    ui.spacing_mut().slider_width = 160.0;
    ui.label(egui::RichText::new("Quick tags").strong());
    ui.label(
        egui::RichText::new("Alt and a number adds or removes the tag; / to nest.")
            .color(palette.muted)
            .size(12.0),
    );
    egui::Grid::new("quick-tags")
        .num_columns(4)
        .spacing([8.0, 6.0])
        .show(ui, |ui| {
            for (index, tag) in settings.quick_tags.iter_mut().enumerate() {
                let key = egui::RichText::new(format!("Alt+{}", index + 1))
                    .monospace()
                    .color(palette.accent);
                ui.add_sized([48.0, 24.0], egui::Label::new(key));
                let fill = ui.visuals().widgets.inactive.bg_fill;
                let field = egui::TextEdit::singleline(tag).background_color(fill);
                ui.add_sized([170.0, 24.0], field);
                if index % 2 == 1 {
                    ui.end_row();
                }
            }
        });
    ui.add_space(10.0);
    ui.label(egui::RichText::new("Updates").strong());
    hand(ui.checkbox(&mut settings.check_updates, "Check for updates once a day"));
    ui.label(
        egui::RichText::new("Asks GitHub for the newest version number; nothing else is sent.")
            .color(palette.muted)
            .size(12.0),
    );
    ui.add_space(10.0);
    ui.label(egui::RichText::new("Video seek bar").strong());
    hand(ui.checkbox(&mut settings.waveform, "Show audio waveform"));
    ui.add_enabled_ui(settings.waveform, |ui| {
        ui.horizontal(|ui| {
            ui.label("Style");
            hand(ui.selectable_value(&mut settings.wave_filled, false, "Bars"));
            hand(ui.selectable_value(&mut settings.wave_filled, true, "Filled"));
        });
        egui::Grid::new("wave-settings")
            .num_columns(2)
            .spacing([10.0, 6.0])
            .show(ui, |ui| {
                ui.label("Min height");
                ui.add(
                    egui::Slider::new(&mut settings.wave_min, 1.0..=40.0)
                        .suffix(" px")
                        .max_decimals(0),
                );
                ui.end_row();
                ui.label("Max height");
                ui.add(
                    egui::Slider::new(&mut settings.wave_max, 2.0..=40.0)
                        .suffix(" px")
                        .max_decimals(0),
                );
                ui.end_row();
                ui.label("Smoothing");
                ui.add(egui::Slider::new(&mut settings.wave_smooth, 0.0..=1.0).max_decimals(2));
                ui.end_row();
            });
    });
}

/// Every keyboard shortcut, in two compact columns that each line their
/// groups up in one grid.
fn shortcuts_ui(ui: &mut egui::Ui, palette: &crate::theme::Palette) {
    // Browse, Video and Tags on the left; Photo, View and Save on the right.
    let (left, right) = SHORTCUTS.split_at(3);
    ui.columns(2, |columns| {
        for (column, groups) in columns.iter_mut().zip([left, right]) {
            column.spacing_mut().interact_size.y = 0.0;
            egui::Grid::new(groups[0].0)
                .num_columns(2)
                .spacing([10.0, 3.0])
                .show(column, |ui| {
                    for (index, (group, keys)) in groups.iter().enumerate() {
                        if index > 0 {
                            ui.add_space(8.0);
                            ui.end_row();
                        }
                        // Headings in the text colour and larger, apart from
                        // the accent-coloured keys under them.
                        let heading = egui::RichText::new(*group).size(15.0).color(palette.fg);
                        ui.label(heading);
                        ui.end_row();
                        for (key, action) in *keys {
                            let key = egui::RichText::new(*key).monospace().size(11.5);
                            ui.label(key.color(palette.accent));
                            ui.label(egui::RichText::new(*action).size(12.5));
                            ui.end_row();
                        }
                    }
                });
        }
    });
}

/// A quiet button at `rect`: an icon and a label, muted until hovered.
fn icon_label_button(
    ui: &mut egui::Ui,
    rect: Rect,
    icon: Icon,
    label: &str,
    palette: &crate::theme::Palette,
) -> egui::Response {
    let response = ui.interact(rect, Id::unique(("icon-label", label)), Sense::click());
    let color = if response.hovered() {
        let fill = ui.visuals().widgets.hovered.weak_bg_fill;
        ui.painter().rect_filled(rect, CornerRadius::same(8), fill);
        palette.fg
    } else {
        palette.muted
    };
    let galley = ui.painter().layout_no_wrap(
        label.to_string(),
        FontId::proportional(14.0),
        color,
    );
    let width = 16.0 + 8.0 + galley.size().x;
    let left = rect.center().x - width / 2.0;
    paint_icon(ui.painter(), icon, pos2(left + 8.0, rect.center().y), color);
    let text_pos = pos2(left + 24.0, rect.center().y - galley.size().y / 2.0);
    ui.painter().galley(text_pos, galley, color);
    focus_ring(ui, &response);
    hand(response)
}

/// A dialog over the dimmed window, in the start screen's background colour
/// rather than egui's brighter window fill.
fn modal(name: &str, ctx: &egui::Context, palette: &crate::theme::Palette) -> egui::Modal {
    let frame = egui::Frame::popup(&ctx.global_style())
        .fill(palette.bg)
        .inner_margin(20.0);
    egui::Modal::new(Id::unique(name)).frame(frame)
}

/// Show the pointing hand over a clickable widget egui leaves at the arrow.
pub(crate) fn hand(response: egui::Response) -> egui::Response {
    response.on_hover_cursor(CursorIcon::PointingHand)
}

/// Outline a hand-drawn widget while it has keyboard focus (Tab), as egui
/// does for its own.
pub(crate) fn focus_ring(ui: &egui::Ui, response: &egui::Response) {
    if response.has_focus() {
        let stroke = Stroke::new(1.5, ui.visuals().selection.stroke.color);
        ui.painter().rect_stroke(
            response.rect.expand(2.0),
            CornerRadius::same(8),
            stroke,
            egui::StrokeKind::Outside,
        );
    }
}

/// A button filled with `fill` (accent or danger). egui keeps a custom fill
/// the same on hover, so this one lightens on hover and darkens when pressed.
fn filled_button(
    ui: &mut egui::Ui,
    text: impl Into<String>,
    fill: Color32,
    palette: &crate::theme::Palette,
) -> egui::Response {
    let text = egui::RichText::new(text.into()).color(palette.bg);
    let galley = egui::WidgetText::from(text).into_galley(
        ui,
        Some(egui::TextWrapMode::Extend),
        f32::INFINITY,
        egui::TextStyle::Button,
    );
    let padding = ui.spacing().button_padding;
    let size = vec2(
        galley.size().x + 2.0 * padding.x,
        (galley.size().y + 2.0 * padding.y).max(ui.spacing().interact_size.y),
    );
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    let fill = if response.is_pointer_button_down_on() {
        lerp(fill, Color32::BLACK, 0.15)
    } else if response.hovered() {
        lerp(fill, Color32::WHITE, 0.15)
    } else {
        fill
    };
    let radius = ui.visuals().widgets.inactive.corner_radius;
    ui.painter().rect_filled(rect, radius, fill);
    let text_pos = rect.center() - galley.size() / 2.0;
    ui.painter().galley(text_pos, galley, palette.bg);
    focus_ring(ui, &response);
    hand(response)
}

fn volume_bar(
    ui: &mut egui::Ui,
    width: f32,
    volume: &mut f32,
    muted: bool,
    palette: &crate::theme::Palette,
) -> bool {
    let (rect, response) = ui.allocate_exact_size(vec2(width, 24.0), Sense::click_and_drag());
    let track = Rect::from_center_size(rect.center(), vec2(rect.width() - 8.0, 4.0));
    let mut changed = false;
    if let Some(pointer) = response
        .interact_pointer_pos()
        .filter(|_| response.dragged() || response.clicked())
    {
        *volume = ((pointer.x - track.left()) / track.width()).clamp(0.0, 1.0);
        changed = true;
    }
    let shown = if muted { 0.0 } else { *volume };
    let x = track.left() + shown * track.width();
    let painter = ui.painter();
    painter.rect_filled(
        track,
        CornerRadius::same(2),
        lerp(palette.surface, palette.fg, 0.18),
    );
    painter.rect_filled(
        Rect::from_x_y_ranges(track.left()..=x, track.y_range()),
        CornerRadius::same(2),
        palette.fg,
    );
    if response.hovered() || response.dragged() {
        painter.circle_filled(pos2(x, track.center().y), 5.0, palette.fg);
    }
    changed
}

/// The largest size with width/height `aspect` that fits in `rect`.
fn aspect_size(rect: Rect, aspect: f32) -> Vec2 {
    let (width, height) = (rect.width(), rect.height());
    if width / height > aspect {
        vec2(height * aspect, height)
    } else {
        vec2(width, width / aspect)
    }
}

/// Shrink `rect` to width/height `aspect` (normalized units), keeping the
/// corner the drag is anchored to fixed.
fn constrain_aspect(rect: Rect, aspect: f32, kind: CropDrag) -> Rect {
    let size = aspect_size(rect, aspect);
    let (anchor_right, anchor_bottom) = match kind {
        CropDrag::Edge { left, top, .. } => (left, top),
        CropDrag::New(anchor) => (anchor.x > rect.center().x, anchor.y > rect.center().y),
        CropDrag::Move => (false, false),
    };
    let x = if anchor_right {
        rect.max.x - size.x
    } else {
        rect.min.x
    };
    let y = if anchor_bottom {
        rect.max.y - size.y
    } else {
        rect.min.y
    };
    Rect::from_min_size(pos2(x, y), size)
}

/// Which crop edges a pointer at `pointer` grabs: within `hit` px of an edge
/// line, and along that edge's extent. Two edges at once means a corner.
fn crop_edges(screen: Rect, pointer: Pos2, hit: f32) -> CropDrag {
    let near = |position: f32, edge: f32| (position - edge).abs() < hit;
    let inside_y = pointer.y > screen.top() - hit && pointer.y < screen.bottom() + hit;
    let inside_x = pointer.x > screen.left() - hit && pointer.x < screen.right() + hit;
    CropDrag::Edge {
        left: near(pointer.x, screen.left()) && inside_y,
        right: near(pointer.x, screen.right()) && inside_y,
        top: near(pointer.y, screen.top()) && inside_x,
        bottom: near(pointer.y, screen.bottom()) && inside_x,
    }
}

const ASPECTS: [(&str, Option<f32>); 5] = [
    ("Free", None),
    ("1:1", Some(1.0)),
    ("4:3", Some(4.0 / 3.0)),
    ("3:2", Some(1.5)),
    ("16:9", Some(16.0 / 9.0)),
];

/// Largest centered rect inside `rect` with pixel aspect `aspect`, for an
/// image of pixel size `size` (`rect` is normalized).
fn fit_aspect_centered(rect: Rect, aspect: f32, size: [usize; 2]) -> Rect {
    let normalized = aspect / (size[0] as f32 / size[1] as f32);
    Rect::from_center_size(rect.center(), aspect_size(rect, normalized))
}

/// `size` scaled to fit centered in `area`, at most `max_scale`.
fn fit(area: Rect, size: Vec2, max_scale: f32) -> Rect {
    let scale = (area.width() / size.x)
        .min(area.height() / size.y)
        .min(max_scale);
    Rect::from_center_size(area.center(), size * scale)
}

fn centered_text(ui: &egui::Ui, rect: Rect, text: &str, color: Color32) {
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        text,
        FontId::proportional(15.0),
        color,
    );
}

/// Horizontal padding inside the bars.
const BAR_MARGIN: f32 = 10.0;

fn bar_frame(palette: &crate::theme::Palette) -> egui::Frame {
    egui::Frame::new()
        .fill(palette.surface.gamma_multiply(0.94))
        .stroke(Stroke::new(1.0, lerp(palette.surface, palette.fg, 0.10)))
        .corner_radius(CornerRadius::same(14))
        .inner_margin(egui::Margin::symmetric(BAR_MARGIN as i8, 6))
        .shadow(egui::Shadow {
            offset: [0, 4],
            blur: 16,
            spread: 0,
            color: Color32::from_black_alpha(60),
        })
}

#[derive(Clone, Copy)]
pub(crate) enum Icon {
    Play,
    Pause,
    RotateLeft,
    RotateRight,
    Flip,
    Crop,
    Trash,
    Fullscreen,
    ExitFullscreen,
    Volume,
    Muted,
    Settings,
    Trim,
    Tag,
    /// A circle with a slash: the file is rejected.
    Reject,
    Help,
}

/// Icons are drawn as vector paths: no icon font to ship, crisp at any scale.
fn icon(ui: &mut egui::Ui, icon: Icon, tip: &str) -> egui::Response {
    icon_toggle(ui, icon, false, tip)
}

/// Icon button that stays highlighted while `selected` (a mode is on).
fn icon_toggle(ui: &mut egui::Ui, icon: Icon, selected: bool, tip: &str) -> egui::Response {
    icon_button(ui, icon, selected, None, tip)
}

/// Icon button drawn in `color` instead of the text colour, when given.
pub(crate) fn icon_button(
    ui: &mut egui::Ui,
    icon: Icon,
    selected: bool,
    color: Option<Color32>,
    tip: &str,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(vec2(28.0, 28.0), Sense::click());
    let visuals = ui.style().interact(&response);
    if selected {
        let fill = ui.visuals().selection.bg_fill;
        ui.painter().rect_filled(rect, CornerRadius::same(8), fill);
    } else if response.hovered() {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(8), visuals.weak_bg_fill);
    }
    let color = color.unwrap_or(visuals.fg_stroke.color);
    paint_icon(ui.painter(), icon, rect.center(), color);
    focus_ring(ui, &response);
    hand(response).on_hover_text(tip)
}

/// Draw `icon` centred on `center`, about 16 px across.
pub(crate) fn paint_icon(painter: &egui::Painter, icon: Icon, center: Pos2, color: Color32) {
    let stroke = Stroke::new(1.6, color);
    let at = |x: f32, y: f32| center + vec2(x, y);
    let arc = |clockwise: bool| {
        // 270° arc with an arrowhead at its open end.
        let radius = 6.0_f32;
        let (first, last) = (-0.25 * std::f32::consts::PI, 1.25 * std::f32::consts::PI);
        let points: Vec<Pos2> = (0..=24)
            .map(|i| {
                let angle = first + (last - first) * i as f32 / 24.0;
                let x = if clockwise { -angle.cos() } else { angle.cos() };
                at(x * radius, -angle.sin() * radius)
            })
            .collect();
        let end = points[0];
        painter.add(Shape::line(points, stroke));
        let side = if clockwise { 1.0 } else { -1.0 };
        painter.add(Shape::convex_polygon(
            vec![
                end + vec2(-3.5 * side, -3.5),
                end + vec2(1.5 * side, -0.5),
                end + vec2(-3.0 * side, 1.5),
            ],
            color,
            Stroke::NONE,
        ));
    };
    match icon {
        Icon::Play => {
            painter.add(Shape::convex_polygon(
                vec![at(-4.0, -6.5), at(6.5, 0.0), at(-4.0, 6.5)],
                color,
                Stroke::NONE,
            ));
        }
        Icon::Pause => {
            for x in [-3.5, 3.5] {
                painter.rect_filled(
                    Rect::from_center_size(at(x, 0.0), vec2(3.0, 12.0)),
                    CornerRadius::same(1),
                    color,
                );
            }
        }
        Icon::RotateLeft => arc(true),
        Icon::RotateRight => arc(false),
        Icon::Flip => {
            painter.add(Shape::convex_polygon(
                vec![at(-1.8, -6.0), at(-1.8, 6.0), at(-7.5, 6.0)],
                color,
                Stroke::NONE,
            ));
            painter.add(Shape::closed_line(
                vec![at(1.8, -6.0), at(1.8, 6.0), at(7.5, 6.0)],
                stroke,
            ));
            painter.add(Shape::dashed_line(
                &[at(0.0, -8.0), at(0.0, 8.0)],
                Stroke::new(1.0, color),
                2.0,
                2.0,
            ));
        }
        Icon::Crop => {
            painter.add(Shape::line(
                vec![at(-4.0, -8.0), at(-4.0, 4.0), at(8.0, 4.0)],
                stroke,
            ));
            painter.add(Shape::line(
                vec![at(-8.0, -4.0), at(4.0, -4.0), at(4.0, 8.0)],
                stroke,
            ));
        }
        Icon::Trash => {
            painter.line_segment([at(-6.5, -4.5), at(6.5, -4.5)], stroke);
            painter.add(Shape::line(
                vec![at(-2.5, -4.5), at(-2.5, -7.0), at(2.5, -7.0), at(2.5, -4.5)],
                stroke,
            ));
            painter.add(Shape::line(
                vec![at(-5.0, -4.5), at(-4.0, 7.0), at(4.0, 7.0), at(5.0, -4.5)],
                stroke,
            ));
            for x in [-1.5, 1.5] {
                painter.line_segment([at(x, -1.5), at(x, 4.0)], Stroke::new(1.2, color));
            }
        }
        Icon::ExitFullscreen => {
            // The corners turned inward.
            for (x_sign, y_sign) in [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
                let corner = at(3.0 * x_sign, 3.0 * y_sign);
                painter.add(Shape::line(
                    vec![
                        corner + vec2(0.0, 4.0 * y_sign),
                        corner,
                        corner + vec2(4.0 * x_sign, 0.0),
                    ],
                    stroke,
                ));
            }
        }
        Icon::Fullscreen => {
            for (x_sign, y_sign) in [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
                let corner = at(7.0 * x_sign, 7.0 * y_sign);
                painter.add(Shape::line(
                    vec![
                        corner - vec2(0.0, 4.0 * y_sign),
                        corner,
                        corner - vec2(4.0 * x_sign, 0.0),
                    ],
                    stroke,
                ));
            }
        }
        Icon::Trim => {
            // Scissors: two finger loops, blades crossing above them.
            for side in [-1.0, 1.0] {
                painter.circle_stroke(at(4.0 * side, 5.0), 2.6, stroke);
                painter.line_segment([at(2.4 * side, 3.0), at(-4.5 * side, -7.5)], stroke);
            }
        }
        Icon::Tag => {
            // A luggage tag: pointed on the right, with its hole.
            let outline = vec![
                at(-7.0, -5.0),
                at(2.0, -5.0),
                at(7.5, 0.0),
                at(2.0, 5.0),
                at(-7.0, 5.0),
            ];
            painter.add(Shape::closed_line(outline, stroke));
            painter.circle_stroke(at(2.0, 0.0), 1.4, stroke);
        }
        Icon::Reject => {
            painter.circle_stroke(center, 7.0, stroke);
            painter.line_segment([at(-4.9, 4.9), at(4.9, -4.9)], stroke);
        }
        Icon::Help => {
            painter.circle_stroke(center, 7.5, stroke);
            // A drawn "?" in the same stroke as the ring: a hook over the
            // top, a short stem, and a dot.
            let hook: Vec<Pos2> = (0..=10)
                .map(|step| {
                    let angle = -std::f32::consts::PI * (1.0 - 1.25 * step as f32 / 10.0);
                    at(2.6 * angle.cos(), -2.0 + 2.6 * angle.sin())
                })
                .chain([at(0.0, 1.4), at(0.0, 2.0)])
                .collect();
            painter.add(Shape::line(hook, stroke));
            painter.circle_filled(at(0.0, 4.4), 1.1, color);
        }
        Icon::Settings => {
            painter.circle_stroke(center, 3.0, stroke);
            for i in 0..8 {
                let angle = i as f32 * std::f32::consts::FRAC_PI_4;
                let direction = vec2(angle.cos(), angle.sin());
                let tooth = [center + direction * 5.5, center + direction * 8.0];
                painter.line_segment(tooth, Stroke::new(2.4, color));
            }
            painter.circle_stroke(center, 6.0, stroke);
        }
        Icon::Volume | Icon::Muted => {
            painter.add(Shape::convex_polygon(
                vec![
                    at(-7.5, -2.5),
                    at(-4.5, -2.5),
                    at(0.0, -6.5),
                    at(0.0, 6.5),
                    at(-4.5, 2.5),
                    at(-7.5, 2.5),
                ],
                color,
                Stroke::NONE,
            ));
            if matches!(icon, Icon::Muted) {
                painter.line_segment([at(3.0, -3.0), at(8.0, 3.0)], stroke);
                painter.line_segment([at(3.0, 3.0), at(8.0, -3.0)], stroke);
            } else {
                for radius in [4.0_f32, 7.5] {
                    let points: Vec<Pos2> = (0..=12)
                        .map(|i| {
                            let angle = -0.9 + 1.8 * i as f32 / 12.0;
                            at(angle.cos() * radius, angle.sin() * radius)
                        })
                        .collect();
                    painter.add(Shape::line(points, stroke));
                }
            }
        }
    }
}

fn toggle_fullscreen(ctx: &egui::Context) {
    let fullscreen = ctx.input(|input| input.viewport().fullscreen.unwrap_or(false));
    ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(!fullscreen));
}

/// Shorten `text` to `max` characters by cutting out its middle.
pub(crate) fn ellipsize(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let head: String = text.chars().take(max / 2).collect();
    let tail: String = text
        .chars()
        .rev()
        .take(max / 2 - 1)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{head}…{tail}")
}

/// `m:ss.t`, or `h:mm:ss` from an hour up.
fn fmt_time(time: f64) -> String {
    let total = time.max(0.0) as u64;
    let (hours, minutes, seconds) = (total / 3600, total / 60 % 60, total % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}
