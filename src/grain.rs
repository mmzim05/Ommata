//! AV1 film grain synthesis (AV1 specification 7.18.3), done by ommata
//! instead of the decoder.
//!
//! VA-API drivers don't apply film grain reliably (NVIDIA's leaves the
//! grained frame black), so the GPU decoder exports each frame's grain
//! parameters and leaves the frame clean. This module turns the parameters
//! into the small tables the video shader needs (grain templates, block
//! offsets, scaling curves), and applies the same grain on the CPU for frames
//! that end up there. Both follow the specification bit for bit, like dav1d.

use ffmpeg_next as ff;

const GRAIN_WIDTH: usize = 82;
const GRAIN_HEIGHT: usize = 73;
const SUB_GRAIN_WIDTH: usize = 44;
const SUB_GRAIN_HEIGHT: usize = 38;
const BLOCK: usize = 32;

/// Rows of the stacked grain texture: luma, then Cb and Cr.
pub const TEXTURE_WIDTH: usize = GRAIN_WIDTH;
pub const TEXTURE_HEIGHT: usize = GRAIN_HEIGHT + 2 * SUB_GRAIN_HEIGHT;
pub const CB_ROW: usize = GRAIN_HEIGHT;
pub const CR_ROW: usize = GRAIN_HEIGHT + SUB_GRAIN_HEIGHT;

/// libavutil's AVFilmGrainAOMParams.
#[repr(C)]
struct AomParams {
    num_y_points: i32,
    y_points: [[u8; 2]; 14],
    chroma_scaling_from_luma: i32,
    num_uv_points: [i32; 2],
    uv_points: [[[u8; 2]; 10]; 2],
    scaling_shift: i32,
    ar_coeff_lag: i32,
    ar_coeffs_y: [i8; 24],
    ar_coeffs_uv: [[i8; 25]; 2],
    ar_coeff_shift: i32,
    grain_scale_shift: i32,
    uv_mult: [i32; 2],
    uv_mult_luma: [i32; 2],
    uv_offset: [i32; 2],
    overlap_flag: i32,
    limit_output_range: i32,
}

/// libavutil's AVFilmGrainParams, up to the AV1 member of its codec union.
#[repr(C)]
struct FilmGrainParams {
    kind: i32,
    seed: u64,
    width: i32,
    height: i32,
    subsampling_x: i32,
    subsampling_y: i32,
    color_range: i32,
    color_primaries: i32,
    color_trc: i32,
    color_space: i32,
    bit_depth_luma: i32,
    bit_depth_chroma: i32,
    aom: AomParams,
}

const AV_FILM_GRAIN_PARAMS_AV1: i32 = 1;

/// Film grain of one 4:2:0 frame, ready to apply.
pub struct Grain {
    pub depth: u32,
    pub width: usize,
    pub height: usize,
    /// Grain templates: luma 73×82, Cb and Cr 38×44.
    templates: [Vec<i32>; 3],
    /// Scaling curve per plane, indexed by pixel value (1 << depth entries).
    scaling: [Vec<u8>; 3],
    /// Random template offset of each 32×32 block, row by row.
    offsets: Vec<u8>,
    pub blocks: usize,
    pub rows: usize,
    pub apply_luma: bool,
    pub apply_chroma: [bool; 2],
    pub from_luma: bool,
    pub overlap: bool,
    pub scaling_shift: i32,
    pub uv_mult: [i32; 2],
    pub uv_mult_luma: [i32; 2],
    pub uv_offset: [i32; 2],
    pub min: i32,
    pub max_luma: i32,
    pub max_chroma: i32,
}

/// Bit depth of a frame's pixels; for a hardware frame, of its surfaces.
fn depth(frame: &ff::frame::Video) -> Option<u32> {
    // SAFETY: plain reads of a valid frame and its hardware frames context.
    unsafe {
        let raw = frame.as_ptr();
        let mut format = (*raw).format;
        if !(*raw).hw_frames_ctx.is_null() {
            let frames = (*(*raw).hw_frames_ctx)
                .data
                .cast::<ff::ffi::AVHWFramesContext>();
            format = (*frames).sw_format as i32;
        }
        let descriptor = ff::ffi::av_pix_fmt_desc_get(std::mem::transmute::<
            i32,
            ff::ffi::AVPixelFormat,
        >(format));
        (!descriptor.is_null()).then(|| (*descriptor).comp[0].depth as u32)
    }
}

impl Grain {
    /// The grain FFmpeg exported with `frame` (AV_CODEC_EXPORT_DATA_FILM_GRAIN),
    /// if any.
    pub fn of(frame: &ff::frame::Video) -> Option<Self> {
        // SAFETY: FFmpeg stores an AVFilmGrainParams in this side data; the
        // size check guards the fields read.
        let params = unsafe {
            let side = ff::ffi::av_frame_get_side_data(
                frame.as_ptr(),
                ff::ffi::AVFrameSideDataType::AV_FRAME_DATA_FILM_GRAIN_PARAMS,
            );
            if side.is_null() || (*side).size < size_of::<FilmGrainParams>() {
                return None;
            }
            &*(*side).data.cast::<FilmGrainParams>()
        };
        if params.kind != AV_FILM_GRAIN_PARAMS_AV1 {
            return None;
        }
        let depth = depth(frame).filter(|depth| (8..=12).contains(depth))?;
        let identity = params.color_space == ff::ffi::AVColorSpace::AVCOL_SPC_RGB as i32;
        Some(Self::new(
            &params.aom,
            params.seed as u32,
            frame.width() as usize,
            frame.height() as usize,
            depth,
            identity,
        ))
    }

