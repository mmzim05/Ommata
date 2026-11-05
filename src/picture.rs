//! Still images: background decode with EXIF orientation applied, plus
//! non-destructive rotate/flip/crop that is only baked into pixels on save.

use eframe::egui::{Color32, ColorImage, Pos2, Rect, pos2};
use image::{DynamicImage, ImageDecoder, ImageReader};
use std::path::Path;

/// A still image decoded for display.
pub struct Decoded {
    pub image: ColorImage,
    /// Pixel size of the oriented original (`image` may be downscaled).
    pub size: [usize; 2],
}

/// Oriented pixels plus the raw EXIF and ICC chunks to carry over on save.
type Read = (DynamicImage, Option<Vec<u8>>, Option<Vec<u8>>);

/// Whether `path` is a camera RAW file.
pub fn is_raw(path: &Path) -> bool {
    crate::library::RAW_EXT.contains(&ext(path).as_str())
}

/// Display develops of RAW files without an embedded preview run at half
/// size when that still leaves this many pixels on the long side.
const HALF_DEVELOP_MIN_SIDE: usize = 3000;

/// Camera RAW. For display, the camera's embedded JPEG preview is used: it
/// decodes in milliseconds instead of a full demosaic. Without one (Olympus,
/// Samsung, Phase One...), display develops at half size. `full` develops
/// all the sensor data (for saving).
fn read_raw(path: &Path, full: bool) -> Result<Read, String> {
    use rawler::decoders::RawDecodeParams;
    let to_text = |error: rawler::RawlerError| error.to_string();
    let src = rawler::rawsource::RawSource::new(path).map_err(|error| error.to_string())?;
    let decoder = rawler::get_decoder(&src).map_err(to_text)?;
    let params = RawDecodeParams::default();
    let embedded = || {
        let image = decoder
            .full_image(&src, &params)
            .ok()
            .flatten()
            .or_else(|| decoder.preview_image(&src, &params).ok().flatten())?;
        // DNGs carry at most a 1024 px preview (omalens' own saves among
        // them): too soft to view, so those develop instead.
        let long = image.width().max(image.height());
        (ext(path) != "dng" || long > 1024).then_some(image)
    };
    let develop = |half: bool| -> Result<DynamicImage, String> {
        let raw = decoder.raw_image(&src, &params, false).map_err(to_text)?;
        let developed = if half && can_develop_half(&raw) {
            develop_half(raw)
        } else {
            rawler::imgop::develop::RawDevelop::default().develop_intermediate(&raw)
        };
        developed
            .map_err(to_text)?
            .to_dynamic_image()
            .ok_or_else(|| "unsupported RAW layout".to_string())
    };
    let mut picture = match (full, embedded()) {
        (false, Some(preview)) => preview,
        (false, None) => develop(true)?,
        (true, _) => develop(false)?,
    };
    let orientation = decoder
        .raw_metadata(&src, &params)
        .ok()
        .and_then(|metadata| metadata.exif.orientation)
        .and_then(|exif| image::metadata::Orientation::from_exif(exif as u8));
    if let Some(orientation) = orientation {
        picture.apply_orientation(orientation);
    }
    Ok((picture, None, None))
}

/// Whether `raw` is a Bayer RGB mosaic big enough to develop at half size.
fn can_develop_half(raw: &rawler::RawImage) -> bool {
    use rawler::imgop::sensor::SensorType;
    use rawler::rawimage::RawPhotometricInterpretation;
    let RawPhotometricInterpretation::Cfa(config) = &raw.photometric else {
        return false;
    };
    raw.cpp == 1
        && config.cfa.is_rgb()
        && config.sensor == SensorType::Bayer
        && raw.fuji_rotation_width.is_none()
        && raw.width.max(raw.height) / 2 >= HALF_DEVELOP_MIN_SIDE
}

