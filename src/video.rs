//! Video playback: one FFmpeg decode thread feeds RGBA frames and f32 audio to the
//! UI thread and a cpal output stream. The decoder sleeps on a condvar whenever
//! its queues are full, so a paused video costs no CPU.

use crate::gpu::{self, Garbage, Gpu, VaDevice, VaFrame};
use crate::grain::Grain;
use crate::lock;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use eframe::egui::{self, Color32, ColorImage, TextureHandle, TextureOptions};
use eframe::glow;
use ffmpeg_next as ff;
use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

const MAX_FRAMES: usize = 4;
/// Memory the video queue may grow to while audio is starving. Cameras
/// interleave audio in large chunks, so reaching the next audio can mean
/// decoding well ahead in the video; capping at MAX_FRAMES there would starve
/// the audio clock and stall playback.
const MAX_QUEUE_BYTES: usize = 320 << 20;
const MAX_AUDIO_SECS: f64 = 0.5;

/// Stream facts known once the file is open.
pub struct Info {
    pub duration: f64,
    pub size: [usize; 2],
    pub fps: f64,
    audio_rate: Option<u32>,
}

/// Pixels of a decoded frame: in system memory, or still in a VA-API
/// surface that the UI thread draws straight from.
enum Picture {
    Cpu(Arc<ColorImage>),
    Gpu(VaFrame),
}

impl Picture {
    fn bytes(&self) -> usize {
        match self {
            Self::Cpu(image) => image.pixels.len() * 4,
            Self::Gpu(frame) => frame.bytes(),
        }
    }
}

struct VideoFrame {
    serial: u64,
    pts: f64,
    picture: Picture,
    /// Shown right away but doesn't start the clock (the fast first frame
    /// while the main decoder warms up).
    still: bool,
}

#[derive(Clone, Copy)]
struct Seek {
    time: f64,
    exact: bool,
}

/// Active drag decodes this far past the pointer, so small steps right are
/// already in the cache.
const LOOKAHEAD: f64 = 0.5;
/// While catching up to a far drag target, frames this far behind it are only
/// converted for display every CATCHUP_EVERY (the rest are decoded, not shown).
const CATCHUP_WINDOW: f64 = 0.5;
const CATCHUP_EVERY: Duration = Duration::from_millis(33);
/// A pointer unchanged this long has stopped: the screen then holds its frame
/// until the exact one for the pointer is decoded.
const DRAG_SETTLE: Duration = Duration::from_millis(150);
/// A forward jump shorter than this keeps decoding instead of seeking.
const CONTINUE_WITHIN: f64 = 3.0;
/// Packets this far before the drag target skip non-reference frames: nothing
/// shows them, and nothing else needs them to decode.
const NONREF_BEFORE: f64 = 0.3;
/// Drag cache budget; freed when playback resumes.
const CACHE_BYTES: usize = 768 << 20;
/// Active-drag frames are cached at most this wide: on a HiDPI screen a
/// window-size 4K frame is ~19 MB, which would leave room for under a second.
/// Releasing the drag re-decodes the frame at full size.
const DRAG_MAX_WIDTH: u32 = 1280;
/// When idle during a drag, fill uncached frames this far before the pointer
/// so dragging left is served from the cache too.
const PREFETCH_BACK: f64 = 4.0;
/// VA-API surfaces the decoder allocates beyond its own references: every
/// GPU frame queued for playback or waiting in the drag cache holds one, and
/// running out stops decoding. The two limits below stay under it.
const EXTRA_SURFACES: i32 = 32;
const MAX_GPU_QUEUE: usize = 12;
const MAX_PENDING: usize = 12;

/// Active-drag state the UI hands the decoder: no serials or flushes, just
/// where the pointer is now.
#[derive(Clone, Copy, Default)]
struct Drag {
    active: bool,
    target: f64,
}

/// A drag-cache entry: CPU pixels, or (GPU path) a decoded surface waiting
/// for the UI thread, then the GL texture it was converted into.
enum Cached {
    Cpu(Arc<ColorImage>),
    Pending(VaFrame),
    Texture {
        texture: glow::Texture,
        size: [u32; 2],
    },
}

impl Cached {
    fn bytes(&self) -> usize {
        match self {
            Self::Cpu(image) => image.pixels.len() * 4,
            Self::Pending(_) => 0,
            Self::Texture { size, .. } => size[0] as usize * size[1] as usize * 4,
        }
    }

    /// What the screen can show of this entry; pending surfaces need
    /// converting first.
    fn shown(&self) -> Option<Shown> {
        match self {
            Self::Cpu(image) => Some(Shown::Cpu(image.clone())),
            Self::Pending(_) => None,
            Self::Texture { texture, size } => Some(Shown::Texture(*texture, *size)),
        }
    }
}

/// A cached frame ready to show.
enum Shown {
    Cpu(Arc<ColorImage>),
    Texture(glow::Texture, [u32; 2]),
}

/// Frames decoded during active drag, already scaled for display and keyed by
/// time in microseconds. The screen shows the best one for the pointer, so
/// going back over frames already seen needs no decoding.
struct FrameCache {
    frames: BTreeMap<i64, Cached>,
    bytes: usize,
    /// Entries still holding a decoder surface.
    pending: usize,
    /// Evicted GL textures, for the UI thread to delete.
    released: Vec<glow::Texture>,
    /// One frame's duration: a frame covers [pts, pts + frame).
    frame: f64,
}

impl Default for FrameCache {
    fn default() -> Self {
        Self {
            frames: BTreeMap::new(),
            bytes: 0,
            pending: 0,
            released: Vec::new(),
            frame: 1.0 / 30.0,
        }
    }
}

fn micros(time: f64) -> i64 {
    (time * 1e6).round() as i64
}

impl FrameCache {
    fn insert(&mut self, pts: f64, entry: Cached, focus: f64) {
        self.bytes += entry.bytes();
        if matches!(entry, Cached::Pending(_)) {
            self.pending += 1;
        }
        if let Some(old) = self.frames.insert(micros(pts), entry) {
            self.forget(old);
        }
        // Over budget: drop whichever end is further from the pointer.
        let focus = micros(focus);
        while self.bytes > CACHE_BYTES {
            let (Some((&first, _)), Some((&last, _))) =
                (self.frames.first_key_value(), self.frames.last_key_value())
            else {
                break;
            };
            if first == last {
                break;
            }
            let evict = if focus - first > last - focus {
                first
            } else {
                last
            };
            if let Some(entry) = self.frames.remove(&evict) {
                self.forget(entry);
            }
        }
    }

    /// Account for an entry leaving the cache.
    fn forget(&mut self, entry: Cached) {
        self.bytes -= entry.bytes();
        match entry {
            Cached::Pending(_) => self.pending -= 1,
            Cached::Texture { texture, .. } => self.released.push(texture),
            Cached::Cpu(_) => {}
        }
    }

    fn clear(&mut self) {
        for entry in std::mem::take(&mut self.frames).into_values() {
            self.forget(entry);
        }
    }

    /// Average bytes per entry, for sizing prefetch.
    fn frame_bytes(&self) -> usize {
        (self.bytes / self.frames.len().max(1)).max(1)
    }

    /// Convert pending surfaces into GL textures; the surfaces go back to
    /// the decoder once the GPU has read them.
    fn convert_pending(&mut self, gpu: &mut Gpu) -> Result<(), String> {
        for entry in self.frames.values_mut() {
            let Cached::Pending(frame) = entry else {
                continue;
            };
            let size = [frame.width, frame.height];
            let texture = gpu.new_texture(size[0], size[1])?;
            let Cached::Pending(frame) =
                std::mem::replace(entry, Cached::Texture { texture, size })
            else {
                continue;
            };
            self.pending -= 1;
            self.bytes += entry.bytes();
            gpu.convert(frame, texture, size[0], size[1])?;
        }
        Ok(())
    }

    /// Time of the cached frame showing at `time` (latest at or before it),
    /// else of the next one.
    fn key_at(&self, time: f64) -> Option<i64> {
        let key = micros(time + self.frame * 0.5);
        self.frames
            .range(..=key)
            .next_back()
            .or_else(|| self.frames.range(key..).next())
            .map(|(&key, _)| key)
    }