    fn new(
        aom: &AomParams,
        seed: u32,
        width: usize,
        height: usize,
        depth: u32,
        identity: bool,
    ) -> Self {
        let from_luma = aom.chroma_scaling_from_luma != 0;
        let apply_chroma = [0, 1].map(|plane| aom.num_uv_points[plane] > 0 || from_luma);
        let luma = luma_template(aom, seed, depth);
        let chroma = [0, 1].map(|plane| {
            if apply_chroma[plane] {
                chroma_template(aom, seed, depth, plane, &luma)
            } else {
                vec![0; SUB_GRAIN_WIDTH * SUB_GRAIN_HEIGHT]
            }
        });
        let [cb, cr] = chroma;
        let y_points = &aom.y_points[..aom.num_y_points.clamp(0, 14) as usize];
        let uv_points = [0, 1]
            .map(|plane| &aom.uv_points[plane][..aom.num_uv_points[plane].clamp(0, 10) as usize]);
        let rows = height.div_ceil(BLOCK);
        let blocks = width.div_ceil(BLOCK);
        let mut offsets = Vec::with_capacity(rows * blocks);
        for row in 0..rows {
            let row = row as u32;
            let mut state = seed;
            state ^= ((row * 37 + 178) & 0xff) << 8;
            state ^= (row * 173 + 105) & 0xff;
            for _ in 0..blocks {
                offsets.push(random(8, &mut state) as u8);
            }
        }
        let high = depth - 8;
        let (min, max_luma, max_chroma) = if aom.limit_output_range != 0 {
            (
                16 << high,
                235 << high,
                (if identity { 235 } else { 240 }) << high,
            )
        } else {
            (0, (1 << depth) - 1, (1 << depth) - 1)
        };
        Self {
            depth,
            width,
            height,
            templates: [luma, cb, cr],
            scaling: [
                scaling(y_points, depth),
                scaling(uv_points[0], depth),
                scaling(uv_points[1], depth),
            ],
            offsets,
            blocks,
            rows,
            apply_luma: aom.num_y_points > 0,
            apply_chroma,
            from_luma,
            overlap: aom.overlap_flag != 0,
            scaling_shift: aom.scaling_shift,
            uv_mult: aom.uv_mult,
            uv_mult_luma: aom.uv_mult_luma,
            uv_offset: aom.uv_offset,
            min,
            max_luma,
            max_chroma,
        }
    }

    /// Smallest and largest grain value.
    pub fn grain_range(&self) -> (i32, i32) {
        let center = 128 << (self.depth - 8);
        (-center, center - 1)
    }

    /// The three grain templates stacked into one 82×149 texture.
    pub fn template_texture(&self) -> Vec<f32> {
        let mut texture = vec![0.0; TEXTURE_WIDTH * TEXTURE_HEIGHT];
        for (plane, first_row) in [(0, 0), (1, CB_ROW), (2, CR_ROW)] {
            let width = if plane == 0 {
                GRAIN_WIDTH
            } else {
                SUB_GRAIN_WIDTH
            };
            for (index, &value) in self.templates[plane].iter().enumerate() {
                let (row, column) = (index / width, index % width);
                texture[(first_row + row) * TEXTURE_WIDTH + column] = value as f32;
            }
        }
        texture
    }

    /// Block offsets as a `blocks`×`rows` texture.
    pub fn offset_texture(&self) -> Vec<f32> {
        self.offsets
            .iter()
            .map(|&offset| f32::from(offset))
            .collect()
    }

    /// Scaling curves as a 256-wide texture: each plane takes
    /// `scaling_rows()` rows, value v at (v % 256, v / 256).
    pub fn scaling_texture(&self) -> Vec<f32> {
        self.scaling
            .iter()
            .flatten()
            .map(|&value| f32::from(value))
            .collect()
    }

    pub fn scaling_rows(&self) -> usize {
        (1 << self.depth) / 256
    }

    fn grain_at(&self, plane: usize, row: usize, block: usize, i: usize, j: usize) -> i32 {
        let random = usize::from(self.offsets[row * self.blocks + block]);
        let sub = usize::from(plane > 0);
        let x = 3 + (2 >> sub) * (3 + (random >> 4)) + j;
        let y = 3 + (2 >> sub) * (3 + (random & 15)) + i;
        let width = if plane == 0 {
            GRAIN_WIDTH
        } else {
            SUB_GRAIN_WIDTH
        };
        self.templates[plane][y * width + x]
    }

    fn blend(&self, old: i32, new: i32, index: usize, sub: usize) -> i32 {
        let mixed = match (sub, index) {
            (1, _) => old * 23 + new * 22,
            (_, 0) => old * 27 + new * 17,
            _ => old * 17 + new * 27,
        };
        let (min, max) = self.grain_range();
        round2(mixed, 5).clamp(min, max)
    }

