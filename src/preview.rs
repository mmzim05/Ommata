//! Timeline hover thumbnails: a second, small decoder that shows the keyframe
//! at or before the hovered time, like YouTube's storyboard previews. Only
//! keyframes are decoded (one packet each), so it stays fast on long-GOP 4K.

use crate::video::Scaler;
use eframe::egui::{self, ColorImage, TextureHandle, TextureOptions};
use ffmpeg_next as ff;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Condvar, Mutex};

const WIDTH: u32 = 256;
const CACHE: usize = 256;

type Request = Arc<(Mutex<Option<f64>>, Condvar)>;

pub struct Preview {
    request: Request,
    rx: Receiver<(f64, f64, ColorImage)>,
    /// Keyframe time in ms to texture.
    cache: HashMap<i64, TextureHandle>,
    /// Keyframe shown for each requested time (rounded to 10 ms).
    lookup: HashMap<i64, i64>,
    shown: Option<i64>,
}

impl Preview {
    pub fn spawn(path: PathBuf, ctx: egui::Context) -> Self {
        let request: Request = Default::default();
        let (tx, rx) = channel();
        let req = request.clone();
        std::thread::Builder::new()
            .name("preview".into())
            .spawn(move || {
                let _ = run(&path, &req, &tx, &ctx);
            })
            .ok();
        Self {
            request,
            rx,
            cache: HashMap::new(),
            lookup: HashMap::new(),
            shown: None,
        }
    }

    /// Thumbnail for time `t`: returns the best one available now and asks the
    /// decoder for the right one if it isn't cached yet.
    pub fn at(&mut self, t: f64, ctx: &egui::Context) -> Option<&TextureHandle> {
        while let Ok((req, key, image)) = self.rx.try_recv() {
            let k = ms(key);
            if self.cache.len() >= CACHE {
                self.cache.clear();
                self.lookup.clear();
            }
            self.cache
                .entry(k)
                .or_insert_with(|| ctx.load_texture("preview", image, TextureOptions::LINEAR));
            self.lookup.insert(ms(req) / 10, k);
            self.shown = Some(k);
        }
        match self.lookup.get(&(ms(t) / 10)) {
            Some(&k) => self.shown = Some(k),
            None => {
                let (lock, cv) = &*self.request;
                *lock.lock().unwrap() = Some(t);
                cv.notify_one();
            }
        }
        self.shown.and_then(|k| self.cache.get(&k))
    }
}

impl Drop for Preview {
    fn drop(&mut self) {
        // A negative time tells the thread to exit.
        let (lock, cv) = &*self.request;
        *lock.lock().unwrap() = Some(-1.0);
        cv.notify_one();
    }
}

fn ms(t: f64) -> i64 {
    (t * 1000.0).round() as i64
}

fn run(
    path: &std::path::Path,
    request: &Request,
    tx: &Sender<(f64, f64, ColorImage)>,
    ctx: &egui::Context,
) -> Result<(), ff::Error> {
    let mut ictx = ff::format::input(path)?;
    // SAFETY: plain field read on a live, opened context.
    let t0 = match unsafe { (*ictx.as_ptr()).start_time } {
        ff::ffi::AV_NOPTS_VALUE => 0.0,
        s => s as f64 / f64::from(ff::ffi::AV_TIME_BASE),
    };
    let vs = ictx
        .streams()
        .best(ff::media::Type::Video)
        .ok_or(ff::Error::StreamNotFound)?;
    let (vi, vtb) = (vs.index(), f64::from(vs.time_base()));
    // Slice threads, not frame threads: frame threading would hold the one
    // frame back, while camera files split each frame into slices that decode
    // in parallel (about 6x faster on a 4K keyframe).
    let mut ctx_ = ff::codec::Context::from_parameters(vs.parameters())?;
    ctx_.set_threading(ff::codec::threading::Config {
        kind: ff::codec::threading::Type::Slice,
        count: 0,
    });
    let mut dec = ctx_.decoder().video()?;
    let mut scaler = Scaler::new().ok_or(ff::Error::Bug)?;
    let (mut frame, mut rgba) = (ff::frame::Video::empty(), ff::frame::Video::empty());

    loop {
        let t = {
            let (lock, cv) = &**request;
            let mut r = lock.lock().unwrap();
            loop {
                if let Some(t) = r.take() {
                    break t;
                }
                r = cv.wait(r).unwrap();
            }
        };
        if t < 0.0 {
            return Ok(());
        }
        let ts = ((t + t0) * f64::from(ff::ffi::AV_TIME_BASE)) as i64;
        if ictx.seek(ts, ..ts).is_err() {
            continue;
        }
        let Some((key, packet)) = ictx.packets().find_map(|(s, p)| {
            (s.index() == vi && p.is_key())
                .then(|| p.pts().or(p.dts()).map(|v| (v as f64 * vtb - t0, p)))
                .flatten()
        }) else {
            continue;
        };
        dec.flush();
        let _ = dec.send_packet(&packet);
        let _ = dec.send_eof();
        if dec.receive_frame(&mut frame).is_err() {
            dec.flush();
            continue;
        }
        dec.flush();
        let (w, h) = (frame.width().max(1), frame.height().max(1));
        let th = ((WIDTH as f64 * h as f64 / w as f64).round() as u32).max(2) & !1;
        if scaler.run(&frame, &mut rgba, WIDTH, th) {
            let _ = tx.send((t, key.max(0.0), crate::video::to_color_image(&rgba)));
            ctx.request_repaint();
        }
    }
}

#[cfg(test)]
mod tests {
    /// ANYVIEW_TEST_VIDEO=clip.mp4 cargo test hover_ -- --ignored
    #[test]
    #[ignore]
    fn hover_thumbnails() {
        let path = std::path::PathBuf::from(std::env::var("ANYVIEW_TEST_VIDEO").unwrap());
        ffmpeg_next::init().unwrap();
        let ctx = eframe::egui::Context::default();
        let mut p = super::Preview::spawn(path, ctx.clone());
        for t in [1.3, 5.2, 9.0] {
            let start = std::time::Instant::now();
            while !p.lookup.contains_key(&(super::ms(t) / 10)) && start.elapsed().as_secs() < 5 {
                p.at(t, &ctx);
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            let key = p.lookup.get(&(super::ms(t) / 10)).copied();
            let size = p.at(t, &ctx).map(|t| t.size());
            eprintln!(
                "hover {t}: keyframe {key:?} ms, {size:?}, {:?}",
                start.elapsed()
            );
            assert!(key.is_some_and(|k| k <= super::ms(t)));
        }
    }
}