/// Develop at half size: each 2x2 Bayer cell becomes one pixel (rawler's
/// superpixel demosaic, a quarter of the work of a full demosaic), then the
/// usual white balance, colour calibration, crop and sRGB steps.
fn develop_half(mut raw: rawler::RawImage) -> rawler::Result<rawler::imgop::develop::Intermediate> {
    use rawler::imgop::develop::{ProcessingStep, RawDevelop};
    use rawler::imgop::sensor::Demosaic;
    use rawler::imgop::sensor::bayer::superpixel::Superpixel3Channel;
    use rawler::rawimage::RawPhotometricInterpretation;
    raw.apply_scaling()?;
    let RawPhotometricInterpretation::Cfa(config) = raw.photometric.clone() else {
        return RawDevelop::default().develop_intermediate(&raw);
    };
    let pixels =
        rawler::pixarray::PixF32::new_with(raw.data.as_f32().into_owned(), raw.width, raw.height);
    let roi = raw.active_area.unwrap_or(pixels.rect());
    let rgb = Superpixel3Channel::new().demosaic(&pixels, &config.cfa, &config.colors, roi);
    // The default crop is relative to the full sensor: move it into the
    // active area, then halve it like the image.
    let crop = raw.crop_area.map(|crop| {
        let mut crop = match &raw.active_area {
            Some(active) => crop.adapt(active),
            None => crop,
        };
        crop.scale(0.5);
        crop
    });
    raw.width = rgb.width;
    raw.height = rgb.height;
    raw.cpp = 3;
    raw.data = rawler::RawImageData::Float(rgb.data.into_iter().flatten().collect());
    raw.photometric = RawPhotometricInterpretation::LinearRaw;
    raw.active_area = None;
    raw.crop_area = crop;
    RawDevelop::new_with(&[
        ProcessingStep::WhiteBalance,
        ProcessingStep::Calibrate,
        ProcessingStep::CropDefault,
        ProcessingStep::SRgb,
    ])
    .develop_intermediate(&raw)
}

fn read(path: &Path, full: bool) -> Result<Read, String> {
    if is_raw(path) {
        return read_raw(path, full);
    }
    let reader = ImageReader::open(path)
        .and_then(|reader| reader.with_guessed_format())
        .map_err(|error| error.to_string())?;
    let mut decoder = reader.into_decoder().map_err(|error| error.to_string())?;
    let orientation = decoder
        .orientation()
        .unwrap_or(image::metadata::Orientation::NoTransforms);
    let exif = decoder.exif_metadata().ok().flatten();
    let icc = decoder.icc_profile().ok().flatten();
    let mut picture = DynamicImage::from_decoder(decoder).map_err(|error| error.to_string())?;
    picture.apply_orientation(orientation);
    Ok((picture, exif, icc))
}

/// Decode for display, downscaled to fit `max_side` (the GPU texture limit).
pub fn decode(path: &Path, max_side: u32) -> Result<Decoded, String> {
    let (picture, _, _) = read(path, false)?;
    let size = [picture.width() as usize, picture.height() as usize];
    // Over the GPU's texture limit: scale down to fit.
    let picture = shrink(picture, max_side);
    Ok(Decoded {
        image: to_color_image(picture),
        size,
    })
}

/// Scale `picture` down to fit `max_side` on its long side, unchanged if it
/// already fits. Box filter (area average): as good as Triangle on screen,
/// and SIMD over all cores keeps 100 MP images well under a second.
fn shrink(picture: DynamicImage, max_side: u32) -> DynamicImage {
    use fast_image_resize::{FilterType, ResizeAlg, ResizeOptions, Resizer};
    let (width, height) = (picture.width(), picture.height());
    let long = width.max(height);
    if long <= max_side {
        return picture;
    }
    let fit = |side: u32| {
        let side = (u64::from(side) * u64::from(max_side)).div_ceil(u64::from(long));
        (side as u32).max(1)
    };
    let mut small = DynamicImage::new(fit(width), fit(height), picture.color());
    let options = ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Box));
    match Resizer::new().resize(&picture, &mut small, &options) {
        Ok(()) => small,
        // Pixel layouts the resizer lacks.
        Err(_) => picture.thumbnail(max_side, max_side),
    }
}

/// Convert for upload in one pass spread over all cores (opaque RGB, the JPEG
/// and RAW case, needs no premultiply); other layouts go through RGBA.
fn to_color_image(picture: DynamicImage) -> ColorImage {
    let (width, height) = (picture.width() as usize, picture.height() as usize);
    let DynamicImage::ImageRgb8(rgb) = picture else {
        let rgba = picture.into_rgba8();
        return ColorImage::from_rgba_unmultiplied([width, height], &rgba);
    };
    let mut pixels = vec![Color32::BLACK; width * height];
    let threads = std::thread::available_parallelism().map_or(4, |count| count.get());
    let rows = height.div_ceil(threads).max(1);
    std::thread::scope(|scope| {
        for (dst, src) in pixels
            .chunks_mut(rows * width)
            .zip(rgb.as_raw().chunks(rows * width * 3))
        {
            scope.spawn(move || {
                for (pixel, rgb) in dst.iter_mut().zip(src.as_chunks::<3>().0) {
                    *pixel = Color32::from_rgb(rgb[0], rgb[1], rgb[2]);
                }
            });
        }
    });
    ColorImage::new([width, height], pixels)
}