    /// Grain of `plane` at column `x` of block row `row`, line `i` of it
    /// (past the block for the overlap with the next row), blended with the
    /// block to the left.
    fn row_noise(&self, plane: usize, row: usize, i: usize, x: usize) -> i32 {
        let sub = usize::from(plane > 0);
        let size = BLOCK >> sub;
        let (block, j) = (x / size, x % size);
        let grain = self.grain_at(plane, row, block, i, j);
        if self.overlap && block > 0 && j < 2 >> sub {
            let old = self.grain_at(plane, row, block - 1, i, j + size);
            return self.blend(old, grain, j, sub);
        }
        grain
    }

    /// Grain of `plane` at pixel (`x`, `y`) of that plane. The shader does
    /// the same per pixel.
    pub fn noise(&self, plane: usize, x: usize, y: usize) -> i32 {
        let sub = usize::from(plane > 0);
        let size = BLOCK >> sub;
        let (row, i) = (y / size, y % size);
        let grain = self.row_noise(plane, row, i, x);
        if self.overlap && row > 0 && i < 2 >> sub {
            let old = self.row_noise(plane, row - 1, i + size, x);
            return self.blend(old, grain, i, sub);
        }
        grain
    }

    fn scale(&self, plane: usize, value: i32) -> i32 {
        i32::from(self.scaling[plane][value as usize])
    }

    /// Grained luma value at (`x`, `y`), from its original value.
    pub fn luma(&self, value: i32, x: usize, y: usize) -> i32 {
        if !self.apply_luma {
            return value;
        }
        let noise = round2(
            self.scale(0, value) * self.noise(0, x, y),
            self.scaling_shift,
        );
        (value + noise).clamp(self.min, self.max_luma)
    }

    /// Grained Cb (`plane` 0) or Cr (1) value at chroma pixel (`x`, `y`),
    /// from its original value and the average of the (original) luma pixels
    /// it covers horizontally.
    pub fn chroma(&self, plane: usize, value: i32, average_luma: i32, x: usize, y: usize) -> i32 {
        if !self.apply_chroma[plane] {
            return value;
        }
        let (merged, scaling) = if self.from_luma {
            (average_luma, 0)
        } else {
            let combined = average_luma * self.uv_mult_luma[plane] + value * self.uv_mult[plane];
            let merged = (combined >> 6) + (self.uv_offset[plane] << (self.depth - 8));
            (merged.clamp(0, (1 << self.depth) - 1), plane + 1)
        };
        let noise = round2(
            self.scale(scaling, merged) * self.noise(plane + 1, x, y),
            self.scaling_shift,
        );
        (value + noise).clamp(self.min, self.max_chroma)
    }

    /// Apply the grain to 4:2:0 planes of `width`×`height` luma pixels.
    fn apply(&self, luma: &mut [i32], cb: &mut [i32], cr: &mut [i32]) {
        let (width, height) = (self.width, self.height);
        let chroma_width = width.div_ceil(2);
        for y in 0..height.div_ceil(2) {
            for x in 0..chroma_width {
                let at = y * 2 * width + x * 2;
                let average = if x * 2 + 1 < width {
                    (luma[at] + luma[at + 1] + 1) >> 1
                } else {
                    // Odd width: the missing pixel repeats the last one.
                    luma[at]
                };
                let index = y * chroma_width + x;
                cb[index] = self.chroma(0, cb[index], average, x, y);
                cr[index] = self.chroma(1, cr[index], average, x, y);
            }
        }
        for y in 0..height {
            for x in 0..width {
                luma[y * width + x] = self.luma(luma[y * width + x], x, y);
            }
        }
    }

    /// Apply the grain to a decoded 4:2:0 frame in system memory: planar
    /// (yuv420p, yuv420p10le) or semi-planar (nv12, p010le). False for any
    /// other format.
    pub fn apply_to(&self, frame: &mut ff::frame::Video) -> bool {
        use ff::format::Pixel;
        let (width, height) = (self.width, self.height);
        let (chroma_width, chroma_height) = (width.div_ceil(2), height.div_ceil(2));
        if frame.width() as usize != width || frame.height() as usize != height {
            return false;
        }
        // Bytes per sample, the shift of the value inside it, and whether
        // Cb and Cr share one interleaved plane.
        let (bytes, shift, interleaved) = match frame.format() {
            Pixel::YUV420P => (1, 0, false),
            Pixel::YUV420P10LE => (2, 0, false),
            Pixel::NV12 => (1, 0, true),
            Pixel::P010LE => (2, 6, true),
            _ => return false,
        };
        let read = |data: &[u8], index: usize| -> i32 {
            if bytes == 1 {
                i32::from(data[index])
            } else {
                i32::from(u16::from_le_bytes([data[index * 2], data[index * 2 + 1]]) >> shift)
            }
        };
        let write = |data: &mut [u8], index: usize, value: i32| {
            if bytes == 1 {
                data[index] = value as u8;
            } else {
                let bytes = ((value as u16) << shift).to_le_bytes();
                data[index * 2..index * 2 + 2].copy_from_slice(&bytes);
            }
        };
        // (plane, sample index) of luma pixel / chroma pixel `index` of a row.
        let chroma_at = |plane: usize, y: usize, x: usize, stride: usize| -> (usize, usize) {
            if interleaved {
                (1, y * stride / bytes + x * 2 + plane)
            } else {
                (1 + plane, y * stride / bytes + x)
            }
        };
        let strides = [0, 1, 2].map(|plane| frame.stride(plane));
        let mut luma = vec![0; width * height];
        let mut chroma = [
            vec![0; chroma_width * chroma_height],
            vec![0; chroma_width * chroma_height],
        ];
        for y in 0..height {
            let data = frame.data(0);
            for x in 0..width {
                luma[y * width + x] = read(data, y * strides[0] / bytes + x);
            }
        }
        for (plane, values) in chroma.iter_mut().enumerate() {
            for y in 0..chroma_height {
                for x in 0..chroma_width {
                    let (index, at) = chroma_at(
                        plane,
                        y,
                        x,
                        strides[if interleaved { 1 } else { 1 + plane }],
                    );
                    values[y * chroma_width + x] = read(frame.data(index), at);
                }
            }
        }
        let [cb, cr] = &mut chroma;
        self.apply(&mut luma, cb, cr);
        for y in 0..height {
            let data = frame.data_mut(0);
            for x in 0..width {
                write(data, y * strides[0] / bytes + x, luma[y * width + x]);
            }
        }
        for (plane, values) in chroma.iter().enumerate() {
            for y in 0..chroma_height {
                for x in 0..chroma_width {
                    let (index, at) = chroma_at(
                        plane,
                        y,
                        x,
                        strides[if interleaved { 1 } else { 1 + plane }],
                    );
                    write(frame.data_mut(index), at, values[y * chroma_width + x]);
                }
            }
        }
        true
    }
}

