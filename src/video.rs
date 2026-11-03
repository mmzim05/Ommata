//! Video playback: one FFmpeg decode thread feeds RGBA frames and f32 audio to the
//! UI thread and a cpal output stream. The decoder sleeps on a condvar whenever
//! its queues are full, so a paused video costs no CPU.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use eframe::egui::{self, Color32, ColorImage, TextureHandle, TextureOptions};
use ffmpeg_next as ff;
use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

const MAX_FRAMES: usize = 4;
/// Memory the video queue may grow to while audio is starving. Cameras
/// interleave audio in large chunks, so reaching the next audio can mean
/// decoding well ahead in the video; capping at MAX_FRAMES there would starve
/// the audio clock and stall playback.
const MAX_QUEUE_BYTES: usize = 320 << 20;
const MAX_AUDIO_SECS: f64 = 0.5;

pub struct Info {
    pub duration: f64,
    pub size: [usize; 2],
    pub fps: f64,
    audio_rate: Option<u32>,
}

struct VFrame {
    serial: u64,
    pts: f64,
    image: Arc<ColorImage>,
    /// Shown right away but doesn't start the clock (the fast first frame
    /// while the main decoder warms up).
    still: bool,
}

#[derive(Clone, Copy)]
struct Seek {
    t: f64,
    exact: bool,
}

/// Active drag decodes this far past the pointer, so small steps right are
/// already in the cache.
const LOOKAHEAD: f64 = 0.5;
/// While catching up to a far drag target, frames this far behind it are only
/// converted for display every CATCHUP_EVERY (the rest are decoded, not shown).
const CATCHUP_WINDOW: f64 = 0.5;
const CATCHUP_EVERY: Duration = Duration::from_millis(33);
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

/// Active-drag state the UI hands the decoder: no serials or flushes, just
/// where the pointer is now.
#[derive(Clone, Copy, Default)]
struct Drag {
    active: bool,
    target: f64,
}

/// Frames decoded during active drag, already scaled for display and keyed by
/// time in microseconds. The screen shows the best one for the pointer, so
/// going back over frames already seen needs no decoding.
struct FrameCache {
    frames: BTreeMap<i64, Arc<ColorImage>>,
    bytes: usize,
    /// One frame's duration: a frame covers [pts, pts + frame).
    frame: f64,
}

impl Default for FrameCache {
    fn default() -> Self {
        Self {
            frames: BTreeMap::new(),
            bytes: 0,
            frame: 1.0 / 30.0,
        }
    }
}

fn micros(t: f64) -> i64 {
    (t * 1e6).round() as i64
}

impl FrameCache {
    fn insert(&mut self, pts: f64, image: Arc<ColorImage>, focus: f64) {
        let size = image.pixels.len() * 4;
        if let Some(old) = self.frames.insert(micros(pts), image) {
            self.bytes -= old.pixels.len() * 4;
        }
        self.bytes += size;
        // Over budget: drop whichever end is further from the pointer.
        let f = micros(focus);
        while self.bytes > CACHE_BYTES && self.frames.len() > 1 {
            let (&lo, _) = self.frames.first_key_value().unwrap();
            let (&hi, _) = self.frames.last_key_value().unwrap();
            let k = if f - lo > hi - f { lo } else { hi };
            let img = self.frames.remove(&k).unwrap();
            self.bytes -= img.pixels.len() * 4;
        }
    }

    /// The frame showing at `t` (latest at or before it), else the next one.
    fn best(&self, t: f64) -> Option<(f64, Arc<ColorImage>)> {
        let k = micros(t + self.frame * 0.5);
        self.frames
            .range(..=k)
            .next_back()
            .or_else(|| self.frames.range(k..).next())
            .map(|(&k, img)| (k as f64 / 1e6, img.clone()))
    }

    /// Whether the frame showing at `t` is cached.
    fn has(&self, t: f64) -> bool {
        self.covers(t, self.best(t).map(|b| b.0))
    }

    /// Whether the frame at `pts` is the one showing at `t`.
    fn covers(&self, t: f64, pts: Option<f64>) -> bool {
        pts.is_some_and(|p| p <= t + self.frame * 0.5 && p > t - self.frame * 1.01)
    }
}

#[derive(Default)]
struct State {
    frames: VecDeque<VFrame>,
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
    off: usize,
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
        self.chunks.lock().unwrap().clear();
        self.queued.store(0, Relaxed);
    }

    fn fill(&self, out: &mut [f32]) {
        if self.paused.load(Relaxed) {
            out.fill(0.0);
            return;
        }
        let vol = f32::from_bits(self.volume.load(Relaxed));
        let serial = self.serial.load(Relaxed);
        let rate = self.rate.load(Relaxed).max(1) as f64;
        let mut q = self.chunks.lock().unwrap();
        let mut i = 0;
        while i < out.len() {
            let Some(c) = q.front_mut() else { break };
            let left = c.data.len() - c.off;
            if c.serial != serial {
                self.queued.fetch_sub(left as u64, Relaxed);
                q.pop_front();
                continue;
            }
            let n = left.min(out.len() - i);
            for (o, s) in out[i..i + n].iter_mut().zip(&c.data[c.off..c.off + n]) {
                *o = s * vol;
            }
            c.off += n;
            i += n;
            self.queued.fetch_sub(n as u64, Relaxed);
            let pos = c.pts + (c.off / 2) as f64 / rate;
            self.pos.store(pos.to_bits(), Relaxed);
            if c.off == c.data.len() {
                q.pop_front();
            }
        }
        out[i..].fill(0.0);
    }
}

