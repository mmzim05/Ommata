//! Video trim. The start is frame-exact for H.264 and HEVC: the frames from
//! the cut to the next keyframe are re-encoded at the bitrate the source
//! spent on them, and everything after is copied packet for packet, so the
//! quality is kept and the size stays proportional to the length kept. Other
//! codecs are copied from the keyframe at or before the in point (the UI
//! shows that snapped point).

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
        let target = ((time - back + start) * f64::from(ff::ffi::AV_TIME_BASE)) as i64;
        input.seek(target, ..target)?;
        let key_pts = input.packets().find_map(|(stream, packet)| {
            (stream.index() == video_index && packet.is_key())
                .then(|| packet.pts().or(packet.dts()))
                .flatten()
        });
        let keyframe = key_pts.ok_or(ff::Error::Eof)? as f64 * time_base - start;
        // Decode times can start before the file's start time (B-frames in
        // MPEG-TS), so the search goes a little past it before giving up.
        if keyframe <= time + 1e-4 || time - back <= -2.0 {
            return Ok((video_index, keyframe, target));
        }
        back = if back == 0.0 { 0.1 } else { back * 2.0 };
    }
}

/// Where a trim starting at `time` really starts: `time` itself when the cut
/// can be frame-exact, else the keyframe at or before it.
pub fn cut_point(path: &Path, time: f64) -> Option<f64> {
    let mut input = ff::format::input(path).ok()?;
    let exact = input
        .streams()
        .best(ff::media::Type::Video)
        .is_some_and(|stream| Codec::of(stream.parameters().id()).is_some());
    if exact {
        return Some(time.max(0.0));
    }
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

    fn set(&self, fraction: f64) {
        let fraction = fraction.clamp(0.0, 1.0) as f32;
        self.done.store(fraction.to_bits(), Relaxed);
    }
}

/// A codec the start can be cut frame-exactly in.
#[derive(Clone, Copy, PartialEq)]
enum Codec {
    H264,
    Hevc,
}

impl Codec {
    fn of(id: ff::codec::Id) -> Option<Self> {
        let codec = match id {
            ff::codec::Id::H264 => Self::H264,
            ff::codec::Id::HEVC => Self::Hevc,
            _ => return None,
        };
        ff::encoder::find_by_name(codec.encoder()).map(|_| codec)
    }

    fn encoder(self) -> &'static str {
        match self {
            Self::H264 => "libx264",
            Self::Hevc => "libx265",
        }
    }

    fn nal_type(self, nal: &[u8]) -> u8 {
        match self {
            Self::H264 => nal[0] & 0x1f,
            Self::Hevc => (nal[0] >> 1) & 0x3f,
        }
    }

    /// Whether decoding can restart at a picture made of these NAL units,
    /// with new parameter sets. HEVC CRA pictures qualify once their leading
    /// pictures are dropped: they are relabelled BLA_N_LP.
    fn restart_point(self, nals: &mut [&mut [u8]]) -> bool {
        const H264_IDR: u8 = 5;
        const HEVC_CRA: u8 = 21;
        const HEVC_BLA_N_LP: u8 = 18;
        for nal in nals.iter_mut().filter(|nal| !nal.is_empty()) {
            let kind = self.nal_type(nal);
            match self {
                Self::H264 if (1..=5).contains(&kind) => return kind == H264_IDR,
                Self::Hevc if kind == HEVC_CRA => {
                    nal[0] = (nal[0] & 0x81) | (HEVC_BLA_N_LP << 1);
                    return true;
                }
                Self::Hevc if kind < 32 => return (16..=21).contains(&kind),
                _ => {}
            }
        }
        false
    }
}

/// How the stream frames NAL units in packets: with big-endian length
/// prefixes of `length` bytes (MP4, Matroska) or with start codes (MPEG-TS).
#[derive(Clone, Copy)]
enum Framing {
    Length(usize),
    AnnexB,
}