/// Round2 of the specification: `value` / 2^`shift`, rounded, for any sign.
fn round2(value: i32, shift: i32) -> i32 {
    if shift == 0 {
        value
    } else {
        (value + (1 << (shift - 1))) >> shift
    }
}

/// The specification's 16-bit linear feedback shift register.
fn random(bits: u32, state: &mut u32) -> u32 {
    let r = *state;
    let bit = (r ^ (r >> 1) ^ (r >> 3) ^ (r >> 12)) & 1;
    *state = (r >> 1) | (bit << 15);
    (*state >> (16 - bits)) & ((1 << bits) - 1)
}

fn luma_template(aom: &AomParams, seed: u32, depth: u32) -> Vec<i32> {
    let shift = 12 - depth as i32 + aom.grain_scale_shift;
    let center = 128 << (depth - 8);
    let (min, max) = (-center, center - 1);
    let mut state = seed;
    let mut grain: Vec<i32> = (0..GRAIN_WIDTH * GRAIN_HEIGHT)
        .map(|_| round2(i32::from(GAUSSIAN[random(11, &mut state) as usize]), shift))
        .collect();
    let lag = aom.ar_coeff_lag;
    for y in 3..GRAIN_HEIGHT {
        for x in 3..GRAIN_WIDTH - 3 {
            let mut coefficients = aom.ar_coeffs_y.iter();
            let mut sum = 0;
            'taps: for dy in -lag..=0 {
                for dx in -lag..=lag {
                    if dx == 0 && dy == 0 {
                        break 'taps;
                    }
                    let at = (y as i32 + dy) as usize * GRAIN_WIDTH + (x as i32 + dx) as usize;
                    sum += i32::from(*coefficients.next().unwrap_or(&0)) * grain[at];
                }
            }
            let at = y * GRAIN_WIDTH + x;
            grain[at] = (grain[at] + round2(sum, aom.ar_coeff_shift)).clamp(min, max);
        }
    }
    grain
}

fn chroma_template(aom: &AomParams, seed: u32, depth: u32, plane: usize, luma: &[i32]) -> Vec<i32> {
    let shift = 12 - depth as i32 + aom.grain_scale_shift;
    let center = 128 << (depth - 8);
    let (min, max) = (-center, center - 1);
    let mut state = seed ^ if plane == 0 { 0xb524 } else { 0x49d8 };
    let mut grain: Vec<i32> = (0..SUB_GRAIN_WIDTH * SUB_GRAIN_HEIGHT)
        .map(|_| round2(i32::from(GAUSSIAN[random(11, &mut state) as usize]), shift))
        .collect();
    let lag = aom.ar_coeff_lag;
    for y in 3..SUB_GRAIN_HEIGHT {
        for x in 3..SUB_GRAIN_WIDTH - 3 {
            let mut coefficients = aom.ar_coeffs_uv[plane].iter();
            let mut sum = 0;
            'taps: for dy in -lag..=0 {
                for dx in -lag..=lag {
                    let coefficient = i32::from(*coefficients.next().unwrap_or(&0));
                    if dx == 0 && dy == 0 {
                        // The last tap weighs the luma grain under this pixel.
                        if aom.num_y_points > 0 {
                            let (luma_x, luma_y) = ((x - 3) * 2 + 3, (y - 3) * 2 + 3);
                            let total: i32 = [0, 1]
                                .iter()
                                .flat_map(|i| {
                                    [0, 1].map(|j| luma[(luma_y + i) * GRAIN_WIDTH + luma_x + j])
                                })
                                .sum();
                            sum += round2(total, 2) * coefficient;
                        }
                        break 'taps;
                    }
                    let at = (y as i32 + dy) as usize * SUB_GRAIN_WIDTH + (x as i32 + dx) as usize;
                    sum += coefficient * grain[at];
                }
            }
            let at = y * SUB_GRAIN_WIDTH + x;
            grain[at] = (grain[at] + round2(sum, aom.ar_coeff_shift)).clamp(min, max);
        }
    }
    grain
}