/// File-manager thumbnail: `size` px on the long side, written as PNG. RAW
/// files use the camera's embedded preview, so this stays fast.
pub fn thumbnail(path: &Path, out: &Path, size: u32) -> Result<(), String> {
    let (picture, _, _) = read(path, false)?;
    let picture = shrink(picture, size);
    let picture = if picture.color().has_alpha() {
        DynamicImage::ImageRgba8(picture.into_rgba8())
    } else {
        DynamicImage::ImageRgb8(picture.into_rgb8())
    };
    picture
        .save_with_format(out, image::ImageFormat::Png)
        .map_err(|error| error.to_string())
}

/// Rotation (quarter turns clockwise) then horizontal flip, then crop. The crop
/// rect is normalized to the rotated/flipped image.
#[derive(Clone, Copy, PartialEq, Default, Debug)]
pub struct Edit {
    /// Quarter turns clockwise, 0 to 3.
    pub turns: u8,
    pub flip: bool,
    pub crop: Option<Rect>,
}

impl Edit {
    /// Whether the edit leaves the image unchanged.
    pub fn is_identity(&self) -> bool {
        *self == Self::default()
    }

    /// Rotate a quarter turn, keeping the crop on the same pixels.
    pub fn rotate(&mut self, clockwise: bool) {
        // Flip conjugates rotation: R∘F = F∘R⁻¹.
        let turn_clockwise = clockwise != self.flip;
        self.turns = (self.turns + if turn_clockwise { 1 } else { 3 }) % 4;
        self.crop = self.crop.map(|crop| {
            let (first, second) = if clockwise {
                (
                    pos2(1.0 - crop.max.y, crop.min.x),
                    pos2(1.0 - crop.min.y, crop.max.x),
                )
            } else {
                (
                    pos2(crop.min.y, 1.0 - crop.max.x),
                    pos2(crop.max.y, 1.0 - crop.min.x),
                )
            };
            Rect::from_two_pos(first, second)
        });
    }

    /// Mirror horizontally, keeping the crop on the same pixels.
    pub fn flip_h(&mut self) {
        self.flip = !self.flip;
        self.crop = self.crop.map(|crop| {
            Rect::from_two_pos(
                pos2(1.0 - crop.max.x, crop.min.y),
                pos2(1.0 - crop.min.x, crop.max.y),
            )
        });
    }

    /// Size after rotation, before crop.
    pub fn rotated_size(&self, size: [usize; 2]) -> [usize; 2] {
        if self.turns % 2 == 1 {
            [size[1], size[0]]
        } else {
            size
        }
    }

    /// Size of the final image.
    pub fn output_size(&self, size: [usize; 2]) -> [usize; 2] {
        let rotated = self.rotated_size(size);
        match self.crop {
            Some(crop) => [
                ((crop.width() * rotated[0] as f32).round() as usize).max(1),
                ((crop.height() * rotated[1] as f32).round() as usize).max(1),
            ],
            None => rotated,
        }
    }

    /// Map a normalized point of the edited (rotated/flipped) image to texture UV.
    pub fn to_uv(self, mut point: Pos2) -> Pos2 {
        if self.flip {
            point.x = 1.0 - point.x;
        }
        for _ in 0..self.turns {
            point = pos2(point.y, 1.0 - point.x);
        }
        point
    }

    /// Bake the edit into `picture`'s pixels.
    pub fn apply(&self, mut picture: DynamicImage) -> DynamicImage {
        picture = match self.turns {
            1 => picture.rotate90(),
            2 => picture.rotate180(),
            3 => picture.rotate270(),
            _ => picture,
        };
        if self.flip {
            picture = picture.fliph();
        }
        if let Some(crop) = self.crop {
            let (width, height) = (picture.width() as f32, picture.height() as f32);
            let x = (crop.min.x * width).round().clamp(0.0, width - 1.0) as u32;
            let y = (crop.min.y * height).round().clamp(0.0, height - 1.0) as u32;
            let crop_width = ((crop.width() * width).round() as u32).clamp(1, picture.width() - x);
            let crop_height =
                ((crop.height() * height).round() as u32).clamp(1, picture.height() - y);
            picture = picture.crop_imm(x, y, crop_width, crop_height);
        }
        picture
    }
}