/// Split a packet into its NAL units.
fn split_nals(data: &mut [u8], framing: Framing) -> Vec<&mut [u8]> {
    let ranges = match framing {
        Framing::Length(length) => length_ranges(data, length),
        Framing::AnnexB => annex_b_ranges(data),
    };
    let mut nals = Vec::with_capacity(ranges.len());
    let mut rest = data;
    let mut offset = 0;
    for (from, to) in ranges {
        let (_, tail) = rest.split_at_mut(from - offset);
        let (nal, next) = tail.split_at_mut(to - from);
        nals.push(nal);
        rest = next;
        offset = to;
    }
    nals
}

/// NAL unit byte ranges in length-prefixed data.
fn length_ranges(data: &[u8], length: usize) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut at = 0;
    while at + length <= data.len() {
        let size = data[at..at + length]
            .iter()
            .fold(0usize, |size, byte| size << 8 | usize::from(*byte));
        let from = at + length;
        if size == 0 || from + size > data.len() {
            break;
        }
        ranges.push((from, from + size));
        at = from + size;
    }
    ranges
}

/// NAL unit byte ranges in Annex B data (after each start code, up to the
/// next one).
fn annex_b_ranges(data: &[u8]) -> Vec<(usize, usize)> {
    let mut starts = Vec::new();
    let mut index = 0;
    while index + 3 <= data.len() {
        if data[index] == 0 && data[index + 1] == 0 && data[index + 2] == 1 {
            let code = if index > 0 && data[index - 1] == 0 {
                index - 1
            } else {
                index
            };
            starts.push((code, index + 3));
            index += 3;
        } else {
            index += 1;
        }
    }
    starts
        .iter()
        .enumerate()
        .map(|(index, &(_, from))| {
            let to = starts.get(index + 1).map_or(data.len(), |next| next.0);
            (from, to)
        })
        .collect()
}

/// Re-frame NAL units for the stream.
fn frame_nals<'a>(nals: impl IntoIterator<Item = &'a [u8]>, framing: Framing) -> Vec<u8> {
    let mut out = Vec::new();
    for nal in nals {
        match framing {
            Framing::Length(length) => {
                out.extend_from_slice(&(nal.len() as u64).to_be_bytes()[8 - length..]);
            }
            Framing::AnnexB => out.extend_from_slice(&[0, 0, 0, 1]),
        }
        out.extend_from_slice(nal);
    }
    out
}

/// The framing and parameter sets (SPS, PPS, VPS) stored in the stream's
/// extradata: avcC/hvcC for MP4 and Matroska, Annex B otherwise.
fn parameter_sets(codec: Codec, extradata: &[u8]) -> (Framing, Vec<Vec<u8>>) {
    let read_u16 = |at: usize| {
        extradata
            .get(at..at + 2)
            .map(|bytes| usize::from(bytes[0]) << 8 | usize::from(bytes[1]))
    };
    let mut sets = Vec::new();
    if extradata.first() != Some(&1) {
        let mut copy = extradata.to_vec();
        for nal in split_nals(&mut copy, Framing::AnnexB) {
            sets.push(nal.to_vec());
        }
        return (Framing::AnnexB, sets);
    }
    let mut take = |at: &mut usize| -> Option<()> {
        let size = read_u16(*at)?;
        sets.push(extradata.get(*at + 2..*at + 2 + size)?.to_vec());
        *at += 2 + size;
        Some(())
    };
    let length = match codec {
        Codec::H264 => {
            let length = usize::from(extradata.get(4).map_or(3, |byte| byte & 3)) + 1;
            let mut at = 6;
            let sps = extradata.get(5).map_or(0, |byte| byte & 0x1f);
            for _ in 0..sps {
                if take(&mut at).is_none() {
                    break;
                }
            }
            let pps = extradata.get(at).copied().unwrap_or(0);
            at += 1;
            for _ in 0..pps {
                if take(&mut at).is_none() {
                    break;
                }
            }
            length
        }
        Codec::Hevc => {
            let length = usize::from(extradata.get(21).map_or(3, |byte| byte & 3)) + 1;
            let arrays = extradata.get(22).copied().unwrap_or(0);
            let mut at = 23;
            for _ in 0..arrays {
                let Some(count) = read_u16(at + 1) else {
                    break;
                };
                at += 3;
                for _ in 0..count {
                    if take(&mut at).is_none() {
                        break;
                    }
                }
            }
            length
        }
    };
    (Framing::Length(length), sets)
}