/// Scaling curve through `points` (value, scaling), with an entry for every
/// pixel value at `depth`, interpolated as dav1d does.
fn scaling(points: &[[u8; 2]], depth: u32) -> Vec<u8> {
    let size = 1usize << depth;
    let mut scaling = vec![0u8; size];
    let Some(last) = points.last() else {
        return scaling;
    };
    let shift = depth - 8;
    let start = usize::from(points[0][0]) << shift;
    scaling[..start].fill(points[0][1]);
    for pair in points.windows(2) {
        let (begin_x, begin_y) = (i32::from(pair[0][0]), i32::from(pair[0][1]));
        let (end_x, end_y) = (i32::from(pair[1][0]), i32::from(pair[1][1]));
        let dx = end_x - begin_x;
        if dx <= 0 {
            continue;
        }
        let delta = (end_y - begin_y) * ((0x10000 + (dx >> 1)) / dx);
        let mut d = 0x8000;
        for x in 0..dx {
            scaling[((begin_x + x) as usize) << shift] = (begin_y + (d >> 16)) as u8;
            d += delta;
        }
    }
    let end = usize::from(last[0]) << shift;
    scaling[end..].fill(last[1]);
    if shift > 0 {
        let pad = 1usize << shift;
        let rounding = (pad >> 1) as i32;
        for pair in points.windows(2) {
            let begin = usize::from(pair[0][0]) << shift;
            let end = usize::from(pair[1][0]) << shift;
            for x in (0..end.saturating_sub(begin)).step_by(pad) {
                let base = i32::from(scaling[begin + x]);
                let range = i32::from(scaling[begin + x + pad]) - base;
                let mut r = rounding;
                for n in 1..pad {
                    r += range;
                    scaling[begin + x + n] = (base + (r >> shift)) as u8;
                }
            }
        }
    }
    scaling
}