/// Formats we can write back in their own format.
pub fn can_save(path: &Path) -> bool {
    matches!(
        ext(path).as_str(),
        "jpg" | "jpeg" | "jpe" | "png" | "webp" | "tif" | "tiff" | "bmp"
    ) || is_raw(path)
}

fn ext(path: &Path) -> String {
    path.extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
}

/// A quarter-turn rotation then an optional horizontal flip: how both EXIF
/// orientation and an `Edit` turn an image.
#[derive(Clone, Copy, PartialEq, Debug)]
struct Turn {
    /// Quarter turns clockwise, 0 to 3.
    turns: u8,
    flip: bool,
}

impl Turn {
    /// From an EXIF orientation (1 to 8; anything else is upright).
    fn from_exif(code: u16) -> Self {
        let (turns, flip) = match code {
            2 => (0, true),
            3 => (2, false),
            4 => (2, true),
            5 => (1, true),
            6 => (1, false),
            7 => (3, true),
            8 => (3, false),
            _ => (0, false),
        };
        Self { turns, flip }
    }

    fn exif(self) -> u16 {
        match (self.turns % 4, self.flip) {
            (0, false) => 1,
            (0, true) => 2,
            (2, false) => 3,
            (2, true) => 4,
            (1, true) => 5,
            (1, false) => 6,
            (3, true) => 7,
            _ => 8,
        }
    }

    /// `self`, then `next`. A flip reverses the direction of the turns that
    /// follow it: F·Rⁿ = R⁻ⁿ·F.
    fn then(self, next: Self) -> Self {
        let next_turns = if self.flip {
            (4 - next.turns % 4) % 4
        } else {
            next.turns % 4
        };
        Self {
            turns: (self.turns + next_turns) % 4,
            flip: self.flip != next.flip,
        }
    }

    /// Where a normalized point of the turned image lies in the original.
    fn unmap(self, point: Pos2) -> Pos2 {
        let mut point = if self.flip {
            pos2(1.0 - point.x, point.y)
        } else {
            point
        };
        for _ in 0..self.turns % 4 {
            // Undo a clockwise quarter turn.
            point = pos2(point.y, 1.0 - point.x);
        }
        point
    }
}

