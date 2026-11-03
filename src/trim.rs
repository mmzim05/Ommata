//! Lossless video trim: copy packets from the keyframe at or before the in point
//! up to the out point. No re-encode, so it is fast and keeps full quality; the
//! start snaps to a keyframe, and the UI shows that snapped point up front.

use ffmpeg_next as ff;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};

fn tb(ictx: &ff::format::context::Input) -> f64 {
    // SAFETY: plain field read on a live, opened context.
    let s = unsafe { (*ictx.as_ptr()).start_time };
    if s == ff::ffi::AV_NOPTS_VALUE {
        0.0
    } else {
        s as f64 / f64::from(ff::ffi::AV_TIME_BASE)
    }
}

/// Find the last keyframe that is *displayed* at or before `t`. Returns the
/// video stream, the keyframe's time, and the seek target that lands on it.
///
/// Demuxers seek by decode time, and with B-frames a keyframe is displayed a
/// few frames after it is decoded, so a plain seek to `t` can land on a
/// keyframe shown just after `t`. Step back until the keyframe precedes `t`.
fn seek_keyframe(
    ictx: &mut ff::format::context::Input,
    t: f64,
) -> Result<(usize, f64, i64), ff::Error> {
    let t0 = tb(ictx);
    let vs = ictx
        .streams()
        .best(ff::media::Type::Video)
        .ok_or(ff::Error::StreamNotFound)?;
    let (vi, vtb) = (vs.index(), f64::from(vs.time_base()));
    let mut back = 0.0;
    loop {
        let ts = ((t - back + t0).max(t0) * f64::from(ff::ffi::AV_TIME_BASE)) as i64;
        ictx.seek(ts, ..ts)?;
        let key = ictx.packets().find_map(|(stream, p)| {
            (stream.index() == vi && p.is_key())
                .then(|| p.pts().or(p.dts()))
                .flatten()
        });
        let k = key.ok_or(ff::Error::Eof)? as f64 * vtb - t0;
        // At the start of the file there is nothing earlier to find.
        if k <= t + 1e-4 || t - back <= 0.0 {
            return Ok((vi, k, ts));
        }
        back = if back == 0.0 { 0.1 } else { back * 2.0 };
    }
}

pub fn keyframe_at_or_before(path: &Path, t: f64) -> Option<f64> {
    let mut ictx = ff::format::input(path).ok()?;
    seek_keyframe(&mut ictx, t).ok().map(|(_, k, _)| k.max(0.0))
}

#[derive(Default)]
pub struct Progress {
    /// Fraction done, as f32 bits.
    pub done: AtomicU32,
    pub cancel: AtomicBool,
}

impl Progress {
    pub fn fraction(&self) -> f32 {
        f32::from_bits(self.done.load(Relaxed))
    }
}

