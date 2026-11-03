//! Audio loudness overview for the seek bar. A background thread decodes only
//! the audio stream into fixed-length RMS buckets; the bar fills in as it goes.

use ffmpeg_next as ff;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, Mutex};

const BUCKET_SECS: f64 = 0.02;

#[derive(Default)]
pub struct Waveform {
    levels: Mutex<Vec<f32>>,
    pub done: AtomicBool,
    /// Set when the file has no decodable audio.
    pub none: AtomicBool,
    cancel: AtomicBool,
}

impl Waveform {
    pub fn spawn(path: PathBuf, ctx: eframe::egui::Context) -> Arc<Self> {
        let w = Arc::new(Self::default());
        let w2 = w.clone();
        std::thread::Builder::new()
            .name("waveform".into())
            .spawn(move || {
                if scan(&path, &w2, &ctx).is_err() && w2.levels.lock().unwrap().is_empty() {
                    w2.none.store(true, Relaxed);
                }
                w2.done.store(true, Relaxed);
                ctx.request_repaint();
            })
            .ok();
        w
    }

    pub fn len(&self) -> usize {
        self.levels.lock().unwrap().len()
    }

    /// `columns` values in 0..=1 spanning `duration`: the loudest bucket per
    /// column, smoothed with a moving average, normalized to the loudest column.
    pub fn columns(&self, columns: usize, duration: f64, smooth: f32) -> Vec<f32> {
        let levels = self.levels.lock().unwrap();
        let total = ((duration / BUCKET_SECS).ceil() as usize).max(1);
        let mut out: Vec<f32> = (0..columns)
            .map(|c| {
                let a = c * total / columns;
                let b = ((c + 1) * total / columns).max(a + 1);
                levels
                    .get(a..b.min(levels.len()))
                    .map_or(0.0, |s| s.iter().copied().fold(0.0, f32::max))
            })
            .collect();
        let radius = (smooth * smooth * columns as f32 * 0.08).round() as usize;
        if radius > 0 {
            let src = out.clone();
            for (i, v) in out.iter_mut().enumerate() {
                let (lo, hi) = (i.saturating_sub(radius), (i + radius + 1).min(columns));
                *v = src[lo..hi].iter().sum::<f32>() / (hi - lo) as f32;
            }
        }
        let max = out.iter().copied().fold(0.0, f32::max);
        if max > 0.0 {
            out.iter_mut().for_each(|v| *v /= max);
        }
        out
    }
}

impl Drop for Waveform {
    fn drop(&mut self) {
        self.cancel.store(true, Relaxed);
    }
}

fn scan(
    path: &std::path::Path,
    w: &Arc<Waveform>,
    ctx: &eframe::egui::Context,
) -> Result<(), ff::Error> {
    let mut ictx = ff::format::input(path)?;
    let stream = ictx
        .streams()
        .best(ff::media::Type::Audio)
        .ok_or(ff::Error::StreamNotFound)?;
    let (ai, tb) = (stream.index(), f64::from(stream.time_base()));
    // SAFETY: plain field read on a live, opened context.
    let t0 = match unsafe { (*ictx.as_ptr()).start_time } {
        ff::ffi::AV_NOPTS_VALUE => 0.0,
        s => s as f64 / f64::from(ff::ffi::AV_TIME_BASE),
    };
    let mut dec = ff::codec::Context::from_parameters(stream.parameters())?
        .decoder()
        .audio()?;
    // Skip non-audio streams in the demuxer so we don't read video packets' data.
    for mut s in ictx.streams_mut() {
        if s.index() != ai {
            // SAFETY: setting the discard flag on a stream of an open context.
            unsafe { (*s.as_mut_ptr()).discard = ff::ffi::AVDiscard::AVDISCARD_ALL };
        }
    }
    let mut frame = ff::frame::Audio::empty();
    let mut resampler: Option<(
        ff::software::resampling::Context,
        (ff::format::Sample, u64, u32),
    )> = None;
    let (mut bucket, mut sum, mut n) = (0usize, 0.0f64, 0usize);
    let mut last_repaint = std::time::Instant::now();
    let flush = |levels: &mut Vec<f32>, idx: usize, sum: f64, n: usize| {
        if levels.len() <= idx {
            levels.resize(idx + 1, 0.0);
        }
        levels[idx] = levels[idx].max(((sum / n.max(1) as f64).sqrt()) as f32);
    };

    let mut eof = false;
    while !eof {
        if w.cancel.load(Relaxed) {
            return Ok(());
        }
        match ictx.packets().next() {
            Some((s, p)) if s.index() == ai => {
                let _ = dec.send_packet(&p);
            }
            Some(_) => continue,
            None => {
                let _ = dec.send_eof();
                eof = true;
            }
        }
        while dec.receive_frame(&mut frame).is_ok() {
            let Some(ts) = frame.timestamp().or(frame.pts()) else {
                continue;
            };
            let layout = crate::video::known_layout(&mut frame);
            let key = (frame.format(), layout.bits(), frame.rate());
            if resampler.as_ref().is_none_or(|(_, k)| *k != key) {
                resampler = ff::software::resampling::Context::get(
                    key.0,
                    layout,
                    key.2,
                    ff::format::Sample::F32(ff::format::sample::Type::Packed),
                    ff::ChannelLayout::MONO,
                    key.2,
                )
                .ok()
                .map(|r| (r, key));
            }
            let Some((swr, _)) = &mut resampler else {
                continue;
            };
            let mut out = ff::frame::Audio::empty();
            if swr.run(&frame, &mut out).is_err() {
                continue;
            }
            let rate = key.2.max(1) as f64;
            let start = ts as f64 * tb - t0;
            let bytes = &out.data(0)[..out.samples() * 4];
            let mut levels = w.levels.lock().unwrap();
            for (i, b) in bytes.as_chunks::<4>().0.iter().enumerate() {
                let v = f32::from_ne_bytes([b[0], b[1], b[2], b[3]]) as f64;
                let idx = ((start + i as f64 / rate) / BUCKET_SECS).max(0.0) as usize;
                if idx != bucket && n > 0 {
                    flush(&mut levels, bucket, sum, n);
                    (sum, n) = (0.0, 0);
                }
                bucket = idx;
                sum += v * v;
                n += 1;
            }
        }
        if last_repaint.elapsed().as_millis() > 250 {
            last_repaint = std::time::Instant::now();
            ctx.request_repaint();
        }
    }
    if n > 0 {
        flush(&mut w.levels.lock().unwrap(), bucket, sum, n);
    }
    Ok(())
}