/// The stream's extradata (codec configuration) bytes.
fn extradata(parameters: &ff::codec::Parameters) -> Vec<u8> {
    // SAFETY: plain reads of the parameters' extradata buffer.
    unsafe {
        let raw = parameters.as_ptr();
        if (*raw).extradata.is_null() || (*raw).extradata_size <= 0 {
            return Vec::new();
        }
        std::slice::from_raw_parts((*raw).extradata, (*raw).extradata_size as usize).to_vec()
    }
}

/// A new packet holding `data`, with the timing and flags of `from`.
fn repacket(data: &[u8], from: &ff::Packet) -> ff::Packet {
    let mut packet = ff::Packet::copy(data);
    packet.set_pts(from.pts());
    packet.set_dts(from.dts());
    packet.set_duration(from.duration());
    packet.set_flags(from.flags());
    packet
}

/// Bits per second the source spends on the group of pictures starting at
/// the keyframe the input was seeked to: the budget for re-encoding it.
fn gop_bit_rate(
    input: &mut ff::format::context::Input,
    video_index: usize,
    seek_target: i64,
) -> Result<usize, ff::Error> {
    input.seek(seek_target, ..seek_target)?;
    let time_base = f64::from(
        input
            .stream(video_index)
            .ok_or(ff::Error::StreamNotFound)?
            .time_base(),
    );
    let (mut bytes, mut first, mut last) = (0usize, None, 0i64);
    for (stream, packet) in input.packets() {
        if stream.index() != video_index {
            continue;
        }
        let Some(pts) = packet.pts().or(packet.dts()) else {
            continue;
        };
        if first.is_some() && packet.is_key() {
            last = pts;
            break;
        }
        first.get_or_insert(pts);
        bytes += packet.size();
        last = last.max(pts);
    }
    let seconds = (last - first.unwrap_or(last)) as f64 * time_base;
    let seconds = if seconds > 0.0 { seconds } else { 1.0 };
    Ok(((bytes * 8) as f64 / seconds) as usize)
}

fn frame_rate(stream: &ff::format::stream::Stream) -> ff::Rational {
    let rate = stream.avg_frame_rate();
    if rate.numerator() > 0 && rate.denominator() > 0 {
        rate
    } else {
        ff::Rational(30, 1)
    }
}

/// Re-encodes the frames between the cut and the next restart point.
struct Head {
    codec: Codec,
    decoder: ff::decoder::Video,
    encoder: Option<ff::encoder::Video>,
    frame: ff::frame::Video,
    /// Source time base of the video stream.
    time_base: ff::Rational,
    frame_rate: ff::Rational,
    /// Frames from here (stream ticks) up to the restart point are encoded.
    from: i64,
    /// Bitrate the source spent on the group of pictures around the cut.
    bit_rate: usize,
    packets: Vec<ff::Packet>,
}

impl Head {
    fn new(
        codec: Codec,
        stream: &ff::format::stream::Stream,
        from: i64,
        bit_rate: usize,
    ) -> Result<Self, ff::Error> {
        let parameters = stream.parameters();
        let mut context = ff::codec::Context::from_parameters(parameters)?;
        context.set_threading(ff::codec::threading::Config {
            kind: ff::codec::threading::Type::Frame,
            count: 0,
        });
        Ok(Self {
            codec,
            decoder: context.decoder().video()?,
            encoder: None,
            frame: ff::frame::Video::empty(),
            time_base: stream.time_base(),
            frame_rate: frame_rate(stream),
            from,
            bit_rate,
            packets: Vec::new(),
        })
    }

