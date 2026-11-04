//! Lossless video trim: copy packets from the keyframe at or before the in point
//! up to the out point. No re-encode, so it is fast and keeps full quality; the
//! start snaps to a keyframe, and the UI shows that snapped point up front.

use crate::video::start_time;
use ffmpeg_next as ff;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};

/// Find the last keyframe that is *displayed* at or before `time`. Returns the
/// video stream, the keyframe's time, and the seek target that lands on it.
///
/// Demuxers seek by decode time, and with B-frames a keyframe is displayed a
/// few frames after it is decoded, so a plain seek to `time` can land on a
/// keyframe shown just after it. Step back until the keyframe precedes it.
fn seek_keyframe(
    input: &mut ff::format::context::Input,
    time: f64,
) -> Result<(usize, f64, i64), ff::Error> {
    let start = start_time(input);
    let stream = input
        .streams()
        .best(ff::media::Type::Video)
        .ok_or(ff::Error::StreamNotFound)?;
    let (video_index, time_base) = (stream.index(), f64::from(stream.time_base()));
    let mut back = 0.0;
    loop {
        let target = (time - back + start).max(start) * f64::from(ff::ffi::AV_TIME_BASE);
        let target = target as i64;
        input.seek(target, ..target)?;
        let key_pts = input.packets().find_map(|(stream, packet)| {
            (stream.index() == video_index && packet.is_key())
                .then(|| packet.pts().or(packet.dts()))
                .flatten()
        });
        let keyframe = key_pts.ok_or(ff::Error::Eof)? as f64 * time_base - start;
        // At the start of the file there is nothing earlier to find.
        if keyframe <= time + 1e-4 || time - back <= 0.0 {
            return Ok((video_index, keyframe, target));
        }
        back = if back == 0.0 { 0.1 } else { back * 2.0 };
    }
}

/// Time of the last keyframe displayed at or before `time`, where a trim
/// starting at `time` really starts.
pub fn keyframe_at_or_before(path: &Path, time: f64) -> Option<f64> {
    let mut input = ff::format::input(path).ok()?;
    seek_keyframe(&mut input, time)
        .ok()
        .map(|(_, keyframe, _)| keyframe.max(0.0))
}

/// Progress of a running export, shared with the UI.
#[derive(Default)]
pub struct Progress {
    /// Fraction done, as f32 bits.
    pub done: AtomicU32,
    pub cancel: AtomicBool,
}

impl Progress {
    /// Fraction done, 0 to 1.
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
    let to_text = |error: ff::Error| error.to_string();
    let mut input = ff::format::input(src).map_err(to_text)?;
    let start_offset = start_time(&input);
    let (video_index, keyframe, seek_target) = seek_keyframe(&mut input, start).map_err(to_text)?;
    // Re-seek so the keyframe packet itself is read again below.
    input.seek(seek_target, ..seek_target).map_err(to_text)?;

    let demuxer = input
        .format()
        .name()
        .split(',')
        .next()
        .unwrap_or("")
        .to_string();
    let mut output = ff::format::output_as(dst, &guess_muxer(src, &demuxer)).map_err(to_text)?;

    let stream_count = input.nb_streams() as usize;
    let mut output_index = vec![None; stream_count];
    let mut input_time_base = vec![ff::Rational(0, 1); stream_count];
    let mut output_time_base = vec![ff::Rational(0, 1); stream_count];
    for in_stream in input.streams() {
        let medium = in_stream.parameters().medium();
        if !matches!(
            medium,
            ff::media::Type::Video | ff::media::Type::Audio | ff::media::Type::Subtitle
        ) {
            continue;
        }
        let mut out_stream = output
            .add_stream(ff::encoder::find(ff::codec::Id::None))
            .map_err(to_text)?;
        out_stream.set_parameters(in_stream.parameters());
        out_stream.set_time_base(in_stream.time_base());
        out_stream.set_metadata(in_stream.metadata().to_owned());
        output_index[in_stream.index()] = Some(out_stream.index());
        input_time_base[in_stream.index()] = in_stream.time_base();
    }
    output.set_metadata(input.metadata().to_owned());
    let mut options = ff::Dictionary::new();
    options.set("avoid_negative_ts", "make_zero");
    output.write_header_with(options).map_err(to_text)?;
    // The muxer may change stream time bases while writing the header.
    for (index, out_stream) in output.streams().enumerate() {
        output_time_base[index] = out_stream.time_base();
    }