/// The specification's Gaussian sequence.
#[rustfmt::skip]
const GAUSSIAN: [i16; 2048] = [
    56, 568, -180, 172, 124, -84, 172, -64, -900, 24, 820, 224, 1248, 996, 272, -8,
    -916, -388, -732, -104, -188, 800, 112, -652, -320, -376, 140, -252, 492, -168, 44, -788,
    588, -584, 500, -228, 12, 680, 272, -476, 972, -100, 652, 368, 432, -196, -720, -192,
    1000, -332, 652, -136, -552, -604, -4, 192, -220, -136, 1000, -52, 372, -96, -624, 124,
    -24, 396, 540, -12, -104, 640, 464, 244, -208, -84, 368, -528, -740, 248, -968, -848,
    608, 376, -60, -292, -40, -156, 252, -292, 248, 224, -280, 400, -244, 244, -60, 76,
    -80, 212, 532, 340, 128, -36, 824, -352, -60, -264, -96, -612, 416, -704, 220, -204,
    640, -160, 1220, -408, 900, 336, 20, -336, -96, -792, 304, 48, -28, -1232, -1172, -448,
    104, -292, -520, 244, 60, -948, 0, -708, 268, 108, 356, -548, 488, -344, -136, 488,
    -196, -224, 656, -236, -1128, 60, 4, 140, 276, -676, -376, 168, -108, 464, 8, 564,
    64, 240, 308, -300, -400, -456, -136, 56, 120, -408, -116, 436, 504, -232, 328, 844,
    -164, -84, 784, -168, 232, -224, 348, -376, 128, 568, 96, -1244, -288, 276, 848, 832,
    -360, 656, 464, -384, -332, -356, 728, -388, 160, -192, 468, 296, 224, 140, -776, -100,
    280, 4, 196, 44, -36, -648, 932, 16, 1428, 28, 528, 808, 772, 20, 268, 88,
    -332, -284, 124, -384, -448, 208, -228, -1044, -328, 660, 380, -148, -300, 588, 240, 540,
    28, 136, -88, -436, 256, 296, -1000, 1400, 0, -48, 1056, -136, 264, -528, -1108, 632,
    -484, -592, -344, 796, 124, -668, -768, 388, 1296, -232, -188, -200, -288, -4, 308, 100,
    -168, 256, -500, 204, -508, 648, -136, 372, -272, -120, -1004, -552, -548, -384, 548, -296,
    428, -108, -8, -912, -324, -224, -88, -112, -220, -100, 996, -796, 548, 360, -216, 180,
    428, -200, -212, 148, 96, 148, 284, 216, -412, -320, 120, -300, -384, -604, -572, -332,
    -8, -180, -176, 696, 116, -88, 628, 76, 44, -516, 240, -208, -40, 100, -592, 344,
    -308, -452, -228, 20, 916, -1752, -136, -340, -804, 140, 40, 512, 340, 248, 184, -492,
    896, -156, 932, -628, 328, -688, -448, -616, -752, -100, 560, -1020, 180, -800, -64, 76,
    576, 1068, 396, 660, 552, -108, -28, 320, -628, 312, -92, -92, -472, 268, 16, 560,
    516, -672, -52, 492, -100, 260, 384, 284, 292, 304, -148, 88, -152, 1012, 1064, -228,
    164, -376, -684, 592, -392, 156, 196, -524, -64, -884, 160, -176, 636, 648, 404, -396,
    -436, 864, 424, -728, 988, -604, 904, -592, 296, -224, 536, -176, -920, 436, -48, 1176,
    -884, 416, -776, -824, -884, 524, -548, -564, -68, -164, -96, 692, 364, -692, -1012, -68,
    260, -480, 876, -1116, 452, -332, -352, 892, -1088, 1220, -676, 12, -292, 244, 496, 372,
    -32, 280, 200, 112, -440, -96, 24, -644, -184, 56, -432, 224, -980, 272, -260, 144,
    -436, 420, 356, 364, -528, 76, 172, -744, -368, 404, -752, -416, 684, -688, 72, 540,
    416, 92, 444, 480, -72, -1416, 164, -1172, -68, 24, 424, 264, 1040, 128, -912, -524,
    -356, 64, 876, -12, 4, -88, 532, 272, -524, 320, 276, -508, 940, 24, -400, -120,
    756, 60, 236, -412, 100, 376, -484, 400, -100, -740, -108, -260, 328, -268, 224, -200,
    -416, 184, -604, -564, -20, 296, 60, 892, -888, 60, 164, 68, -760, 216, -296, 904,
    -336, -28, 404, -356, -568, -208, -1480, -512, 296, 328, -360, -164, -1560, -776, 1156, -428,
    164, -504, -112, 120, -216, -148, -264, 308, 32, 64, -72, 72, 116, 176, -64, -272,
    460, -536, -784, -280, 348, 108, -752, -132, 524, -540, -776, 116, -296, -1196, -288, -560,
    1040, -472, 116, -848, -1116, 116, 636, 696, 284, -176, 1016, 204, -864, -648, -248, 356,
    972, -584, -204, 264, 880, 528, -24, -184, 116, 448, -144, 828, 524, 212, -212, 52,
    12, 200, 268, -488, -404, -880, 824, -672, -40, 908, -248, 500, 716, -576, 492, -576,
    16, 720, -108, 384, 124, 344, 280, 576, -500, 252, 104, -308, 196, -188, -8, 1268,
    296, 1032, -1196, 436, 316, 372, -432, -200, -660, 704, -224, 596, -132, 268, 32, -452,
    884, 104, -1008, 424, -1348, -280, 4, -1168, 368, 476, 696, 300, -8, 24, 180, -592,
    -196, 388, 304, 500, 724, -160, 244, -84, 272, -256, -420, 320, 208, -144, -156, 156,
    364, 452, 28, 540, 316, 220, -644, -248, 464, 72, 360, 32, -388, 496, -680, -48,
    208, -116, -408, 60, -604, -392, 548, -840, 784, -460, 656, -544, -388, -264, 908, -800,
    -628, -612, -568, 572, -220, 164, 288, -16, -308, 308, -112, -636, -760, 280, -668, 432,
    364, 240, -196, 604, 340, 384, 196, 592, -44, -500, 432, -580, -132, 636, -76, 392,
    4, -412, 540, 508, 328, -356, -36, 16, -220, -64, -248, -60, 24, -192, 368, 1040,
    92, -24, -1044, -32, 40, 104, 148, 192, -136, -520, 56, -816, -224, 732, 392, 356,
    212, -80, -424, -1008, -324, 588, -1496, 576, 460, -816, -848, 56, -580, -92, -1372, -112,
    -496, 200, 364, 52, -140, 48, -48, -60, 84, 72, 40, 132, -356, -268, -104, -284,
    -404, 732, -520, 164, -304, -540, 120, 328, -76, -460, 756, 388, 588, 236, -436, -72,
    -176, -404, -316, -148, 716, -604, 404, -72, -88, -888, -68, 944, 88, -220, -344, 960,
    472, 460, -232, 704, 120, 832, -228, 692, -508, 132, -476, 844, -748, -364, -44, 1116,
    -1104, -1056, 76, 428, 552, -692, 60, 356, 96, -384, -188, -612, -576, 736, 508, 892,
    352, -1132, 504, -24, -352, 324, 332, -600, -312, 292, 508, -144, -8, 484, 48, 284,
    -260, -240, 256, -100, -292, -204, -44, 472, -204, 908, -188, -1000, -256, 92, 1164, -392,
    564, 356, 652, -28, -884, 256, 484, -192, 760, -176, 376, -524, -452, -436, 860, -736,
    212, 124, 504, -476, 468, 76, -472, 552, -692, -944, -620, 740, -240, 400, 132, 20,
    192, -196, 264, -668, -1012, -60, 296, -316, -828, 76, -156, 284, -768, -448, -832, 148,
    248, 652, 616, 1236, 288, -328, -400, -124, 588, 220, 520, -696, 1032, 768, -740, -92,
    -272, 296, 448, -464, 412, -200, 392, 440, -200, 264, -152, -260, 320, 1032, 216, 320,
    -8, -64, 156, -1016, 1084, 1172, 536, 484, -432, 132, 372, -52, -256, 84, 116, -352,
    48, 116, 304, -384, 412, 924, -300, 528, 628, 180, 648, 44, -980, -220, 1320, 48,
    332, 748, 524, -268, -720, 540, -276, 564, -344, -208, -196, 436, 896, 88, -392, 132,
    80, -964, -288, 568, 56, -48, -456, 888, 8, 552, -156, -292, 948, 288, 128, -716,
    -292, 1192, -152, 876, 352, -600, -260, -812, -468, -28, -120, -32, -44, 1284, 496, 192,
    464, 312, -76, -516, -380, -456, -1012, -48, 308, -156, 36, 492, -156, -808, 188, 1652,
    68, -120, -116, 316, 160, -140, 352, 808, -416, 592, 316, -480, 56, 528, -204, -568,
    372, -232, 752, -344, 744, -4, 324, -416, -600, 768, 268, -248, -88, -132, -420, -432,
    80, -288, 404, -316, -1216, -588, 520, -108, 92, -320, 368, -480, -216, -92, 1688, -300,
    180, 1020, -176, 820, -68, -228, -260, 436, -904, 20, 40, -508, 440, -736, 312, 332,
    204, 760, -372, 728, 96, -20, -632, -520, -560, 336, 1076, -64, -532, 776, 584, 192,
    396, -728, -520, 276, -188, 80, -52, -612, -252, -48, 648, 212, -688, 228, -52, -260,
    428, -412, -272, -404, 180, 816, -796, 48, 152, 484, -88, -216, 988, 696, 188, -528,
    648, -116, -180, 316, 476, 12, -564, 96, 476, -252, -364, -376, -392, 556, -256, -576,
    260, -352, 120, -16, -136, -260, -492, 72, 556, 660, 580, 616, 772, 436, 424, -32,
    -324, -1268, 416, -324, -80, 920, 160, 228, 724, 32, -516, 64, 384, 68, -128, 136,
    240, 248, -204, -68, 252, -932, -120, -480, -628, -84, 192, 852, -404, -288, -132, 204,
    100, 168, -68, -196, -868, 460, 1080, 380, -80, 244, 0, 484, -888, 64, 184, 352,
    600, 460, 164, 604, -196, 320, -64, 588, -184, 228, 12, 372, 48, -848, -344, 224,
    208, -200, 484, 128, -20, 272, -468, -840, 384, 256, -720, -520, -464, -580, 112, -120,
    644, -356, -208, -608, -528, 704, 560, -424, 392, 828, 40, 84, 200, -152, 0, -144,
    584, 280, -120, 80, -556, -972, -196, -472, 724, 80, 168, -32, 88, 160, -688, 0,
    160, 356, 372, -776, 740, -128, 676, -248, -480, 4, -364, 96, 544, 232, -1032, 956,
    236, 356, 20, -40, 300, 24, -676, -596, 132, 1120, -104, 532, -1096, 568, 648, 444,
    508, 380, 188, -376, -604, 1488, 424, 24, 756, -220, -192, 716, 120, 920, 688, 168,
    44, -460, 568, 284, 1144, 1160, 600, 424, 888, 656, -356, -320, 220, 316, -176, -724,
    -188, -816, -628, -348, -228, -380, 1012, -452, -660, 736, 928, 404, -696, -72, -268, -892,
    128, 184, -344, -780, 360, 336, 400, 344, 428, 548, -112, 136, -228, -216, -820, -516,
    340, 92, -136, 116, -300, 376, -244, 100, -316, -520, -284, -12, 824, 164, -548, -180,
    -128, 116, -924, -828, 268, -368, -580, 620, 192, 160, 0, -1676, 1068, 424, -56, -360,
    468, -156, 720, 288, -528, 556, -364, 548, -148, 504, 316, 152, -648, -620, -684, -24,
    -376, -384, -108, -920, -1032, 768, 180, -264, -508, -1268, -260, -60, 300, -240, 988, 724,
    -376, -576, -212, -736, 556, 192, 1092, -620, -880, 376, -56, -4, -216, -32, 836, 268,
    396, 1332, 864, -600, 100, 56, -412, -92, 356, 180, 884, -468, -436, 292, -388, -804,
    -704, -840, 368, -348, 140, -724, 1536, 940, 372, 112, -372, 436, -480, 1136, 296, -32,
    -228, 132, -48, -220, 868, -1016, -60, -1044, -464, 328, 916, 244, 12, -736, -296, 360,
    468, -376, -108, -92, 788, 368, -56, 544, 400, -672, -420, 728, 16, 320, 44, -284,
    -380, -796, 488, 132, 204, -596, -372, 88, -152, -908, -636, -572, -624, -116, -692, -200,
    -56, 276, -88, 484, -324, 948, 864, 1000, -456, -184, -276, 292, -296, 156, 676, 320,
    160, 908, -84, -1236, -288, -116, 260, -372, -644, 732, -756, -96, 84, 344, -520, 348,
    -688, 240, -84, 216, -1044, -136, -676, -396, -1500, 960, -40, 176, 168, 1516, 420, -504,
    -344, -364, -360, 1216, -940, -380, -212, 252, -660, -708, 484, -444, -152, 928, -120, 1112,
    476, -260, 560, -148, -344, 108, -196, 228, -288, 504, 560, -328, -88, 288, -1008, 460,
    -228, 468, -836, -196, 76, 388, 232, 412, -1168, -716, -644, 756, -172, -356, -504, 116,
    432, 528, 48, 476, -168, -608, 448, 160, -532, -272, 28, -676, -12, 828, 980, 456,
    520, 104, -104, 256, -344, -4, -28, -368, -52, -524, -572, -556, -200, 768, 1124, -208,
    -512, 176, 232, 248, -148, -888, 604, -600, -304, 804, -156, -212, 488, -192, -804, -256,
    368, -360, -916, -328, 228, -240, -448, -472, 856, -556, -364, 572, -12, -156, -368, -340,
    432, 252, -752, -152, 288, 268, -580, -848, -592, 108, -76, 244, 312, -716, 592, -80,
    436, 360, 4, -248, 160, 516, 584, 732, 44, -468, -280, -292, -156, -588, 28, 308,
    912, 24, 124, 156, 180, -252, 944, -924, -772, -520, -428, -624, 300, -212, -1144, 32,
    -724, 800, -1128, -212, -1288, -848, 180, -416, 440, 192, -576, -792, -76, -1080, 80, -532,
    -352, -132, 380, -820, 148, 1112, 128, 164, 456, 700, -924, 144, -668, -384, 648, -832,
    508, 552, -52, -100, -656, 208, -568, 748, -88, 680, 232, 300, 192, -408, -1012, -152,
    -252, -268, 272, -876, -664, -648, -332, -136, 16, 12, 1152, -28, 332, -536, 320, -672,
    -460, -316, 532, -260, 228, -40, 1052, -816, 180, 88, -496, -556, -672, -368, 428, 92,
    356, 404, -408, 252, 196, -176, -556, 792, 268, 32, 372, 40, 96, -332, 328, 120,
    372, -900, -40, 472, -264, -592, 952, 128, 656, 112, 664, -232, 420, 4, -344, -464,
    556, 244, -416, -32, 252, 0, -412, 188, -696, 508, -476, 324, -1096, 656, -312, 560,
    264, -136, 304, 160, -64, -580, 248, 336, -720, 560, -348, -288, -276, -196, -500, 852,
    -544, -236, -1128, -992, -776, 116, 56, 52, 860, 884, 212, -12, 168, 1020, 512, -552,
    924, -148, 716, 188, 164, -340, -520, -184, 880, -152, -680, -208, -1156, -300, -528, -472,
    364, 100, -744, -1056, -32, 540, 280, 144, -676, -32, -232, -280, -224, 96, 568, -76,
    172, 148, 148, 104, 32, -296, -32, 788, -80, 32, -16, 280, 288, 944, 428, -484,
];

