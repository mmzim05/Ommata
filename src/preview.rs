//! Timeline hover thumbnails: a second, small decoder that shows the keyframe
//! at or before the hovered time, like YouTube's storyboard previews. Only
//! keyframes are decoded (one packet each), so it stays fast on long-GOP 4K.

use crate::lock;
use crate::video::{Scaler, start_time, video_stream};
use eframe::egui::{self, ColorImage, TextureHandle, TextureOptions};
use ffmpeg_next as ff;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Condvar, Mutex};

const WIDTH: u32 = 256;
const CACHE: usize = 256;

type Request = Arc<(Mutex<Option<f64>>, Condvar)>;

/// Requested time, keyframe time and the keyframe's thumbnail.
type Thumbnail = (f64, f64, ColorImage);

/// Hover thumbnails for one video, decoded on a background thread.
pub struct Preview {
    request: Request,
    receiver: Receiver<Thumbnail>,
    /// Keyframe time in ms to texture.
    cache: HashMap<i64, TextureHandle>,
    /// Keyframe shown for each requested time (rounded to 10 ms).
    lookup: HashMap<i64, i64>,
    shown: Option<i64>,
}

impl Preview {
    /// Start the thumbnail decoder for `path`; `ctx` repaints on each result.
    pub fn spawn(path: PathBuf, ctx: egui::Context) -> Self {
        let request: Request = Default::default();
        let (sender, receiver) = channel();
        let thread_request = request.clone();
        let spawned = std::thread::Builder::new()
            .name("preview".into())
            .spawn(move || {
                if let Err(error) = run(&path, &thread_request, &sender, &ctx) {
                    eprintln!("ommata: preview {}: {error}", path.display());
                }
            });
        if let Err(error) = spawned {
            eprintln!("ommata: preview thread: {error}");
        }
        Self {
            request,
            receiver,
            cache: HashMap::new(),
            lookup: HashMap::new(),
            shown: None,
        }
    }

    /// Thumbnail for `time`: returns the best one available now and asks the
    /// decoder for the right one if it isn't cached yet.
    pub fn at(&mut self, time: f64, ctx: &egui::Context) -> Option<&TextureHandle> {
        while let Ok((requested, keyframe, image)) = self.receiver.try_recv() {
            let key = millis(keyframe);
            if self.cache.len() >= CACHE {
                self.cache.clear();
                self.lookup.clear();
            }
            self.cache
                .entry(key)
                .or_insert_with(|| ctx.load_texture("preview", image, TextureOptions::LINEAR));
            self.lookup.insert(millis(requested) / 10, key);
            self.shown = Some(key);
        }
        match self.lookup.get(&(millis(time) / 10)) {
            Some(&key) => self.shown = Some(key),
            None => self.send(time),
        }
        self.shown.and_then(|key| self.cache.get(&key))
    }

    fn send(&self, time: f64) {
        let (pending, wake) = &*self.request;
        *lock(pending) = Some(time);
        wake.notify_one();
    }
}

impl Drop for Preview {
    fn drop(&mut self) {
        // A negative time tells the thread to exit.
        self.send(-1.0);
    }
}

fn millis(time: f64) -> i64 {
    (time * 1000.0).round() as i64
}

fn run(
    path: &std::path::Path,
    request: &Request,
    sender: &Sender<Thumbnail>,
    ctx: &egui::Context,
) -> Result<(), ff::Error> {
    let mut input = ff::format::input(path)?;
    let start_offset = start_time(&input);
    let stream = video_stream(&input).ok_or(ff::Error::StreamNotFound)?;
    let (video_index, time_base) = (stream.index(), f64::from(stream.time_base()));
    let turns = crate::video::display_turns(&stream);
    // Slice threads, not frame threads: frame threading would hold the one
    // frame back, while camera files split each frame into slices that decode
    // in parallel (about 6x faster on a 4K keyframe).
    let mut codec = ff::codec::Context::from_parameters(stream.parameters())?;
    codec.set_threading(ff::codec::threading::Config {
        kind: ff::codec::threading::Type::Slice,
        count: 0,
    });
    let mut decoder = codec.decoder().video()?;
    let mut scaler = Scaler::new().ok_or(ff::Error::Bug)?;
    let (mut frame, mut rgba) = (ff::frame::Video::empty(), ff::frame::Video::empty());

    loop {
        let time = {
            let (pending, wake) = &**request;
            let mut guard = lock(pending);
            loop {
                if let Some(time) = guard.take() {
                    break time;
                }
                guard = wake
                    .wait(guard)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        };
        if time < 0.0 {
            return Ok(());
        }
        let target = ((time + start_offset) * f64::from(ff::ffi::AV_TIME_BASE)) as i64;
        if input.seek(target, ..target).is_err() {
            continue;
        }
        let Some((keyframe, packet)) = input.packets().find_map(|(stream, packet)| {
            (stream.index() == video_index && packet.is_key())
                .then(|| {
                    packet
                        .pts()
                        .or(packet.dts())
                        .map(|pts| (pts as f64 * time_base - start_offset, packet))
                })
                .flatten()
        }) else {
            continue;
        };
        decoder.flush();
        // A packet the decoder rejects yields no frame, handled just below.
        let _ = decoder.send_packet(&packet);
        let _ = decoder.send_eof();
        let decoded = decoder.receive_frame(&mut frame).is_ok();
        decoder.flush();
        if !decoded {
            continue;
        }
        let (width, height) = (frame.width().max(1), frame.height().max(1));
        let scaled = (f64::from(WIDTH) * f64::from(height) / f64::from(width)).round() as u32;
        let thumb_height = scaled.max(2) & !1;
        if scaler.run(&frame, &mut rgba, WIDTH, thumb_height) {
            let image = crate::video::turn_image(crate::video::to_color_image(&rgba), turns);
            if sender.send((time, keyframe.max(0.0), image)).is_err() {
                // The Preview is gone.
                return Ok(());
            }
            ctx.request_repaint();
        }
    }
}

#[cfg(test)]
mod tests {
    /// OMMATA_TEST_VIDEO=clip.mp4 cargo test hover_ -- --ignored
    #[test]
    #[ignore]
    fn hover_thumbnails() {
        let path = std::path::PathBuf::from(std::env::var("OMMATA_TEST_VIDEO").unwrap());
        ffmpeg_next::init().unwrap();
        let ctx = eframe::egui::Context::default();
        let mut preview = super::Preview::spawn(path, ctx.clone());
        for t in [1.3, 5.2, 9.0] {
            let start = std::time::Instant::now();
            while !preview.lookup.contains_key(&(super::millis(t) / 10))
                && start.elapsed().as_secs() < 5
            {
                preview.at(t, &ctx);
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            let key = preview.lookup.get(&(super::millis(t) / 10)).copied();
            let size = preview.at(t, &ctx).map(|texture| texture.size());
            eprintln!(
                "hover {t}: keyframe {key:?} ms, {size:?}, {:?}",
                start.elapsed()
            );
            assert!(key.is_some_and(|key| key <= super::millis(t)));
        }
    }
}