/// Save a camera RAW as a DNG: the sensor data untouched, the crop written
/// as the DNG default crop and the rotation as its orientation, which RAW
/// editors apply. Its preview is developed from the cropped RAW.
fn save_dng(src: &Path, dst: &Path, edit: &Edit) -> Result<(), String> {
    use rawler::decoders::{RawDecodeParams, WellKnownIFD};
    use rawler::dng::writer::DngWriter;
    use rawler::dng::{CropMode, DNG_VERSION_V1_4, DngCompression, DngPhotometricConversion};
    use rawler::formats::tiff::{
        CompressionMethod, PhotometricInterpretation, PreviewColorSpace, Value,
    };
    use rawler::imgop::{Dim2, Point, Rect as RawRect};
    use rawler::tags::{DngTag, ExifTag, TiffCommonTag};
    fn text(error: impl std::fmt::Display) -> String {
        error.to_string()
    }
    let source = rawler::rawsource::RawSource::new(src).map_err(text)?;
    let decoder = rawler::get_decoder(&source).map_err(text)?;
    let params = RawDecodeParams::default();
    let mut raw = decoder.raw_image(&source, &params, false).map_err(text)?;
    if raw.fuji_rotation_width.is_some() {
        return Err("RAW files from this camera can't be saved as DNG yet".into());
    }
    let metadata = decoder.raw_metadata(&source, &params).map_err(text)?;
    let camera = Turn::from_exif(metadata.exif.orientation.unwrap_or(raw.orientation.to_u16()));
    let turn = camera.then(Turn {
        turns: edit.turns,
        flip: edit.flip,
    });
    // The crop is normalized to the image as shown (turned); map its corners
    // back to the sensor, normalized to the camera's default crop.
    let sensor_crop = edit.crop.map(|crop| {
        let (a, b) = (turn.unmap(crop.min), turn.unmap(crop.max));
        (a.min(b), a.max(b))
    });
    if let Some((min, max)) = sensor_crop {
        let full = RawRect::new(Point::new(0, 0), Dim2::new(raw.width, raw.height));
        let area = raw.crop_area.or(raw.active_area).unwrap_or(full);
        let scale = |at: f32, size: usize| (at.clamp(0.0, 1.0) * size as f32).round() as usize;
        let x = scale(min.x, area.d.w).min(area.d.w - 1);
        let y = scale(min.y, area.d.h).min(area.d.h - 1);
        let width = (scale(max.x, area.d.w) - x).clamp(1, area.d.w - x);
        let height = (scale(max.y, area.d.h) - y).clamp(1, area.d.h - y);
        raw.crop_area = Some(RawRect::new(
            Point::new(area.p.x + x, area.p.y + y),
            Dim2::new(width, height),
        ));
    }
    // The preview is the camera's own full-size JPEG, cropped like the RAW,
    // so viewers that show it (omalens among them) keep the camera's colour
    // and tone; a plain develop only when the camera embedded none.
    let camera = decoder
        .full_image(&source, &params)
        .ok()
        .flatten()
        .or_else(|| decoder.preview_image(&source, &params).ok().flatten())
        .filter(|image| image.width().max(image.height()) > 1024);
    let preview = match camera {
        Some(image) => match sensor_crop {
            Some((min, max)) => {
                let (width, height) = (image.width() as f32, image.height() as f32);
                let x = (min.x * width).round().clamp(0.0, width - 1.0) as u32;
                let y = (min.y * height).round().clamp(0.0, height - 1.0) as u32;
                let crop_width =
                    (((max.x - min.x) * width).round() as u32).clamp(1, image.width() - x);
                let crop_height =
                    (((max.y - min.y) * height).round() as u32).clamp(1, image.height() - y);
                image.crop_imm(x, y, crop_width, crop_height)
            }
            None => image,
        },
        None => rawler::imgop::develop::RawDevelop::default()
            .develop_intermediate(&raw)
            .map_err(text)?
            .to_dynamic_image()
            .ok_or("unsupported RAW layout")?,
    };
    let preview = DynamicImage::ImageRgb8(preview.into_rgb8());
    let mut jpeg = Vec::new();
    preview
        .write_with_encoder(image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 92))
        .map_err(text)?;

    let file = std::fs::File::create(dst).map_err(text)?;
    let mut dng = DngWriter::new(std::io::BufWriter::new(file), DNG_VERSION_V1_4).map_err(text)?;
    let mut image = dng.subframe(0);
    image
        .raw_image(
            &raw,
            CropMode::Best,
            DngCompression::Lossless,
            DngPhotometricConversion::Original,
            1,
        )
        .map_err(text)?;
    // A DNG source's own raw tags, except those just written (the crop).
    if let Some(tags) = decoder.ifd(WellKnownIFD::VirtualDngRawTags).map_err(text)? {
        image.ifd_mut().copy(tags.value_iter());
    }
    image.finalize().map_err(text)?;
    // rawler's own preview writer shrinks to 1024 px; this one keeps the
    // full size, written as the DNG spec's JPEG preview IFD.
    let offset = dng.dng.write_data(&jpeg).map_err(text)?;
    let mut full = dng.subframe(1);
    let ifd = full.ifd_mut();
    ifd.add_tag(TiffCommonTag::ImageWidth, Value::long(preview.width()));
    ifd.add_tag(TiffCommonTag::ImageLength, Value::long(preview.height()));
    ifd.add_tag(TiffCommonTag::Compression, CompressionMethod::ModernJPEG);
    ifd.add_tag(TiffCommonTag::BitsPerSample, [8_u16, 8, 8]);
    ifd.add_tag(TiffCommonTag::SampleFormat, [1_u16, 1, 1]);
    ifd.add_tag(TiffCommonTag::PhotometricInt, PhotometricInterpretation::YCbCr);
    ifd.add_tag(TiffCommonTag::RowsPerStrip, Value::long(preview.height()));
    ifd.add_tag(TiffCommonTag::SamplesPerPixel, 3_u16);
    ifd.add_tag(DngTag::PreviewColorSpace, PreviewColorSpace::SRgb);
    ifd.add_value(TiffCommonTag::StripOffsets, Value::Long(vec![offset]));
    ifd.add_tag(TiffCommonTag::StripByteCounts, Value::Long(vec![jpeg.len() as u32]));
    full.finalize().map_err(text)?;
    dng.thumbnail(&shrink(preview, 512)).map_err(text)?;
    dng.load_base_tags(&raw).map_err(text)?;
    dng.load_metadata(&metadata).map_err(text)?;
    dng.root_ifd_mut().add_tag(ExifTag::Orientation, turn.exif());
    if let Some(tags) = decoder.ifd(WellKnownIFD::VirtualDngRootTags).map_err(text)? {
        dng.root_ifd_mut().copy(tags.value_iter());
    }
    if let Some(xmp) = decoder.xpacket(&source, &params).map_err(text)? {
        dng.xpacket(&xmp).map_err(text)?;
    }
    dng.root_ifd_mut()
        .add_tag(TiffCommonTag::Software, format!("omalens {}", env!("CARGO_PKG_VERSION")));
    // Maker notes are copied only when the camera marks them safe to move.
    let safe = decoder
        .ifd(WellKnownIFD::VirtualDngRootTags)
        .map_err(text)?
        .and_then(|tags| tags.get_entry(DngTag::MakerNoteSafety).cloned())
        .is_some_and(|entry| {
            matches!(entry.value, rawler::formats::tiff::Value::Short(v) if v.first() == Some(&1))
        });
    if !safe {
        dng.exif_ifd_mut().remove_tag(ExifTag::MakerNotes);
    }
    dng.close().map_err(text)
}

