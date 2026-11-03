//! Window, input, and the save flow. Edits stay in memory per file until the user
//! saves (overwrite in place) or saves a copy (`name-edit.ext`).

use crate::library::{self, Kind};
use crate::picture::{self, Decoded, Edit};
use crate::settings::Settings;
use crate::theme::{Theme, lerp};
use crate::trim::{self, Progress};
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

const CHROME_SECS: f64 = 2.5;

type LoadResult = (PathBuf, Result<Decoded, String>);

/// Background image decoding. Created before the window so the first image
/// decodes while the GL context is still starting up.
pub struct Loader {
    tx: Sender<LoadResult>,
    rx: Receiver<LoadResult>,
    ctx: Arc<OnceLock<egui::Context>>,
    max_side: Arc<AtomicU32>,
}

impl Loader {
    pub fn new() -> Self {
        let (tx, rx) = channel();
        Self {
            tx,
            rx,
            ctx: Default::default(),
            max_side: Arc::new(AtomicU32::new(8192)),
        }
    }

    fn spawn(&self, path: PathBuf) {
        let (tx, ctx, max) = (self.tx.clone(), self.ctx.clone(), self.max_side.clone());
        std::thread::spawn(move || {
            let r = picture::decode(&path, max.load(Relaxed));
            let _ = tx.send((path, r));
            if let Some(ctx) = ctx.get() {
                ctx.request_repaint();
            }
        });
    }
}

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
        match files.iter().position(|f| *f == path) {
            Some(i) => (files, i),
            None => (vec![path], 0),
        }
    };
    if let Some(p) = files
        .get(index)
        .filter(|p| library::kind(p) == Some(Kind::Image))
    {
        loader.spawn(p.clone());
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

struct CropTool {
    rect: Rect,
    aspect: Option<f32>,
    /// Active drag: what it does, the rect when it began, and where it began
    /// (normalized).
    drag: Option<(CropDrag, Rect, Pos2)>,
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
    snap_tx: Sender<(PathBuf, f64)>,
    snap_rx: Receiver<(PathBuf, f64)>,
    drag: Option<Handle>,
    zoom: Option<f32>,
    pan: Vec2,
    crop: Option<CropTool>,
    job: Option<Job>,
    toast: Option<(String, f64)>,
    chrome_until: f64,
    confirm_quit: bool,
    quitting: bool,
    settings: Settings,
    /// Set while the playhead is dragged: whether to resume playing after.
    resume_after_drag: Option<bool>,
    wave: Option<(PathBuf, Arc<Waveform>)>,
    /// Seek-bar columns, cached by (columns, smoothing, levels decoded so far).
    wave_cache: Option<((usize, u32, usize), Vec<f32>)>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext, loader: Loader, start: Option<Start>) -> Self {
        let ctx = &cc.egui_ctx;
        let _ = loader.ctx.set(ctx.clone());
        loader
            .max_side
            .store(ctx.input(|i| i.max_texture_side) as u32, Relaxed);
        ctx.all_styles_mut(|s| {
            s.spacing.button_padding = vec2(10.0, 6.0);
            s.spacing.item_spacing = vec2(6.0, 6.0);
            s.spacing.interact_size.y = 28.0;
            s.interaction.selectable_labels = false;
        });
        let (snap_tx, snap_rx) = channel();
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
            snap_tx,
            snap_rx,
            drag: None,
            zoom: None,
            pan: Vec2::ZERO,
            crop: None,
            job: None,
            toast: None,
            chrome_until: CHROME_SECS,
            confirm_quit: false,
            quitting: false,
            settings: Settings::load(),
            resume_after_drag: None,
            wave: None,
            wave_cache: None,
        };
        if let Some(s) = start {
            if let Some(p) = s.files.get(s.index)
                && library::kind(p) == Some(Kind::Image)
            {
                app.images.insert(p.clone(), Slot::Loading);
            }
            app.files = s.files;
            app.go(ctx, s.index);
        }
        app
    }

    fn current(&self) -> Option<&PathBuf> {
        self.files.get(self.index)
    }

    fn current_kind(&self) -> Option<Kind> {
        self.current().and_then(|p| library::kind(p))
    }

    fn open_path(&mut self, ctx: &egui::Context, path: &Path) {
        let s = resolve_start(path, &self.loader);
        if let Some(p) = s.files.get(s.index)
            && library::kind(p) == Some(Kind::Image)
        {
            self.images.insert(p.clone(), Slot::Loading);
        }
        self.files = s.files;
        self.go(ctx, s.index);
    }

    /// Show file `index`: start or reuse decodes, preload neighbours, drop the rest.
    fn go(&mut self, ctx: &egui::Context, index: usize) {
        if self.files.is_empty() {
            self.player = None;
            ctx.send_viewport_cmd(egui::ViewportCommand::Title("anyview".into()));
            return;
        }
        self.index = index.min(self.files.len() - 1);
        self.zoom = None;
        self.pan = Vec2::ZERO;
        self.crop = None;
        self.drag = None;
        let path = self.files[self.index].clone();
        self.trim_mode = self.trims.contains_key(&path);

        let n = self.files.len();
        let keep: Vec<PathBuf> = [n - 1, 0, 1]
            .iter()
            .map(|d| self.files[(self.index + d) % n].clone())
            .collect();
        self.images.retain(|p, _| keep.contains(p));
        for p in &keep {
            if library::kind(p) == Some(Kind::Image) && !self.images.contains_key(p) {
                self.images.insert(p.clone(), Slot::Loading);
                self.loader.spawn(p.clone());
            }
        }

        match library::kind(&path) {
            Some(Kind::Video) => {
                if self.player.as_ref().is_none_or(|(p, _)| *p != path) {
                    self.player = Some((path.clone(), Player::open(path.clone(), ctx)));
                }
            }
            _ => self.player = None,
        }
        self.sync_waveform(ctx);
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        ctx.send_viewport_cmd(egui::ViewportCommand::Title(format!("{name} — anyview")));
    }

    /// Keep a waveform scan running for the current video while the setting is on.
    fn sync_waveform(&mut self, ctx: &egui::Context) {
        let want = self
            .player
            .as_ref()
            .filter(|_| self.settings.waveform)
            .map(|(p, _)| p.clone());
        if self.wave.as_ref().map(|(p, _)| p) != want.as_ref() {
            self.wave = want.map(|p| (p.clone(), Waveform::spawn(p, ctx.clone())));
            self.wave_cache = None;
        }
    }

    fn wave_columns(&mut self, columns: usize, duration: f64) -> Option<&[f32]> {
        let (_, w) = self.wave.as_ref()?;
        if w.none.load(Relaxed) {
            return None;
        }
        let key = (columns, self.settings.wave_smooth.to_bits(), w.len());
        if self.wave_cache.as_ref().is_none_or(|(k, _)| *k != key) {
            self.wave_cache = Some((key, w.columns(columns, duration, self.settings.wave_smooth)));
        }
        self.wave_cache.as_ref().map(|(_, c)| c.as_slice())
    }

    fn step(&mut self, ctx: &egui::Context, delta: isize) {
        if self.files.len() > 1 {
            let n = self.files.len() as isize;
            self.go(ctx, (self.index as isize + delta).rem_euclid(n) as usize);
        }
    }

    fn reload_listing(&mut self, ctx: &egui::Context, focus: &Path) {
        self.files = library::siblings(focus);
        let i = self.files.iter().position(|f| f == focus).unwrap_or(0);
        self.go(ctx, i);
    }

    fn is_dirty(&self, path: &Path) -> bool {
        self.edits.get(path).is_some_and(|e| !e.is_identity())
            || self.trims.get(path).is_some_and(|t| self.trim_is_change(t))
    }

    fn trim_is_change(&self, t: &Trim) -> bool {
        let dur = self
            .player
            .as_ref()
            .map_or(f64::INFINITY, |(_, p)| p.duration());
        t.start > 0.01 || t.end < dur - 0.01
    }

    fn any_dirty(&self) -> bool {
        self.edits.values().any(|e| !e.is_identity()) || !self.trims.is_empty()
    }

    fn notify(&mut self, ctx: &egui::Context, msg: impl Into<String>) {
        self.toast = Some((msg.into(), ctx.input(|i| i.time) + 2.5));
    }

    // ---- background results -------------------------------------------------

    fn poll(&mut self, ctx: &egui::Context) {
        while let Ok((path, r)) = self.loader.rx.try_recv() {
            if !self.images.contains_key(&path) {
                continue;
            }
            let slot = match r {
                Ok(d) => Slot::Ready {
                    texture: ctx.load_texture(
                        path.to_string_lossy(),
                        d.image,
                        TextureOptions::LINEAR,
                    ),
                    size: d.size,
                },
                Err(e) => Slot::Failed(e),
            };
            self.images.insert(path, slot);
        }
        while let Ok((path, t)) = self.snap_rx.try_recv() {
            if let Some(tr) = self.trims.get_mut(&path) {
                tr.start = t.min(tr.end);
                // Show the frame the trimmed video will actually start on.
                if let Some((pp, p)) = &mut self.player
                    && *pp == path
                    && p.paused
                {
                    p.seek(tr.start, true);
                }
            }
        }
        if self.job.as_ref().is_some_and(|j| j.handle.is_finished()) {
            let job = self.job.take().unwrap();
            match job
                .handle
                .join()
                .unwrap_or_else(|_| Err("save crashed".into()))
            {
                Ok((out, note)) => {
                    self.edits.remove(&job.src);
                    self.trims.remove(&job.src);
                    self.images.remove(&job.src);
                    if self.player.as_ref().is_some_and(|(p, _)| *p == job.src) {
                        self.player = None;
                    }
                    let name = out
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string();
                    let msg = match (job.mode, note) {
                        (_, Some(note)) => note,
                        (SaveMode::Overwrite, None) => format!("Saved {name}"),
                        (SaveMode::Copy, None) => format!("Saved copy {name}"),
                    };
                    self.notify(ctx, msg);
                    self.reload_listing(ctx, &out);
                }
                Err(e) => self.notify(ctx, format!("Save failed: {e}")),
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
        let raw = picture::is_raw(&src);
        if raw && mode == SaveMode::Overwrite {
            self.notify(
                ctx,
                "RAW originals are never overwritten. Use Save copy (saves a JPEG)",
            );
            return;
        }
        let dst = match mode {
            SaveMode::Overwrite => src.clone(),
            SaveMode::Copy => library::edit_copy_path(&src, raw.then_some("jpg")),
        };
        let progress = Arc::new(Progress::default());
        let handle = match library::kind(&src) {
            Some(Kind::Image) => {
                if !picture::can_save(&src) {
                    self.notify(ctx, "This format can't be saved");
                    return;
                }
                let edit = self.edits[&src];
                let (s, ctx2) = (src.clone(), ctx.clone());
                std::thread::spawn(move || {
                    let r = save_to(&dst, mode, |tmp| picture::save(&s, tmp, &edit));
                    ctx2.request_repaint();
                    r.map(|note| (dst, note))
                })
            }
            Some(Kind::Video) => {
                let t = self.trims[&src];
                let (s, p, ctx2) = (src.clone(), progress.clone(), ctx.clone());
                std::thread::spawn(move || {
                    let r = save_to(&dst, mode, |tmp| trim::export(&s, tmp, t.start, t.end, p));
                    ctx2.request_repaint();
                    r.map(|note| (dst, note))
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
        if let Some(p) = self.current().cloned() {
            let had = self.is_dirty(&p) || self.crop.is_some();
            self.edits.remove(&p);
            self.trims.remove(&p);
            self.crop = None;
            self.trim_mode = false;
            if had {
                self.notify(ctx, "Edits reverted");
            }
        }
    }

    fn trash_current(&mut self, ctx: &egui::Context) {
        let Some(p) = self.current().cloned() else {
            return;
        };
        if self.player.as_ref().is_some_and(|(pp, _)| *pp == p) {
            self.player = None;
        }
        match trash::delete(&p) {
            Ok(()) => {
                self.edits.remove(&p);
                self.trims.remove(&p);
                self.images.remove(&p);
                self.files.remove(self.index);
                let name = p
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                self.notify(ctx, format!("Moved {name} to trash"));
                let i = self.index.min(self.files.len().saturating_sub(1));
                self.go(ctx, i);
            }
            Err(e) => {
                self.notify(ctx, format!("Couldn't trash: {e}"));
                self.go(ctx, self.index);
            }
        }
    }

    fn open_dialog(&mut self, ctx: &egui::Context) {
        let start = self
            .current()
            .and_then(|p| p.parent().map(Path::to_path_buf));
        let mut dlg = rfd::FileDialog::new().set_title("Open");
        if let Some(d) = start {
            dlg = dlg.set_directory(d);
        }
        if let Some(p) = dlg.pick_file() {
            self.open_path(ctx, &p);
        }
    }

    // ---- crop ---------------------------------------------------------------

    fn start_crop(&mut self) {
        let Some(p) = self.current().cloned() else {
            return;
        };
        if self.current_kind() != Some(Kind::Image) {
            return;
        }
        let e = self.edits.entry(p).or_default();
        let rect = e
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
        if let (Some(a), Some(size)) = (aspect, size) {
            tool.rect = fit_aspect_centered(tool.rect, a, size);
        }
    }

    fn apply_crop(&mut self) {
        let (Some(tool), Some(p)) = (self.crop.take(), self.current().cloned()) else {
            return;
        };
        let full = tool.rect.width() > 0.999 && tool.rect.height() > 0.999;
        self.edits.entry(p).or_default().crop = (!full).then_some(tool.rect);
    }

    fn edit_mut(&mut self) -> Option<&mut Edit> {
        let p = self.current().cloned()?;
        (library::kind(&p) == Some(Kind::Image)).then(|| self.edits.entry(p).or_default())
    }

    fn rotate(&mut self, cw: bool) {
        let tool = self.crop.take();
        if let Some(e) = self.edit_mut() {
            // Rotate the in-progress crop rect along with the image.
            let mut tmp = Edit {
                crop: tool.as_ref().map(|t| t.rect),
                ..*e
            };
            tmp.rotate(cw);
            e.rotate(cw);
            self.crop = tool.map(|t| CropTool {
                rect: tmp.crop.unwrap(),
                aspect: t.aspect.map(|a| 1.0 / a),
                drag: None,
            });
        }
    }

    fn flip(&mut self) {
        let tool = self.crop.take();
        if let Some(e) = self.edit_mut() {
            let mut tmp = Edit {
                crop: tool.as_ref().map(|t| t.rect),
                ..*e
            };
            tmp.flip_h();
            e.flip_h();
            self.crop = tool.map(|t| CropTool {
                rect: tmp.crop.unwrap(),
                ..t
            });
        }
    }

    // ---- trim ---------------------------------------------------------------

    fn toggle_trim(&mut self) {
        let Some(path) = self.current().cloned() else {
            return;
        };
        let Some((_, p)) = &self.player else { return };
        if self.trim_mode {
            self.trim_mode = false;
            if self
                .trims
                .get(&path)
                .is_some_and(|t| !self.trim_is_change(t))
            {
                self.trims.remove(&path);
            }
        } else {
            let d = p.duration();
            self.trims
                .entry(path)
                .or_insert(Trim { start: 0.0, end: d });
            self.trim_mode = true;
        }
    }

    /// Move a trim point. Dragging pauses and scrubs to the exact frame; `done`
    /// (end of a drag, or the I/O keys) snaps the start to the keyframe the
    /// export will use. Keys set during playback don't interrupt it.
    fn set_trim_point(&mut self, start: bool, t: f64, done: bool) {
        let Some(path) = self.current().cloned() else {
            return;
        };
        let Some((_, p)) = &mut self.player else {
            return;
        };
        let d = p.duration();
        if !done {
            p.set_paused(true);
        }
        self.trim_mode = true;
        let tr = self
            .trims
            .entry(path.clone())
            .or_insert(Trim { start: 0.0, end: d });
        let shown = if start {
            tr.start = t.min(tr.end - 0.05).max(0.0);
            if done {
                let (tx, t) = (self.snap_tx.clone(), tr.start);
                std::thread::spawn(move || {
                    if let Some(k) = trim::keyframe_at_or_before(&path, t) {
                        let _ = tx.send((path, k));
                    }
                });
            }
            tr.start
        } else {
            tr.end = t.max(tr.start + 0.05).min(d);
            tr.end
        };
        if p.paused {
            if done {
                // Releasing: refine to the full-resolution exact frame.
                p.seek(shown, true);
            } else {
                p.scrub(shown);
            }
        }
    }

    // ---- input --------------------------------------------------------------

    fn keys(&mut self, ctx: &egui::Context) {
        if ctx.egui_wants_keyboard_input() || self.confirm_quit {
            return;
        }
        let (pressed, mods) = ctx.input(|i| {
            let keys: Vec<Key> = i
                .events
                .iter()
                .filter_map(|e| match e {
                    egui::Event::Key {
                        key, pressed: true, ..
                    } => Some(*key),
                    _ => None,
                })
                .collect();
            (keys, i.modifiers)
        });
        for key in pressed {
            let cmd = mods.command;
            match key {
                Key::S if cmd && mods.shift => self.save(ctx, SaveMode::Copy),
                Key::S if cmd => self.save(ctx, SaveMode::Overwrite),
                Key::O if cmd => self.open_dialog(ctx),
                Key::Z if cmd => self.revert(ctx),
                Key::Q if cmd => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
                Key::ArrowLeft if mods.shift => self.seek_by(-5.0),
                Key::ArrowRight if mods.shift => self.seek_by(5.0),
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
                    if let Some((_, p)) = &mut self.player {
                        p.muted = !p.muted;
                    }
                }
                Key::ArrowUp | Key::ArrowDown => {
                    if let Some((_, p)) = &mut self.player {
                        let d = if key == Key::ArrowUp { 0.05 } else { -0.05 };
                        p.volume = (p.volume + d).clamp(0.0, 1.0);
                        p.muted = false;
                    }
                }
                Key::F | Key::F11 => toggle_fullscreen(ctx),
                Key::Escape => {
                    if self.crop.is_some() {
                        self.crop = None;
                    } else if self.trim_mode {
                        self.toggle_trim();
                    } else if ctx.input(|i| i.viewport().fullscreen.unwrap_or(false)) {
                        toggle_fullscreen(ctx);
                    }
                }
                Key::Enter if self.crop.is_some() => self.apply_crop(),
                Key::Num1 | Key::Num2 | Key::Num3 | Key::Num4 | Key::Num5
                    if self.crop.is_some() =>
                {
                    let i = [Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5]
                        .iter()
                        .position(|k| *k == key)
                        .unwrap_or(0);
                    self.set_crop_aspect(ASPECTS[i].1);
                }
                Key::Delete => self.trash_current(ctx),
                Key::C if self.crop.is_some() => self.apply_crop(),
                Key::C => self.start_crop(),
                Key::R => self.rotate(!mods.shift),
                Key::H => self.flip(),
                Key::T => self.toggle_trim(),
                Key::I | Key::O => {
                    if let Some((_, p)) = &self.player {
                        let t = p.position;
                        self.set_trim_point(key == Key::I, t, true);
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

    fn zoom_by(&mut self, f: f32) {
        if self.crop.is_none() {
            let z = self.zoom.unwrap_or(1.0);
            self.zoom = Some((z * f).clamp(0.02, 50.0));
        }
    }

    fn toggle_play(&mut self) {
        let trim = self.current().and_then(|p| self.trims.get(p).copied());
        let Some((_, p)) = &mut self.player else {
            return;
        };
        if p.paused {
            let outside =
                trim.is_some_and(|t| p.position < t.start - 0.05 || p.position >= t.end - 0.02);
            if let (true, Some(t)) = (outside, trim) {
                p.seek(t.start, true);
            } else if p.at_end() {
                p.seek(0.0, true);
            }
        }
        let paused = !p.paused;
        p.set_paused(paused);
    }

    fn seek_by(&mut self, d: f64) {
        if let Some((_, p)) = &mut self.player {
            let t = p.position + d;
            p.seek(t, true);
        }
    }

    fn frame_step(&mut self, n: i32) {
        if let Some((_, p)) = &mut self.player {
            p.step(n);
        }
    }

    // ---- drawing ------------------------------------------------------------

    fn canvas(&mut self, ui: &mut egui::Ui, rect: Rect) {
        let pal = self.theme.pal;
        let resp = ui.interact(rect, Id::unique("canvas"), Sense::click_and_drag());
        if resp.double_clicked() && self.crop.is_none() {
            toggle_fullscreen(ui.ctx());
        }
        let Some(path) = self.current().cloned() else {
            self.empty_state(ui, rect);
            return;
        };
        match library::kind(&path) {
            Some(Kind::Image) => self.image_canvas(ui, rect, &resp, &path),
            Some(Kind::Video) => {
                let Some((_, p)) = &self.player else { return };
                if let Some(e) = &p.error {
                    centered_text(ui, rect, &format!("Can't play this video\n{e}"), pal.muted);
                    return;
                }
                let (Some(tex), Some(info)) = (p.texture(), &p.info) else {
                    return;
                };
                let size = vec2(info.size[0] as f32, info.size[1] as f32);
                let r = fit(rect, size, f32::INFINITY);
                let ppp = ui.ctx().pixels_per_point();
                p.set_target((r.width() * ppp) as u32, (r.height() * ppp) as u32);
                let uv = Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0));
                ui.painter().image(tex.id(), r, uv, Color32::WHITE);
                if resp.clicked() {
                    self.toggle_play();
                }
            }
            None => {}
        }
    }

    fn empty_state(&mut self, ui: &mut egui::Ui, rect: Rect) {
        let pal = self.theme.pal;
        let c = rect.center();
        ui.painter().text(
            c - vec2(0.0, 30.0),
            Align2::CENTER_CENTER,
            "anyview",
            FontId::proportional(28.0),
            pal.fg,
        );
        ui.painter().text(
            c + vec2(0.0, 4.0),
            Align2::CENTER_CENTER,
            "Drop a photo or video here",
            FontId::proportional(14.0),
            pal.muted,
        );
        let btn = Rect::from_center_size(c + vec2(0.0, 48.0), vec2(120.0, 32.0));
        if ui.put(btn, egui::Button::new("Open…")).clicked() {
            self.open_dialog(ui.ctx());
        }
    }

    fn image_canvas(&mut self, ui: &mut egui::Ui, rect: Rect, resp: &egui::Response, path: &Path) {
        let pal = self.theme.pal;
        let (texture, size) = match self.images.get(path) {
            Some(Slot::Ready { texture, size }) => (texture.clone(), *size),
            Some(Slot::Failed(e)) => {
                let e = e.clone();
                centered_text(ui, rect, &format!("Can't open this image\n{e}"), pal.muted);
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
        let scale = self.zoom.unwrap_or(fit_scale);

        // Zoom at the cursor, pan by dragging.
        if self.crop.is_none() {
            let (scroll, pinch, hover) = ui.input(|i| {
                (
                    i.smooth_scroll_delta.y,
                    i.zoom_delta(),
                    i.pointer.hover_pos(),
                )
            });
            let factor = (scroll * 0.0025).exp() * pinch;
            if (factor - 1.0).abs() > 1e-4
                && let Some(h) = hover.filter(|h| rect.contains(*h))
            {
                let new = (scale * factor).clamp(0.02, 50.0);
                let anchor = h - rect.center() - self.pan;
                self.pan -= anchor * (new / scale - 1.0);
                self.zoom = Some(new);
            }
            if resp.dragged() {
                self.pan += resp.drag_delta();
                if self.zoom.is_none() {
                    self.zoom = Some(scale);
                }
            }
        }
        let scale = self.zoom.unwrap_or(fit_scale);
        let img_rect = Rect::from_center_size(area.center() + self.pan, out * scale);

        let c = shown
            .crop
            .unwrap_or(Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0)));
        let mut mesh = Mesh::with_texture(texture.id());
        let corners = [
            img_rect.left_top(),
            img_rect.right_top(),
            img_rect.right_bottom(),
            img_rect.left_bottom(),
        ];
        let norm = [
            c.left_top(),
            c.right_top(),
            c.right_bottom(),
            c.left_bottom(),
        ];
        for (p, n) in corners.into_iter().zip(norm) {
            mesh.vertices.push(egui::epaint::Vertex {
                pos: p,
                uv: shown.to_uv(n),
                color: Color32::WHITE,
            });
        }
        mesh.indices.extend([0, 1, 2, 0, 2, 3]);
        ui.painter().with_clip_rect(rect).add(Shape::mesh(mesh));

        if self.crop.is_some() {
            self.crop_overlay(ui, img_rect, resp, size, &edit);
        }
    }

    fn crop_overlay(
        &mut self,
        ui: &mut egui::Ui,
        img: Rect,
        resp: &egui::Response,
        size: [usize; 2],
        edit: &Edit,
    ) {
        let pal = self.theme.pal;
        let tool = self.crop.as_mut().unwrap();
        let rsize = edit.rotated_size(size);
        let px_aspect = rsize[0] as f32 / rsize[1] as f32;
        let to_screen = |p: Pos2| img.min + vec2(p.x * img.width(), p.y * img.height());
        let to_norm = |p: Pos2| {
            pos2(
                (p.x - img.min.x) / img.width(),
                (p.y - img.min.y) / img.height(),
            )
        };
        let screen = Rect::from_min_max(to_screen(tool.rect.min), to_screen(tool.rect.max));

        let hit = 12.0;
        // egui reports a drag only after the pointer has moved a few pixels;
        // decide what it grabs from where the button went down, which is
        // where the hover cursor was showing.
        let press = ui.input(|i| i.pointer.press_origin());
        if resp.drag_started()
            && let Some(p) = press.or(resp.interact_pointer_pos())
        {
            let kind = match crop_edges(screen, p, hit) {
                CropDrag::Edge {
                    left: false,
                    right: false,
                    top: false,
                    bottom: false,
                } => {
                    if screen.contains(p) {
                        CropDrag::Move
                    } else {
                        CropDrag::New(to_norm(p))
                    }
                }
                e => e,
            };
            tool.drag = Some((kind, tool.rect, to_norm(p)));
        }
        if let (Some((kind, orig, origin)), Some(p)) = (tool.drag, resp.interact_pointer_pos()) {
            let unit = Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0));
            let n = to_norm(p);
            let n = pos2(n.x.clamp(0.0, 1.0), n.y.clamp(0.0, 1.0));
            let mut r = match kind {
                CropDrag::Move => {
                    let d = n - origin;
                    let d = vec2(
                        d.x.clamp(-orig.min.x, 1.0 - orig.max.x),
                        d.y.clamp(-orig.min.y, 1.0 - orig.max.y),
                    );
                    orig.translate(d)
                }
                CropDrag::New(a) => Rect::from_two_pos(a, n),
                CropDrag::Edge {
                    left,
                    right,
                    top,
                    bottom,
                } => {
                    let mut r = orig;
                    if left {
                        r.min.x = n.x.min(r.max.x - 0.01)
                    }
                    if right {
                        r.max.x = n.x.max(r.min.x + 0.01)
                    }
                    if top {
                        r.min.y = n.y.min(r.max.y - 0.01)
                    }
                    if bottom {
                        r.max.y = n.y.max(r.min.y + 0.01)
                    }
                    r
                }
            };
            if let (Some(a), false) = (tool.aspect, kind == CropDrag::Move) {
                r = constrain_aspect(r, a / px_aspect, kind);
            }
            tool.rect = r.intersect(unit);
        }
        if resp.drag_stopped() {
            tool.drag = None;
        }

        // Dim outside, frame, thirds, handles.
        let screen = Rect::from_min_max(to_screen(tool.rect.min), to_screen(tool.rect.max));
        let dim = Color32::from_black_alpha(150);
        let p = ui.painter().with_clip_rect(img);
        p.rect_filled(
            Rect::from_min_max(img.min, pos2(img.max.x, screen.min.y)),
            0.0,
            dim,
        );
        p.rect_filled(
            Rect::from_min_max(pos2(img.min.x, screen.max.y), img.max),
            0.0,
            dim,
        );
        p.rect_filled(
            Rect::from_min_max(
                pos2(img.min.x, screen.min.y),
                pos2(screen.min.x, screen.max.y),
            ),
            0.0,
            dim,
        );
        p.rect_filled(
            Rect::from_min_max(
                pos2(screen.max.x, screen.min.y),
                pos2(img.max.x, screen.max.y),
            ),
            0.0,
            dim,
        );
        let thin = Stroke::new(1.0, Color32::from_white_alpha(70));
        for k in [1.0 / 3.0, 2.0 / 3.0] {
            let x = screen.min.x + screen.width() * k;
            let y = screen.min.y + screen.height() * k;
            p.line_segment([pos2(x, screen.min.y), pos2(x, screen.max.y)], thin);
            p.line_segment([pos2(screen.min.x, y), pos2(screen.max.x, y)], thin);
        }
        let painter = ui.painter();
        painter.rect_stroke(
            screen,
            0.0,
            Stroke::new(1.5, pal.accent),
            egui::StrokeKind::Middle,
        );
        for c in [
            screen.left_top(),
            screen.right_top(),
            screen.left_bottom(),
            screen.right_bottom(),
        ] {
            painter.circle_filled(c, 5.0, pal.accent);
        }
        let o = edit.rotated_size(size);
        let label = format!(
            "{} × {}",
            (tool.rect.width() * o[0] as f32).round(),
            (tool.rect.height() * o[1] as f32).round()
        );
        painter.text(
            screen.center_bottom() + vec2(0.0, 14.0),
            Align2::CENTER_CENTER,
            label,
            FontId::proportional(12.0),
            pal.fg,
        );
        // While dragging, keep the cursor of what the drag does.
        let hover = match tool.drag {
            Some((kind, ..)) => Some(kind),
            None => resp.hover_pos().map(|h| match crop_edges(screen, h, hit) {
                CropDrag::Edge {
                    left: false,
                    right: false,
                    top: false,
                    bottom: false,
                } if screen.contains(h) => CropDrag::Move,
                CropDrag::Edge {
                    left: false,
                    right: false,
                    top: false,
                    bottom: false,
                } => CropDrag::New(Pos2::ZERO),
                e => e,
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

    fn top_bar(&mut self, ctx: &egui::Context, opacity: f32) {
        let Some(path) = self.current().cloned() else {
            return;
        };
        let kind = library::kind(&path);
        let dirty = self.is_dirty(&path);
        let pal = self.theme.pal;
        egui::Area::new(Id::unique("top"))
            .anchor(Align2::CENTER_TOP, vec2(0.0, 12.0))
            .show(ctx, |ui| {
                ui.set_opacity(opacity);
                bar_frame(&pal).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        let name = path.file_name().unwrap_or_default().to_string_lossy();
                        let short = ellipsize(&name, 48);
                        ui.label(egui::RichText::new(short).strong());
                        if dirty {
                            let (r, resp) = ui.allocate_exact_size(vec2(8.0, 8.0), Sense::hover());
                            ui.painter().circle_filled(r.center(), 3.5, pal.accent);
                            resp.on_hover_text("Unsaved edits");
                        }
                        if self.files.len() > 1 {
                            ui.label(
                                egui::RichText::new(format!(
                                    "{} / {}",
                                    self.index + 1,
                                    self.files.len()
                                ))
                                .color(pal.muted),
                            );
                        }
                        ui.separator();
                        if let Some(current) = self.crop.as_ref().map(|t| t.aspect) {
                            for (label, a) in ASPECTS {
                                if ui.selectable_label(current == a, label).clicked() {
                                    self.set_crop_aspect(a);
                                }
                            }
                            ui.separator();
                            if ui.button("Cancel").clicked() {
                                self.crop = None;
                            }
                            if ui
                                .add(
                                    egui::Button::new(egui::RichText::new("Done").color(pal.bg))
                                        .fill(pal.accent),
                                )
                                .clicked()
                            {
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
                        if dirty || self.job.is_some() {
                            ui.separator();
                            if let Some(job) = &self.job {
                                ui.add(egui::Spinner::new());
                                let f = job.progress.fraction();
                                ui.label(if f > 0.0 {
                                    format!("Saving {:.0}%", f * 100.0)
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
                                let save =
                                    egui::Button::new(egui::RichText::new("Save").color(pal.bg))
                                        .fill(pal.accent);
                                if ui
                                    .add(save)
                                    .on_hover_text("Overwrite the original (Ctrl+S)")
                                    .clicked()
                                {
                                    self.save(ctx, SaveMode::Overwrite);
                                }
                            }
                        }
                        ui.separator();
                        if icon(ui, Icon::Trash, "Move to trash (Del)").clicked() {
                            self.trash_current(ctx);
                        }
                        if icon(ui, Icon::Fullscreen, "Fullscreen (F)").clicked() {
                            toggle_fullscreen(ctx);
                        }
                    });
                });
            });
    }

    fn bottom_bar(&mut self, ctx: &egui::Context, opacity: f32) {
        let Some(path) = self.current().cloned() else {
            return;
        };
        let trim = self
            .trim_mode
            .then(|| self.trims.get(&path).copied())
            .flatten();
        let pal = self.theme.pal;
        let Some(dur) = self
            .player
            .as_ref()
            .filter(|(_, p)| p.info.is_some())
            .map(|(_, p)| p.duration())
        else {
            return;
        };
        let width = (ctx.content_rect().width() - 32.0).min(900.0);
        // Fixed layout: play, time, settings, mute and volume take ~290 px.
        let tl_w = (width - 24.0 - 290.0).max(40.0);
        let cols = ((tl_w - 12.0) / WAVE_STEP).max(1.0) as usize;
        let wave = self.wave_columns(cols, dur).map(<[f32]>::to_vec);
        let st = self.settings;
        let mut new_settings = st;
        let Some((_, p)) = &mut self.player else {
            return;
        };
        let mut action = None;
        egui::Area::new(Id::unique("bottom"))
            .anchor(Align2::CENTER_BOTTOM, vec2(0.0, -12.0))
            .show(ctx, |ui| {
                ui.set_opacity(opacity);
                bar_frame(&pal).show(ui, |ui| {
                    ui.set_width(width - 24.0);
                    ui.horizontal(|ui| {
                        let play = if p.paused { Icon::Play } else { Icon::Pause };
                        if icon(ui, play, "Play/pause (Space)").clicked() {
                            action = Some(BarAction::TogglePlay);
                        }
                        ui.label(
                            egui::RichText::new(format!(
                                "{} / {}",
                                fmt_time(p.position),
                                fmt_time(dur)
                            ))
                            .monospace()
                            .size(12.0),
                        );
                        let vol_w = 90.0;
                        let wave = wave
                            .as_deref()
                            .filter(|_| st.waveform)
                            .map(|w| (w, st.wave_min, st.wave_max, st.wave_filled));
                        let (a, hover) =
                            timeline(ui, tl_w, p.position, dur, trim, wave, &mut self.drag, &pal);
                        if a.is_some() {
                            action = a;
                        }
                        if let Some((t, at)) = hover {
                            hover_preview(ctx, p, t, at, &pal);
                        }
                        let gear = icon(ui, Icon::Settings, "Settings");
                        egui::Popup::from_toggle_button_response(&gear)
                            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                            .show(|ui| settings_ui(ui, &mut new_settings));
                        let vol_icon = if p.muted || p.volume == 0.0 {
                            Icon::Muted
                        } else {
                            Icon::Volume
                        };
                        if icon(ui, vol_icon, "Mute (M)").clicked() {
                            p.muted = !p.muted;
                        }
                        if volume_bar(ui, vol_w - 10.0, &mut p.volume, p.muted, &pal) {
                            p.muted = false;
                        }
                    });
                });
            });
        if new_settings != st {
            self.settings = new_settings.clamp();
            if let Err(e) = self.settings.save() {
                self.notify(ctx, format!("Couldn't save settings: {e}"));
            }
            self.sync_waveform(ctx);
        }
        match action {
            Some(BarAction::TogglePlay) => self.toggle_play(),
            // Dragging the playhead pauses so each frame under the pointer
            // shows; playback resumes on release if it was running.
            Some(BarAction::Scrub(t)) => {
                if let Some((_, p)) = &mut self.player {
                    if self.resume_after_drag.is_none() {
                        self.resume_after_drag = Some(!p.paused);
                        p.set_paused(true);
                    }
                    p.scrub(t);
                }
            }
            Some(BarAction::Seek(t, exact)) => {
                if let Some((_, p)) = &mut self.player {
                    // Ending an active drag: the drag preview stays on screen
                    // until the full-resolution exact frame replaces it.
                    p.seek(t, exact);
                    if self.resume_after_drag.take() == Some(true) {
                        p.set_paused(false);
                    }
                }
            }
            Some(BarAction::Trim { start, t, done }) => self.set_trim_point(start, t, done),
            None => {}
        }
    }

    fn toast(&mut self, ctx: &egui::Context) {
        let Some((msg, until)) = &self.toast else {
            return;
        };
        let now = ctx.input(|i| i.time);
        if now > *until {
            self.toast = None;
            return;
        }
        ctx.request_repaint_after(Duration::from_secs_f64(until - now));
        let pal = self.theme.pal;
        let y = if self.player.is_some() { -84.0 } else { -24.0 };
        egui::Area::new(Id::unique("toast"))
            .anchor(Align2::CENTER_BOTTOM, vec2(0.0, y))
            .interactable(false)
            .show(ctx, |ui| {
                bar_frame(&pal).show(ui, |ui| ui.label(msg.as_str()));
            });
    }

    fn quit_dialog(&mut self, ctx: &egui::Context) {
        if ctx.input(|i| i.viewport().close_requested()) && !self.quitting && self.any_dirty() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.confirm_quit = true;
        }
        if !self.confirm_quit {
            return;
        }
        let pal = self.theme.pal;
        egui::Modal::new(Id::unique("quit")).show(ctx, |ui| {
            ui.set_width(300.0);
            ui.heading("Discard unsaved edits?");
            ui.label(
                egui::RichText::new("Your crops, rotations and trims haven't been saved.")
                    .color(pal.muted),
            );
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Cancel").clicked() {
                    self.confirm_quit = false;
                }
                let b = egui::Button::new(egui::RichText::new("Discard").color(pal.bg))
                    .fill(pal.danger);
                if ui.add(b).clicked() {
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
    /// Set a trim point (`start` or end). `done` snaps the start to its
    /// keyframe; before that (mid-drag) it only scrubs.
    Trim {
        start: bool,
        t: f64,
        done: bool,
    },
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.theme.poll(&ctx);
        self.poll(&ctx);

        let dropped = ctx.input(|i| {
            i.raw
                .dropped_files
                .first()
                .map(|f| f.path().to_path_buf())
                .filter(|p| !p.as_os_str().is_empty())
        });
        if let Some(p) = dropped {
            self.open_path(&ctx, &p);
        }
        self.keys(&ctx);

        // Video clock and trim-range preview.
        let trim = self
            .current()
            .and_then(|p| self.trims.get(p).copied())
            .filter(|_| self.trim_mode);
        let mut next = None;
        if let Some((_, p)) = &mut self.player {
            next = p.update(&ctx);
            if let Some(t) = trim
                && !p.paused
                && p.position >= t.end
            {
                p.set_paused(true);
                p.seek(t.end, true);
            }
            if !p.paused && p.at_end() {
                p.set_paused(true);
            }
        }

        let rect = ui.max_rect();
        ui.painter().rect_filled(rect, 0.0, self.theme.pal.bg);
        self.canvas(ui, rect);

        // Auto-hiding chrome.
        let now = ctx.input(|i| i.time);
        let active =
            ctx.input(|i| i.pointer.is_moving() || !i.events.is_empty() || i.pointer.any_down());
        if active {
            self.chrome_until = now + CHROME_SECS;
        }
        let paused_video = self.player.as_ref().is_some_and(|(_, p)| p.paused);
        let pinned = self.crop.is_some()
            || self.trim_mode
            || paused_video
            || self.job.is_some()
            || self.current().is_some_and(|p| self.is_dirty(p))
            || ctx.is_pointer_over_egui();
        let visible = pinned || now < self.chrome_until;
        if !visible {
            ctx.set_cursor_icon(CursorIcon::None);
        } else if !pinned {
            ctx.request_repaint_after(Duration::from_secs_f64((self.chrome_until - now).max(0.0)));
        }
        let opacity = ctx.animate_bool_with_time(Id::unique("chrome"), visible, 0.15);
        if opacity > 0.0 {
            self.top_bar(&ctx, opacity);
            self.bottom_bar(&ctx, opacity);
        }
        self.toast(&ctx);
        self.quit_dialog(&ctx);

        if let Some(d) = next {
            ctx.request_repaint_after(d);
        }
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        self.theme.pal.bg.to_normalized_gamma_f32()
    }
}

/// Write via a temp file in the target's directory, then rename over the target,
/// so a failed or cancelled save never leaves a half-written file behind.
fn write_atomic(dst: &Path, write: impl FnOnce(&Path) -> Result<(), String>) -> Result<(), String> {
    let tmp = library::temp_path(dst);
    let r = write(&tmp).and_then(|_| std::fs::rename(&tmp, dst).map_err(|e| e.to_string()));
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r
}

/// Write a copy atomically, or overwrite while keeping the original in the trash.
fn save_to(
    dst: &Path,
    mode: SaveMode,
    write: impl FnOnce(&Path) -> Result<(), String>,
) -> Result<Option<String>, String> {
    match mode {
        SaveMode::Overwrite => replace_trashing_original(dst, write),
        SaveMode::Copy => write_atomic(dst, write).map(|_| None),
    }
}

/// Overwrite `dst` without destroying the original: write the new file to a
/// temp path, rename the original to `name-original.ext`, move the new file
/// into place, then send the original to the trash so it can be restored.
/// Returns a note for the user saying where the original went.
fn replace_trashing_original(
    dst: &Path,
    write: impl FnOnce(&Path) -> Result<(), String>,
) -> Result<Option<String>, String> {
    let tmp = library::temp_path(dst);
    if let Err(e) = write(&tmp) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    let original = library::original_path(dst);
    if let Err(e) = std::fs::rename(dst, &original) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("couldn't move the original aside: {e}"));
    }
    if let Err(e) = std::fs::rename(&tmp, dst) {
        // Put the original back; nothing has changed for the user.
        let _ = std::fs::rename(&original, dst);
        let _ = std::fs::remove_file(&tmp);
        return Err(e.to_string());
    }
    let name = original
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    Ok(Some(match trash::delete(&original) {
        Ok(()) => format!("Saved. Original is in the trash as {name}"),
        Err(e) => format!("Saved. Couldn't trash the original ({e}); kept as {name}"),
    }))
}

#[allow(clippy::too_many_arguments)]
fn timeline(
    ui: &mut egui::Ui,
    width: f32,
    pos: f64,
    dur: f64,
    trim: Option<Trim>,
    wave: Option<(&[f32], f32, f32, bool)>,
    drag: &mut Option<Handle>,
    pal: &crate::theme::Palette,
) -> (Option<BarAction>, Option<(f64, Pos2)>) {
    let height = wave.map_or(24.0, |(_, _, max, _)| (max + 8.0).max(24.0));
    let (rect, resp) =
        ui.allocate_exact_size(vec2(width.max(40.0), height), Sense::click_and_drag());
    let track = Rect::from_center_size(rect.center(), vec2(rect.width() - 12.0, 4.0));
    let x_of = |t: f64| track.left() + (t / dur.max(1e-6)).clamp(0.0, 1.0) as f32 * track.width();
    let t_of = |x: f32| ((x - track.left()) / track.width()).clamp(0.0, 1.0) as f64 * dur;
    let mut action = None;

    // Trim handle under `x`, if any: the nearer one within the grab zone.
    const HANDLE_HIT: f32 = 14.0;
    let handle_at = |x: f32| {
        let t = trim?;
        let (ds, de) = ((x - x_of(t.start)).abs(), (x - x_of(t.end)).abs());
        if ds.min(de) > HANDLE_HIT {
            None
        } else if ds <= de {
            Some(Handle::Start)
        } else {
            Some(Handle::End)
        }
    };
    // Decide from where the button went down (a drag registers only after
    // the pointer has moved a few pixels), matching the hover cursor.
    let press = ui.input(|i| i.pointer.press_origin());
    if (resp.drag_started() || resp.clicked())
        && let Some(p) = press.or(resp.interact_pointer_pos())
    {
        *drag = Some(handle_at(p.x).unwrap_or(Handle::Seek));
    }
    if let (Some(h), Some(p)) = (*drag, resp.interact_pointer_pos()) {
        let t = t_of(p.x);
        let released = resp.drag_stopped() || resp.clicked();
        action = Some(match h {
            Handle::Seek if released => BarAction::Seek(t, true),
            Handle::Seek => BarAction::Scrub(t),
            Handle::Start | Handle::End => BarAction::Trim {
                start: h == Handle::Start,
                t,
                done: released,
            },
        });
        if released {
            *drag = None;
        }
    }
    let over_handle = match *drag {
        Some(h) => h != Handle::Seek,
        None => resp.hover_pos().and_then(|h| handle_at(h.x)).is_some(),
    };
    if over_handle {
        ui.ctx().set_cursor_icon(CursorIcon::ResizeColumn);
    } else if resp.hovered() || drag.is_some() {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }

    // Hovered time and the point above the bar where its preview goes.
    // Hidden while any drag is underway (or about to start) and over a trim
    // handle, where the main view already shows the exact frame.
    let pressed = ui.input(|i| i.pointer.any_down());
    let hover = resp
        .hover_pos()
        .filter(|h| drag.is_none() && !pressed && handle_at(h.x).is_none())
        .map(|h| {
            (
                t_of(h.x),
                pos2(h.x.clamp(track.left(), track.right()), rect.top()),
            )
        });
    let p = ui.painter();
    let base = lerp(pal.surface, pal.fg, 0.18);
    // Grey out the parts the trim will cut, over whatever the bar shows.
    let shade_clipped = || {
        let Some(t) = trim else { return };
        let veil = pal.surface.gamma_multiply(0.8);
        let (a, b) = (x_of(t.start), x_of(t.end));
        for (l, r) in [(rect.left(), a), (b, rect.right())] {
            if r > l {
                p.rect_filled(Rect::from_x_y_ranges(l..=r, rect.y_range()), 0.0, veil);
            }
        }
    };
    if let Some((levels, min, max, filled)) = wave {
        // Waveform, accent up to the playhead.
        let px = x_of(pos);
        let unplayed = lerp(pal.surface, pal.fg, 0.32);
        let color_at = |x: f32| if x <= px { pal.accent } else { unplayed };
        let cy = rect.center().y;
        let col = |i: usize| {
            let x = track.left() + (i as f32 + 0.5) * WAVE_STEP;
            (x, min + (max - min) * levels[i])
        };
        if filled {
            // One continuous shape: a quad between each pair of columns,
            // coloured per segment so the playhead split stays sharp.
            let mut mesh = Mesh::default();
            for i in 1..levels.len() {
                let ((x0, h0), (x1, h1)) = (col(i - 1), col(i));
                let c = color_at((x0 + x1) * 0.5);
                let base = mesh.vertices.len() as u32;
                for (x, h) in [(x0, h0), (x1, h1)] {
                    mesh.colored_vertex(pos2(x, cy - h * 0.5), c);
                    mesh.colored_vertex(pos2(x, cy + h * 0.5), c);
                }
                mesh.add_triangle(base, base + 1, base + 2);
                mesh.add_triangle(base + 1, base + 3, base + 2);
            }
            p.add(Shape::mesh(mesh));
        } else {
            for i in 0..levels.len() {
                let (x, h) = col(i);
                let bar = Rect::from_center_size(pos2(x, cy), vec2(WAVE_STEP - 1.0, h));
                p.rect_filled(bar, CornerRadius::same(1), color_at(x));
            }
        }
        shade_clipped();
        if let Some(t) = trim {
            for (x, active) in [
                (x_of(t.start), *drag == Some(Handle::Start)),
                (x_of(t.end), *drag == Some(Handle::End)),
            ] {
                let h = Rect::from_center_size(
                    pos2(x, rect.center().y),
                    vec2(if active { 4.0 } else { 3.0 }, height),
                );
                p.rect_filled(h, CornerRadius::same(2), pal.fg);
            }
        }
        p.rect_filled(
            Rect::from_center_size(pos2(px, rect.center().y), vec2(2.0, height)),
            CornerRadius::same(1),
            pal.fg,
        );
        return (action, hover);
    }
    p.rect_filled(track, CornerRadius::same(2), base);
    let played = Rect::from_x_y_ranges(track.left()..=x_of(pos), track.y_range());
    p.rect_filled(played, CornerRadius::same(2), pal.accent);
    shade_clipped();
    if let Some(t) = trim {
        for (x, active) in [
            (x_of(t.start), *drag == Some(Handle::Start)),
            (x_of(t.end), *drag == Some(Handle::End)),
        ] {
            let h = Rect::from_center_size(
                pos2(x, rect.center().y),
                vec2(if active { 6.0 } else { 4.0 }, 18.0),
            );
            p.rect_filled(h, CornerRadius::same(2), pal.accent);
        }
    }
    p.circle_filled(
        pos2(x_of(pos), track.center().y),
        if resp.hovered() { 6.0 } else { 5.0 },
        pal.fg,
    );
    (action, hover)
}

const WAVE_STEP: f32 = 3.0;

/// YouTube-style thumbnail above the hovered timeline point: the nearest
/// keyframe at or before `t`, with the hovered time under it.
fn hover_preview(
    ctx: &egui::Context,
    p: &mut Player,
    t: f64,
    at: Pos2,
    pal: &crate::theme::Palette,
) {
    let tex = p.preview_at(t, ctx).map(|t| (t.id(), t.size_vec2()));
    let size = tex.map_or(vec2(0.0, 0.0), |(_, s)| s * 0.75);
    egui::Area::new(Id::unique("hover-preview"))
        .order(egui::Order::Tooltip)
        .interactable(false)
        .pivot(Align2::CENTER_BOTTOM)
        .fixed_pos(at - vec2(0.0, 18.0))
        .show(ctx, |ui| {
            egui::Frame::new()
                .fill(pal.surface)
                .stroke(Stroke::new(1.0, lerp(pal.surface, pal.fg, 0.15)))
                .corner_radius(CornerRadius::same(10))
                .inner_margin(egui::Margin::same(4))
                .show(ui, |ui| {
                    ui.vertical_centered(|ui| {
                        if let Some((id, _)) = tex {
                            let img =
                                egui::Image::new((id, size)).corner_radius(CornerRadius::same(7));
                            ui.add(img);
                        }
                        ui.label(egui::RichText::new(fmt_time(t)).monospace().size(11.0));
                    });
                });
        });
}

fn settings_ui(ui: &mut egui::Ui, s: &mut Settings) {
    ui.set_width(240.0);
    ui.spacing_mut().interact_size.y = 20.0;
    ui.spacing_mut().slider_width = 130.0;
    ui.label(egui::RichText::new("Seek bar").strong());
    ui.checkbox(&mut s.waveform, "Show audio waveform");
    ui.add_enabled_ui(s.waveform, |ui| {
        ui.horizontal(|ui| {
            ui.label("Style");
            ui.selectable_value(&mut s.wave_filled, false, "Bars");
            ui.selectable_value(&mut s.wave_filled, true, "Filled");
        });
        egui::Grid::new("wave-settings")
            .num_columns(2)
            .spacing([10.0, 6.0])
            .show(ui, |ui| {
                ui.label("Min height");
                ui.add(
                    egui::Slider::new(&mut s.wave_min, 1.0..=40.0)
                        .suffix(" px")
                        .max_decimals(0),
                );
                ui.end_row();
                ui.label("Max height");
                ui.add(
                    egui::Slider::new(&mut s.wave_max, 2.0..=40.0)
                        .suffix(" px")
                        .max_decimals(0),
                );
                ui.end_row();
                ui.label("Smoothing");
                ui.add(egui::Slider::new(&mut s.wave_smooth, 0.0..=1.0).max_decimals(2));
                ui.end_row();
            });
    });
}

fn volume_bar(
    ui: &mut egui::Ui,
    width: f32,
    volume: &mut f32,
    muted: bool,
    pal: &crate::theme::Palette,
) -> bool {
    let (rect, resp) = ui.allocate_exact_size(vec2(width, 24.0), Sense::click_and_drag());
    let track = Rect::from_center_size(rect.center(), vec2(rect.width() - 8.0, 4.0));
    let mut changed = false;
    if let Some(p) = resp
        .interact_pointer_pos()
        .filter(|_| resp.dragged() || resp.clicked())
    {
        *volume = ((p.x - track.left()) / track.width()).clamp(0.0, 1.0);
        changed = true;
    }
    let v = if muted { 0.0 } else { *volume };
    let x = track.left() + v * track.width();
    let painter = ui.painter();
    painter.rect_filled(
        track,
        CornerRadius::same(2),
        lerp(pal.surface, pal.fg, 0.18),
    );
    painter.rect_filled(
        Rect::from_x_y_ranges(track.left()..=x, track.y_range()),
        CornerRadius::same(2),
        pal.fg,
    );
    if resp.hovered() || resp.dragged() {
        painter.circle_filled(pos2(x, track.center().y), 5.0, pal.fg);
    }
    changed
}

fn constrain_aspect(r: Rect, a: f32, kind: CropDrag) -> Rect {
    // `a` is width/height in normalized units. Keep the anchored corner fixed.
    let (w, h) = (r.width(), r.height());
    let (nw, nh) = if w / h > a { (h * a, h) } else { (w, w / a) };
    let (anchor_x_right, anchor_y_bottom) = match kind {
        CropDrag::Edge { left, top, .. } => (left, top),
        CropDrag::New(a0) => (a0.x > r.center().x, a0.y > r.center().y),
        CropDrag::Move => (false, false),
    };
    let x0 = if anchor_x_right {
        r.max.x - nw
    } else {
        r.min.x
    };
    let y0 = if anchor_y_bottom {
        r.max.y - nh
    } else {
        r.min.y
    };
    Rect::from_min_size(pos2(x0, y0), vec2(nw, nh))
}

/// Which crop edges a pointer at `p` grabs: within `hit` px of an edge line,
/// and along that edge's extent. Two edges at once means a corner.
fn crop_edges(screen: Rect, p: Pos2, hit: f32) -> CropDrag {
    let near = |a: f32, b: f32| (a - b).abs() < hit;
    let inside_y = p.y > screen.top() - hit && p.y < screen.bottom() + hit;
    let inside_x = p.x > screen.left() - hit && p.x < screen.right() + hit;
    CropDrag::Edge {
        left: near(p.x, screen.left()) && inside_y,
        right: near(p.x, screen.right()) && inside_y,
        top: near(p.y, screen.top()) && inside_x,
        bottom: near(p.y, screen.bottom()) && inside_x,
    }
}

const ASPECTS: [(&str, Option<f32>); 5] = [
    ("Free", None),
    ("1:1", Some(1.0)),
    ("4:3", Some(4.0 / 3.0)),
    ("3:2", Some(1.5)),
    ("16:9", Some(16.0 / 9.0)),
];

/// Largest centered rect inside `r` with pixel aspect `aspect`, for an image of
/// pixel size `size` (`r` is normalized).
fn fit_aspect_centered(r: Rect, aspect: f32, size: [usize; 2]) -> Rect {
    let a = aspect / (size[0] as f32 / size[1] as f32);
    let (w, h) = (r.width(), r.height());
    let (nw, nh) = if w / h > a { (h * a, h) } else { (w, w / a) };
    Rect::from_center_size(r.center(), vec2(nw, nh))
}

fn fit(area: Rect, size: Vec2, max_scale: f32) -> Rect {
    let s = (area.width() / size.x)
        .min(area.height() / size.y)
        .min(max_scale);
    Rect::from_center_size(area.center(), size * s)
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

fn bar_frame(pal: &crate::theme::Palette) -> egui::Frame {
    egui::Frame::new()
        .fill(pal.surface.gamma_multiply(0.94))
        .stroke(Stroke::new(1.0, lerp(pal.surface, pal.fg, 0.10)))
        .corner_radius(CornerRadius::same(14))
        .inner_margin(egui::Margin::symmetric(10, 6))
        .shadow(egui::Shadow {
            offset: [0, 4],
            blur: 16,
            spread: 0,
            color: Color32::from_black_alpha(60),
        })
}

#[derive(Clone, Copy)]
enum Icon {
    Play,
    Pause,
    RotateLeft,
    RotateRight,
    Flip,
    Crop,
    Trash,
    Fullscreen,
    Volume,
    Muted,
    Settings,
    Trim,
}

/// Icons are drawn as vector paths: no icon font to ship, crisp at any scale.
fn icon(ui: &mut egui::Ui, icon: Icon, tip: &str) -> egui::Response {
    icon_toggle(ui, icon, false, tip)
}

/// Icon button that stays highlighted while `selected` (a mode is on).
fn icon_toggle(ui: &mut egui::Ui, icon: Icon, selected: bool, tip: &str) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(28.0, 28.0), Sense::click());
    let v = ui.style().interact(&resp);
    if selected {
        let fill = ui.visuals().selection.bg_fill;
        ui.painter().rect_filled(rect, CornerRadius::same(8), fill);
    } else if resp.hovered() {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(8), v.weak_bg_fill);
    }
    let color = v.fg_stroke.color;
    let st = Stroke::new(1.6, color);
    let p = ui.painter();
    let c = rect.center();
    let at = |x: f32, y: f32| c + vec2(x, y);
    let arc = |cw: bool| {
        // 270° arc with an arrowhead at its open end.
        let (r, a0, a1) = (
            6.0_f32,
            -0.25 * std::f32::consts::PI,
            1.25 * std::f32::consts::PI,
        );
        let pts: Vec<Pos2> = (0..=24)
            .map(|i| {
                let a = a0 + (a1 - a0) * i as f32 / 24.0;
                let x = if cw { -a.cos() } else { a.cos() };
                at(x * r, -a.sin() * r)
            })
            .collect();
        let end = pts[0];
        p.add(Shape::line(pts, st));
        let s = if cw { 1.0 } else { -1.0 };
        p.add(Shape::convex_polygon(
            vec![
                end + vec2(-3.5 * s, -3.5),
                end + vec2(1.5 * s, -0.5),
                end + vec2(-3.0 * s, 1.5),
            ],
            color,
            Stroke::NONE,
        ));
    };
    match icon {
        Icon::Play => {
            p.add(Shape::convex_polygon(
                vec![at(-4.0, -6.5), at(6.5, 0.0), at(-4.0, 6.5)],
                color,
                Stroke::NONE,
            ));
        }
        Icon::Pause => {
            for x in [-3.5, 3.5] {
                p.rect_filled(
                    Rect::from_center_size(at(x, 0.0), vec2(3.0, 12.0)),
                    CornerRadius::same(1),
                    color,
                );
            }
        }
        Icon::RotateLeft => arc(true),
        Icon::RotateRight => arc(false),
        Icon::Flip => {
            p.add(Shape::convex_polygon(
                vec![at(-1.8, -6.0), at(-1.8, 6.0), at(-7.5, 6.0)],
                color,
                Stroke::NONE,
            ));
            p.add(Shape::closed_line(
                vec![at(1.8, -6.0), at(1.8, 6.0), at(7.5, 6.0)],
                st,
            ));
            p.add(Shape::dashed_line(
                &[at(0.0, -8.0), at(0.0, 8.0)],
                Stroke::new(1.0, color),
                2.0,
                2.0,
            ));
        }
        Icon::Crop => {
            p.add(Shape::line(
                vec![at(-4.0, -8.0), at(-4.0, 4.0), at(8.0, 4.0)],
                st,
            ));
            p.add(Shape::line(
                vec![at(-8.0, -4.0), at(4.0, -4.0), at(4.0, 8.0)],
                st,
            ));
        }
        Icon::Trash => {
            p.line_segment([at(-6.5, -4.5), at(6.5, -4.5)], st);
            p.add(Shape::line(
                vec![at(-2.5, -4.5), at(-2.5, -7.0), at(2.5, -7.0), at(2.5, -4.5)],
                st,
            ));
            p.add(Shape::line(
                vec![at(-5.0, -4.5), at(-4.0, 7.0), at(4.0, 7.0), at(5.0, -4.5)],
                st,
            ));
            for x in [-1.5, 1.5] {
                p.line_segment([at(x, -1.5), at(x, 4.0)], Stroke::new(1.2, color));
            }
        }
        Icon::Fullscreen => {
            for (sx, sy) in [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
                let corner = at(7.0 * sx, 7.0 * sy);
                p.add(Shape::line(
                    vec![
                        corner - vec2(0.0, 4.0 * sy),
                        corner,
                        corner - vec2(4.0 * sx, 0.0),
                    ],
                    st,
                ));
            }
        }
        Icon::Trim => {
            // Scissors: two finger loops, blades crossing above them.
            for sx in [-1.0, 1.0] {
                p.circle_stroke(at(4.0 * sx, 5.0), 2.6, st);
                p.line_segment([at(2.4 * sx, 3.0), at(-4.5 * sx, -7.5)], st);
            }
        }
        Icon::Settings => {
            p.circle_stroke(c, 3.0, st);
            for i in 0..8 {
                let a = i as f32 * std::f32::consts::FRAC_PI_4;
                let d = vec2(a.cos(), a.sin());
                p.line_segment([c + d * 5.5, c + d * 8.0], Stroke::new(2.4, color));
            }
            p.circle_stroke(c, 6.0, st);
        }
        Icon::Volume | Icon::Muted => {
            p.add(Shape::convex_polygon(
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
                p.line_segment([at(3.0, -3.0), at(8.0, 3.0)], st);
                p.line_segment([at(3.0, 3.0), at(8.0, -3.0)], st);
            } else {
                for r in [4.0_f32, 7.5] {
                    let pts: Vec<Pos2> = (0..=12)
                        .map(|i| {
                            let a = -0.9 + 1.8 * i as f32 / 12.0;
                            at(a.cos() * r, a.sin() * r)
                        })
                        .collect();
                    p.add(Shape::line(pts, st));
                }
            }
        }
    }
    resp.on_hover_text(tip)
}

fn toggle_fullscreen(ctx: &egui::Context) {
    let fs = ctx.input(|i| i.viewport().fullscreen.unwrap_or(false));
    ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(!fs));
}

fn ellipsize(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max / 2).collect();
    let tail: String = s
        .chars()
        .rev()
        .take(max / 2 - 1)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{head}…{tail}")
}

fn fmt_time(t: f64) -> String {
    let t = t.max(0.0);
    let s = t as u64;
    let (h, m, s) = (s / 3600, s / 60 % 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}.{}", ((t.fract()) * 10.0) as u32)
    }
}