    /// The frame showing at `time`, if it is ready to show.
    fn best(&self, time: f64) -> Option<(f64, Shown)> {
        let key = self.key_at(time)?;
        let shown = self.frames.get(&key)?.shown()?;
        Some((key as f64 / 1e6, shown))
    }

    /// The cached frame nearest `to` strictly between the frame at `from` and
    /// `to`: a step toward the pointer that never overshoots it.
    fn toward(&self, from: f64, to: f64) -> Option<(f64, Shown)> {
        let half = self.frame * 0.5;
        let ready =
            |(&key, entry): (&i64, &Cached)| entry.shown().map(|shown| (key as f64 / 1e6, shown));
        if to >= from {
            self.frames
                .range(micros(from + half)..=micros(to + half))
                .rev()
                .find_map(ready)
        } else {
            self.frames
                .range(micros(to - half)..micros(from - half))
                .find_map(ready)
        }
    }

    /// Whether the frame showing at `time` is cached (or being converted).
    fn has(&self, time: f64) -> bool {
        let key = self.key_at(time).map(|key| key as f64 / 1e6);
        self.covers(time, key)
    }

    /// Whether the frame at `pts` is the one showing at `time`.
    fn covers(&self, time: f64, pts: Option<f64>) -> bool {
        pts.is_some_and(|pts| pts <= time + self.frame * 0.5 && pts > time - self.frame * 1.01)
    }
}

#[derive(Default)]
struct State {
    frames: VecDeque<VideoFrame>,
    seek: Option<Seek>,
    serial: u64,
    quit: bool,
    eof: bool,
    info: Option<Result<Arc<Info>, String>>,
}

struct Chunk {
    serial: u64,
    pts: f64,
    data: Vec<f32>,
    /// Samples already played.
    offset: usize,
}

/// State shared with the audio callback. Samples are interleaved stereo.
#[derive(Default)]
struct Audio {
    chunks: Mutex<VecDeque<Chunk>>,
    queued: AtomicU64,
    serial: AtomicU64,
    pos: AtomicU64,
    paused: AtomicBool,
    volume: AtomicU32,
    rate: AtomicU32,
}

impl Audio {
    fn pos(&self) -> f64 {
        f64::from_bits(self.pos.load(Relaxed))
    }

    fn queued_secs(&self) -> f64 {
        let rate = self.rate.load(Relaxed).max(1) as f64;
        self.queued.load(Relaxed) as f64 / 2.0 / rate
    }

    fn clear(&self) {
        lock(&self.chunks).clear();
        self.queued.store(0, Relaxed);
    }

    fn fill(&self, out: &mut [f32]) {
        if self.paused.load(Relaxed) {
            out.fill(0.0);
            return;
        }
        let volume = f32::from_bits(self.volume.load(Relaxed));
        let serial = self.serial.load(Relaxed);
        let rate = self.rate.load(Relaxed).max(1) as f64;
        let mut chunks = lock(&self.chunks);
        let mut written = 0;
        while written < out.len() {
            let Some(chunk) = chunks.front_mut() else {
                break;
            };
            let left = chunk.data.len() - chunk.offset;
            if chunk.serial != serial {
                self.queued.fetch_sub(left as u64, Relaxed);
                chunks.pop_front();
                continue;
            }
            let count = left.min(out.len() - written);
            let src = &chunk.data[chunk.offset..chunk.offset + count];
            for (sample, value) in out[written..written + count].iter_mut().zip(src) {
                *sample = value * volume;
            }
            chunk.offset += count;
            written += count;
            self.queued.fetch_sub(count as u64, Relaxed);
            let position = chunk.pts + (chunk.offset / 2) as f64 / rate;
            self.pos.store(position.to_bits(), Relaxed);
            if chunk.offset == chunk.data.len() {
                chunks.pop_front();
            }
        }
        out[written..].fill(0.0);
    }
}

struct Shared {
    state: Mutex<State>,
    wake: Condvar,
    audio: Audio,
    /// On-screen size in physical pixels (width << 32 | height); frames are scaled down
    /// to it so 4K video doesn't upload 33 MB per frame into a small window.
    target: AtomicU64,
    drag: Mutex<Drag>,
    cache: Mutex<FrameCache>,
    /// Set when the GPU path failed: the decoder then copies VA-API frames
    /// into system memory instead of handing over surfaces.
    cpu_only: AtomicBool,
}

/// One open video: decode thread, audio output and the frame on screen.
pub struct Player {
    shared: Arc<Shared>,
    thread: Option<std::thread::JoinHandle<()>>,
    stream: Option<cpal::Stream>,
    pub info: Option<Arc<Info>>,
    pub error: Option<String>,
    /// Texture for frames decoded on the CPU.
    cpu_texture: Option<TextureHandle>,
    /// What the screen shows: `cpu_texture` or the GPU screen texture.
    shown: Option<egui::TextureId>,
    /// Where cache textures go when this player lets them go.
    garbage: Option<Garbage>,
    /// Copy of the last CPU frame shown, for comparing paths in tests.
    #[cfg(test)]
    last_cpu: Option<ColorImage>,
    serial: u64,
    pub position: f64,
    pub paused: bool,
    /// Wall clock: media time `base` at instant `since` (None while paused).
    base: f64,
    since: Option<Instant>,
    /// Show the next frame even while paused (after a seek or frame step).
    want_frame: bool,
    /// Active drag in progress (paused): show the cached frame for `scrub_time`.
    scrubbing: bool,
    scrub_time: f64,
    /// When `scrub_time` last changed.
    scrub_moved: Instant,
    /// Time of the frame currently on screen.
    pub frame_pts: f64,
    pub volume: f32,
    pub muted: bool,
    path: PathBuf,
    preview: Option<crate::preview::Preview>,
}