struct Shared {
    st: Mutex<State>,
    cv: Condvar,
    audio: Audio,
    /// On-screen size in physical pixels (w << 32 | h); frames are scaled down
    /// to it so 4K video doesn't upload 33 MB per frame into a small window.
    target: AtomicU64,
    drag: Mutex<Drag>,
    cache: Mutex<FrameCache>,
}

pub struct Player {
    sh: Arc<Shared>,
    thread: Option<std::thread::JoinHandle<()>>,
    stream: Option<cpal::Stream>,
    pub info: Option<Arc<Info>>,
    pub error: Option<String>,
    texture: Option<TextureHandle>,
    serial: u64,
    pub position: f64,
    pub paused: bool,
    /// Wall clock: media time `base` at instant `since` (None while paused).
    base: f64,
    since: Option<Instant>,
    /// Show the next frame even while paused (after a seek or frame step).
    want_frame: bool,
    /// Active drag in progress (paused): show the cached frame for `scrub_t`.
    scrubbing: bool,
    scrub_t: f64,
    /// Time of the frame currently on screen.
    pub frame_pts: f64,
    pub volume: f32,
    pub muted: bool,
    path: PathBuf,
    preview: Option<crate::preview::Preview>,
}

impl Player {
    pub fn open(path: PathBuf, ctx: &egui::Context) -> Self {
        let path_for_preview = path.clone();
        let sh = Arc::new(Shared {
            st: Mutex::new(State::default()),
            cv: Condvar::new(),
            audio: Audio::default(),
            target: AtomicU64::new(0),
            drag: Mutex::default(),
            cache: Mutex::default(),
        });
        let (sh2, ctx2) = (sh.clone(), ctx.clone());
        let thread = std::thread::Builder::new()
            .name("decode".into())
            .spawn(move || {
                if let Err(e) = decode(&path, &sh2, &ctx2) {
                    eprintln!("anyview: decode {}: {e}", path.display());
                    let mut st = sh2.st.lock().unwrap();
                    if st.info.is_none() {
                        st.info = Some(Err(e.to_string()));
                    }
                    st.eof = true;
                }
                ctx2.request_repaint();
            })
            .ok();
        Self {
            sh,
            thread,
            stream: None,
            info: None,
            error: None,
            texture: None,
            serial: 0,
            position: 0.0,
            paused: false,
            base: 0.0,
            since: None,
            want_frame: true,
            scrubbing: false,
            scrub_t: 0.0,
            frame_pts: 0.0,
            volume: 1.0,
            muted: false,
            path: path_for_preview,
            preview: None,
        }
    }

    pub fn set_target(&self, w: u32, h: u32) {
        self.sh
            .target
            .store((u64::from(w) << 32) | u64::from(h), Relaxed);
    }

    /// Hover thumbnail (nearest earlier keyframe) for time `t`. The preview
    /// decoder starts on first use.
    pub fn preview_at(&mut self, t: f64, ctx: &egui::Context) -> Option<&TextureHandle> {
        let path = &self.path;
        self.preview
            .get_or_insert_with(|| crate::preview::Preview::spawn(path.clone(), ctx.clone()))
            .at(t, ctx)
    }

    pub fn texture(&self) -> Option<&TextureHandle> {
        self.texture.as_ref()
    }

    fn clock(&self) -> f64 {
        self.base + self.since.map_or(0.0, |s| s.elapsed().as_secs_f64())
    }

    fn set_clock(&mut self, t: f64) {
        self.base = t;
        self.since = (!self.paused).then(Instant::now);
    }

    pub fn duration(&self) -> f64 {
        self.info.as_ref().map_or(0.0, |i| i.duration)
    }

    pub fn set_paused(&mut self, paused: bool) {
        if !paused && self.scrubbing {
            // The drag decoder skipped audio and holds lookahead frames;
            // restart normal decoding from the frame on screen.
            self.seek(self.position, true);
        }
        if !paused {
            // Watching doesn't need the drag cache; don't hold its memory.
            let mut c = self.sh.cache.lock().unwrap();
            c.frames.clear();
            c.bytes = 0;
        }
        let now = self.clock();
        self.paused = paused;
        self.set_clock(now);
        self.sh.audio.paused.store(paused, Relaxed);
    }

    /// Active drag to time `t` (pauses). The screen shows the best decoded
    /// frame for `t` at once while the decoder works toward the exact one.
    pub fn scrub(&mut self, t: f64) {
        if !self.paused {
            self.set_paused(true);
        }
        let t = t.clamp(0.0, self.duration().max(0.0));
        self.scrubbing = true;
        self.scrub_t = t;
        self.position = t;
        *self.sh.drag.lock().unwrap() = Drag {
            active: true,
            target: t,
        };
        self.sh.cv.notify_all();
    }

    /// Step `n` frames (negative = back) from the frame on screen.
    pub fn step(&mut self, n: i32) {
        let fps = self.info.as_ref().map_or(30.0, |i| i.fps);
        let from = if self.scrubbing {
            self.scrub_t
        } else {
            self.frame_pts
        };
        self.scrub(from + f64::from(n) / fps);
    }

    /// `exact` decodes forward from the previous keyframe to `t`; inexact seeks
    /// land on the keyframe.
    pub fn seek(&mut self, t: f64, exact: bool) {
        if self.scrubbing {
            self.scrubbing = false;
            self.sh.drag.lock().unwrap().active = false;
        }
        self.send_seek(t.clamp(0.0, self.duration().max(0.0)), exact);
        self.want_frame = true;
    }