#[cfg(test)]
mod tests {
    use super::Grain;
    use ffmpeg_next as ff;

    /// Every frame of `path` from dav1d, with grain exported instead of
    /// applied when `export` is set.
    fn decode(path: &str, export: bool) -> Vec<ff::frame::Video> {
        let mut input = ff::format::input(path).unwrap();
        let stream = input.streams().best(ff::media::Type::Video).unwrap();
        let index = stream.index();
        let mut context = ff::codec::Context::from_parameters(stream.parameters()).unwrap();
        if export {
            // SAFETY: plain field write before the decoder opens.
            unsafe {
                (*context.as_mut_ptr()).export_side_data |=
                    ff::ffi::AV_CODEC_EXPORT_DATA_FILM_GRAIN;
            }
        }
        let dav1d = ff::decoder::find_by_name("libdav1d").expect("dav1d");
        let mut decoder = context.decoder().open_as(dav1d).unwrap().video().unwrap();
        let mut frames = Vec::new();
        let mut receive = |decoder: &mut ff::decoder::Video| {
            let mut frame = ff::frame::Video::empty();
            while decoder.receive_frame(&mut frame).is_ok() {
                frames.push(std::mem::replace(&mut frame, ff::frame::Video::empty()));
            }
        };
        for (stream, packet) in input.packets() {
            if stream.index() == index {
                decoder.send_packet(&packet).unwrap();
                receive(&mut decoder);
            }
        }
        decoder.send_eof().unwrap();
        receive(&mut decoder);
        frames
    }