impl Player {
    /// Open `path` and start decoding; `ctx` repaints as frames arrive.
    /// With `gpu`, frames decode on the GPU and stay there.
    pub fn open(path: PathBuf, ctx: &egui::Context, gpu: Option<&Gpu>) -> Self {
        let path_for_preview = path.clone();
        let use_gpu = gpu.is_some();
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            wake: Condvar::new(),
            audio: Audio::default(),
            target: AtomicU64::new(0),
            drag: Mutex::default(),
            cache: Mutex::default(),
            cpu_only: AtomicBool::new(false),
        });
        let (thread_shared, thread_ctx) = (shared.clone(), ctx.clone());
        let thread = std::thread::Builder::new()
            .name("decode".into())
            .spawn(move || {
                if let Err(error) = decode(&path, &thread_shared, &thread_ctx, use_gpu) {
                    eprintln!("ommata: decode {}: {error}", path.display());
                    let mut state = lock(&thread_shared.state);
                    if state.info.is_none() {
                        state.info = Some(Err(error.to_string()));
                    }
                    state.eof = true;
                }
                thread_ctx.request_repaint();
            });
        let (thread, error) = match thread {
            Ok(handle) => (Some(handle), None),
            Err(error) => (None, Some(format!("can't start decoder: {error}"))),
        };
        Self {
            shared,
            thread,
            stream: None,
            info: None,
            error,
            cpu_texture: None,
            shown: None,
            garbage: gpu.map(Gpu::garbage),
            #[cfg(test)]
            last_cpu: None,
            serial: 0,
            position: 0.0,
            paused: false,
            base: 0.0,
            since: None,
            want_frame: true,
            scrubbing: false,
            scrub_time: 0.0,
            scrub_moved: Instant::now(),
            frame_pts: 0.0,
            volume: 1.0,
            muted: false,
            path: path_for_preview,
            preview: None,
        }
    }

    /// Set the on-screen size in physical pixels that frames are scaled to.
    pub fn set_target(&self, width: u32, height: u32) {
        self.shared
            .target
            .store((u64::from(width) << 32) | u64::from(height), Relaxed);
    }

    /// Hover thumbnail (nearest earlier keyframe) for `time`. The preview
    /// decoder starts on first use.
    pub fn preview_at(&mut self, time: f64, ctx: &egui::Context) -> Option<&TextureHandle> {
        let path = &self.path;
        self.preview
            .get_or_insert_with(|| crate::preview::Preview::spawn(path.clone(), ctx.clone()))
            .at(time, ctx)
    }

    /// The frame on screen, once one has been decoded.
    pub fn texture(&self) -> Option<egui::TextureId> {
        self.shown
    }

    fn clock(&self) -> f64 {
        self.base
            + self
                .since
                .map_or(0.0, |since| since.elapsed().as_secs_f64())
    }

    fn set_clock(&mut self, time: f64) {
        self.base = time;
        self.since = (!self.paused).then(Instant::now);
    }

    /// Length in seconds, 0 until the file is open.
    pub fn duration(&self) -> f64 {
        self.info.as_ref().map_or(0.0, |info| info.duration)
    }

    /// Pause or resume playback and audio.
    pub fn set_paused(&mut self, paused: bool) {
        if !paused && self.scrubbing {
            // The drag decoder skipped audio and holds lookahead frames;
            // restart normal decoding from the frame on screen.
            self.seek(self.position, true);
        }
        if !paused {
            // Watching doesn't need the drag cache; don't hold its memory.
            lock(&self.shared.cache).clear();
        }
        let now = self.clock();
        self.paused = paused;
        self.set_clock(now);
        self.shared.audio.paused.store(paused, Relaxed);
    }

    /// Active drag to `time` (pauses). The screen shows the frame for `time`
    /// as soon as it is decoded; see `update_drag`.
    pub fn scrub(&mut self, time: f64) {
        if !self.paused {
            self.set_paused(true);
        }
        let time = time.clamp(0.0, self.duration().max(0.0));
        self.scrubbing = true;
        if time != self.scrub_time {
            self.scrub_moved = Instant::now();
        }
        self.scrub_time = time;
        self.position = time;
        *lock(&self.shared.drag) = Drag {
            active: true,
            target: time,
        };
        self.shared.wake.notify_all();
    }

    /// Step `frames` frames (negative = back) from the frame on screen.
    pub fn step(&mut self, frames: i32) {
        let fps = self.info.as_ref().map_or(30.0, |info| info.fps);
        let from = if self.scrubbing {
            self.scrub_time
        } else {
            self.frame_pts
        };
        self.scrub(from + f64::from(frames) / fps);
    }

    /// `exact` decodes forward from the previous keyframe to `time`; inexact
    /// seeks land on the keyframe.
    pub fn seek(&mut self, time: f64, exact: bool) {
        if self.scrubbing {
            self.scrubbing = false;
            lock(&self.shared.drag).active = false;
        }
        self.send_seek(time.clamp(0.0, self.duration().max(0.0)), exact);
        self.want_frame = true;
    }

    fn send_seek(&mut self, time: f64, exact: bool) {
        let mut state = lock(&self.shared.state);
        state.serial += 1;
        self.serial = state.serial;
        state.seek = Some(Seek { time, exact });
        state.frames.clear();
        state.eof = false;
        drop(state);
        self.shared.audio.serial.store(self.serial, Relaxed);
        self.shared.audio.clear();
        self.shared.audio.pos.store(time.to_bits(), Relaxed);
        self.shared.wake.notify_all();
        self.position = time;
        self.set_clock(time);
    }

    /// Whether playback has shown the last frame.
    pub fn at_end(&self) -> bool {
        let state = lock(&self.shared.state);
        state.eof && state.frames.iter().all(|frame| frame.serial != self.serial)
    }

    /// Advance playback; returns how long until the next frame is due. Pass
    /// the `gpu` the player was opened with, on the thread owning its context.
    pub fn update(&mut self, ctx: &egui::Context, mut gpu: Option<&mut Gpu>) -> Option<Duration> {
        if self.info.is_none() && self.error.is_none() {
            match lock(&self.shared.state).info.take() {
                Some(Ok(info)) => {
                    if let Some(rate) = info.audio_rate {
                        self.shared.audio.rate.store(rate, Relaxed);
                        self.stream = start_audio(&self.shared, rate);
                    }
                    self.info = Some(info);
                }
                Some(Err(error)) => self.error = Some(error),
                None => return None,
            }
        }
        let volume = if self.muted { 0.0 } else { self.volume };
        self.shared.audio.volume.store(volume.to_bits(), Relaxed);
        self.convert_cache(gpu.as_deref_mut());

        if self.scrubbing {
            return self.update_drag(ctx, gpu);
        }

        // Audio is the master clock whenever it's flowing.
        let audio = &self.shared.audio;
        if self.stream.is_some() && !self.paused && audio.queued.load(Relaxed) > 0 {
            let audio_time = audio.pos();
            if (audio_time - self.clock()).abs() > 0.04 {
                self.set_clock(audio_time);
            }
        }

        let now = self.clock();
        let mut state = lock(&self.shared.state);
        let mut shown = None;
        let mut synced = false;
        while let Some(frame) = state.frames.front() {
            let due = self.want_frame || (!self.paused && frame.pts <= now);
            if frame.serial == self.serial && !due {
                break;
            }
            let Some(frame) = state.frames.pop_front() else {
                break;
            };
            if frame.serial != self.serial {
                continue;
            }
            if frame.still {
                // Keep waiting for the first real frame to start the clock.
                shown = Some(frame);
                break;
            }
            let first = self.want_frame;
            self.want_frame = false;
            shown = Some(frame);
            if first {
                synced = true;
                break;
            }
        }
        let next_due = state.frames.front().map(|frame| frame.pts - now);
        let starving = state.frames.is_empty() && !state.eof;
        // A seek past the last frame never delivers one; don't wait on it.
        if self.want_frame && state.eof && state.frames.is_empty() {
            self.want_frame = false;
        }
        drop(state);
        if let Some(frame) = shown {
            self.shared.wake.notify_all();
            // Paused, or the first frame after opening/seeking: the clock starts
            // from this frame, so playback begins here instead of skipping
            // ahead by however long the decode took.
            if self.paused || synced {
                self.position = frame.pts;
                self.set_clock(frame.pts);
            }
            self.show_picture(ctx, frame.pts, frame.picture, gpu);
        }
        if !self.paused && !synced {
            self.position = self.clock().min(self.duration());
        }

        if self.paused && !self.want_frame {
            None
        } else if let Some(wait) = next_due {
            Some(Duration::from_secs_f64(wait.clamp(0.0, 0.25)))
        } else if starving || self.want_frame {
            Some(Duration::from_millis(4))
        } else {
            None
        }
    }
}

impl Player {
    fn show_cpu(&mut self, ctx: &egui::Context, pts: f64, image: &ColorImage) {
        self.frame_pts = pts;
        #[cfg(test)]
        {
            self.last_cpu = Some(image.clone());
        }
        let image = image.clone();
        let texture = match &mut self.cpu_texture {
            Some(texture) => {
                texture.set(image, TextureOptions::LINEAR);
                texture
            }
            None => {
                self.cpu_texture
                    .insert(ctx.load_texture("video", image, TextureOptions::LINEAR))
            }
        };
        self.shown = Some(texture.id());
    }

    fn show_picture(
        &mut self,
        ctx: &egui::Context,
        pts: f64,
        picture: Picture,
        gpu: Option<&mut Gpu>,
    ) {
        match (picture, gpu) {
            (Picture::Cpu(image), _) => self.show_cpu(ctx, pts, &image),
            (Picture::Gpu(frame), Some(gpu)) => match gpu.show_frame(frame) {
                Ok(()) => {
                    self.frame_pts = pts;
                    self.shown = gpu.screen_id();
                }
                Err(error) => self.gpu_failed(Some(gpu), &error),
            },
            (Picture::Gpu(_), None) => self.gpu_failed(None, "no GL context"),
        }
    }

    fn show_cached(&mut self, ctx: &egui::Context, pts: f64, shown: Shown, gpu: Option<&mut Gpu>) {
        match (shown, gpu) {
            (Shown::Cpu(image), _) => self.show_cpu(ctx, pts, &image),
            (Shown::Texture(texture, [width, height]), Some(gpu)) => {
                gpu.show_texture(texture, width, height);
                self.frame_pts = pts;
                self.shown = gpu.screen_id();
            }
            (Shown::Texture(..), None) => {}
        }
    }

