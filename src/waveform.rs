//! Audio loudness overview for the seek bar. A background thread decodes only
//! the audio stream into fixed-length RMS buckets; the bar fills in as it goes.

use crate::lock;
use crate::video::{AudioConverter, start_time};
use ffmpeg_next as ff;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, Mutex};

const BUCKET_SECS: f64 = 0.02;

/// Loudness per BUCKET_SECS of a file's audio, filled in by a background scan.
#[derive(Default)]
pub struct Waveform {
    levels: Mutex<Vec<f32>>,
    pub done: AtomicBool,
    /// Set when the file has no decodable audio.
    pub none: AtomicBool,
    cancel: AtomicBool,
}

impl Waveform {
    /// Start scanning the audio of `path`; `ctx` repaints as levels arrive.
    pub fn spawn(path: PathBuf, ctx: eframe::egui::Context) -> Arc<Self> {
        let waveform = Arc::new(Self::default());
        let scanned = waveform.clone();
        let spawned = std::thread::Builder::new()
            .name("waveform".into())
            .spawn(move || {
                if scan(&path, &scanned, &ctx).is_err() && lock(&scanned.levels).is_empty() {
                    scanned.none.store(true, Relaxed);
                }
                scanned.done.store(true, Relaxed);
                ctx.request_repaint();
            });
        if let Err(error) = spawned {
            eprintln!("omalens: waveform thread: {error}");
            waveform.none.store(true, Relaxed);
            waveform.done.store(true, Relaxed);
        }
        waveform
    }

    /// Number of buckets scanned so far.
    pub fn len(&self) -> usize {
        lock(&self.levels).len()
    }

    /// `columns` values in 0..=1 spanning `duration`: the loudest bucket per
    /// column, smoothed with a moving average, normalized to the loudest column.
    pub fn columns(&self, columns: usize, duration: f64, smooth: f32) -> Vec<f32> {
        let levels = lock(&self.levels);
        let total = ((duration / BUCKET_SECS).ceil() as usize).max(1);
        let mut out: Vec<f32> = (0..columns)
            .map(|column| {
                let first = column * total / columns;
                let last = ((column + 1) * total / columns).max(first + 1);
                levels
                    .get(first..last.min(levels.len()))
                    .map_or(0.0, |span| span.iter().copied().fold(0.0, f32::max))
            })
            .collect();
        let radius = (smooth * smooth * columns as f32 * 0.08).round() as usize;
        if radius > 0 {
            let src = out.clone();
            for (i, value) in out.iter_mut().enumerate() {
                let (low, high) = (i.saturating_sub(radius), (i + radius + 1).min(columns));
                *value = src[low..high].iter().sum::<f32>() / (high - low) as f32;
            }
        }
        let max = out.iter().copied().fold(0.0, f32::max);
        if max > 0.0 {
            out.iter_mut().for_each(|value| *value /= max);
        }
        out
    }
}

impl Drop for Waveform {
    fn drop(&mut self) {
        self.cancel.store(true, Relaxed);
    }
}

/// Fold one bucket's sum of squares into its RMS level.
fn flush(levels: &mut Vec<f32>, bucket: usize, sum: f64, count: usize) {
    if levels.len() <= bucket {
        levels.resize(bucket + 1, 0.0);
    }
    let rms = (sum / count.max(1) as f64).sqrt() as f32;
    levels[bucket] = levels[bucket].max(rms);
}

fn scan(
    path: &std::path::Path,
    waveform: &Arc<Waveform>,
    ctx: &eframe::egui::Context,
) -> Result<(), ff::Error> {
    let mut input = ff::format::input(path)?;
    let stream = input
        .streams()
        .best(ff::media::Type::Audio)
        .ok_or(ff::Error::StreamNotFound)?;
    let (audio_index, time_base) = (stream.index(), f64::from(stream.time_base()));
    let start_offset = start_time(&input);
    let mut decoder = ff::codec::Context::from_parameters(stream.parameters())?
        .decoder()
        .audio()?;
    // Skip non-audio streams in the demuxer so we don't read video packets' data.
    for mut other in input.streams_mut() {
        if other.index() != audio_index {
            // SAFETY: setting the discard flag on a stream of an open context.
            unsafe { (*other.as_mut_ptr()).discard = ff::ffi::AVDiscard::AVDISCARD_ALL };
        }
    }
    let mut frame = ff::frame::Audio::empty();
    let mut converter = AudioConverter::new(ff::ChannelLayout::MONO, 1);
    let (mut bucket, mut sum, mut count) = (0usize, 0.0f64, 0usize);
    let mut last_repaint = std::time::Instant::now();

    let mut eof = false;
    while !eof {
        if waveform.cancel.load(Relaxed) {
            return Ok(());
        }
        // A corrupt packet only loses its own samples, so decode errors are
        // skipped rather than ending the scan.
        match input.packets().next() {
            Some((stream, packet)) if stream.index() == audio_index => {
                let _ = decoder.send_packet(&packet);
            }
            Some(_) => continue,
            None => {
                let _ = decoder.send_eof();
                eof = true;
            }
        }
        while decoder.receive_frame(&mut frame).is_ok() {
            let Some(pts) = frame.timestamp().or(frame.pts()) else {
                continue;
            };
            let rate = f64::from(frame.rate().max(1));
            let Some(samples) = converter.run(&mut frame) else {
                continue;
            };
            let start = pts as f64 * time_base - start_offset;
            let mut levels = lock(&waveform.levels);
            for (i, sample) in samples.iter().enumerate() {
                let index = ((start + i as f64 / rate) / BUCKET_SECS).max(0.0) as usize;
                if index != bucket && count > 0 {
                    flush(&mut levels, bucket, sum, count);
                    (sum, count) = (0.0, 0);
                }
                bucket = index;
                sum += f64::from(*sample) * f64::from(*sample);
                count += 1;
            }
        }
        if last_repaint.elapsed().as_millis() > 250 {
            last_repaint = std::time::Instant::now();
            ctx.request_repaint();
        }
    }
    if count > 0 {
        flush(&mut lock(&waveform.levels), bucket, sum, count);
    }
    Ok(())
}