/// Re-decode the full-resolution original, apply `edit`, and encode to `dst`
/// in the source's format, keeping EXIF (orientation reset, since it is baked
/// into the pixels now) and the ICC profile. Camera RAW saves as DNG.
pub fn save(src: &Path, dst: &Path, edit: &Edit) -> Result<(), String> {
    if is_raw(src) {
        return save_dng(src, dst, edit);
    }
    let (picture, mut exif, icc) = read(src, true)?;
    let picture = edit.apply(picture);
    if let Some(chunk) = &mut exif {
        // A chunk without an orientation tag needs no change.
        let _ = image::metadata::Orientation::remove_from_exif_chunk(chunk);
    }
    let file = std::fs::File::create(dst).map_err(|error| error.to_string())?;
    let mut writer = std::io::BufWriter::new(file);
    let format = ext(src);
    let result = match format.as_str() {
        "jpg" | "jpeg" | "jpe" => {
            let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut writer, 95);
            add_metadata(&mut encoder, exif, icc);
            let picture = if picture.color().has_alpha() || picture.color().bytes_per_pixel() > 3 {
                DynamicImage::ImageRgb8(picture.into_rgb8())
            } else {
                picture
            };
            picture.write_with_encoder(encoder)
        }
        "png" => {
            let mut encoder = image::codecs::png::PngEncoder::new(&mut writer);
            add_metadata(&mut encoder, exif, icc);
            picture.write_with_encoder(encoder)
        }
        "webp" => {
            let mut encoder = image::codecs::webp::WebPEncoder::new_lossless(&mut writer);
            add_metadata(&mut encoder, exif, icc);
            let picture = if picture.color().has_alpha() {
                DynamicImage::ImageRgba8(picture.into_rgba8())
            } else {
                DynamicImage::ImageRgb8(picture.into_rgb8())
            };
            picture.write_with_encoder(encoder)
        }
        "tif" | "tiff" => picture.write_to(&mut writer, image::ImageFormat::Tiff),
        "bmp" => picture.write_to(&mut writer, image::ImageFormat::Bmp),
        other => return Err(format!("can't save .{other} files")),
    };
    result.map_err(|error| error.to_string())?;
    let file = writer.into_inner().map_err(|error| error.to_string())?;
    file.sync_all().map_err(|error| error.to_string())
}