    /// Hand released cache textures to the GPU's garbage and convert surfaces
    /// the decoder left in the cache, freeing them for further decoding.
    fn convert_cache(&mut self, mut gpu: Option<&mut Gpu>) {
        let mut cache = lock(&self.shared.cache);
        if let Some(garbage) = &self.garbage {
            lock(garbage).append(&mut cache.released);
        }
        if cache.pending == 0 {
            return;
        }
        let result = match gpu.as_deref_mut() {
            Some(gpu) => cache.convert_pending(gpu),
            None => Err("no GL context".into()),
        };
        drop(cache);
        self.shared.wake.notify_all();
        if let Err(error) = result {
            self.gpu_failed(gpu, &error);
        }
    }

    /// Give up on the GPU path for this file: the decoder switches to copying
    /// frames into system memory and the current position is decoded again.
    fn gpu_failed(&mut self, gpu: Option<&mut Gpu>, error: &str) {
        eprintln!("ommata: GPU video path failed, using the CPU: {error}");
        if let Some(gpu) = gpu {
            gpu.failed = true;
        }
        self.shared.cpu_only.store(true, Relaxed);
        lock(&self.shared.cache).clear();
        if self.scrubbing {
            // The drag decoder refills the cache from the CPU now.
            self.shared.wake.notify_all();
        } else {
            self.seek(self.position, true);
        }
    }

    /// Active drag: show the frame under the pointer once it is cached. While
    /// the pointer moves, cached frames on the way to it show too; once it
    /// rests the screen holds until the exact frame is decoded, so catching up
    /// through a long GOP never plays out on screen.
    fn update_drag(&mut self, ctx: &egui::Context, gpu: Option<&mut Gpu>) -> Option<Duration> {
        let at_end = lock(&self.shared.state).eof;
        let moving = self.scrub_moved.elapsed() < DRAG_SETTLE;
        let cache = lock(&self.shared.cache);
        let best = cache.best(self.scrub_time);
        let best_exact = cache.covers(self.scrub_time, best.as_ref().map(|(pts, _)| *pts));
        let next = if best_exact || at_end || self.shown.is_none() {
            best
        } else if moving {
            cache.toward(self.frame_pts, self.scrub_time)
        } else {
            None
        };
        drop(cache);
        if let Some((pts, shown)) = next
            && pts != self.frame_pts
        {
            self.show_cached(ctx, pts, shown, gpu);
        }
        (!best_exact && !at_end).then_some(Duration::from_millis(4))
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        if let Some(garbage) = &self.garbage {
            let mut cache = lock(&self.shared.cache);
            cache.clear();
            lock(garbage).append(&mut cache.released);
        }
        lock(&self.shared.state).quit = true;
        self.shared.wake.notify_all();
        self.stream = None;
        // Detach rather than join: closing a file must never stall the UI.
        drop(self.thread.take());
    }
}

/// Open the default output device at `rate`. Without one the video plays
/// silently, clocked by the wall clock.
fn start_audio(shared: &Arc<Shared>, rate: u32) -> Option<cpal::Stream> {
    let Some(device) = cpal::default_host().default_output_device() else {
        eprintln!("ommata: audio: no output device");
        return None;
    };
    let config = cpal::StreamConfig {
        channels: 2,
        sample_rate: rate,
        buffer_size: cpal::BufferSize::Default,
    };
    let shared = shared.clone();
    let stream = device
        .build_output_stream(
            config,
            move |out: &mut [f32], _| shared.audio.fill(out),
            |error| eprintln!("ommata: audio: {error}"),
            None,
        )
        .inspect_err(|error| eprintln!("ommata: audio: {error}"))
        .ok()?;
    stream
        .play()
        .inspect_err(|error| eprintln!("ommata: audio: {error}"))
        .ok()?;
    Some(stream)
}

/// Media time of the first packet in seconds: streams' timestamps are offset
/// by it, and every time this app shows starts at zero.
pub fn start_time(input: &ff::format::context::Input) -> f64 {
    // SAFETY: plain field read on a live, opened context.
    let start = unsafe { (*input.as_ptr()).start_time };
    if start == ff::ffi::AV_NOPTS_VALUE {
        0.0
    } else {
        start as f64 / f64::from(ff::ffi::AV_TIME_BASE)
    }
}

/// Frame converter to RGBA at display size. Uses swscale's frame API, which
/// spreads the work over all cores (a 4K frame is too slow on one) and reads
/// colour range/space from the frames themselves.
pub struct Scaler(*mut ff::ffi::SwsContext);

impl Scaler {
    /// A bilinear converter using all cores; `None` if allocation fails.
    pub fn new() -> Option<Self> {
        // SAFETY: plain allocation; fields are set before first use and the
        // context is freed in Drop.
        unsafe {
            let context = ff::ffi::sws_alloc_context();
            if context.is_null() {
                return None;
            }
            (*context).flags = ff::ffi::SwsFlags::SWS_BILINEAR as _;
            (*context).threads = 0; // automatic
            Some(Self(context))
        }
    }

    /// Convert `src` into `dst` as RGBA at `width`×`height`; false on failure.
    pub fn run(
        &mut self,
        src: &ff::frame::Video,
        dst: &mut ff::frame::Video,
        width: u32,
        height: u32,
    ) -> bool {
        if dst.width() != width || dst.height() != height {
            *dst = ff::frame::Video::new(ff::format::Pixel::RGBA, width, height);
        }
        // SAFETY: both frames are valid; dst is allocated with matching size.
        unsafe { ff::ffi::sws_scale_frame(self.0, dst.as_mut_ptr(), src.as_ptr()) >= 0 }
    }
}

impl Drop for Scaler {
    fn drop(&mut self) {
        // SAFETY: frees the context from new().
        unsafe { ff::ffi::sws_free_context(&mut self.0) };
    }
}

/// Seek `input` to the keyframe at or before `time`. A failed seek leaves the
/// demuxer where it was, which only delays the frame the caller waits for.
fn seek_to(input: &mut ff::format::context::Input, time: f64, start_offset: f64) {
    let target = ((time + start_offset) * f64::from(ff::ffi::AV_TIME_BASE)) as i64;
    if let Err(error) = input.seek(target, ..target) {
        eprintln!("ommata: seek to {time:.3}: {error}");
    }
}

/// Active-drag frames are cached at most DRAG_MAX_WIDTH wide.
fn drag_size(width: u32, height: u32) -> (u32, u32) {
    if width <= DRAG_MAX_WIDTH {
        return (width, height);
    }
    let scaled = f64::from(height) * f64::from(DRAG_MAX_WIDTH) / f64::from(width);
    (DRAG_MAX_WIDTH, ((scaled as u32) & !1).max(2))
}