    /// Feed a source packet; frames shown from `from` up to `until` (stream
    /// ticks, exclusive) are encoded.
    fn decode(&mut self, packet: Option<&ff::Packet>, until: i64) -> Result<(), ff::Error> {
        match packet {
            Some(packet) => {
                // A corrupt packet only loses its own frame.
                let _ = self.decoder.send_packet(packet);
            }
            None => self.decoder.send_eof()?,
        }
        while self.decoder.receive_frame(&mut self.frame).is_ok() {
            let Some(pts) = self.frame.timestamp().or(self.frame.pts()) else {
                continue;
            };
            if pts < self.from || pts >= until {
                continue;
            }
            self.frame.set_pts(Some(pts));
            self.frame.set_kind(ff::picture::Type::None);
            let frame = std::mem::replace(&mut self.frame, ff::frame::Video::empty());
            self.encode(Some(&frame))?;
        }
        Ok(())
    }

    fn encode(&mut self, frame: Option<&ff::frame::Video>) -> Result<(), ff::Error> {
        if self.encoder.is_none() {
            let Some(frame) = frame else {
                return Ok(());
            };
            self.encoder = Some(self.open_encoder(frame)?);
        }
        let Some(encoder) = self.encoder.as_mut() else {
            return Ok(());
        };
        match frame {
            Some(frame) => encoder.send_frame(frame)?,
            None => encoder.send_eof()?,
        }
        let mut packet = ff::Packet::empty();
        while encoder.receive_packet(&mut packet).is_ok() {
            packet.rescale_ts(encoder.time_base(), self.time_base);
            self.packets
                .push(std::mem::replace(&mut packet, ff::Packet::empty()));
        }
        Ok(())
    }

    /// An encoder matching the source frames, with in-band parameter sets and
    /// no B-frames so its timestamps splice onto the copied part.
    fn open_encoder(&self, frame: &ff::frame::Video) -> Result<ff::encoder::Video, ff::Error> {
        let codec =
            ff::encoder::find_by_name(self.codec.encoder()).ok_or(ff::Error::EncoderNotFound)?;
        let mut encoder = ff::codec::Context::new_with_codec(codec)
            .encoder()
            .video()?;
        encoder.set_width(frame.width());
        encoder.set_height(frame.height());
        encoder.set_format(frame.format());
        encoder.set_time_base(self.time_base);
        // Rate control needs the real frame rate: time bases are often 1 ms.
        encoder.set_frame_rate(Some(self.frame_rate));
        encoder.set_colorspace(frame.color_space());
        encoder.set_color_range(frame.color_range());
        encoder.set_bit_rate(self.bit_rate);
        encoder.set_max_bit_rate(self.bit_rate * 3 / 2);
        encoder.set_max_b_frames(0);
        encoder.set_gop(u32::MAX >> 1);
        // SAFETY: plain field writes on an encoder context that isn't open.
        unsafe {
            let context = encoder.as_mut_ptr();
            (*context).rc_buffer_size = (self.bit_rate * 2).min(i32::MAX as usize) as i32;
            (*context).color_primaries = (*frame.as_ptr()).color_primaries;
            (*context).color_trc = (*frame.as_ptr()).color_trc;
            (*context).sample_aspect_ratio = (*frame.as_ptr()).sample_aspect_ratio;
        }
        // x265 at "medium" manages about a frame a second on 4K; above 1080p
        // "fast" keeps a long head quick at a small cost in efficiency.
        let large = frame.width() * frame.height() > 1920 * 1088;
        let preset = if self.codec == Codec::Hevc && large {
            "fast"
        } else {
            "medium"
        };
        let mut options = ff::Dictionary::new();
        options.set("preset", preset);
        encoder.open_with(options)
    }