/// Carry EXIF and ICC over where the encoder supports them; an encoder that
/// doesn't still writes the pixels.
fn add_metadata(
    encoder: &mut impl image::ImageEncoder,
    exif: Option<Vec<u8>>,
    icc: Option<Vec<u8>>,
) {
    if let Some(chunk) = exif {
        let _ = encoder.set_exif_metadata(chunk);
    }
    if let Some(profile) = icc {
        let _ = encoder.set_icc_profile(profile);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotate_four_times_is_identity() {
        let mut edit = Edit {
            crop: Some(Rect::from_min_max(pos2(0.1, 0.2), pos2(0.5, 0.9))),
            ..Default::default()
        };
        let original = edit;
        for _ in 0..4 {
            edit.rotate(true);
        }
        assert_eq!(edit.turns, 0);
        let crop = edit.crop.unwrap();
        assert!((crop.min - original.crop.unwrap().min).length() < 1e-6);
    }

    #[test]
    fn uv_matches_pixels() {
        // 3x2 image with distinct pixels; the edited image's pixel at each
        // normalized position must equal the source pixel at to_uv().
        let src = image::RgbImage::from_fn(3, 2, |x, y| image::Rgb([x as u8, y as u8, 0]));
        for turns in 0..4 {
            for flip in [false, true] {
                let edit = Edit {
                    turns,
                    flip,
                    crop: None,
                };
                let out = edit.apply(DynamicImage::ImageRgb8(src.clone())).into_rgb8();
                for (x, y, px) in out.enumerate_pixels() {
                    let point = pos2(
                        (x as f32 + 0.5) / out.width() as f32,
                        (y as f32 + 0.5) / out.height() as f32,
                    );
                    let uv = edit.to_uv(point);
                    let sx = (uv.x * 3.0) as u8;
                    let sy = (uv.y * 2.0) as u8;
                    assert_eq!(px.0, [sx, sy, 0], "turns {turns} flip {flip} at {x},{y}");
                }
            }
        }
    }

    #[test]
    fn rotate_with_flip_matches_pixels() {
        let src = image::RgbImage::from_fn(3, 2, |x, y| image::Rgb([x as u8, y as u8, 0]));
        let mut edit = Edit::default();
        edit.flip_h();
        edit.rotate(true);
        // Expected: flip the source, then rotate the result clockwise.
        let expect = DynamicImage::ImageRgb8(src.clone())
            .fliph()
            .rotate90()
            .into_rgb8();
        assert_eq!(edit.apply(DynamicImage::ImageRgb8(src)).into_rgb8(), expect);
    }
}

#[cfg(test)]
mod turn_tests {
    use super::*;

    /// A small image whose pixels are all different.
    fn grid() -> DynamicImage {
        DynamicImage::ImageRgb8(image::RgbImage::from_fn(3, 2, |x, y| {
            image::Rgb([x as u8 * 40, y as u8 * 90, 7])
        }))
    }

    fn turned(picture: DynamicImage, turn: Turn) -> DynamicImage {
        Edit {
            turns: turn.turns,
            flip: turn.flip,
            crop: None,
        }
        .apply(picture)
    }

    #[test]
    fn exif_codes_match_image_orientation() {
        for code in 1..=8 {
            let mut expect = grid();
            expect.apply_orientation(image::metadata::Orientation::from_exif(code as u8).unwrap());
            assert_eq!(turned(grid(), Turn::from_exif(code)), expect, "code {code}");
            assert_eq!(Turn::from_exif(code).exif(), code);
        }
    }

    #[test]
    fn composing_matches_applying_in_turn() {
        for code in 1..=8 {
            for turns in 0..4 {
                for flip in [false, true] {
                    let (first, next) = (Turn::from_exif(code), Turn { turns, flip });
                    let expect = turned(turned(grid(), first), next);
                    assert_eq!(turned(grid(), first.then(next)), expect, "{code} {turns} {flip}");
                }
            }
        }
    }

    #[test]
    fn unmap_finds_the_same_pixel() {
        let source = grid().into_rgb8();
        for code in 1..=8 {
            let turn = Turn::from_exif(code);
            let shown = turned(grid(), turn).into_rgb8();
            for (x, y, pixel) in shown.enumerate_pixels() {
                let at = pos2(
                    (x as f32 + 0.5) / shown.width() as f32,
                    (y as f32 + 0.5) / shown.height() as f32,
                );
                let back = turn.unmap(at);
                let sx = (back.x * source.width() as f32) as u32;
                let sy = (back.y * source.height() as f32) as u32;
                assert_eq!(source.get_pixel(sx, sy), pixel, "code {code} at {x},{y}");
            }
        }
    }
}

#[cfg(test)]
mod raw_tests {
    /// A cropped, turned DNG develops to the same picture as the edit applied
    /// to the original: OMALENS_TEST_RAW=photo.rw2 cargo test --release dng -- --ignored
    /// (OMALENS_TEST_DNG_OUT=out.dng keeps the DNG to look at.)
    #[test]
    #[ignore]
    fn dng_keeps_crop_and_turn() {
        let path = std::path::PathBuf::from(std::env::var("OMALENS_TEST_RAW").unwrap());
        let edit = super::Edit {
            turns: 1,
            flip: true,
            crop: Some(eframe::egui::Rect::from_min_max(
                eframe::egui::pos2(0.1, 0.2),
                eframe::egui::pos2(0.7, 0.9),
            )),
        };
        let dst = std::env::var("OMALENS_TEST_DNG_OUT")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::env::temp_dir().join("omalens-test.dng"));
        let started = std::time::Instant::now();
        super::save(&path, &dst, &edit).unwrap();
        eprintln!("saved DNG in {:?}", started.elapsed());
        let (saved, _, _) = super::read(&dst, true).unwrap();
        let (original, _, _) = super::read(&path, true).unwrap();
        let expect = edit.apply(original);
        eprintln!(
            "DNG {}x{}, expected {}x{}",
            saved.width(),
            saved.height(),
            expect.width(),
            expect.height()
        );
        assert!(saved.width().abs_diff(expect.width()) <= 2);
        assert!(saved.height().abs_diff(expect.height()) <= 2);
        let width = saved.width().min(expect.width());
        let height = saved.height().min(expect.height());
        let (a, b) = (saved.into_rgb8(), expect.into_rgb8());
        let mut difference = 0u64;
        for y in 0..height {
            for x in 0..width {
                let (p, q) = (a.get_pixel(x, y), b.get_pixel(x, y));
                difference += (0..3).map(|c| u64::from(p[c].abs_diff(q[c]))).sum::<u64>();
            }
        }
        let mean = difference as f64 / f64::from(width * height * 3);
        eprintln!("mean difference {mean:.2}");
        assert!(mean < 6.0, "DNG differs by {mean:.2} on average");
        let (preview, _, _) = super::read(&dst, false).unwrap();
        eprintln!("display {}x{}", preview.width(), preview.height());
        if std::env::var_os("OMALENS_TEST_DNG_OUT").is_none() {
            let _ = std::fs::remove_file(&dst);
        }
    }

    /// Display decode time for any image: OMALENS_TEST_IMAGE=x.jpg cargo test image_decode -- --ignored
    #[test]
    #[ignore]
    fn image_decode_time() {
        let path = std::path::PathBuf::from(std::env::var("OMALENS_TEST_IMAGE").unwrap());
        for _ in 0..3 {
            let started = std::time::Instant::now();
            let decoded = super::decode(&path, 8192).unwrap();
            eprintln!("decode {:?} in {:?}", decoded.size, started.elapsed());
        }
    }

    /// The half-size display develop matches the full develop, scaled down.
    /// Needs a large RAW without an embedded preview (Olympus, Samsung...):
    /// OMALENS_TEST_RAW=photo.orf cargo test --release half_develop -- --ignored
    #[test]
    #[ignore]
    fn half_develop_matches_full() {
        let path = std::path::PathBuf::from(std::env::var("OMALENS_TEST_RAW").unwrap());
        let started = std::time::Instant::now();
        let (half, _, _) = super::read(&path, false).unwrap();
        let half_time = started.elapsed();
        let started = std::time::Instant::now();
        let (full, _, _) = super::read(&path, true).unwrap();
        let full_time = started.elapsed();
        let (width, height) = (half.width(), half.height());
        let scaled = full
            .resize_exact(width, height, image::imageops::FilterType::Triangle)
            .into_rgb8();
        let half = half.into_rgb8();
        let difference: u64 = half
            .as_raw()
            .iter()
            .zip(scaled.as_raw())
            .map(|(a, b)| u64::from(a.abs_diff(*b)))
            .sum();
        let mean = difference as f64 / half.as_raw().len() as f64;
        eprintln!(
            "half {width}x{height} in {half_time:?}, full {}x{} in {full_time:?}, mean difference {mean:.2}",
            full.width(),
            full.height()
        );
        assert!(mean < 4.0, "half develop differs by {mean:.2} on average");
    }

    /// Needs a real RAW file: OMALENS_TEST_RAW=photo.rw2 cargo test -- --ignored
    #[test]
    #[ignore]
    fn raw_preview_and_develop() {
        let path = std::path::PathBuf::from(std::env::var("OMALENS_TEST_RAW").unwrap());
        let started = std::time::Instant::now();
        let decoded = super::decode(&path, 8192).unwrap();
        eprintln!("preview {:?} in {:?}", decoded.size, started.elapsed());
        let started = std::time::Instant::now();
        let (full, _, _) = super::read(&path, true).unwrap();
        eprintln!(
            "develop {}x{} in {:?}",
            full.width(),
            full.height(),
            started.elapsed()
        );
    }
}