/// Decode thread: plays from the seek the UI last sent, or fills the drag
/// cache while a drag is active. With `use_gpu`, supported codecs decode on
/// the GPU and frames are handed over as VA-API surfaces.
fn decode(
    path: &std::path::Path,
    shared: &Shared,
    ctx: &egui::Context,
    use_gpu: bool,
) -> Result<(), ff::Error> {
    let mut input = ff::format::input(path)?;
    let start_offset = start_time(&input);
    let duration = input.duration().max(0) as f64 / f64::from(ff::ffi::AV_TIME_BASE);

    let video_stream = input
        .streams()
        .best(ff::media::Type::Video)
        .ok_or(ff::Error::StreamNotFound)?;
    let video_index = video_stream.index();
    let video_time_base = f64::from(video_stream.time_base());
    let stream_fps = f64::from(video_stream.avg_frame_rate());
    let fps = if stream_fps.is_finite() && stream_fps > 0.0 {
        stream_fps
    } else {
        30.0
    };
    let frame = 1.0 / fps;
    let mut video_codec = ff::codec::Context::from_parameters(video_stream.parameters())?;
    let hardware = use_gpu
        .then(|| gpu::decoder(video_codec.id()))
        .flatten()
        .and_then(|codec| Some((VaDevice::open()?, codec)))
        .filter(|(device, _)| device.decodes(&video_stream.parameters()));
    let va_display = hardware.as_ref().map(|(device, _)| device.display());
    let mut video_decoder = if let Some((device, codec)) = &hardware {
        // The GPU decodes; extra CPU frame threads would only add latency.
        device.attach(&mut video_codec, EXTRA_SURFACES);
        video_codec.decoder().open_as(*codec)?.video()?
    } else {
        video_codec.set_threading(ff::codec::threading::Config {
            kind: ff::codec::threading::Type::Frame,
            count: 0,
        });
        video_codec.decoder().video()?
    };

    let mut audio_decoder = None;
    let mut audio_time_base = 0.0;
    let mut audio_index = usize::MAX;
    if let Some(stream) = input.streams().best(ff::media::Type::Audio)
        && let Ok(decoder) = ff::codec::Context::from_parameters(stream.parameters())
            .and_then(|codec| codec.decoder().audio())
    {
        audio_index = stream.index();
        audio_time_base = f64::from(stream.time_base());
        audio_decoder = Some(decoder);
    }

    let info = Arc::new(Info {
        duration,
        size: [
            video_decoder.width() as usize,
            video_decoder.height() as usize,
        ],
        fps,
        audio_rate: audio_decoder
            .as_ref()
            .map(|decoder| decoder.rate())
            .filter(|&rate| rate > 0),
    });
    lock(&shared.state).info = Some(Ok(info));
    ctx.request_repaint();

    // The main decoder's frame threads need a dozen packets before the first
    // frame comes out (~0.3 s on 4K). Show frame one from a slice-threaded
    // decoder first, then rewind.
    let first = first_frame(
        &mut input,
        video_index,
        video_time_base,
        start_offset,
        shared,
    );
    if let Some((pts, image)) = first {
        push_frame(
            shared,
            VideoFrame {
                serial: 0,
                pts,
                picture: Picture::Cpu(Arc::new(image)),
                still: true,
            },
        );
        ctx.request_repaint();
    }
    seek_to(&mut input, 0.0, start_offset);

    lock(&shared.cache).frame = frame;
    let mut scaler = Scaler::new().ok_or(ff::Error::Bug)?;
    let mut converter = AudioConverter::new(ff::ChannelLayout::STEREO, 2);
    let mut video_frame = ff::frame::Video::empty();
    let mut downloaded = ff::frame::Video::empty();
    let mut rgba = ff::frame::Video::empty();
    let mut audio_frame = ff::frame::Audio::empty();
    let mut serial = 0;
    let mut skip_until = f64::NEG_INFINITY;
    // Newest frame out of the decoder, and whether it has been told the file
    // ended: lets a forward scrub keep decoding instead of seeking back.
    let mut last_decoded = f64::NEG_INFINITY;
    let mut eof_sent = false;
    // Active drag: current target, and whether a frame has come out since the
    // last seek (a new seek before that would starve the screen on slow 4K).
    let mut drag: Option<f64> = None;
    let mut produced = true;
    // Target of the last drag seek: anything up to CONTINUE_WITHIN past it is
    // reached by decoding on (seeking again would land on the same keyframe).
    let mut run_target = f64::NEG_INFINITY;
    // Backward prefetch only looks below this, so each pass moves further back
    // and prefetch finishes even if some frame never appears.
    let mut floor = f64::NAN;
    // Pointer direction: prefetch only when moving left or resting, so a
    // forward drag never loses its decode position.
    let mut last_target = f64::NAN;
    let mut backward = false;
    let mut moved_at = Instant::now();
    let mut last_catchup: Option<Instant> = None;

    loop {
        // Commands and backpressure.
        {
            let mut state = lock(&shared.state);
            loop {
                if state.quit {
                    return Ok(());
                }
                if let Some(seek) = state.seek.take() {
                    serial = state.serial;
                    drop(state);
                    drag = None;
                    set_skip_nonref(&mut video_decoder, false);
                    // A short step forward (dragging right) continues from the
                    // current position; anything else seeks to the keyframe.
                    let ahead = seek.time - last_decoded;
                    let continue_forward = seek.exact && !eof_sent && (0.0..3.0).contains(&ahead);
                    if !continue_forward {
                        seek_to(&mut input, seek.time, start_offset);
                        video_decoder.flush();
                        if let Some(decoder) = &mut audio_decoder {
                            decoder.flush();
                        }
                        last_decoded = f64::NEG_INFINITY;
                        eof_sent = false;
                    }
                    skip_until = if seek.exact {
                        seek.time
                    } else {
                        f64::NEG_INFINITY
                    };
                    state = lock(&shared.state);
                    continue;
                }
                let pointer = *lock(&shared.drag);
                if pointer.active {
                    if lock(&shared.cache).pending >= MAX_PENDING {
                        // Wait for the UI to convert surfaces it holds.
                        ctx.request_repaint();
                        state = shared
                            .wake
                            .wait_timeout(state, Duration::from_millis(10))
                            .unwrap_or_else(PoisonError::into_inner)
                            .0;
                        continue;
                    }
                    let target = pointer.target;
                    if target.to_bits() != last_target.to_bits() {
                        backward = target < last_target;
                        last_target = target;
                        moved_at = Instant::now();
                        floor = target;
                    }
                    let resting = moved_at.elapsed() > Duration::from_millis(250);
                    // A frame the current run should have produced but didn't:
                    // doesn't exist (or was a skipped catch-up frame of a run
                    // aimed elsewhere, which a fresh seek fixes).
                    let passed = |time: f64| {
                        last_decoded.is_finite()
                            && time < last_decoded
                            && (time - run_target).abs() < frame
                    };
                    // Jobs by priority: the frame under the pointer, the
                    // lookahead right of it, then (moving left or resting)
                    // backward prefetch below `floor`, so passes finish.
                    let job = {
                        let cache = lock(&shared.cache);
                        let first_gap = |from: f64, to: f64| {
                            let mut time = from;
                            while time <= to {
                                if !cache.has(time) && !passed(time) {
                                    return Some(time);
                                }
                                time += frame;
                            }
                            None
                        };
                        let end = (duration - frame).max(0.0);
                        if !cache.has(target) && !passed(target) {
                            Some(target)
                        } else if let Some(gap) =
                            first_gap(target + frame, (target + LOOKAHEAD).min(end))
                        {
                            Some(gap)
                        } else if backward || resting {
                            // No further back than the cache can hold next to
                            // the lookahead, or prefetch would evict itself.
                            let fits = (CACHE_BYTES / cache.frame_bytes()) as f64 * frame;
                            let back = PREFETCH_BACK.min(fits * 0.6 - LOOKAHEAD).max(0.0);
                            let low = (target - back).max(0.0);
                            let mut time = floor - frame;
                            while time > low && cache.has(time) {
                                time -= frame;
                            }
                            (time > low).then_some(time)
                        } else {
                            None
                        }
                    };
                    let Some(goal) = job else {
                        drag = Some(target);
                        // Nothing to do until the pointer moves (or rests long
                        // enough to start prefetching).
                        state = shared
                            .wake
                            .wait_timeout(state, Duration::from_millis(100))
                            .unwrap_or_else(PoisonError::into_inner)
                            .0;
                        continue;
                    };
                    if goal < target - frame {
                        floor = goal;
                    }
                    drag = Some(goal);
                    let ahead = goal - last_decoded;
                    let can_continue = !eof_sent
                        && last_decoded.is_finite()
                        && ahead >= -frame
                        && (ahead < CONTINUE_WITHIN || goal <= run_target + CONTINUE_WITHIN);
                    // Seek only once the last seek has produced a frame: on slow
                    // 4K, seeking on every pointer move would show nothing.
                    if !can_continue && produced {
                        drop(state);
                        seek_to(&mut input, goal, start_offset);
                        video_decoder.flush();
                        last_decoded = f64::NEG_INFINITY;
                        eof_sent = false;
                        produced = false;
                        run_target = goal;
                        state = lock(&shared.state);
                        state.eof = false;
                    }
                    break;
                }
                // Audio only holds the decoder back once a video frame is
                // queued: while paused nothing drains audio, and the frame a
                // seek is waiting for may sit behind it in the file.
                let audio_secs = shared.audio.queued_secs();
                let use_audio = audio_decoder.is_some();
                let audio_low = use_audio && audio_secs < 0.2;
                let frame_bytes = state
                    .frames
                    .front()
                    .map_or(0, |queued| queued.picture.bytes());
                let surfaces_used = state.frames.len() >= MAX_GPU_QUEUE
                    && state
                        .frames
                        .back()
                        .is_some_and(|queued| matches!(queued.picture, Picture::Gpu(_)));
                let video_full = surfaces_used
                    || (state.frames.len() >= MAX_FRAMES
                        && !(audio_low && state.frames.len() * frame_bytes < MAX_QUEUE_BYTES));
                let full = video_full
                    || (use_audio && !state.frames.is_empty() && audio_secs > MAX_AUDIO_SECS);
                if !full && !state.eof {
                    break;
                }
                // Audio drains without notifying, so wake periodically while it does.
                let timeout = if state.eof {
                    Duration::from_secs(3600)
                } else {
                    Duration::from_millis(50)
                };
                state = shared
                    .wake
                    .wait_timeout(state, timeout)
                    .unwrap_or_else(PoisonError::into_inner)
                    .0;
            }
        }

        // A packet the decoder rejects (corrupt data) loses only its own
        // frame or samples, so send errors are not fatal.
        let packet = input
            .packets()
            .next()
            .map(|(stream, packet)| (stream.index(), packet));
        match &packet {
            Some((index, packet)) if *index == video_index => {
                if let Some(goal) = drag {
                    let pts = packet.pts().or(packet.dts()).map_or(f64::INFINITY, |pts| {
                        pts as f64 * video_time_base - start_offset
                    });
                    set_skip_nonref(&mut video_decoder, pts < goal - NONREF_BEFORE);
                }
                let _ = video_decoder.send_packet(packet);
            }
            Some((index, _)) if *index == audio_index && drag.is_some() => continue,
            Some((index, packet)) if *index == audio_index => {
                if let Some(decoder) = &mut audio_decoder {
                    let _ = decoder.send_packet(packet);
                }
            }
            Some(_) => continue,
            None => {
                eof_sent = true;
                let _ = video_decoder.send_eof();
                if let Some(decoder) = &mut audio_decoder {
                    let _ = decoder.send_eof();
                }
            }
        }

        while video_decoder.receive_frame(&mut video_frame).is_ok() {
            let Some(timestamp) = video_frame.timestamp().or(video_frame.pts()) else {
                continue;
            };
            let pts = timestamp as f64 * video_time_base - start_offset;
            last_decoded = last_decoded.max(pts);
            let target = shared.target.load(Relaxed);
            let (width, height) = scaled_size(video_frame.width(), video_frame.height(), target);
            // Surfaces go to the UI as they are, unless the GPU path failed:
            // then they are copied into system memory and scaled on the CPU.
            let surface_display = va_display
                .filter(|_| gpu::is_hardware(&video_frame) && !shared.cpu_only.load(Relaxed));
            if surface_display.is_none() && gpu::is_hardware(&video_frame) {
                // The decoder exports film grain instead of applying it;
                // add it to the copy in system memory.
                let grain = Grain::of(&video_frame);
                if !gpu::download(&video_frame, &mut downloaded) {
                    continue;
                }
                std::mem::swap(&mut video_frame, &mut downloaded);
                if let Some(grain) = grain {
                    grain.apply_to(&mut video_frame);
                }
            }
            let mut take_surface = |width: u32, height: u32| {
                let decoded = std::mem::replace(&mut video_frame, ff::frame::Video::empty());
                surface_display.and_then(|display| VaFrame::new(decoded, display, width, height))
            };
            if let Some(goal) = drag {
                // Active drag: frames go to the cache. Far behind the goal,
                // only a few, so catching up isn't slowed by conversion.
                produced = true;
                if pts < goal - CATCHUP_WINDOW {
                    if last_catchup.is_some_and(|at| at.elapsed() < CATCHUP_EVERY) {
                        continue;
                    }
                    last_catchup = Some(Instant::now());
                }
                let (cache_width, cache_height) = drag_size(width, height);
                let entry = if surface_display.is_some() {
                    take_surface(cache_width, cache_height).map(Cached::Pending)
                } else if scaler.run(&video_frame, &mut rgba, cache_width, cache_height) {
                    Some(Cached::Cpu(Arc::new(to_color_image(&rgba))))
                } else {
                    None
                };
                if let Some(entry) = entry {
                    lock(&shared.cache).insert(pts, entry, goal);
                    ctx.request_repaint();
                }
                continue;
            }
            if pts < skip_until - 0.5 * frame {
                continue;
            }
            let picture = if surface_display.is_some() {
                take_surface(width, height).map(Picture::Gpu)
            } else if scaler.run(&video_frame, &mut rgba, width, height) {
                Some(Picture::Cpu(Arc::new(to_color_image(&rgba))))
            } else {
                None
            };
            if let Some(picture) = picture {
                push_frame(
                    shared,
                    VideoFrame {
                        serial,
                        pts,
                        still: false,
                        picture,
                    },
                );
            }
        }

        if let Some(decoder) = &mut audio_decoder {
            while decoder.receive_frame(&mut audio_frame).is_ok() {
                let Some(timestamp) = audio_frame.timestamp().or(audio_frame.pts()) else {
                    continue;
                };
                let pts = timestamp as f64 * audio_time_base - start_offset;
                if pts < skip_until - 0.05 {
                    continue;
                }
                let Some(data) = converter.run(&mut audio_frame) else {
                    continue;
                };
                push_audio(
                    shared,
                    Chunk {
                        serial,
                        pts,
                        data,
                        offset: 0,
                    },
                );
            }
        }

        if packet.is_none() {
            lock(&shared.state).eof = true;
            ctx.request_repaint();
        }
    }
}