    fn send_seek(&mut self, t: f64, exact: bool) {
        let mut st = self.sh.st.lock().unwrap();
        st.serial += 1;
        self.serial = st.serial;
        st.seek = Some(Seek { t, exact });
        st.frames.clear();
        st.eof = false;
        drop(st);
        self.sh.audio.serial.store(self.serial, Relaxed);
        self.sh.audio.clear();
        self.sh.audio.pos.store(t.to_bits(), Relaxed);
        self.sh.cv.notify_all();
        self.position = t;
        self.set_clock(t);
    }

    pub fn at_end(&self) -> bool {
        let st = self.sh.st.lock().unwrap();
        st.eof && st.frames.iter().all(|f| f.serial != self.serial)
    }

    /// Advance playback; returns how long until the next frame is due.
    pub fn update(&mut self, ctx: &egui::Context) -> Option<Duration> {
        if self.info.is_none() && self.error.is_none() {
            match self.sh.st.lock().unwrap().info.take() {
                Some(Ok(info)) => {
                    if let Some(rate) = info.audio_rate {
                        self.sh.audio.rate.store(rate, Relaxed);
                        self.stream = start_audio(&self.sh, rate);
                    }
                    self.info = Some(info);
                }
                Some(Err(e)) => self.error = Some(e),
                None => return None,
            }
        }
        let vol = if self.muted { 0.0 } else { self.volume };
        self.sh.audio.volume.store(vol.to_bits(), Relaxed);

        if self.scrubbing {
            return self.update_drag(ctx);
        }

        // Audio is the master clock whenever it's flowing.
        let audio = &self.sh.audio;
        if self.stream.is_some() && !self.paused && audio.queued.load(Relaxed) > 0 {
            let a = audio.pos();
            if (a - self.clock()).abs() > 0.04 {
                self.set_clock(a);
            }
        }

        let now = self.clock();
        let mut st = self.sh.st.lock().unwrap();
        let mut shown = None;
        let mut synced = false;
        while let Some(f) = st.frames.front() {
            if f.serial != self.serial {
                st.frames.pop_front();
            } else if self.want_frame || (!self.paused && f.pts <= now) {
                let f = st.frames.pop_front().unwrap();
                if f.still {
                    // Keep waiting for the first real frame to start the clock.
                    shown = Some(f);
                    break;
                }
                let first = self.want_frame;
                self.want_frame = false;
                shown = Some(f);
                if first {
                    synced = true;
                    break;
                }
            } else {
                break;
            }
        }
        let next_due = st.frames.front().map(|f| f.pts - now);
        let starving = st.frames.is_empty() && !st.eof;
        // A seek past the last frame never delivers one; don't wait on it.
        if self.want_frame && st.eof && st.frames.is_empty() {
            self.want_frame = false;
        }
        drop(st);
        if let Some(f) = shown {
            self.sh.cv.notify_all();
            // Paused, or the first frame after opening/seeking: the clock starts
            // from this frame, so playback begins here instead of skipping
            // ahead by however long the decode took.
            if self.paused || synced {
                self.position = f.pts;
                self.set_clock(f.pts);
            }
            self.show(ctx, f.pts, &f.image);
        }
        if !self.paused && !synced {
            self.position = self.clock().min(self.duration());
        }

        if self.paused && !self.want_frame {
            None
        } else if let Some(d) = next_due {
            Some(Duration::from_secs_f64(d.clamp(0.0, 0.25)))
        } else if starving || self.want_frame {
            Some(Duration::from_millis(4))
        } else {
            None
        }
    }
}

impl Player {
    fn show(&mut self, ctx: &egui::Context, pts: f64, image: &ColorImage) {
        self.frame_pts = pts;
        let image = image.clone();
        match &mut self.texture {
            Some(t) => t.set(image, TextureOptions::LINEAR),
            None => self.texture = Some(ctx.load_texture("video", image, TextureOptions::LINEAR)),
        }
    }

    /// Active drag: show the cached frame for the pointer; keep polling until
    /// the exact one has been decoded.
    fn update_drag(&mut self, ctx: &egui::Context) -> Option<Duration> {
        let best = self.sh.cache.lock().unwrap().best(self.scrub_t);
        if let Some((pts, img)) = best
            && (pts != self.frame_pts || self.texture.is_none())
        {
            self.show(ctx, pts, &img);
        }
        let exact = self
            .sh
            .cache
            .lock()
            .unwrap()
            .covers(self.scrub_t, Some(self.frame_pts));
        let at_end = self.sh.st.lock().unwrap().eof;
        (!exact && !at_end).then_some(Duration::from_millis(4))
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.sh.st.lock().unwrap().quit = true;
        self.sh.cv.notify_all();
        self.stream = None;
        // Detach rather than join: closing a file must never stall the UI.
        drop(self.thread.take());
    }
}

fn start_audio(sh: &Arc<Shared>, rate: u32) -> Option<cpal::Stream> {
    let device = cpal::default_host().default_output_device()?;
    let config = cpal::StreamConfig {
        channels: 2,
        sample_rate: rate,
        buffer_size: cpal::BufferSize::Default,
    };
    let sh = sh.clone();
    let stream = device
        .build_output_stream(
            config,
            move |out: &mut [f32], _| sh.audio.fill(out),
            |e| eprintln!("anyview: audio: {e}"),
            None,
        )
        .ok()?;
    stream.play().ok()?;
    Some(stream)
}