    /// The visible bytes of each plane, without row padding.
    fn planes(frame: &ff::frame::Video) -> Vec<Vec<u8>> {
        let bytes = if frame.format() == ff::format::Pixel::YUV420P {
            1
        } else {
            2
        };
        (0..3)
            .map(|plane| {
                let (width, height) = if plane == 0 {
                    (frame.width() as usize, frame.height() as usize)
                } else {
                    (
                        frame.width().div_ceil(2) as usize,
                        frame.height().div_ceil(2) as usize,
                    )
                };
                let stride = frame.stride(plane);
                (0..height)
                    .flat_map(|y| {
                        frame.data(plane)[y * stride..y * stride + width * bytes].to_vec()
                    })
                    .collect()
            })
            .collect()
    }

    /// Our grain matches dav1d's bit for bit: 8 and 10 bits, odd sizes,
    /// overlap, chroma curves and chroma scaled from luma.
    #[test]
    fn grain_matches_dav1d() {
        ff::init().unwrap();
        for path in [
            "testdata/grain/svt8.ivf",
            "testdata/grain/svt10.ivf",
            "testdata/grain/aom10.ivf",
        ] {
            let grained = decode(path, false);
            let mut clean = decode(path, true);
            assert_eq!(grained.len(), clean.len(), "{path}: frame count");
            assert!(!clean.is_empty(), "{path}: no frames");
            for (number, (expected, frame)) in grained.iter().zip(&mut clean).enumerate() {
                let grain = Grain::of(frame).unwrap_or_else(|| panic!("{path}: no grain exported"));
                assert_ne!(
                    planes(expected),
                    planes(frame),
                    "{path}: grain changes nothing"
                );
                assert!(grain.apply_to(frame), "{path}: format {:?}", frame.format());
                for (plane, (want, got)) in planes(expected).iter().zip(planes(frame)).enumerate() {
                    let differences = want.iter().zip(&got).filter(|(a, b)| a != b).count();
                    assert_eq!(differences, 0, "{path} frame {number} plane {plane}");
                }
            }
        }
    }
}