/// Frame one from a quick slice-threaded decoder, scaled for the screen.
fn first_frame(
    input: &mut ff::format::context::Input,
    video_index: usize,
    video_time_base: f64,
    start_offset: f64,
    shared: &Shared,
) -> Option<(f64, ColorImage)> {
    let parameters = input.stream(video_index)?.parameters();
    // dav1d's own threading already delivers frame one quickly.
    if parameters.id() == ff::codec::Id::AV1 {
        return None;
    }
    let mut codec = ff::codec::Context::from_parameters(parameters).ok()?;
    codec.set_threading(ff::codec::threading::Config {
        kind: ff::codec::threading::Type::Slice,
        count: 0,
    });
    let mut decoder = codec.decoder().video().ok()?;
    let mut frame = ff::frame::Video::empty();
    // A packet the decoder rejects just delays frame one to the next.
    for (stream, packet) in input.packets() {
        if stream.index() != video_index {
            continue;
        }
        let _ = decoder.send_packet(&packet);
        if decoder.receive_frame(&mut frame).is_ok() {
            break;
        }
    }
    if frame.width() == 0 {
        let _ = decoder.send_eof();
        decoder.receive_frame(&mut frame).ok()?;
    }
    let pts = frame.timestamp().or(frame.pts())? as f64 * video_time_base - start_offset;
    let target = shared.target.load(Relaxed);
    let (width, height) = scaled_size(frame.width(), frame.height(), target);
    let mut rgba = ff::frame::Video::empty();
    Scaler::new()?
        .run(&frame, &mut rgba, width, height)
        .then(|| (pts, to_color_image(&rgba)))
}

/// Skip decoding non-reference frames (or stop skipping). Frame threads pick
/// the setting up per packet.
fn set_skip_nonref(decoder: &mut ff::decoder::Video, skip: bool) {
    let discard = if skip {
        ff::ffi::AVDiscard::AVDISCARD_NONREF
    } else {
        ff::ffi::AVDiscard::AVDISCARD_DEFAULT
    };
    // SAFETY: plain field write on an open decoder, read at packet submission.
    unsafe { (*decoder.as_mut_ptr()).skip_frame = discard };
}

/// Fit `width`×`height` inside the packed on-screen target, never upscaling.
/// Sizes snap to 64 px steps so resizing the window doesn't rebuild the scaler
/// every frame.
fn scaled_size(width: u32, height: u32, target: u64) -> (u32, u32) {
    let (target_width, target_height) = ((target >> 32) as u32, target as u32);
    if target_width == 0 || target_height == 0 {
        return (width, height);
    }
    let target_width = f64::from(target_width.div_ceil(64) * 64);
    let target_height = f64::from(target_height.div_ceil(64) * 64);
    let scale = (target_width / f64::from(width))
        .min(target_height / f64::from(height))
        .min(1.0);
    let even = |size: f64| ((size.round() as u32) & !1).max(2);
    (
        even(f64::from(width) * scale),
        even(f64::from(height) * scale),
    )
}