    /// Flush both codecs and return the encoded packets.
    fn finish(mut self, until: i64) -> Result<Vec<ff::Packet>, ff::Error> {
        self.decode(None, until)?;
        self.encode(None)?;
        Ok(self.packets)
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
    let result = export_inner(src, dst, start, end, &progress);
    if result.is_err() {
        // Best effort: a partial file left behind is only clutter.
        let _ = std::fs::remove_file(dst);
    }
    result
}

fn export_inner(
    src: &Path,
    dst: &Path,
    start: f64,
    end: f64,
    progress: &Progress,
) -> Result<(), String> {
    let to_text = |error: ff::Error| error.to_string();
    let mut input = ff::format::input(src).map_err(to_text)?;
    let start_offset = start_time(&input);
    let (video_index, keyframe, seek_target) = seek_keyframe(&mut input, start).map_err(to_text)?;
    let video = input.stream(video_index).ok_or("no video stream")?;
    let video_time_base = video.time_base();
    let codec = Codec::of(video.parameters().id());
    let half_frame = 0.5 * f64::from(frame_rate(&video).invert());
    let exact = codec.is_some() && start - keyframe > half_frame;
    let (framing, sets) = match codec {
        Some(codec) => parameter_sets(codec, &extradata(&video.parameters())),
        None => (Framing::AnnexB, Vec::new()),
    };
    let first = if exact { start } else { keyframe };
    let ticks = |time: f64| ((time + start_offset) / f64::from(video_time_base)).round() as i64;
    let mut head = match codec.filter(|_| exact) {
        Some(codec) => {
            let bit_rate = gop_bit_rate(&mut input, video_index, seek_target).map_err(to_text)?;
            let video = input.stream(video_index).ok_or("no video stream")?;
            Some(Head::new(codec, &video, ticks(start), bit_rate).map_err(to_text)?)
        }
        None => None,
    };
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

    // Shift every stream so the output starts at zero.
    let write = |output: &mut ff::format::context::Output,
                 mut packet: ff::Packet,
                 index: usize|
     -> Result<(), String> {
        let Some(out_index) = output_index[index] else {
            return Ok(());
        };
        let shift = ((first + start_offset) / f64::from(input_time_base[index])).round() as i64;
        packet.set_pts(packet.pts().map(|pts| pts - shift));
        packet.set_dts(packet.dts().map(|dts| dts - shift));
        packet.rescale_ts(input_time_base[index], output_time_base[out_index]);
        packet.set_position(-1);
        packet.set_stream(out_index);
        packet.write_interleaved(output).map_err(to_text)
    };
    // Encoded packets come with start codes; re-frame them like the source
    // and keep their decode times `delay` ticks before display, as the
    // source's are, so they run straight into the copied packets.
    let write_head = |output: &mut ff::format::context::Output,
                      head: Head,
                      until: i64,
                      delay: i64|
     -> Result<(), String> {
        for encoded in head.finish(until).map_err(to_text)? {
            let mut data = encoded.data().unwrap_or(&[]).to_vec();
            let nals = split_nals(&mut data, Framing::AnnexB);
            let framed = frame_nals(nals.iter().map(|nal| &**nal), framing);
            let mut packet = repacket(&framed, &encoded);
            packet.set_dts(encoded.pts().map(|pts| pts - delay));
            write(output, packet, video_index)?;
        }
        Ok(())
    };

    // Audio and subtitles run a little past `end` so the tail is never short.
    let tail = end + 0.1;
    let end_ticks = ticks(end) + 1;
    let mut copy_from = i64::MIN;
    // Seeks in some containers (MPEG-TS) land between keyframes: decode and
    // copy only from the first keyframe on.
    let mut started = false;
    let mut video_done = false;
    let mut tail_fill = true;
    let mut max_kept = f64::NEG_INFINITY;
    let video_seconds = f64::from(video_time_base);
    for (stream, mut packet) in input.packets() {
        if progress.cancel.load(Relaxed) {
            return Err("cancelled".into());
        }
        let index = stream.index();
        if output_index[index].is_none() {
            continue;
        }
        let Some(pts) = packet.pts().or(packet.dts()) else {
            continue;
        };
        let time = pts as f64 * f64::from(input_time_base[index]) - start_offset;
        if index != video_index {
            if video_done && time > tail + 1.0 {
                break;
            }
            if time >= first - 1e-3 && time <= tail {
                write(&mut output, packet, index)?;
            }
            continue;
        }
        if !started {
            if !packet.is_key() {
                continue;
            }
            started = true;
        }
        let decode_time = packet
            .dts()
            .map_or(time, |dts| dts as f64 * video_seconds - start_offset);
        if head.is_some() {
            let restart = packet.is_key()
                && time > start
                && time <= end
                && codec.is_some_and(|codec| {
                    packet
                        .data_mut()
                        .is_some_and(|data| codec.restart_point(&mut split_nals(data, framing)))
                });
            if !restart {
                if decode_time <= end {
                    if let Some(head) = head.as_mut() {
                        head.decode(Some(&packet), end_ticks).map_err(to_text)?;
                    }
                    progress.set((time - first) / (end - first).max(1e-3));
                    continue;
                }
                // Past the out point without a restart point: the head is
                // the whole video.
                if let Some(head) = head.take() {
                    write_head(&mut output, head, end_ticks, 0)?;
                }
                video_done = true;
                continue;
            }
            // Splice: the head covers up to this picture, which restarts
            // decoding with the source's own parameter sets.
            let delay = pts - packet.dts().unwrap_or(pts);
            if let Some(head) = head.take() {
                write_head(&mut output, head, pts.min(end_ticks), delay)?;
            }
            copy_from = pts;
            if !sets.is_empty() {
                let mut joined = frame_nals(sets.iter().map(Vec::as_slice), framing);
                joined.extend_from_slice(packet.data().unwrap_or(&[]));
                packet = repacket(&joined, &packet);
            }
        }
        if video_done || pts < copy_from {
            // Leading pictures of the restart point were re-encoded already.
            continue;
        }
        // Stop by decode time, not display time: the last B-frames before
        // `end` reference a frame displayed after it, which must be kept.
        // Past that point, keep only frames that fill gaps before the
        // furthest frame kept, so the tail plays without skips.
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
        progress.set((time - first) / (end - first).max(1e-3));
        write(&mut output, packet, index)?;
    }
    if let Some(head) = head.take() {
        // The file ended inside the head.
        write_head(&mut output, head, end_ticks, 0)?;
    }
    output.write_trailer().map_err(to_text)?;
    progress.set(1.0);
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
    /// Needs a real clip: OMALENS_TEST_VIDEO=clip.mp4 cargo test -- --ignored
    /// Optional OMALENS_TRIM=start,end and OMALENS_TRIM_OUT=path.
    #[test]
    #[ignore]
    fn export_real_clip() {
        let src = std::path::PathBuf::from(std::env::var("OMALENS_TEST_VIDEO").unwrap());
        let (start, end) = std::env::var("OMALENS_TRIM")
            .ok()
            .and_then(|range| {
                let (from, to) = range.split_once(',')?;
                Some((from.parse().ok()?, to.parse().ok()?))
            })
            .unwrap_or((3.3, 7.0));
        ffmpeg_next::init().unwrap();
        let first = super::cut_point(&src, start).unwrap();
        let ext = src.extension().unwrap().to_string_lossy().to_string();
        let dst = std::env::var("OMALENS_TRIM_OUT")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| src.with_file_name(format!("trim-test-out.{ext}")));
        super::export(&src, &dst, start, end, Default::default()).unwrap();
        let out = ffmpeg_next::format::input(&dst).unwrap();
        let duration = out.duration() as f64 / 1e6;
        let source = std::fs::metadata(&src).unwrap().len() as f64;
        let size = std::fs::metadata(&dst).unwrap().len() as f64;
        let total = ffmpeg_next::format::input(&src).unwrap().duration() as f64 / 1e6;
        eprintln!(
            "cut at {first:.3}, output duration {duration:.3}, size {:.1}% for {:.1}% of the length",
            size / source * 100.0,
            (end - first) / total * 100.0
        );
        // Never shorter than asked; at most a mini-GOP plus audio tail longer.
        let want = end - first;
        assert!(
            duration >= want - 0.05 && duration <= want + 0.5,
            "duration {duration}, want {want}"
        );
    }
}