    // Audio and subtitles run a little past `end` so the tail is never short.
    let tail = end + 0.1;
    let mut started = false;
    let mut video_done = false;
    let mut tail_fill = true;
    let mut max_kept = f64::NEG_INFINITY;
    for (stream, mut packet) in input.packets() {
        if progress.cancel.load(Relaxed) {
            drop(output);
            // Best effort: a partial file left behind is only clutter.
            let _ = std::fs::remove_file(dst);
            return Err("cancelled".into());
        }
        let index = stream.index();
        let Some(out_index) = output_index[index] else {
            continue;
        };
        let time_base = f64::from(input_time_base[index]);
        let Some(pts) = packet.pts().or(packet.dts()) else {
            continue;
        };
        let time = pts as f64 * time_base - start_offset;
        if index == video_index {
            if !started {
                if !packet.is_key() {
                    continue;
                }
                started = true;
            }
            // Stop by decode time, not display time: the last B-frames before
            // `end` reference a frame displayed after it, which must be kept.
            // Past that point, keep only frames that fill gaps before the
            // furthest frame kept, so the tail plays without skips.
            let decode_time = packet
                .dts()
                .map_or(time, |dts| dts as f64 * time_base - start_offset);
            if decode_time > end {
                video_done = true;
            }
            if video_done {
                if !tail_fill || time > max_kept {
                    tail_fill = false;
                    continue;
                }
            } else {
                max_kept = max_kept.max(time);
            }
            let fraction = ((time - keyframe) / (end - keyframe).max(1e-3)).clamp(0.0, 1.0);
            progress.done.store((fraction as f32).to_bits(), Relaxed);
        } else if video_done && time > tail + 1.0 {
            break;
        } else if !started || time < keyframe || time > tail {
            continue;
        }
        // Shift so the output starts at zero.
        let shift = ((keyframe + start_offset) / time_base).round() as i64;
        packet.set_pts(packet.pts().map(|pts| pts - shift));
        packet.set_dts(packet.dts().map(|dts| dts - shift));
        packet.rescale_ts(input_time_base[index], output_time_base[out_index]);
        packet.set_position(-1);
        packet.set_stream(out_index);
        packet.write_interleaved(&mut output).map_err(to_text)?;
    }
    output.write_trailer().map_err(to_text)?;
    progress.done.store(1f32.to_bits(), Relaxed);
    Ok(())
}

/// The demuxer reports e.g. "mov,mp4,m4a,..."; pick the muxer for the file's own
/// extension so an .mp4 stays an mp4.
fn guess_muxer(src: &Path, demuxer: &str) -> String {
    let ext = src
        .extension()
        .and_then(|ext| ext.to_str())
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
            .and_then(|range| {
                let (from, to) = range.split_once(',')?;
                Some((from.parse().ok()?, to.parse().ok()?))
            })
            .unwrap_or((3.3, 7.0));
        ffmpeg_next::init().unwrap();
        let keyframe = super::keyframe_at_or_before(&src, start).unwrap();
        let ext = src.extension().unwrap().to_string_lossy().to_string();
        let dst = std::env::var("ANYVIEW_TRIM_OUT")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| src.with_file_name(format!("trim-test-out.{ext}")));
        super::export(&src, &dst, start, end, Default::default()).unwrap();
        let out = ffmpeg_next::format::input(&dst).unwrap();
        let duration = out.duration() as f64 / 1e6;
        eprintln!("keyframe {keyframe:.3}, output duration {duration:.3}");
        // Never shorter than asked; at most a mini-GOP plus audio tail longer.
        let want = end - keyframe;
        assert!(
            duration >= want - 0.05 && duration <= want + 0.5,
            "duration {duration}, want {want}"
        );
    }
}