/// Give the frame the default layout for its channel count when the file
/// doesn't specify one (common for PCM in camera MOVs): swresample rejects
/// unspecified layouts, both when configured and per frame. Returns the layout.
fn known_layout(frame: &mut ff::frame::Audio) -> ff::ChannelLayout {
    if frame.channel_layout().bits() == 0 {
        let channels = i32::from(frame.channels()).max(1);
        frame.set_channel_layout(ff::ChannelLayout::default(channels));
    }
    frame.channel_layout()
}

/// Input format a resampler was built for: sample format, layout, rate.
type AudioFormat = (ff::format::Sample, u64, u32);

/// Converts decoded audio of any format to interleaved f32 in a fixed
/// channel layout at the source rate. Rebuilt when the input format changes.
pub struct AudioConverter {
    layout: ff::ChannelLayout,
    channels: usize,
    resampler: Option<(ff::software::resampling::Context, AudioFormat)>,
}

impl AudioConverter {
    /// A converter to `layout`, which has `channels` channels.
    pub fn new(layout: ff::ChannelLayout, channels: usize) -> Self {
        Self {
            layout,
            channels,
            resampler: None,
        }
    }

    /// The samples of `frame`, or `None` if its format can't be converted.
    pub fn run(&mut self, frame: &mut ff::frame::Audio) -> Option<Vec<f32>> {
        let layout = known_layout(frame);
        let format = (frame.format(), layout.bits(), frame.rate());
        if self
            .resampler
            .as_ref()
            .is_none_or(|(_, current)| *current != format)
        {
            // Same rate in and out: the output device runs at the source rate.
            self.resampler = ff::software::resampling::Context::get(
                format.0,
                layout,
                format.2,
                ff::format::Sample::F32(ff::format::sample::Type::Packed),
                self.layout,
                format.2,
            )
            .ok()
            .map(|resampler| (resampler, format));
        }
        let (resampler, _) = self.resampler.as_mut()?;
        let mut out = ff::frame::Audio::empty();
        resampler.run(frame, &mut out).ok()?;
        let bytes = &out.data(0)[..out.samples() * self.channels * 4];
        let samples = bytes.as_chunks::<4>().0;
        Some(
            samples
                .iter()
                .map(|sample| f32::from_ne_bytes(*sample))
                .collect(),
        )
    }
}

fn push_frame(shared: &Shared, frame: VideoFrame) {
    let mut state = lock(&shared.state);
    if frame.serial == state.serial && state.seek.is_none() {
        state.frames.push_back(frame);
    }
}

fn push_audio(shared: &Shared, chunk: Chunk) {
    if chunk.serial != shared.audio.serial.load(Relaxed) {
        return;
    }
    let samples = chunk.data.len() - chunk.offset;
    shared.audio.queued.fetch_add(samples as u64, Relaxed);
    lock(&shared.audio.chunks).push_back(chunk);
}

/// Copy an RGBA frame from the scaler into an egui image.
pub fn to_color_image(frame: &ff::frame::Video) -> ColorImage {
    let (width, height) = (frame.width() as usize, frame.height() as usize);
    let stride = frame.stride(0);
    let data = frame.data(0);
    let mut pixels = vec![Color32::BLACK; width * height];
    // SAFETY: Color32 is #[repr(C)] over [u8; 4], so every byte pattern is a
    // valid value; the byte view covers exactly the vector's initialized length.
    // swscale's RGBA output is opaque, so it is already premultiplied.
    let bytes = unsafe {
        std::slice::from_raw_parts_mut(pixels.as_mut_ptr().cast::<u8>(), width * height * 4)
    };
    for (dst, src) in bytes.chunks_exact_mut(width * 4).zip(data.chunks(stride)) {
        dst.copy_from_slice(&src[..width * 4]);
    }
    ColorImage::new([width, height], pixels)
}

#[cfg(test)]
mod tests {
    use crate::gpu::{Gpu, headless};
    use crate::lock;
    use std::cell::RefCell;

    thread_local! {
        /// GPU path for this test thread, with OMMATA_TEST_GPU=1.
        static GPU: RefCell<Option<(headless::Context, Gpu)>> = const { RefCell::new(None) };
    }

    fn update(ctx: &eframe::egui::Context, player: &mut super::Player) {
        GPU.with_borrow_mut(|gpu| player.update(ctx, gpu.as_mut().map(|(_, gpu)| gpu)));
    }