fn start_time(ictx: &ff::format::context::Input) -> f64 {
    // SAFETY: plain field read on a live, opened context.
    let s = unsafe { (*ictx.as_ptr()).start_time };
    if s == ff::ffi::AV_NOPTS_VALUE {
        0.0
    } else {
        s as f64 / f64::from(ff::ffi::AV_TIME_BASE)
    }
}

/// Frame converter to RGBA at display size. Uses swscale's frame API, which
/// spreads the work over all cores (a 4K frame is too slow on one) and reads
/// colour range/space from the frames themselves.
pub struct Scaler(*mut ff::ffi::SwsContext);

impl Scaler {
    pub fn new() -> Option<Self> {
        // SAFETY: plain allocation; fields are set before first use and the
        // context is freed in Drop.
        unsafe {
            let ctx = ff::ffi::sws_alloc_context();
            if ctx.is_null() {
                return None;
            }
            (*ctx).flags = ff::ffi::SwsFlags::SWS_BILINEAR as _;
            (*ctx).threads = 0; // automatic
            Some(Self(ctx))
        }
    }

    pub fn run(
        &mut self,
        src: &ff::frame::Video,
        dst: &mut ff::frame::Video,
        w: u32,
        h: u32,
    ) -> bool {
        if dst.width() != w || dst.height() != h {
            *dst = ff::frame::Video::new(ff::format::Pixel::RGBA, w, h);
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

fn decode(path: &std::path::Path, sh: &Shared, ctx: &egui::Context) -> Result<(), ff::Error> {
    let mut ictx = ff::format::input(path)?;
    let t0 = start_time(&ictx);
    let duration = ictx.duration().max(0) as f64 / f64::from(ff::ffi::AV_TIME_BASE);

    let vs = ictx
        .streams()
        .best(ff::media::Type::Video)
        .ok_or(ff::Error::StreamNotFound)?;
    let (vi, vtb) = (vs.index(), f64::from(vs.time_base()));
    let fps = f64::from(vs.avg_frame_rate());
    let info_fps = if fps.is_finite() && fps > 0.0 {
        fps
    } else {
        30.0
    };
    let mut vctx = ff::codec::Context::from_parameters(vs.parameters())?;
    vctx.set_threading(ff::codec::threading::Config {
        kind: ff::codec::threading::Type::Frame,
        count: 0,
    });
    let mut vdec = vctx.decoder().video()?;

    let mut adec = None;
    let mut atb = 0.0;
    let mut ai = usize::MAX;
    if let Some(s) = ictx.streams().best(ff::media::Type::Audio)
        && let Ok(d) =
            ff::codec::Context::from_parameters(s.parameters()).and_then(|c| c.decoder().audio())
    {
        ai = s.index();
        atb = f64::from(s.time_base());
        adec = Some(d);
    }

    let info = Arc::new(Info {
        duration,
        size: [vdec.width() as usize, vdec.height() as usize],
        fps: info_fps,
        audio_rate: adec.as_ref().map(|d| d.rate()).filter(|&r| r > 0),
    });
    sh.st.lock().unwrap().info = Some(Ok(info));
    ctx.request_repaint();

    // The main decoder's frame threads need a dozen packets before the first
    // frame comes out (~0.3 s on 4K). Show frame one from a slice-threaded
    // decoder first, then rewind.
    if let Some((pts, image)) = first_frame(&mut ictx, vi, vtb, t0, sh) {
        push_frame(
            sh,
            VFrame {
                serial: 0,
                pts,
                image: Arc::new(image),
                still: true,
            },
        );
        ctx.request_repaint();
    }
    let ts = (t0 * f64::from(ff::ffi::AV_TIME_BASE)) as i64;
    let _ = ictx.seek(ts, ..ts);

    sh.cache.lock().unwrap().frame = 1.0 / info_fps;
    let mut scaler = Scaler::new().ok_or(ff::Error::Bug)?;
    let mut resampler: Option<(
        ff::software::resampling::Context,
        (ff::format::Sample, u64, u32),
    )> = None;
    let mut vframe = ff::frame::Video::empty();
    let mut rgba = ff::frame::Video::empty();
    let mut aframe = ff::frame::Audio::empty();
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
    let (mut last_t, mut backward, mut moved_at) = (f64::NAN, false, Instant::now());
    let mut last_catchup: Option<Instant> = None;

    loop {
        // Commands and backpressure.
        {
            let mut st = sh.st.lock().unwrap();
            loop {
                if st.quit {
                    return Ok(());
                }
                if let Some(seek) = st.seek.take() {
                    serial = st.serial;
                    drop(st);
                    drag = None;
                    set_skip_nonref(&mut vdec, false);
                    // A short step forward (dragging right) continues from the
                    // current position; anything else seeks to the keyframe.
                    let ahead = seek.t - last_decoded;
                    let continue_forward = seek.exact && !eof_sent && (0.0..3.0).contains(&ahead);
                    if !continue_forward {
                        let ts = ((seek.t + t0) * f64::from(ff::ffi::AV_TIME_BASE)) as i64;
                        let _ = ictx.seek(ts, ..ts);
                        vdec.flush();
                        if let Some(a) = &mut adec {
                            a.flush();
                        }
                        last_decoded = f64::NEG_INFINITY;
                        eof_sent = false;
                    }
                    skip_until = if seek.exact {
                        seek.t
                    } else {
                        f64::NEG_INFINITY
                    };
                    st = sh.st.lock().unwrap();
                    continue;
                }
                let d = *sh.drag.lock().unwrap();
                if d.active {
                    let t = d.target;
                    if t.to_bits() != last_t.to_bits() {
                        backward = t < last_t;
                        last_t = t;
                        moved_at = Instant::now();
                        floor = t;
                    }
                    let resting = moved_at.elapsed() > Duration::from_millis(250);
                    let frame = 1.0 / info_fps;
                    // A frame the current run should have produced but didn't:
                    // doesn't exist (or was a skipped catch-up frame of a run
                    // aimed elsewhere, which a fresh seek fixes).
                    let passed = |g: f64| {
                        last_decoded.is_finite()
                            && g < last_decoded
                            && (g - run_target).abs() < frame
                    };
                    // Jobs by priority: the frame under the pointer, the
                    // lookahead right of it, then (moving left or resting)
                    // backward prefetch below `floor`, so passes finish.
                    let job = {
                        let c = sh.cache.lock().unwrap();
                        let first_gap = |from: f64, to: f64| {
                            let mut x = from;
                            while x <= to {
                                if !c.has(x) && !passed(x) {
                                    return Some(x);
                                }
                                x += frame;
                            }
                            None
                        };
                        let end = (duration - frame).max(0.0);
                        if !c.has(t) && !passed(t) {
                            Some(t)
                        } else if let Some(g) = first_gap(t + frame, (t + LOOKAHEAD).min(end)) {
                            Some(g)
                        } else if backward || resting {
                            // No further back than the cache can hold next to
                            // the lookahead, or prefetch would evict itself.
                            let frame_bytes =
                                c.frames.values().next().map_or(1, |f| f.pixels.len() * 4);
                            let fits = (CACHE_BYTES / frame_bytes.max(1)) as f64 * frame;
                            let back = PREFETCH_BACK.min(fits * 0.6 - LOOKAHEAD).max(0.0);
                            let low = (t - back).max(0.0);
                            let mut x = floor - frame;
                            while x > low && c.has(x) {
                                x -= frame;
                            }
                            (x > low).then_some(x)
                        } else {
                            None
                        }
                    };
                    let Some(g) = job else {
                        drag = Some(t);
                        // Nothing to do until the pointer moves (or rests long
                        // enough to start prefetching).
                        st = sh
                            .cv
                            .wait_timeout(st, Duration::from_millis(100))
                            .unwrap()
                            .0;
                        continue;
                    };
                    if g < t - frame {
                        floor = g;
                    }
                    drag = Some(g);
                    let ahead = g - last_decoded;
                    let can_continue = !eof_sent
                        && last_decoded.is_finite()
                        && ahead >= -frame
                        && (ahead < CONTINUE_WITHIN || g <= run_target + CONTINUE_WITHIN);
                    // Seek only once the last seek has produced a frame: on slow
                    // 4K, seeking on every pointer move would show nothing.
                    if !can_continue && produced {
                        drop(st);
                        let ts = ((g + t0) * f64::from(ff::ffi::AV_TIME_BASE)) as i64;
                        let _ = ictx.seek(ts, ..ts);
                        vdec.flush();
                        last_decoded = f64::NEG_INFINITY;
                        eof_sent = false;
                        produced = false;
                        run_target = g;
                        st = sh.st.lock().unwrap();
                        st.eof = false;
                    }
                    break;
                }
                // Audio only holds the decoder back once a video frame is
                // queued: while paused nothing drains audio, and the frame a
                // seek is waiting for may sit behind it in the file.
                let audio_secs = sh.audio.queued_secs();
                let use_audio = adec.is_some();
                let audio_low = use_audio && audio_secs < 0.2;
                let frame_bytes = st.frames.front().map_or(0, |f| f.image.pixels.len() * 4);
                let video_full = st.frames.len() >= MAX_FRAMES
                    && !(audio_low && st.frames.len() * frame_bytes < MAX_QUEUE_BYTES);
                let full = video_full
                    || (use_audio && !st.frames.is_empty() && audio_secs > MAX_AUDIO_SECS);
                if !full && !st.eof {
                    break;
                }
                // Audio drains without notifying, so wake periodically while it does.
                let timeout = if st.eof {
                    Duration::from_secs(3600)
                } else {
                    Duration::from_millis(50)
                };
                st = sh.cv.wait_timeout(st, timeout).unwrap().0;
            }
        }

        let packet = ictx.packets().next().map(|(stream, p)| (stream.index(), p));
        match &packet {
            Some((idx, p)) if *idx == vi => {
                if let Some(t) = drag {
                    let pts = p
                        .pts()
                        .or(p.dts())
                        .map_or(f64::INFINITY, |v| v as f64 * vtb - t0);
                    set_skip_nonref(&mut vdec, pts < t - NONREF_BEFORE);
                }
                let _ = vdec.send_packet(p);
            }
            Some((idx, _)) if *idx == ai && drag.is_some() => continue,
            Some((idx, p)) if *idx == ai => {
                if let Some(a) = &mut adec {
                    let _ = a.send_packet(p);
                }
            }
            Some(_) => continue,
            None => {
                eof_sent = true;
                let _ = vdec.send_eof();
                if let Some(a) = &mut adec {
                    let _ = a.send_eof();
                }
            }
        }

        while vdec.receive_frame(&mut vframe).is_ok() {
            let Some(ts) = vframe.timestamp().or(vframe.pts()) else {
                continue;
            };
            let pts = ts as f64 * vtb - t0;
            last_decoded = last_decoded.max(pts);
            let (w, h) = (vframe.width(), vframe.height());
            let (dw, dh) = scaled_size(w, h, sh.target.load(Relaxed));
            if let Some(t) = drag {
                // Active drag: frames go to the cache. Far behind the target,
                // only a few, so catching up isn't slowed by conversion.
                produced = true;
                if pts < t - CATCHUP_WINDOW {
                    if last_catchup.is_some_and(|i| i.elapsed() < CATCHUP_EVERY) {
                        continue;
                    }
                    last_catchup = Some(Instant::now());
                }
                let (cw, ch) = if dw > DRAG_MAX_WIDTH {
                    let h = (f64::from(dh) * f64::from(DRAG_MAX_WIDTH) / f64::from(dw)) as u32;
                    (DRAG_MAX_WIDTH, (h & !1).max(2))
                } else {
                    (dw, dh)
                };
                if scaler.run(&vframe, &mut rgba, cw, ch) {
                    let img = Arc::new(to_color_image(&rgba));
                    sh.cache.lock().unwrap().insert(pts, img, t);
                    ctx.request_repaint();
                }
                continue;
            }
            if pts < skip_until - 0.5 / info_fps {
                continue;
            }
            if scaler.run(&vframe, &mut rgba, dw, dh) {
                push_frame(
                    sh,
                    VFrame {
                        serial,
                        pts,
                        still: false,
                        image: Arc::new(to_color_image(&rgba)),
                    },
                );
            }
        }

        if let Some(dec) = &mut adec {
            while dec.receive_frame(&mut aframe).is_ok() {
                let Some(ts) = aframe.timestamp().or(aframe.pts()) else {
                    continue;
                };
                let pts = ts as f64 * atb - t0;
                if pts < skip_until - 0.05 {
                    continue;
                }
                let layout = known_layout(&mut aframe);
                let key = (aframe.format(), layout.bits(), aframe.rate());
                if resampler.as_ref().is_none_or(|(_, k)| *k != key) {
                    // Same rate in and out: the output device runs at the source rate.
                    resampler = ff::software::resampling::Context::get(
                        key.0,
                        layout,
                        key.2,
                        ff::format::Sample::F32(ff::format::sample::Type::Packed),
                        ff::ChannelLayout::STEREO,
                        key.2,
                    )
                    .ok()
                    .map(|r| (r, key));
                }
                let Some((swr, _)) = &mut resampler else {
                    continue;
                };
                let mut out = ff::frame::Audio::empty();
                if swr.run(&aframe, &mut out).is_err() {
                    continue;
                }
                let bytes = &out.data(0)[..out.samples() * 2 * 4];
                let data = bytes
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|b| f32::from_ne_bytes([b[0], b[1], b[2], b[3]]))
                    .collect();
                push_audio(
                    sh,
                    Chunk {
                        serial,
                        pts,
                        data,
                        off: 0,
                    },
                );
            }
        }

        if packet.is_none() {
            sh.st.lock().unwrap().eof = true;
            ctx.request_repaint();
        }
    }
}

fn first_frame(
    ictx: &mut ff::format::context::Input,
    vi: usize,
    vtb: f64,
    t0: f64,
    sh: &Shared,
) -> Option<(f64, ColorImage)> {
    let par = ictx.stream(vi)?.parameters();
    // dav1d's own threading already delivers frame one quickly.
    if par.id() == ff::codec::Id::AV1 {
        return None;
    }
    let mut c = ff::codec::Context::from_parameters(par).ok()?;
    c.set_threading(ff::codec::threading::Config {
        kind: ff::codec::threading::Type::Slice,
        count: 0,
    });
    let mut dec = c.decoder().video().ok()?;
    let mut frame = ff::frame::Video::empty();
    for (s, p) in ictx.packets() {
        if s.index() != vi {
            continue;
        }
        let _ = dec.send_packet(&p);
        if dec.receive_frame(&mut frame).is_ok() {
            break;
        }
    }
    if frame.width() == 0 {
        let _ = dec.send_eof();
        dec.receive_frame(&mut frame).ok()?;
    }
    let pts = frame.timestamp().or(frame.pts())? as f64 * vtb - t0;
    let (dw, dh) = scaled_size(frame.width(), frame.height(), sh.target.load(Relaxed));
    let mut rgba = ff::frame::Video::empty();
    Scaler::new()?
        .run(&frame, &mut rgba, dw, dh)
        .then(|| (pts, to_color_image(&rgba)))
}

/// Skip decoding non-reference frames (or stop skipping). Frame threads pick
/// the setting up per packet.
fn set_skip_nonref(dec: &mut ff::decoder::Video, skip: bool) {
    let v = if skip {
        ff::ffi::AVDiscard::AVDISCARD_NONREF
    } else {
        ff::ffi::AVDiscard::AVDISCARD_DEFAULT
    };
    // SAFETY: plain field write on an open decoder, read at packet submission.
    unsafe { (*dec.as_mut_ptr()).skip_frame = v };
}

/// Fit `w`×`h` inside the packed on-screen target, never upscaling. Sizes snap
/// to 64 px steps so resizing the window doesn't rebuild the scaler every frame.
fn scaled_size(w: u32, h: u32, target: u64) -> (u32, u32) {
    let (tw, th) = ((target >> 32) as u32, target as u32);
    if tw == 0 || th == 0 {
        return (w, h);
    }
    let tw = tw.div_ceil(64) * 64;
    let th = th.div_ceil(64) * 64;
    let s = (tw as f64 / w as f64).min(th as f64 / h as f64).min(1.0);
    let even = |v: f64| ((v.round() as u32) & !1).max(2);
    (even(w as f64 * s), even(h as f64 * s))
}

/// Give the frame the default layout for its channel count when the file
/// doesn't specify one (common for PCM in camera MOVs): swresample rejects
/// unspecified layouts, both when configured and per frame. Returns the layout.
pub fn known_layout(f: &mut ff::frame::Audio) -> ff::ChannelLayout {
    if f.channel_layout().bits() == 0 {
        f.set_channel_layout(ff::ChannelLayout::default(i32::from(f.channels()).max(1)));
    }
    f.channel_layout()
}

fn push_frame(sh: &Shared, f: VFrame) {
    let mut st = sh.st.lock().unwrap();
    if f.serial == st.serial && st.seek.is_none() {
        st.frames.push_back(f);
    }
}

fn push_audio(sh: &Shared, c: Chunk) {
    if c.serial != sh.audio.serial.load(Relaxed) {
        return;
    }
    sh.audio
        .queued
        .fetch_add((c.data.len() - c.off) as u64, Relaxed);
    sh.audio.chunks.lock().unwrap().push_back(c);
}

pub fn to_color_image(f: &ff::frame::Video) -> ColorImage {
    let (w, h) = (f.width() as usize, f.height() as usize);
    let stride = f.stride(0);
    let data = f.data(0);
    let mut pixels = vec![Color32::BLACK; w * h];
    // SAFETY: Color32 is #[repr(C)] over [u8; 4], so every byte pattern is a
    // valid value; the byte view covers exactly the vector's initialized length.
    // swscale's RGBA output is opaque, so it is already premultiplied.
    let bytes =
        unsafe { std::slice::from_raw_parts_mut(pixels.as_mut_ptr().cast::<u8>(), w * h * 4) };
    for (dst, src) in bytes.chunks_exact_mut(w * 4).zip(data.chunks(stride)) {
        dst.copy_from_slice(&src[..w * 4]);
    }
    ColorImage::new([w, h], pixels)
}

#[cfg(test)]
mod tests {
    /// Decodes a first frame headlessly: ANYVIEW_TEST_VIDEO=a.webm cargo test first_frame -- --ignored
    #[test]
    #[ignore]
    fn first_frame() {
        let path = std::path::PathBuf::from(std::env::var("ANYVIEW_TEST_VIDEO").unwrap());
        ffmpeg_next::init().unwrap();
        let ctx = eframe::egui::Context::default();
        let mut p = super::Player::open(path, &ctx);
        let t = std::time::Instant::now();
        while p.texture().is_none() && p.error.is_none() && t.elapsed().as_secs() < 5 {
            p.update(&ctx);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(p.error.is_none(), "{:?}", p.error);
        let info = p.info.as_ref().expect("no stream info");
        eprintln!(
            "{:?} {:.2}s, first frame after {:?}",
            info.size,
            info.duration,
            t.elapsed()
        );
        assert!(p.texture().is_some(), "no frame decoded");
    }

    fn open_test_player() -> (eframe::egui::Context, super::Player) {
        let path = std::path::PathBuf::from(std::env::var("ANYVIEW_TEST_VIDEO").unwrap());
        ffmpeg_next::init().unwrap();
        let ctx = eframe::egui::Context::default();
        let p = super::Player::open(path, &ctx);
        // A large window on this HiDPI screen, in physical pixels.
        p.set_target(2800, 1600);
        (ctx, p)
    }

    fn pump(ctx: &eframe::egui::Context, p: &mut super::Player, secs: f64) {
        let t = std::time::Instant::now();
        while t.elapsed().as_secs_f64() < secs {
            p.update(ctx);
            std::thread::sleep(std::time::Duration::from_millis(4));
        }
    }

    /// Playback must advance on its own (regression: the clock never started
    /// until the first pause/resume when there was no audio to drive it).
    #[test]
    #[ignore]
    fn plays_without_pause_toggle() {
        let (ctx, mut p) = open_test_player();
        let t = std::time::Instant::now();
        while p.texture().is_none() && t.elapsed().as_secs() < 5 {
            pump(&ctx, &mut p, 0.01);
        }
        let start = p.position;
        pump(&ctx, &mut p, 1.5);
        let audio = p.sh.audio.pos();
        eprintln!(
            "first frame after {:?}; advanced {:.2}s in 1.5s; audio clock {audio:.2}",
            t.elapsed() - std::time::Duration::from_secs_f64(1.5),
            p.position - start
        );
        assert!(
            p.position - start > 1.3,
            "advanced only {:.2}",
            p.position - start
        );
    }

    /// A fast drag shows intermediate frames and lands on the last target.
    #[test]
    #[ignore]
    fn scrub_shows_frames_and_settles() {
        let (ctx, mut p) = open_test_player();
        pump(&ctx, &mut p, 0.5);
        p.set_paused(true);
        let mut seen = std::collections::HashSet::new();
        for i in 0..60 {
            p.scrub(0.5 + i as f64 * 0.05);
            pump(&ctx, &mut p, 0.02);
            seen.insert((p.frame_pts * 1000.0) as i64);
        }
        pump(&ctx, &mut p, 1.0);
        eprintln!(
            "distinct frames while dragging: {}, on screen {:.3}",
            seen.len(),
            p.frame_pts
        );
        assert!(seen.len() > 5, "drag showed only {} frames", seen.len());
        // Give a slow (long-GOP 4K) decode time to land on the last target.
        pump(&ctx, &mut p, 1.5);
        eprintln!("settled on {:.3}", p.frame_pts);
        assert!(
            (p.frame_pts - 3.45).abs() < 0.05,
            "settled at {:.3}",
            p.frame_pts
        );
        // Dragging back must seek back, not continue forward.
        for i in 0..10 {
            p.scrub(3.0 - i as f64 * 0.2);
            pump(&ctx, &mut p, 0.02);
        }
        pump(&ctx, &mut p, 3.0);
        eprintln!("after dragging back: {:.3}", p.frame_pts);

        assert!(
            (p.frame_pts - 1.2).abs() < 0.05,
            "settled at {:.3}",
            p.frame_pts
        );
        // Latency of a backward jump into the middle of a GOP.
        let before = p.frame_pts;
        let t0 = std::time::Instant::now();
        p.scrub(before + 1.7);
        let mut first = None;
        while t0.elapsed().as_secs() < 5 && (p.frame_pts - (before + 1.7)).abs() > 0.02 {
            pump(&ctx, &mut p, 0.002);
            if first.is_none() && p.frame_pts != before {
                first = Some(t0.elapsed());
            }
        }
        let fwd = t0.elapsed();
        let t1 = std::time::Instant::now();
        p.scrub(before + 0.9);
        let mut first_back = None;
        while t1.elapsed().as_secs() < 5 && (p.frame_pts - (before + 0.9)).abs() > 0.02 {
            pump(&ctx, &mut p, 0.002);
            if first_back.is_none() && (p.frame_pts - (before + 1.7)).abs() > 0.02 {
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
    /// ANYVIEW_TEST_VIDEO=clip.mkv cargo test --release bench_drag -- --ignored --nocapture
    #[test]
    #[ignore]
    fn bench_drag() {
        let (ctx, mut p) = open_test_player();
        pump(&ctx, &mut p, 0.6);
        p.set_paused(true);
        pump(&ctx, &mut p, 0.2);
        let dur = p.duration();
        let base = (dur * 0.3).min(4.0);
        // A click far away first (timed on its own), then drags from there.
        let t = std::time::Instant::now();
        p.scrub(base);
        let mut first = None;
        let start_pts = p.frame_pts;
        while p.frame_pts.to_bits() != base.to_bits()
            && !p.sh.cache.lock().unwrap().covers(base, Some(p.frame_pts))
            && t.elapsed().as_secs() < 10
        {
            pump(&ctx, &mut p, 0.001);
            if first.is_none() && p.frame_pts != start_pts {
                first = Some(t.elapsed());
            }
        }
        eprintln!(
            "BENCH jump to {base:.1}s: first frame {:.0} ms, exact {:.0} ms",
            first.map_or(f64::NAN, |d| d.as_secs_f64() * 1000.0),
            t.elapsed().as_secs_f64() * 1000.0
        );
        // (name, seconds, target(t))
        type Path = Box<dyn Fn(f64) -> f64>;
        let phases: Vec<(&str, f64, Path)> = vec![
            ("fwd 1x", 2.0, Box::new(move |t| base + t)),
            ("fwd 3x", 1.0, Box::new(move |t| base + 2.0 + 3.0 * t)),
            ("back 1x", 2.0, Box::new(move |t| base + 5.0 - t)),
            ("back 3x", 1.0, Box::new(move |t| base + 3.0 - 3.0 * t)),
            (
                "jiggle",
                3.0,
                Box::new(move |t| base + 1.0 + 0.4 * (t * std::f64::consts::TAU * 0.7).sin()),
            ),
        ];
        let mut total = Vec::new();
        for (name, secs, f) in &phases {
            let start = std::time::Instant::now();
            let mut errs = Vec::new();
            let mut frames = std::collections::HashSet::new();
            while start.elapsed().as_secs_f64() < *secs {
                let target = f(start.elapsed().as_secs_f64()).clamp(0.0, dur);
                p.scrub(target);
                let tick = std::time::Instant::now();
                while tick.elapsed().as_millis() < 16 {
                    p.update(&ctx);
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                errs.push((p.frame_pts - target).abs());
                frames.insert((p.frame_pts * 1000.0) as i64);
            }
            errs.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let mean = errs.iter().sum::<f64>() / errs.len() as f64;
            let p95 = errs[errs.len() * 95 / 100];
            let exact = errs.iter().filter(|e| **e < 0.02).count() * 100 / errs.len();
            eprintln!(
                "BENCH {name:8} mean lag {:6.0} ms  p95 {:6.0} ms  exact {exact:3}%  frames {}",
                mean * 1000.0,
                p95 * 1000.0,
                frames.len()
            );
            total.extend(errs);
        }
        let target = base + 1.0;
        p.scrub(target);
        let t = std::time::Instant::now();
        while (p.frame_pts - target).abs() > 0.02 && t.elapsed().as_secs() < 10 {
            pump(&ctx, &mut p, 0.002);
        }
        let mean = total.iter().sum::<f64>() / total.len() as f64;
        eprintln!(
            "BENCH TOTAL mean lag {:.0} ms, settle {:.0} ms",
            mean * 1000.0,
            t.elapsed().as_secs_f64() * 1000.0
        );
    }
}