/// Write `[start, end]` of `src` to `dst` (same container format as `src`).
pub fn export(
    src: &Path,
    dst: &Path,
    start: f64,
    end: f64,
    progress: Arc<Progress>,
) -> Result<(), String> {
    let err = |e: ff::Error| e.to_string();
    let mut ictx = ff::format::input(src).map_err(err)?;
    let t0 = tb(&ictx);
    let (vi, kf, ts) = seek_keyframe(&mut ictx, start).map_err(err)?;
    // Re-seek so the keyframe packet itself is read again below.
    ictx.seek(ts, ..ts).map_err(err)?;

    let fmt = ictx
        .format()
        .name()
        .split(',')
        .next()
        .unwrap_or("")
        .to_string();
    let mut octx = ff::format::output_as(dst, &guess_muxer(src, &fmt)).map_err(err)?;

    let n = ictx.nb_streams() as usize;
    let mut map = vec![None; n];
    let mut in_tb = vec![ff::Rational(0, 1); n];
    for ist in ictx.streams() {
        let medium = ist.parameters().medium();
        if !matches!(
            medium,
            ff::media::Type::Video | ff::media::Type::Audio | ff::media::Type::Subtitle
        ) {
            continue;
        }
        let mut ost = octx
            .add_stream(ff::encoder::find(ff::codec::Id::None))
            .map_err(err)?;
        ost.set_parameters(ist.parameters());
        ost.set_time_base(ist.time_base());
        ost.set_metadata(ist.metadata().to_owned());
        map[ist.index()] = Some(ost.index());
        in_tb[ist.index()] = ist.time_base();
    }
    octx.set_metadata(ictx.metadata().to_owned());
    let mut opts = ff::Dictionary::new();
    opts.set("avoid_negative_ts", "make_zero");
    octx.write_header_with(opts).map_err(err)?;

    // Audio and subtitles run a little past `end` so the tail is never short.
    let tail = end + 0.1;
    let mut started = false;
    let mut video_done = false;
    let mut tail_fill = true;
    let mut max_kept = f64::NEG_INFINITY;
    for (stream, mut p) in ictx.packets() {
        if progress.cancel.load(Relaxed) {
            drop(octx);
            let _ = std::fs::remove_file(dst);
            return Err("cancelled".into());
        }
        let i = stream.index();
        let Some(o) = map[i] else { continue };
        let tbf = f64::from(in_tb[i]);
        let Some(pts) = p.pts().or(p.dts()) else {
            continue;
        };
        let t = pts as f64 * tbf - t0;
        if i == vi {
            if !started {
                if !p.is_key() {
                    continue;
                }
                started = true;
            }
            // Stop by decode time, not display time: the last B-frames before
            // `end` reference a frame displayed after it, which must be kept.
            // Past that point, keep only frames that fill gaps before the
            // furthest frame kept, so the tail plays without skips.
            let dts = p.dts().map_or(t, |d| d as f64 * tbf - t0);
            if dts > end {
                video_done = true;
            }
            if video_done {
                if !tail_fill || t > max_kept {
                    tail_fill = false;
                    continue;
                }
            } else {
                max_kept = max_kept.max(t);
            }
            let f = ((t - kf) / (end - kf).max(1e-3)).clamp(0.0, 1.0) as f32;
            progress.done.store(f.to_bits(), Relaxed);
        } else if video_done && t > tail + 1.0 {
            break;
        } else if !started || t < kf || t > tail {
            continue;
        }
        // Shift so the output starts at zero.
        let shift = ((kf + t0) / tbf).round() as i64;
        p.set_pts(p.pts().map(|v| v - shift));
        p.set_dts(p.dts().map(|v| v - shift));
        let ost_tb = octx.stream(o).unwrap().time_base();
        p.rescale_ts(in_tb[i], ost_tb);
        p.set_position(-1);
        p.set_stream(o);
        p.write_interleaved(&mut octx).map_err(err)?;
    }
    octx.write_trailer().map_err(err)?;
    progress.done.store(1f32.to_bits(), Relaxed);
    Ok(())
}

/// The demuxer reports e.g. "mov,mp4,m4a,..."; pick the muxer for the file's own
/// extension so an .mp4 stays an mp4.
fn guess_muxer(src: &Path, demuxer: &str) -> String {
    let ext = src
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "mp4" | "m4v" => "mp4",
        "mov" | "3gp" => "mov",
        "mkv" => "matroska",
        "webm" => "webm",
        "avi" => "avi",
        "ts" | "mts" | "m2ts" => "mpegts",
        "flv" => "flv",
        "ogv" => "ogg",
        "wmv" => "asf",
        _ => demuxer,
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    /// Needs a real clip: ANYVIEW_TEST_VIDEO=clip.mp4 cargo test -- --ignored
    /// Optional ANYVIEW_TRIM=start,end and ANYVIEW_TRIM_OUT=path.
    #[test]
    #[ignore]
    fn export_real_clip() {
        let src = std::path::PathBuf::from(std::env::var("ANYVIEW_TEST_VIDEO").unwrap());
        let (start, end) = std::env::var("ANYVIEW_TRIM")
            .ok()
            .and_then(|v| {
                let (a, b) = v.split_once(',')?;
                Some((a.parse().ok()?, b.parse().ok()?))
            })
            .unwrap_or((3.3, 7.0));
        ffmpeg_next::init().unwrap();
        let k = super::keyframe_at_or_before(&src, start).unwrap();
        let ext = src.extension().unwrap().to_string_lossy().to_string();
        let dst = std::env::var("ANYVIEW_TRIM_OUT")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| src.with_file_name(format!("trim-test-out.{ext}")));
        super::export(&src, &dst, start, end, Default::default()).unwrap();
        let out = ffmpeg_next::format::input(&dst).unwrap();
        let dur = out.duration() as f64 / 1e6;
        eprintln!("keyframe {k:.3}, output duration {dur:.3}");
        // Never shorter than asked; at most a mini-GOP plus audio tail longer.
        let want = end - k;
        assert!(
            dur >= want - 0.05 && dur <= want + 0.5,
            "duration {dur}, want {want}"
        );
    }
}