    /// Decodes a first frame headlessly: OMMATA_TEST_VIDEO=a.webm cargo test first_frame -- --ignored
    #[test]
    #[ignore]
    fn first_frame() {
        let path = std::path::PathBuf::from(std::env::var("OMMATA_TEST_VIDEO").unwrap());
        ffmpeg_next::init().unwrap();
        let ctx = eframe::egui::Context::default();
        let mut player = super::Player::open(path, &ctx, None);
        let started = std::time::Instant::now();
        while player.texture().is_none()
            && player.error.is_none()
            && started.elapsed().as_secs() < 5
        {
            player.update(&ctx, None);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(player.error.is_none(), "{:?}", player.error);
        let info = player.info.as_ref().expect("no stream info");
        eprintln!(
            "{:?} {:.2}s, first frame after {:?}",
            info.size,
            info.duration,
            started.elapsed()
        );
        assert!(player.texture().is_some(), "no frame decoded");
    }

    fn open_test_player() -> (eframe::egui::Context, super::Player) {
        let path = std::path::PathBuf::from(std::env::var("OMMATA_TEST_VIDEO").unwrap());
        ffmpeg_next::init().unwrap();
        let ctx = eframe::egui::Context::default();
        if std::env::var_os("OMMATA_TEST_GPU").is_some() {
            let (context, gl) = headless::context();
            let mut gpu = Gpu::new(gl).expect("GPU video path");
            gpu.register(|_| eframe::egui::TextureId::User(0));
            GPU.set(Some((context, gpu)));
        }
        let player = GPU
            .with_borrow(|gpu| super::Player::open(path, &ctx, gpu.as_ref().map(|(_, gpu)| gpu)));
        // A large window on this HiDPI screen, in physical pixels.
        player.set_target(2800, 1600);
        (ctx, player)
    }

    fn pump(ctx: &eframe::egui::Context, player: &mut super::Player, secs: f64) {
        let started = std::time::Instant::now();
        while started.elapsed().as_secs_f64() < secs {
            update(ctx, player);
            std::thread::sleep(std::time::Duration::from_millis(4));
        }
    }

    /// The frame shown after an exact seek is the one FFmpeg decodes for that
    /// time, with matching colours (GPU path with OMMATA_TEST_GPU=1).
    /// OMMATA_TEST_VIDEO=clip.mp4 cargo test --release seek_frame -- --ignored --nocapture
    #[test]
    #[ignore]
    fn seek_frame_matches_ffmpeg() {
        let (ctx, mut player) = open_test_player();
        pump(&ctx, &mut player, 0.5);
        player.set_paused(true);
        player.seek(2.0, true);
        pump(&ctx, &mut player, 1.5);
        let gpu_frame = GPU.with_borrow(|gpu| gpu.as_ref().map(|(_, gpu)| gpu.read_screen()));
        let (pixels, [width, height]) = match gpu_frame {
            Some(frame) => {
                let screen = eframe::egui::TextureId::User(0);
                assert_eq!(
                    player.texture(),
                    Some(screen),
                    "frame wasn't drawn on the GPU"
                );
                frame
            }
            None => {
                let image = player.last_cpu.clone().expect("no CPU frame shown");
                let size = [image.size[0] as u32, image.size[1] as u32];
                (image.as_raw().to_vec(), size)
            }
        };
        let path = std::env::var("OMMATA_TEST_VIDEO").unwrap();
        let reference = std::process::Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-ss",
                &format!("{:.6}", player.frame_pts - 0.001),
            ])
            .args(["-i", &path, "-frames:v", "1", "-vf"])
            .arg(format!("scale={width}:{height}:flags=bilinear"))
            .args(["-pix_fmt", "rgba", "-f", "rawvideo", "-"])
            .output()
            .unwrap()
            .stdout;
        assert_eq!(reference.len(), pixels.len(), "reference frame size");
        if let Some(dir) = std::env::var_os("OMMATA_TEST_DUMP") {
            let dir = std::path::PathBuf::from(dir);
            std::fs::write(dir.join("gpu.rgba"), &pixels).unwrap();
            std::fs::write(dir.join("cpu.rgba"), &reference).unwrap();
        }
        let difference: u64 = pixels
            .iter()
            .zip(&reference)
            .enumerate()
            .filter(|(index, _)| index % 4 != 3)
            .map(|(_, (gpu, cpu))| u64::from(gpu.abs_diff(*cpu)))
            .sum();
        let mean = difference as f64 / (pixels.len() / 4 * 3) as f64;
        eprintln!(
            "{width}x{height} at {:.3}: mean difference {mean:.2}",
            player.frame_pts
        );
        assert!(
            mean < 4.0,
            "GPU colours differ from FFmpeg by {mean:.2} on average"
        );
    }

    /// Playback must advance on its own (regression: the clock never started
    /// until the first pause/resume when there was no audio to drive it).
    #[test]
    #[ignore]
    fn plays_without_pause_toggle() {
        let (ctx, mut player) = open_test_player();
        let started = std::time::Instant::now();
        while player.texture().is_none() && started.elapsed().as_secs() < 5 {
            pump(&ctx, &mut player, 0.01);
        }
        let start = player.position;
        pump(&ctx, &mut player, 1.5);
        let audio = player.shared.audio.pos();
        eprintln!(
            "first frame after {:?}; advanced {:.2}s in 1.5s; audio clock {audio:.2}",
            started.elapsed() - std::time::Duration::from_secs_f64(1.5),
            player.position - start
        );
        assert!(
            player.position - start > 1.3,
            "advanced only {:.2}",
            player.position - start
        );
    }

    /// A fast drag shows intermediate frames and lands on the last target.
    #[test]
    #[ignore]
    fn scrub_shows_frames_and_settles() {
        let (ctx, mut player) = open_test_player();
        pump(&ctx, &mut player, 0.5);
        player.set_paused(true);
        let mut seen = std::collections::HashSet::new();
        for i in 0..60 {
            player.scrub(0.5 + i as f64 * 0.05);
            pump(&ctx, &mut player, 0.02);
            seen.insert((player.frame_pts * 1000.0) as i64);
        }
        pump(&ctx, &mut player, 1.0);
        eprintln!(
            "distinct frames while dragging: {}, on screen {:.3}",
            seen.len(),
            player.frame_pts
        );
        assert!(seen.len() > 5, "drag showed only {} frames", seen.len());
        // Give a slow (long-GOP 4K) decode time to land on the last target.
        pump(&ctx, &mut player, 1.5);
        eprintln!("settled on {:.3}", player.frame_pts);
        assert!(
            (player.frame_pts - 3.45).abs() < 0.05,
            "settled at {:.3}",
            player.frame_pts
        );
        // Dragging back must seek back, not continue forward.
        for i in 0..10 {
            player.scrub(3.0 - i as f64 * 0.2);
            pump(&ctx, &mut player, 0.02);
        }
        pump(&ctx, &mut player, 3.0);
        eprintln!("after dragging back: {:.3}", player.frame_pts);

        assert!(
            (player.frame_pts - 1.2).abs() < 0.05,
            "settled at {:.3}",
            player.frame_pts
        );
        // Latency of a backward jump into the middle of a GOP.
        let before = player.frame_pts;
        let start_offset = std::time::Instant::now();
        player.scrub(before + 1.7);
        let mut first = None;
        while start_offset.elapsed().as_secs() < 5
            && (player.frame_pts - (before + 1.7)).abs() > 0.02
        {
            pump(&ctx, &mut player, 0.002);
            if first.is_none() && player.frame_pts != before {
                first = Some(start_offset.elapsed());
            }
        }
        let fwd = start_offset.elapsed();
        let t1 = std::time::Instant::now();
        player.scrub(before + 0.9);
        let mut first_back = None;
        while t1.elapsed().as_secs() < 5 && (player.frame_pts - (before + 0.9)).abs() > 0.02 {
            pump(&ctx, &mut player, 0.002);
            if first_back.is_none() && (player.frame_pts - (before + 1.7)).abs() > 0.02 {
                first_back = Some(t1.elapsed());
            }
        }
        eprintln!(
            "jump +1.7s: first frame {first:?}, exact {fwd:?} | jump back 0.8s: first frame {first_back:?}, exact {:?}",
            t1.elapsed()
        );
    }

    /// Active-drag benchmark: a 60 Hz UI pointer moving like a real trim
    /// session; reports how far the frame on screen trails the pointer.
    /// OMMATA_TEST_VIDEO=clip.mkv cargo test --release bench_drag -- --ignored --nocapture
    #[test]
    #[ignore]
    fn bench_drag() {
        let (ctx, mut player) = open_test_player();
        pump(&ctx, &mut player, 0.6);
        player.set_paused(true);
        pump(&ctx, &mut player, 0.2);
        let duration = player.duration();
        let base = (duration * 0.3).min(4.0);
        // A click far away first (timed on its own), then drags from there.
        let started = std::time::Instant::now();
        player.scrub(base);
        let mut first = None;
        let start_pts = player.frame_pts;
        while player.frame_pts.to_bits() != base.to_bits()
            && !lock(&player.shared.cache).covers(base, Some(player.frame_pts))
            && started.elapsed().as_secs() < 10
        {
            pump(&ctx, &mut player, 0.001);
            if first.is_none() && player.frame_pts != start_pts {
                first = Some(started.elapsed());
            }
        }
        eprintln!(
            "BENCH jump to {base:.1}s: first frame {:.0} ms, exact {:.0} ms",
            first.map_or(f64::NAN, |elapsed| elapsed.as_secs_f64() * 1000.0),
            started.elapsed().as_secs_f64() * 1000.0
        );
        // (name, seconds, target(time))
        type Path = Box<dyn Fn(f64) -> f64>;
        let phases: Vec<(&str, f64, Path)> = vec![
            ("fwd 1x", 2.0, Box::new(move |time| base + time)),
            ("fwd 3x", 1.0, Box::new(move |time| base + 2.0 + 3.0 * time)),
            ("back 1x", 2.0, Box::new(move |time| base + 5.0 - time)),
            (
                "back 3x",
                1.0,
                Box::new(move |time| base + 3.0 - 3.0 * time),
            ),
            (
                "jiggle",
                3.0,
                Box::new(move |time| base + 1.0 + 0.4 * (time * std::f64::consts::TAU * 0.7).sin()),
            ),
        ];
        let mut total = Vec::new();
        for (name, secs, path) in &phases {
            let start = std::time::Instant::now();
            let mut errors = Vec::new();
            let mut frames = std::collections::HashSet::new();
            while start.elapsed().as_secs_f64() < *secs {
                let target = path(start.elapsed().as_secs_f64()).clamp(0.0, duration);
                player.scrub(target);
                let tick = std::time::Instant::now();
                while tick.elapsed().as_millis() < 16 {
                    update(&ctx, &mut player);
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                errors.push((player.frame_pts - target).abs());
                frames.insert((player.frame_pts * 1000.0) as i64);
            }
            errors.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let mean = errors.iter().sum::<f64>() / errors.len() as f64;
            let p95 = errors[errors.len() * 95 / 100];
            let exact = errors.iter().filter(|error| **error < 0.02).count() * 100 / errors.len();
            eprintln!(
                "BENCH {name:8} mean lag {:6.0} ms  p95 {:6.0} ms  exact {exact:3}%  frames {}",
                mean * 1000.0,
                p95 * 1000.0,
                frames.len()
            );
            total.extend(errors);
        }
        let target = base + 1.0;
        player.scrub(target);
        let started = std::time::Instant::now();
        while (player.frame_pts - target).abs() > 0.02 && started.elapsed().as_secs() < 10 {
            pump(&ctx, &mut player, 0.002);
        }
        let mean = total.iter().sum::<f64>() / total.len() as f64;
        eprintln!(
            "BENCH TOTAL mean lag {:.0} ms, settle {:.0} ms",
            mean * 1000.0,
            started.elapsed().as_secs_f64() * 1000.0
        );
    }
}
